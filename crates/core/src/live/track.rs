//! 录制中的一条轨：与上一次播放列表比对、给新分片编号并决定录哪些、记下 init 段，以及判定停滞的原因。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::ControlFlow;
use std::time::Duration;

use hs_m3u8_hls::{InitSection, MediaPlaylist, Segment};
use tokio::time::Instant;
use url::Url;

use crate::fetch::Item;
use crate::ident::Fingerprint;
use crate::workdir::{Layout, SegmentFile, SegmentName};
use crate::{Error, HttpError, LiveEnd, MissReason, Missed, StallCause, StallError, Unsupported};

/// 媒体序号回退且与上次窗口没有重叠，连续出现这么多次才认定编码器重启；一次多半是 CDN 返回了旧缓存。
const RESTART_CONFIRMATIONS: u32 = 2;

pub(super) enum Refresh {
    Due(Instant),
    InFlight { started: Instant },
    Ended,
}

/// 播放列表中一个分片的身份，用于检查服务器前后是否一致。
struct Listed {
    id: Fingerprint,
    /// 会话内的不连续段编号；只用于比对，不再录的旧分片可以为负
    discontinuity: i128,
}

/// 与上一次播放列表比对的结论。
enum Overlap {
    /// 有重叠且一致；带不连续段编号偏移
    Matched(i128),
    /// 没有重叠，或上一次还没有列出过分片
    Disjoint,
    /// 没有重叠，且最新的序号不超过已处理到的（疑似编码器重启）
    Regressed,
    /// 在该序号处与上次矛盾：分片换了，或重叠部分给出的编号偏移不一致
    Inconsistent(u64),
}

/// 一条轨在会话开始时的状况。
pub(super) enum Start {
    /// 新会话，窗口内的分片全部录
    Fresh,
    /// 新会话，序号不超过 `through` 的分片之前已录过，跳过
    After { through: u64 },
    /// 接着之前的会话：沿用其编号，补录窗口内尚未录完的分片
    Continue(Earlier),
}

/// 之前的会话中这条轨已录完的分片，按序号。
pub(super) struct Earlier(BTreeMap<u64, SegmentName>);

impl Earlier {
    /// 第 `session` 个会话的分片；一个都没有时为 None。
    pub(super) fn of(files: &[SegmentFile], session: u32) -> Option<Self> {
        let segments: BTreeMap<u64, SegmentName> = files
            .iter()
            .filter(|f| f.name.session == session)
            .map(|f| (f.name.sequence, f.name))
            .collect();
        (!segments.is_empty()).then_some(Earlier(segments))
    }

    pub(super) fn last(&self) -> u64 {
        *self.0.keys().next_back().expect("Earlier 至少有一个分片")
    }

    /// `playlist` 能否接着这些分片录：与之有重叠，且重叠部分身份与编号都一致。
    pub(super) fn continues_into(&self, playlist: &MediaPlaylist) -> bool {
        matches!(
            compare(&self.listed(), Some(self.last()), playlist),
            Overlap::Matched(_)
        )
    }

    fn listed(&self) -> HashMap<u64, Listed> {
        self.0
            .iter()
            .map(|(&sequence, name)| {
                let listed = Listed {
                    id: name.id,
                    discontinuity: i128::from(name.discontinuity),
                };
                (sequence, listed)
            })
            .collect()
    }
}

/// 一条轨已处理到哪里，据此判定哪些分片要录；刷新任务拿它的副本判定要拉哪些新 init 段。
#[derive(Debug, Clone, Default)]
pub(super) struct Processed {
    /// 已处理（排入下载或记为缺失）的最大序号
    last: Option<u64>,
    /// 续录时之前已录完的序号：不超过 `last` 而不在其中的分片还要补录；处理完第一份非空播放列表即清空
    earlier: BTreeSet<u64>,
}

impl Processed {
    /// 序号为 `sequence` 的分片是否要录：比已处理的都新，或是续录时之前没录完的。
    pub(super) fn is_new(&self, sequence: u64) -> bool {
        self.last.is_none_or(|last| sequence > last)
            || (!self.earlier.is_empty() && !self.earlier.contains(&sequence))
    }
}

