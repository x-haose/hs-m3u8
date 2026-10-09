//! 一条轨的窗口：与上一份播放列表比对、给新分片编号、决定录哪些、记下 init 段。

use std::collections::{BTreeSet, HashMap};
use std::ops::ControlFlow;

use hs_m3u8_hls::{InitSection, MediaPlaylist, Segment};

use super::session::{Recorded, Start};
use crate::fetch::Item;
use crate::ident::Fingerprint;
use crate::workdir::{Layout, SegmentName};
use crate::{Error, HttpError, LiveEnd, MissReason, Missed, Unsupported};

/// 媒体序号回退且与上次窗口没有重叠，连续出现这么多次才认定编码器重启；一次多半是 CDN 返回了旧缓存。
const RESTART_CONFIRMATIONS: u32 = 2;

/// 播放列表中一个分片的身份，用于检查服务器前后是否一致。
struct Listed {
    id: Fingerprint,
    /// 会话内的不连续段编号；只用于比对，不再录的旧分片可以为负
    discontinuity: i128,
}

/// 与上一份播放列表比对的结论。
enum Overlap {
    /// 有重叠且一致；带不连续段编号偏移
    Matched(i128),
    /// 没有重叠，或上一份还没有列出过分片
    Disjoint,
    /// 没有重叠，且最新的序号不超过已处理到的（疑似编码器重启）
    Regressed,
    /// 在该序号处与上一份矛盾：分片换了，或重叠部分给出的编号偏移不一致
    Inconsistent(u64),
}

/// 一条轨已处理到哪里，据此判定哪些分片要录；刷新任务拿它的副本判定要拉哪些新 init 段。
#[derive(Debug, Clone, Default)]
pub(super) struct Processed {
    /// 已处理（排入下载或记为缺失）的最大序号
    last: Option<u64>,
    /// 续录时接着的会话里还要补录的；处理完第一份非空播放列表即为 None
    refill: Option<Refill>,
}

/// 续录时补录的范围：窗口里不超过已处理的最大序号、大于 `skipped_through`、不在 `done` 中的分片。
#[derive(Debug, Clone)]
struct Refill {
    /// 这个会话开始时跳过到的序号，见 [`Recorded::skipped_through`]
    skipped_through: Option<u64>,
    /// 已录完的序号
    done: BTreeSet<u64>,
}

impl Processed {
    /// 序号为 `sequence` 的分片是否要录：比已处理的都新，或是续录时会话里没录完的。
    pub(super) fn is_new(&self, sequence: u64) -> bool {
        self.last.is_none_or(|last| sequence > last)
            || self.refill.as_ref().is_some_and(|r| {
                r.skipped_through.is_none_or(|s| sequence > s) && !r.done.contains(&sequence)
            })
    }
}

/// 与上一份比对后怎么处理这份播放列表。
enum Alignment {
    /// 照常处理
    Proceed,
    /// 疑似编码器重启但还没确认：这份不处理
    Skip,
    /// 应结束录制
    End(LiveEnd),
}

/// 处理一份播放列表需要的外部信息。
pub(super) struct Scope<'a> {
    pub track: usize,
    pub session: u32,
    pub layout: &'a Layout,
    /// [`crate::LiveOptions::max_duration`]，微秒
    pub max_us: Option<u64>,
}

/// 新分片引用、上一份播放列表里没有的 init 段，及其拉取结果。
pub(super) type NewInits = Vec<(InitSection, Result<Vec<u8>, Error>)>;

/// 处理一份播放列表的结果。
pub(super) struct Update {
    pub items: Vec<Item>,
    /// 新拉到的 init 段：(内容指纹, 内容)，须先于引用它的分片落盘
    pub init_files: Vec<(Fingerprint, Vec<u8>)>,
    /// 上次处理到的与这次窗口起点之间已滑出窗口的分片
    pub expired: Option<Missed>,
    /// init 段取不到、记为缺失的新分片，每项一个分片
    pub init_failed: Vec<Missed>,
    /// 出现了要录的新分片（排入下载或记为缺失）
    pub any_new: bool,
    /// 窗口有变化（出现新分片或窗口前移）；决定下次刷新的时刻
    pub changed: bool,
    /// 出现了 EXT-X-ENDLIST
    pub ended: bool,
}