/// 处理一份播放列表需要的外部信息。
pub(super) struct Scope<'a> {
    pub track: usize,
    pub session: u32,
    pub layout: &'a Layout,
    /// [`crate::LiveOptions::max_duration`]，微秒
    pub max_us: Option<u64>,
}

/// 新分片引用、上次播放列表里没有的 init 段，及其拉取结果。
pub(super) type NewInits = Vec<(InitSection, Result<Vec<u8>, Error>)>;

/// 处理一份播放列表的结果。
pub(super) struct Update {
    pub items: Vec<Item>,
    /// 新拉到的 init 段：(内容指纹, 内容)，须先于引用它的分片落盘
    pub init_files: Vec<(Fingerprint, Vec<u8>)>,
    pub missed: Vec<Missed>,
    /// 窗口有变化（出现新分片或窗口前移）；决定下次刷新的时刻
    pub changed: bool,
    /// 出现了 EXT-X-ENDLIST
    pub ended: bool,
}

impl Update {
    fn unchanged() -> Self {
        Update {
            items: Vec::new(),
            init_files: Vec::new(),
            missed: Vec::new(),
            changed: false,
            ended: false,
        }
    }
}

/// 拉到的新 init 段。
struct ReadyInit {
    init: InitSection,
    fingerprint: Fingerprint,
    data: Vec<u8>,
}

/// 重试后仍取不到的新 init 段；引用它的新分片记为缺失。
struct FailedInit {
    init: InitSection,
    kind: HttpError,
}

/// 新分片所用的 init 段。
enum InitState {
    /// 分片不用 init 段
    Absent,
    Ready(Fingerprint),
    /// 重试后仍取不到
    Failed(HttpError),
}

/// 录制中的一条轨。
pub(super) struct LiveTrack {
    pub url: Url,
    /// 刷新间隔的基准：TARGETDURATION，为 0 或缺失时取播放列表中最长的分片时长
    pub target: Duration,
    pub refresh: Refresh,
    processed: Processed,
    /// 上一份非空播放列表中各分片的身份，按序号
    previous: HashMap<u64, Listed>,
    /// 播放列表的不连续段序号加上它即为会话内的编号
    offset: i128,
    /// 会话内已排入下载的分片用过的最大不连续段编号
    max_discontinuity: Option<u64>,
    /// 连续几次刷新疑似编码器重启
    regressions: u32,
    /// 上一份播放列表引用、内容已知的 init 段
    inits: Vec<(InitSection, Fingerprint)>,
    /// 会话内不连续段编号 → 该段所用 init 段的内容指纹
    group_inits: HashMap<u64, Option<Fingerprint>>,
    /// 已排入下载与记为缺失的分片声明时长之和（含之前各会话录到的），微秒
    pub recorded_us: u64,
    /// 最近一次有分片下载成功的时刻（或录制开始）
    pub last_recorded: Instant,
    /// 最近一次列出新分片的时刻（或录制开始）
    last_listed: Instant,
    /// 最近一次刷新失败的原因；刷新成功即清空
    pub last_error: Option<Error>,
}

impl LiveTrack {
    /// `recorded_us` 为之前各会话已录到的时长。
    pub(super) fn new(
        url: Url,
        target: Duration,
        start: Start,
        recorded_us: u64,
        now: Instant,
    ) -> Self {
        let mut track = LiveTrack {
            url,
            target,
            refresh: Refresh::Due(now),
            processed: Processed::default(),
            previous: HashMap::new(),
            offset: 0,
            max_discontinuity: None,
            regressions: 0,
            inits: Vec::new(),
            group_inits: HashMap::new(),
            recorded_us,
            last_recorded: now,
            last_listed: now,
            last_error: None,
        };
        match start {
            Start::Fresh => {}
            Start::After { through } => track.processed.last = Some(through),
            Start::Continue(earlier) => {
                track.processed.last = Some(earlier.last());
                track.previous = earlier.listed();
                track.max_discontinuity = earlier.0.values().map(|n| n.discontinuity).max();
                for name in earlier.0.values() {
                    track.group_inits.insert(name.discontinuity, name.init);
                }
                track.processed.earlier = earlier.0.into_keys().collect();
            }
        }
        track
    }

    pub(super) fn processed(&self) -> &Processed {
        &self.processed
    }

    /// 上一份播放列表引用、内容已知的 init 段。
    pub(super) fn known_inits(&self) -> Vec<InitSection> {
        self.inits.iter().map(|(init, _)| init.clone()).collect()
    }

    /// 处理一份播放列表：与上一份比对，新分片排入下载或记为缺失。`fetched` 为新分片引用的新 init 段。
    /// `Break` 为服务器前后矛盾或编码器重启，应结束录制；`Err` 为任务失败。
    pub(super) fn update(
        &mut self,
        scope: &Scope<'_>,
        playlist: &MediaPlaylist,
        fetched: NewInits,
    ) -> Result<ControlFlow<LiveEnd, Update>, Error> {
        let track = scope.track;
        // 空的播放列表（服务器还没写完）不带任何信息：不改比对基准，也不算序号回退
        if playlist.segments.is_empty() {
            return Ok(ControlFlow::Continue(Update {
                ended: playlist.ended,
                ..Update::unchanged()
            }));
        }
        match compare(&self.previous, self.processed.last, playlist) {
            Overlap::Matched(offset) => self.offset = offset,
            Overlap::Disjoint => self.offset = self.disjoint_offset(playlist),
            Overlap::Regressed => {
                self.regressions += 1;
                if self.regressions >= RESTART_CONFIRMATIONS {
                    return Ok(ControlFlow::Break(LiveEnd::Restarted { track }));
                }
                return Ok(ControlFlow::Continue(Update::unchanged()));
            }
            Overlap::Inconsistent(sequence) => {
                return Ok(ControlFlow::Break(LiveEnd::Inconsistent {
                    track,
                    sequence,
                }));
            }
        }
        self.regressions = 0;
        if let Some(target) = target(playlist) {
            self.target = target;
        }
        let window_moved =
            playlist.segments.first().map(|s| s.sequence) != self.previous.keys().min().copied();
        let mut update = Update {
            ended: playlist.ended,
            ..Update::unchanged()
        };
        update.missed.extend(self.expired(scope, playlist));
        let (ready, failed) = split_fetched(fetched)?;
        let mut inits: Vec<(InitSection, Fingerprint)> = Vec::new();
        let mut any_new = false;
        let mut listed = HashMap::with_capacity(playlist.segments.len());
        for s in &playlist.segments {
            let number = i128::from(s.discontinuity) + self.offset;
            listed.insert(
                s.sequence,
                Listed {
                    id: Fingerprint::of_segment(&s.uri, s.byte_range),
                    discontinuity: number,
                },
            );
            let is_new = self.processed.is_new(s.sequence);
            let init = self.init_state(s, is_new, &ready, &failed, &mut inits);
            if !is_new {
                continue;
            }
            any_new = true;
            self.processed.last = Some(
                self.processed
                    .last
                    .map_or(s.sequence, |l| l.max(s.sequence)),
            );
            let Ok(discontinuity) = u64::try_from(number) else {
                return Ok(ControlFlow::Break(LiveEnd::Inconsistent {
                    track,
                    sequence: s.sequence,
                }));
            };
            self.record_new(scope, s, discontinuity, init, &mut update)?;
        }
        for r in ready {
            if inits.iter().any(|(i, _)| *i == r.init) {
                update.init_files.push((r.fingerprint, r.data));
            }
        }
        update.changed = any_new || window_moved;
        if any_new {
            self.last_listed = Instant::now();
        }
        self.previous = listed;
        self.inits = inits;
        self.processed.earlier.clear();
        Ok(ControlFlow::Continue(update))
    }