impl Update {
    fn unchanged(ended: bool) -> Self {
        Update {
            items: Vec::new(),
            init_files: Vec::new(),
            expired: None,
            init_failed: Vec::new(),
            any_new: false,
            changed: false,
            ended,
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

/// 一份播放列表中各分片的身份与内容已知的 init 段；处理完后成为下一次比对的基准。
struct Listing {
    listed: HashMap<u64, Listed>,
    inits: Vec<(InitSection, Fingerprint)>,
}

/// 新分片所用的 init 段。
enum InitState {
    /// 分片不用 init 段
    Absent,
    Ready(Fingerprint),
    /// 取不到
    Failed(HttpError),
}

/// 一条轨的窗口状态。
pub(super) struct Window {
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
    recorded_us: u64,
}

impl Window {
    /// `recorded_us` 为之前各会话已录到的时长。
    pub(super) fn new(start: Start, recorded_us: u64) -> Self {
        let mut window = Window {
            processed: Processed::default(),
            previous: HashMap::new(),
            offset: 0,
            max_discontinuity: None,
            regressions: 0,
            inits: Vec::new(),
            group_inits: HashMap::new(),
            recorded_us,
        };
        match start {
            Start::Fresh => {}
            Start::After { through } => window.processed.last = Some(through),
            Start::Continue(earlier) => window.continue_from(&earlier),
        }
        window
    }

    /// 接着之前的会话：比对基准、编号与各组的 init 段都沿用它的分片。
    fn continue_from(&mut self, recorded: &Recorded) {
        self.previous = listed_of(recorded);
        for name in recorded.segments().values() {
            self.group_inits.insert(name.discontinuity, name.init);
        }
        self.max_discontinuity = recorded.segments().values().map(|n| n.discontinuity).max();
        self.processed.last = Some(recorded.last());
        self.processed.refill = Some(Refill {
            skipped_through: recorded.skipped_through(),
            done: recorded.segments().keys().copied().collect(),
        });
    }

    pub(super) fn processed(&self) -> &Processed {
        &self.processed
    }

    pub(super) fn recorded_us(&self) -> u64 {
        self.recorded_us
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
        // 空的播放列表（服务器还没写完）不带任何信息：不改比对基准，也不算序号回退
        if playlist.segments.is_empty() {
            return Ok(ControlFlow::Continue(Update::unchanged(playlist.ended)));
        }
        match self.align(scope.track, playlist) {
            Alignment::Proceed => {}
            Alignment::Skip => return Ok(ControlFlow::Continue(Update::unchanged(false))),
            Alignment::End(end) => return Ok(ControlFlow::Break(end)),
        }
        let window_moved =
            playlist.segments.first().map(|s| s.sequence) != self.previous.keys().min().copied();
        let mut update = Update::unchanged(playlist.ended);
        update.expired = self.expired(scope, playlist);
        let (ready, failed) = split_fetched(fetched)?;
        let listing = match self.walk(scope, playlist, &ready, &failed, &mut update)? {
            ControlFlow::Break(end) => return Ok(ControlFlow::Break(end)),
            ControlFlow::Continue(listing) => listing,
        };
        for r in ready {
            if listing.inits.iter().any(|(i, _)| *i == r.init) {
                update.init_files.push((r.fingerprint, r.data));
            }
        }
        update.changed = update.any_new || window_moved;
        self.previous = listing.listed;
        self.inits = listing.inits;
        self.processed.refill = None;
        Ok(ControlFlow::Continue(update))
    }

    /// 逐个分片记下身份与内容已知的 init 段，新分片排入下载或记为缺失（写进 `update`）。
    fn walk(
        &mut self,
        scope: &Scope<'_>,
        playlist: &MediaPlaylist,
        ready: &[ReadyInit],
        failed: &[FailedInit],
        update: &mut Update,
    ) -> Result<ControlFlow<LiveEnd, Listing>, Error> {
        let mut listing = Listing {
            listed: HashMap::with_capacity(playlist.segments.len()),
            inits: Vec::new(),
        };
        for s in &playlist.segments {
            let number = i128::from(s.discontinuity) + self.offset;
            let listed = Listed {
                id: Fingerprint::of_segment(&s.uri, s.byte_range),
                discontinuity: number,
            };
            listing.listed.insert(s.sequence, listed);
            self.note_known_init(s, ready, &mut listing.inits);
            if !self.processed.is_new(s.sequence) {
                continue;
            }
            update.any_new = true;
            let last = self
                .processed
                .last
                .map_or(s.sequence, |l| l.max(s.sequence));
            self.processed.last = Some(last);
            let Ok(discontinuity) = u64::try_from(number) else {
                return Ok(ControlFlow::Break(LiveEnd::Inconsistent {
                    track: scope.track,
                    sequence: s.sequence,
                }));
            };
            let init = new_segment_init(s, &listing.inits, failed);
            self.record_new(scope, s, discontinuity, init, update)?;
        }
        Ok(ControlFlow::Continue(listing))
    }

    /// 与上一份比对并确定本次的编号偏移。
    fn align(&mut self, track: usize, playlist: &MediaPlaylist) -> Alignment {
        match compare(&self.previous, self.processed.last, playlist) {
            Overlap::Matched(offset) => self.offset = offset,
            Overlap::Disjoint => self.offset = self.disjoint_offset(playlist),
            Overlap::Regressed => {
                self.regressions += 1;
                return if self.regressions >= RESTART_CONFIRMATIONS {
                    Alignment::End(LiveEnd::Restarted { track })
                } else {
                    Alignment::Skip
                };
            }
            Overlap::Inconsistent(sequence) => {
                return Alignment::End(LiveEnd::Inconsistent { track, sequence });
            }
        }
        self.regressions = 0;
        Alignment::Proceed
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

    /// 分片的 init 段内容已知时（上一份就有，或这次拉到了）记入本次的 `inits`。
    fn note_known_init(
        &self,
        segment: &Segment,
        ready: &[ReadyInit],
        inits: &mut Vec<(InitSection, Fingerprint)>,
    ) {
        let Some(init) = &segment.init else {
            return;
        };
        if inits.iter().any(|(i, _)| i == init) {
            return;
        }
        let known = self
            .inits
            .iter()
            .find(|(i, _)| i == init)
            .map(|(_, f)| *f)
            .or_else(|| {
                ready
                    .iter()
                    .find(|r| r.init == *init)
                    .map(|r| r.fingerprint)
            });
        if let Some(fingerprint) = known {
            inits.push((init.clone(), fingerprint));
        }
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
                update.init_failed.push(Missed {
                    session: scope.session,
                    track: scope.track,
                    first: segment.sequence,
                    last: segment.sequence,
                    reason: MissReason::InitFailed(kind),
                });
                return Ok(());
            }
        };
        self.claim_group(scope.track, discontinuity, init)?;
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

    /// 第 `discontinuity` 组用 `init`：同一组内 init 段须不变（合并时一组只用一个 init 段）。
    fn claim_group(
        &mut self,
        track: usize,
        discontinuity: u64,
        init: Option<Fingerprint>,
    ) -> Result<(), Error> {
        if *self.group_inits.entry(discontinuity).or_insert(init) != init {
            return Err(Error::Unsupported(Unsupported::InitChangesWithinGroup {
                track,
                discontinuity,
            }));
        }
        self.max_discontinuity = Some(
            self.max_discontinuity
                .map_or(discontinuity, |m| m.max(discontinuity)),
        );
        Ok(())
    }
}

/// 新分片所用 init 段的状况；`inits` 为本次内容已知的 init 段。
fn new_segment_init(
    segment: &Segment,
    inits: &[(InitSection, Fingerprint)],
    failed: &[FailedInit],
) -> InitState {
    let Some(init) = &segment.init else {
        return InitState::Absent;
    };
    if let Some((_, fingerprint)) = inits.iter().find(|(i, _)| i == init) {
        return InitState::Ready(*fingerprint);
    }
    let kind = failed
        .iter()
        .find(|f| f.init == *init)
        .map(|f| f.kind.clone())
        .expect("新分片引用的新 init 段不是拉到了就是记了失败");
    InitState::Failed(kind)
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

/// 之前会话已录的分片作为比对基准。
fn listed_of(recorded: &Recorded) -> HashMap<u64, Listed> {
    recorded
        .segments()
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

/// `playlist` 与 `recorded` 重叠、身份与编号都一致时，重叠的分片，序号从大到小；有矛盾或没有重叠时为空。
pub(super) fn overlaps<'p>(recorded: &Recorded, playlist: &'p MediaPlaylist) -> Vec<&'p Segment> {
    let previous = listed_of(recorded);
    match compare(&previous, Some(recorded.last()), playlist) {
        Overlap::Matched(_) => playlist
            .segments
            .iter()
            .rev()
            .filter(|s| previous.contains_key(&s.sequence))
            .collect(),
        _ => Vec::new(),
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
            Err(e) => match e.missable() {
                Some(kind) => failed.push(FailedInit { init, kind }),
                None => return Err(e),
            },
        }
    }
    Ok((ready, failed))
}