    /// 没有重叠时的编号偏移：沿用当前偏移。不连续段序号按 RFC 8216 本是绝对的（没写
    /// EXT-X-DISCONTINUITY-SEQUENCE 即为 0），偏移恒为 0；服务器不守规定时，当前偏移来自上次重叠的校正。
    /// 这样得出的编号比已用过的小，说明其间编号对不上，新分片另起一组，以免打乱分组顺序。
    fn disjoint_offset(&self, playlist: &MediaPlaylist) -> i128 {
        let first = i128::from(playlist.segments[0].discontinuity);
        match self.max_discontinuity.map(i128::from) {
            Some(max) if first + self.offset < max => max + 1 - first,
            _ => self.offset,
        }
    }

    /// 已处理的最大序号与本次窗口起点之间的分片已滑出窗口，记为缺失。
    fn expired(&self, scope: &Scope<'_>, playlist: &MediaPlaylist) -> Option<Missed> {
        let gap_start = self.processed.last?.checked_add(1)?;
        let first = playlist.segments.first()?.sequence;
        (first > gap_start).then(|| Missed {
            session: scope.session,
            track: scope.track,
            first: gap_start,
            last: first - 1,
            reason: MissReason::Expired,
        })
    }

    /// 分片所用 init 段的状况，并把内容已知的记入本次的 `inits`。不录的分片只记已知的，不查失败。
    fn init_state(
        &self,
        segment: &Segment,
        is_new: bool,
        ready: &[ReadyInit],
        failed: &[FailedInit],
        inits: &mut Vec<(InitSection, Fingerprint)>,
    ) -> InitState {
        let Some(init) = &segment.init else {
            return InitState::Absent;
        };
        let known = inits
            .iter()
            .chain(&self.inits)
            .find(|(i, _)| i == init)
            .map(|(_, f)| *f)
            .or_else(|| {
                ready
                    .iter()
                    .find(|r| r.init == *init)
                    .map(|r| r.fingerprint)
            });
        if let Some(fingerprint) = known {
            if !inits.iter().any(|(i, _)| i == init) {
                inits.push((init.clone(), fingerprint));
            }
            return InitState::Ready(fingerprint);
        }
        if !is_new {
            return InitState::Absent;
        }
        let kind = failed
            .iter()
            .find(|f| f.init == *init)
            .map(|f| f.kind.clone())
            .expect("新分片引用的新 init 段不是拉到了就是记了失败");
        InitState::Failed(kind)
    }

    /// 一个新分片：录满 max_duration 后不再录；init 段取不到时记为缺失；其余排入下载。
    fn record_new(
        &mut self,
        scope: &Scope<'_>,
        segment: &Segment,
        discontinuity: u64,
        init: InitState,
        update: &mut Update,
    ) -> Result<(), Error> {
        if scope.max_us.is_some_and(|max| self.recorded_us >= max) {
            return Ok(());
        }
        self.recorded_us = self.recorded_us.saturating_add(segment.duration_us);
        let init = match init {
            InitState::Absent => None,
            InitState::Ready(fingerprint) => Some(fingerprint),
            InitState::Failed(kind) => {
                update.missed.push(Missed {
                    session: scope.session,
                    track: scope.track,
                    first: segment.sequence,
                    last: segment.sequence,
                    reason: MissReason::InitFailed(kind),
                });
                return Ok(());
            }
        };
        match self.group_inits.get(&discontinuity) {
            Some(&used) if used != init => {
                return Err(Error::Unsupported(Unsupported::InitChangesWithinGroup {
                    track: scope.track,
                    discontinuity,
                }));
            }
            Some(_) => {}
            None => {
                self.group_inits.insert(discontinuity, init);
            }
        }
        self.max_discontinuity = Some(
            self.max_discontinuity
                .map_or(discontinuity, |m| m.max(discontinuity)),
        );
        let name = SegmentName {
            session: scope.session,
            sequence: segment.sequence,
            discontinuity,
            init,
            duration_us: segment.duration_us,
            id: Fingerprint::of_segment(&segment.uri, segment.byte_range),
        };
        update.items.push(Item {
            track: scope.track,
            segment: Box::new(segment.clone()),
            path: scope.layout.segment(scope.track, &name),
        });
        Ok(())
    }

    /// 停滞的时刻；时长大到无法表示时为 None（不会停滞）。
    pub(super) fn stall_at(&self, stall_timeout: Duration) -> Option<Instant> {
        // 至少等三个目标时长：目标时长比 stall_timeout 还长时，正常的直播两次出新分片之间也会超过它
        self.last_recorded
            .checked_add(stall_timeout.max(self.target.saturating_mul(3)))
    }

    /// 停滞的结论：`Ok` 为看起来直播已结束，`Err` 为故障。
    pub(super) fn stall(&mut self, now: Instant) -> Result<StallCause, StallError> {
        if let Refresh::InFlight { started } = self.refresh
            && now.duration_since(started) >= self.target
        {
            return Err(StallError::RefreshPending);
        }
        if let Some(error) = self.last_error.take() {
            return match error {
                Error::Http {
                    kind: HttpError::Status(status @ (404 | 410)),
                    ..
                } => Ok(StallCause::PlaylistGone(status)),
                error => Err(StallError::RefreshFailed(Box::new(error))),
            };
        }
        if self.last_listed > self.last_recorded {
            Err(StallError::Unrecordable)
        } else {
            Ok(StallCause::NoNewSegments)
        }
    }
}

/// 与上一份播放列表比对：重叠的分片身份须一致，且给出一致的不连续段编号偏移。
/// `last` 为已处理的最大序号；上一份还没有列出过分片时，无从比对，视为没有重叠。
fn compare(
    previous: &HashMap<u64, Listed>,
    last: Option<u64>,
    playlist: &MediaPlaylist,
) -> Overlap {
    let mut offset: Option<i128> = None;
    for s in &playlist.segments {
        let Some(listed) = previous.get(&s.sequence) else {
            continue;
        };
        if listed.id != Fingerprint::of_segment(&s.uri, s.byte_range) {
            return Overlap::Inconsistent(s.sequence);
        }
        let this = listed.discontinuity - i128::from(s.discontinuity);
        match offset {
            Some(existing) if existing != this => return Overlap::Inconsistent(s.sequence),
            _ => offset = Some(this),
        }
    }
    if let Some(offset) = offset {
        return Overlap::Matched(offset);
    }
    let newest = playlist.segments.last().map(|s| s.sequence);
    match (last, newest) {
        (Some(last), Some(newest)) if !previous.is_empty() && newest <= last => Overlap::Regressed,
        _ => Overlap::Disjoint,
    }
}

/// 拉到的 init 段分成内容与可记为缺失的失败；其余失败使任务失败。
fn split_fetched(fetched: NewInits) -> Result<(Vec<ReadyInit>, Vec<FailedInit>), Error> {
    let mut ready = Vec::new();
    let mut failed = Vec::new();
    for (init, result) in fetched {
        match result {
            Ok(data) => ready.push(ReadyInit {
                init,
                fingerprint: Fingerprint::of_content(&data),
                data,
            }),
            Err(e) => match missable_http(&e) {
                Some(kind) => failed.push(FailedInit { init, kind }),
                None => return Err(e),
            },
        }
    }
    Ok((ready, failed))
}

/// 取不到的请求：404/410（已过期），或重试后仍失败的临时故障。这类失败记为缺失，录制继续。
pub(super) fn missable_http(error: &Error) -> Option<HttpError> {
    match error {
        Error::Http { kind, .. }
            if kind.retryable() || matches!(kind, HttpError::Status(404 | 410)) =>
        {
            Some(kind.clone())
        }
        _ => None,
    }
}

/// 刷新间隔的基准；播放列表既没有正的 TARGETDURATION 也没有分片时为 None。
pub(super) fn target(playlist: &MediaPlaylist) -> Option<Duration> {
    let longest = playlist.segments.iter().map(|s| s.duration_us).max();
    let us = playlist
        .target_duration_us
        .filter(|&us| us > 0)
        .or(longest)?;
    Some(Duration::from_micros(us))
}
