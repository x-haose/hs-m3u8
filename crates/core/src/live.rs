//! 直播录制：按 RFC 8216 6.3.4 的节奏刷新各轨的媒体播放列表，新分片交给下载器，直到满足结束条件。
//!
//! 每次运行是一次录制，分片文件名带录制次数。合并按（录制次数, 不连续段）分组，只由任务目录中
//! 已完成的分片决定：各次录制首尾相接；同一次录制内的漏段保留原时间戳，时间线留空。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hs_m3u8_hls::{self as hls, ByteRange, InitSection, MediaPlaylist};
use hs_m3u8_remux::{DiscontinuityGroup, TrackSegments};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::fetch::{self, Fetcher, Item, ItemId, record_done};
use crate::hooks::Hooks;
use crate::http::Http;
use crate::request::LiveOptions;
use crate::resolve::{self, ResolvedTrack};
use crate::workdir::{InitFile, Layout, SegmentFile, SegmentName};
use crate::{
    Error, HttpError, LiveEnd, MissReason, Missed, Progress, Stage, StallCause, Unsupported,
    WorkDirProblem, blocking,
};

/// 两次刷新之间的最短间隔，防止 TARGETDURATION 为 0 或极小时空转。
const MIN_REFRESH: Duration = Duration::from_millis(100);

/// 媒体序号回退且与上次窗口没有重叠，连续出现这么多次才认定编码器重启；一次多半是 CDN 返回了旧缓存。
const RESTART_CONFIRMATIONS: u32 = 2;

/// 一次录制的结果。
pub(crate) struct Recording {
    pub end: LiveEnd,
    /// 本次录制的编号
    pub session: u32,
    /// 本次录制中的漏段（未合并成区间）
    pub missed: Vec<Missed>,
}

/// 录制用到的共享对象。
pub(crate) struct Context<'a> {
    pub http: Arc<Http>,
    pub hooks: Arc<dyn Hooks>,
    pub layout: &'a Layout,
    pub fetcher: &'a mut Fetcher,
    pub progress: &'a watch::Sender<Progress>,
    pub cancel: &'a CancellationToken,
    /// 调用方要求停止录制
    pub stop: &'a CancellationToken,
}

enum Refresh {
    Due(Instant),
    InFlight,
    Ended,
}

/// 播放列表中一个分片的身份，用于检查服务器前后是否一致。
#[derive(PartialEq, Eq)]
struct Listed {
    /// 地址的最后一段（不含查询串）：CDN 常在主机、路径前段或查询串里放令牌，每次刷新都可能不同
    file: String,
    byte_range: Option<ByteRange>,
    /// 已换算为本次录制的不连续段编号
    discontinuity: u64,
}

/// 录制中的一条轨。
struct LiveTrack {
    url: Url,
    /// 刷新间隔的基准：TARGETDURATION，为 0 或缺失时取播放列表中最长的分片时长
    target: Duration,
    refresh: Refresh,
    /// 已处理（排入下载或记为漏段）的最大序号
    last: Option<u64>,
    /// 上一次播放列表中各分片的身份，按序号
    previous: HashMap<u64, Listed>,
    /// 播放列表的不连续段序号加上它即为本次录制的编号。服务器不写 EXT-X-DISCONTINUITY-SEQUENCE 时，
    /// 带 DISCONTINUITY 的分片滑出窗口会让编号整体变小，靠与上次重叠的分片校正
    discontinuity_offset: i128,
    /// 连续几次刷新疑似编码器重启
    regressions: u32,
    /// 上一次播放列表引用的 init 段及其编号
    current_inits: Vec<(InitSection, usize)>,
    /// init 编号 → 内容（含之前各次录制落盘的）；地址不同、内容相同的 init 段共用编号
    init_data: BTreeMap<usize, Vec<u8>>,
    /// 本次录制中不连续段编号 → 该段所用的 init 编号
    group_inits: HashMap<u64, Option<usize>>,
    /// 已排入下载的分片声明时长之和，微秒
    recorded_us: u64,
    /// 最近一次出现新分片的时刻
    last_new: Instant,
    /// 最近一次刷新失败的原因；刷新成功即清空
    last_error: Option<String>,
}

/// 新分片引用、上次播放列表里没有的 init 段，及其拉取结果。
type NewInits = Vec<(InitSection, Result<Vec<u8>, Error>)>;

/// 拉到的 init 段及其内容。
type FetchedInits = Vec<(InitSection, Vec<u8>)>;

/// 取不到的 init 段及原因；引用它们的新分片记为漏段。
type FailedInits = Vec<(InitSection, HttpError)>;

/// 一次刷新的结果。
struct Refreshed {
    track: usize,
    started: Instant,
    result: Result<(MediaPlaylist, NewInits), Error>,
}

/// 与上一次播放列表比对的结论。
enum Overlap {
    /// 一致；带本次的不连续段编号偏移
    Consistent(i128),
    /// 同一序号的分片变了
    Changed(u64),
    /// 疑似编码器重启：序号回退且与上次窗口没有重叠
    Regressed,
}

/// 一次刷新中新出现的内容。
struct NewWork {
    items: Vec<Item>,
    /// 新的 init 段：(路径, 内容)，须先于引用它的分片落盘
    init_files: Vec<(PathBuf, Vec<u8>)>,
    missed: Vec<Missed>,
    listed: HashMap<u64, Listed>,
    current_inits: Vec<(InitSection, usize)>,
    /// 出现了新分片（排入下载或记为漏段）
    any_new: bool,
}

struct Recorder<'a> {
    http: Arc<Http>,
    hooks: Arc<dyn Hooks>,
    layout: &'a Layout,
    progress: &'a watch::Sender<Progress>,
    options: LiveOptions,
    session: u32,
    tracks: Vec<LiveTrack>,
    missed: Vec<Missed>,
}

/// 第 `session` 次录制：从 `tracks` 首次拉到的播放列表开始，直到满足结束条件，并等已列出的分片下完。
/// `inits` 为各轨之前落盘的 init 段，按内容与新拉到的 init 段对齐编号。
pub(crate) async fn record(
    ctx: Context<'_>,
    tracks: Vec<ResolvedTrack>,
    options: LiveOptions,
    session: u32,
    inits: Vec<Vec<InitFile>>,
) -> Result<Recording, Error> {
    let Context {
        http,
        hooks,
        layout,
        fetcher,
        progress,
        cancel,
        stop,
    } = ctx;
    progress.send_modify(|p| p.stage = Stage::Recording);
    let init_data = blocking(move || read_inits(inits)).await??;
    let now = Instant::now();
    let mut recorder = Recorder {
        http,
        hooks,
        layout,
        progress,
        options,
        session,
        tracks: Vec::new(),
        missed: Vec::new(),
    };
    for (track, init_data) in tracks.iter().zip(init_data) {
        let target = target(&track.playlist).ok_or_else(|| {
            Error::Unsupported(Unsupported::NoTargetDuration(Box::new(track.url.clone())))
        })?;
        recorder.tracks.push(LiveTrack {
            url: track.url.clone(),
            target,
            refresh: Refresh::Due(now),
            last: None,
            previous: HashMap::new(),
            discontinuity_offset: 0,
            regressions: 0,
            current_inits: Vec::new(),
            init_data,
            group_inits: HashMap::new(),
            recorded_us: 0,
            last_new: now,
            last_error: None,
        });
    }

    let refresh_cancel = cancel.child_token();
    let mut refreshes = JoinSet::new();
    let result = async {
        // 首次拉到的播放列表按一次刷新处理
        for (index, track) in tracks.into_iter().enumerate() {
            let inits = new_inits(&recorder.http, &track.playlist, &[], None, cancel).await?;
            if let Some(end) = recorder
                .apply(index, track.playlist, inits, now, fetcher)
                .await?
            {
                return Ok(end);
            }
        }
        recorder
            .run(fetcher, &mut refreshes, &refresh_cancel, cancel, stop)
            .await
    }
    .await;
    // 刷新任务协作取消：等在途的回调返回后才结束，不直接中止
    refresh_cancel.cancel();
    while let Some(joined) = refreshes.join_next().await {
        if let Err(e) = joined
            && e.is_panic()
        {
            std::panic::resume_unwind(e.into_panic());
        }
    }
    match result {
        Ok(end) => {
            fetcher.drain(|id, r| recorder.on_finished(id, r)).await?;
            Ok(Recording {
                end,
                session,
                missed: recorder.missed,
            })
        }
        Err(e) => {
            fetcher.abort();
            // 在途的项随后以 Cancelled 结束，只等它们收尾，不再处理结果
            while fetcher.next().await.is_some() {}
            Err(e)
        }
    }
}

/// 读入之前落盘的 init 段内容。
fn read_inits(inits: Vec<Vec<InitFile>>) -> Result<Vec<BTreeMap<usize, Vec<u8>>>, Error> {
    inits
        .into_iter()
        .map(|files| {
            files
                .into_iter()
                .map(|f| {
                    std::fs::read(&f.path)
                        .map(|data| (f.index, data))
                        .map_err(|cause| Error::Io {
                            action: "读取",
                            path: f.path,
                            cause,
                        })
                })
                .collect()
        })
        .collect()
}

impl Recorder<'_> {
    async fn run(
        &mut self,
        fetcher: &mut Fetcher,
        refreshes: &mut JoinSet<Refreshed>,
        refresh_cancel: &CancellationToken,
        cancel: &CancellationToken,
        stop: &CancellationToken,
    ) -> Result<LiveEnd, Error> {
        loop {
            if self
                .tracks
                .iter()
                .all(|t| matches!(t.refresh, Refresh::Ended))
            {
                return Ok(LiveEnd::EndList);
            }
            if self.tracks.iter().all(|t| self.finished(t)) {
                return Ok(LiveEnd::MaxDuration);
            }
            let now = Instant::now();
            self.start_due_refreshes(now, refreshes, refresh_cancel);
            let next_due = self
                .tracks
                .iter()
                .filter(|t| !self.finished(t))
                .filter_map(|t| match t.refresh {
                    Refresh::Due(at) => Some(at),
                    Refresh::InFlight | Refresh::Ended => None,
                })
                .min();
            let stall = self.next_stall();
            tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                _ = stop.cancelled() => return Ok(LiveEnd::Stopped),
                _ = sleep_until(stall.map(|(at, _)| at)), if stall.is_some() => {
                    let (_, track) = stall.expect("分支只在有停滞时刻时启用");
                    return Ok(self.stalled(track));
                }
                _ = sleep_until(next_due), if next_due.is_some() => {}
                Some(joined) = refreshes.join_next() => {
                    let refreshed = match joined {
                        Ok(refreshed) => refreshed,
                        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                        Err(_) => unreachable!("刷新任务只经协作取消结束，不被中止"),
                    };
                    if let Some(end) = self.on_refresh(refreshed, fetcher).await? {
                        return Ok(end);
                    }
                }
                Some((id, result)) = fetcher.next(), if !fetcher.is_idle() => {
                    self.on_finished(id, result)?;
                }
            }
        }
    }

    /// 该轨不再需要刷新：已出现 ENDLIST，或已录满 max_duration。
    fn finished(&self, t: &LiveTrack) -> bool {
        matches!(t.refresh, Refresh::Ended)
            || self
                .options
                .max_duration
                .is_some_and(|max| u128::from(t.recorded_us) >= max.as_micros())
    }

    fn start_due_refreshes(
        &mut self,
        now: Instant,
        refreshes: &mut JoinSet<Refreshed>,
        cancel: &CancellationToken,
    ) {
        let due: Vec<usize> = (0..self.tracks.len())
            .filter(|&i| {
                let t = &self.tracks[i];
                !self.finished(t) && matches!(t.refresh, Refresh::Due(at) if at <= now)
            })
            .collect();
        for index in due {
            let t = &mut self.tracks[index];
            t.refresh = Refresh::InFlight;
            let known = t.current_inits.iter().map(|(i, _)| i.clone()).collect();
            refreshes.spawn(refresh(
                self.http.clone(),
                self.hooks.clone(),
                index,
                t.url.clone(),
                known,
                t.last,
                cancel.clone(),
            ));
        }
    }

    /// 最早停滞的轨及其时刻；时长大到无法表示时视为不会停滞。
    fn next_stall(&self) -> Option<(Instant, usize)> {
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| !self.finished(t))
            .filter_map(|(i, t)| Some((t.last_new.checked_add(self.options.stall_timeout)?, i)))
            .min()
    }

    fn stalled(&self, track: usize) -> LiveEnd {
        let t = &self.tracks[track];
        let cause = match (&t.refresh, &t.last_error) {
            (Refresh::InFlight, _) => StallCause::RefreshPending,
            (_, Some(error)) => StallCause::RefreshFailed(error.clone()),
            (_, None) => StallCause::NoNewSegments,
        };
        LiveEnd::Stalled { track, cause }
    }

    async fn on_refresh(
        &mut self,
        refreshed: Refreshed,
        fetcher: &mut Fetcher,
    ) -> Result<Option<LiveEnd>, Error> {
        let Refreshed {
            track,
            started,
            result,
        } = refreshed;
        match result {
            Ok((playlist, inits)) => {
                self.tracks[track].last_error = None;
                self.apply(track, playlist, inits, started, fetcher).await
            }
            Err(e) if waitable_refresh_error(&e) => {
                let t = &mut self.tracks[track];
                t.last_error = Some(e.to_string());
                t.refresh = Refresh::Due(Instant::now() + (t.target / 2).max(MIN_REFRESH));
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// 处理一次刷新得到的播放列表；服务器前后矛盾时返回结束原因。
    async fn apply(
        &mut self,
        track: usize,
        playlist: MediaPlaylist,
        fetched: NewInits,
        started: Instant,
        fetcher: &mut Fetcher,
    ) -> Result<Option<LiveEnd>, Error> {
        let t = &mut self.tracks[track];
        let offset = match compare_with_previous(t, &playlist) {
            Overlap::Consistent(offset) => offset,
            Overlap::Changed(sequence) => {
                return Ok(Some(LiveEnd::SegmentChanged { track, sequence }));
            }
            Overlap::Regressed => {
                t.regressions += 1;
                if t.regressions >= RESTART_CONFIRMATIONS {
                    return Ok(Some(LiveEnd::Restarted { track }));
                }
                t.refresh = Refresh::Due(Instant::now() + (t.target / 2).max(MIN_REFRESH));
                return Ok(None);
            }
        };
        t.regressions = 0;
        t.discontinuity_offset = offset;
        if let Some(target) = target(&playlist) {
            t.target = target;
        }
        self.note_expired(track, &playlist);
        let work = match self.collect_new(track, &playlist, fetched)? {
            Ok(work) => work,
            Err(end) => return Ok(Some(end)),
        };
        self.commit(track, &playlist, work, started, fetcher)
            .await?;
        Ok(None)
    }

    /// 上次已处理的最大序号与本次窗口起点之间的分片已滑出窗口，记为漏段。
    fn note_expired(&mut self, track: usize, playlist: &MediaPlaylist) {
        let t = &self.tracks[track];
        let (Some(last), Some(first)) = (t.last, playlist.segments.first()) else {
            return;
        };
        let Some(gap_start) = last.checked_add(1) else {
            return;
        };
        if first.sequence <= gap_start {
            return;
        }
        let gap_end = first.sequence - 1;
        self.missed.push(Missed {
            session: self.session,
            track,
            first: gap_start,
            last: gap_end,
            reason: MissReason::Expired,
        });
        let count = usize::try_from(gap_end - gap_start + 1).unwrap_or(usize::MAX);
        self.progress
            .send_modify(|p| p.segments_missed = p.segments_missed.saturating_add(count));
    }

    /// 整理本次播放列表：新分片排入下载或记为漏段，新 init 段按内容编号。
    /// 外层 `Err` 为任务失败；内层 `Err` 为服务器前后矛盾，应结束录制。
    fn collect_new(
        &mut self,
        track: usize,
        playlist: &MediaPlaylist,
        fetched: NewInits,
    ) -> Result<Result<NewWork, LiveEnd>, Error> {
        let (fetched, failed) = split_fetched(fetched)?;
        let max_us = self
            .options
            .max_duration
            .map(|max| u64::try_from(max.as_micros()).unwrap_or(u64::MAX));
        let (session, layout) = (self.session, self.layout);
        let t = &mut self.tracks[track];
        let mut work = NewWork {
            items: Vec::new(),
            init_files: Vec::new(),
            missed: Vec::new(),
            listed: HashMap::new(),
            current_inits: Vec::new(),
            any_new: false,
        };
        for s in &playlist.segments {
            let Some(discontinuity) = to_session_number(s.discontinuity, t.discontinuity_offset)
            else {
                return Ok(Err(LiveEnd::SegmentChanged {
                    track,
                    sequence: s.sequence,
                }));
            };
            work.listed.insert(
                s.sequence,
                Listed {
                    file: file_name(&s.uri),
                    byte_range: s.byte_range,
                    discontinuity,
                },
            );
            let is_new = t.last.is_none_or(|last| s.sequence > last);
            let init = match &s.init {
                None => Some(None),
                Some(init) => {
                    let known = t
                        .current_inits
                        .iter()
                        .chain(&work.current_inits)
                        .find(|(i, _)| i == init)
                        .map(|(_, index)| *index);
                    let index = match known {
                        Some(index) => Some(index),
                        None if !is_new => None,
                        None => fetched.iter().find(|(i, _)| i == init).map(|(_, data)| {
                            init_number(&mut t.init_data, data, |index| {
                                work.init_files
                                    .push((layout.init(track, index), data.clone()));
                            })
                        }),
                    };
                    if let Some(index) = index
                        && !work.current_inits.iter().any(|(i, _)| i == init)
                    {
                        work.current_inits.push((init.clone(), index));
                    }
                    index.map(Some)
                }
            };
            if !is_new {
                continue;
            }
            work.any_new = true;
            t.last = Some(s.sequence);
            let Some(init) = init else {
                // 引用的 init 段取不到：分片无法解出，记为漏段
                let kind = failed
                    .iter()
                    .find(|(i, _)| Some(i) == s.init.as_ref())
                    .map(|(_, kind)| kind.clone())
                    .expect("新分片的 init 段不是拉到了就是记了失败");
                work.missed.push(Missed {
                    session,
                    track,
                    first: s.sequence,
                    last: s.sequence,
                    reason: MissReason::Failed(kind),
                });
                continue;
            };
            if max_us.is_some_and(|max| t.recorded_us >= max) {
                continue;
            }
            match t.group_inits.get(&discontinuity) {
                Some(&used) if used != init => {
                    return Err(Error::Unsupported(Unsupported::InitChangesWithinGroup {
                        track,
                        discontinuity,
                    }));
                }
                Some(_) => {}
                None => {
                    t.group_inits.insert(discontinuity, init);
                }
            }
            let name = SegmentName {
                session,
                sequence: s.sequence,
                discontinuity,
                init,
                duration_us: s.duration_us,
            };
            work.items.push(Item::Segment {
                track,
                segment: Box::new(s.clone()),
                path: layout.segment(track, &name),
            });
            t.recorded_us = t.recorded_us.saturating_add(s.duration_us);
        }
        Ok(Ok(work))
    }

    /// 更新该轨状态与下次刷新时刻；新 init 段落盘后再把新分片排入下载。
    async fn commit(
        &mut self,
        track: usize,
        playlist: &MediaPlaylist,
        work: NewWork,
        started: Instant,
        fetcher: &mut Fetcher,
    ) -> Result<(), Error> {
        let now = Instant::now();
        let t = &mut self.tracks[track];
        let window_moved =
            playlist.segments.first().map(|s| s.sequence) != t.previous.keys().min().copied();
        let changed = work.any_new || window_moved;
        t.previous = work.listed;
        t.current_inits = work.current_inits;
        if work.any_new {
            t.last_new = now;
        }
        t.refresh = if playlist.ended {
            Refresh::Ended
        } else {
            // RFC 8216 6.3.4：有变化后从开始加载起至少等一个 target duration，没变化时等半个
            let at = if changed {
                started + t.target
            } else {
                now + t.target / 2
            };
            Refresh::Due(at.max(now + MIN_REFRESH))
        };

        // init 段先落盘、再下载引用它的分片：中断后合并时，分片引用的 init 段一定存在
        for (path, data) in work.init_files {
            let len = data.len() as u64;
            crate::workdir::write(path, data).await?;
            self.progress.send_modify(|p| p.bytes += len);
        }
        let (scheduled, missed) = (work.items.len(), work.missed.len());
        self.progress.send_modify(|p| {
            p.segments_total += scheduled;
            p.segments_missed += missed;
        });
        self.missed.extend(work.missed);
        for item in work.items {
            fetcher.push(item);
        }
        Ok(())
    }

    /// 处理一项下载结果：取不到的分片记为漏段，其余失败上抛。
    fn on_finished(&mut self, id: ItemId, result: Result<u64, Error>) -> Result<(), Error> {
        match (id, result) {
            (_, Ok(len)) => {
                record_done(self.progress, id, len);
                Ok(())
            }
            (ItemId::Segment { track, sequence }, Err(e)) => match missable_segment(&e) {
                Some(kind) => {
                    self.missed.push(Missed {
                        session: self.session,
                        track,
                        first: sequence,
                        last: sequence,
                        reason: MissReason::Failed(kind),
                    });
                    self.progress.send_modify(|p| p.segments_missed += 1);
                    Ok(())
                }
                None => Err(e),
            },
            (ItemId::Init { .. }, Err(e)) => Err(e),
        }
    }
}

/// 与上一次播放列表比对：重叠的分片身份须一致，且给出一致的不连续段编号偏移。
fn compare_with_previous(t: &LiveTrack, playlist: &MediaPlaylist) -> Overlap {
    let mut offset: Option<i128> = None;
    let mut overlapped = false;
    for s in &playlist.segments {
        let Some(previous) = t.previous.get(&s.sequence) else {
            continue;
        };
        overlapped = true;
        if previous.file != file_name(&s.uri) || previous.byte_range != s.byte_range {
            return Overlap::Changed(s.sequence);
        }
        let this = i128::from(previous.discontinuity) - i128::from(s.discontinuity);
        match offset {
            Some(existing) if existing != this => return Overlap::Changed(s.sequence),
            _ => offset = Some(this),
        }
    }
    if !overlapped
        && let (Some(last), Some(newest)) = (t.last, playlist.segments.last())
        && newest.sequence <= last
    {
        return Overlap::Regressed;
    }
    Overlap::Consistent(offset.unwrap_or(t.discontinuity_offset))
}

/// 播放列表的不连续段序号换算为本次录制的编号；结果超出 u64 时为 None（服务器编号前后矛盾）。
fn to_session_number(discontinuity: u64, offset: i128) -> Option<u64> {
    u64::try_from(i128::from(discontinuity) + offset).ok()
}

/// 地址的最后一段，不含查询串。
fn file_name(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or_default()
        .to_owned()
}

/// 拉到的 init 段分成内容与可记为漏段的失败；其余失败使任务失败。
fn split_fetched(fetched: NewInits) -> Result<(FetchedInits, FailedInits), Error> {
    let mut ok = Vec::new();
    let mut failed = Vec::new();
    for (init, result) in fetched {
        match result {
            Ok(data) => ok.push((init, data)),
            Err(e) => match missable_http(&e) {
                Some(kind) => failed.push((init, kind)),
                None => return Err(e),
            },
        }
    }
    Ok((ok, failed))
}

/// 新 init 段的编号：内容与已有的相同则共用，否则分配新编号并调用 `created`。
fn init_number(
    init_data: &mut BTreeMap<usize, Vec<u8>>,
    data: &[u8],
    created: impl FnOnce(usize),
) -> usize {
    if let Some((&index, _)) = init_data.iter().find(|(_, d)| d.as_slice() == data) {
        return index;
    }
    let index = init_data.keys().next_back().map_or(0, |last| last + 1);
    init_data.insert(index, data.to_vec());
    created(index);
    index
}

/// 取不到的请求：404/410（已过期），或重试后仍失败的临时故障。这类失败记为漏段，录制继续。
fn missable_http(error: &Error) -> Option<HttpError> {
    match error {
        Error::Http { kind, .. }
            if kind.retryable() || matches!(kind, HttpError::Status(404 | 410)) =>
        {
            Some(kind.clone())
        }
        _ => None,
    }
}

/// 分片请求本身取不到时可记为漏段；key、回调、校验等失败不在此列。
fn missable_segment(error: &Error) -> Option<HttpError> {
    match error {
        Error::Segment { cause, .. } => missable_http(cause),
        _ => None,
    }
}

/// 刷新失败中可以等下次刷新的：取不到（含 404/410，直播结束时常见）与语法错误（服务器未写完的播放列表）。
/// 其余（401/403 等、内容不是播放列表、DRM、回调出错）使任务失败。
fn waitable_refresh_error(error: &Error) -> bool {
    match error {
        Error::Playlist { cause, .. } => matches!(**cause, hls::Error::Syntax { .. }),
        _ => missable_http(error).is_some(),
    }
}

/// 刷新间隔的基准；播放列表既没有正的 TARGETDURATION 也没有分片时为 None。
fn target(playlist: &MediaPlaylist) -> Option<Duration> {
    let longest = playlist.segments.iter().map(|s| s.duration_us).max();
    let us = playlist
        .target_duration_us
        .filter(|&us| us > 0)
        .or(longest)?;
    Some(Duration::from_micros(us))
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn refresh(
    http: Arc<Http>,
    hooks: Arc<dyn Hooks>,
    track: usize,
    url: Url,
    known: Vec<InitSection>,
    after: Option<u64>,
    cancel: CancellationToken,
) -> Refreshed {
    let started = Instant::now();
    let result = async {
        let playlist = resolve::fetch_media(&http, &hooks, &url, &cancel).await?;
        let inits = new_inits(&http, &playlist, &known, after, &cancel).await?;
        Ok((playlist, inits))
    }
    .await;
    Refreshed {
        track,
        started,
        result,
    }
}

/// 拉取序号大于 `after` 的分片引用、又不在 `known` 中的 init 段；各自的失败随结果返回，只有取消中止。
async fn new_inits(
    http: &Http,
    playlist: &MediaPlaylist,
    known: &[InitSection],
    after: Option<u64>,
    cancel: &CancellationToken,
) -> Result<NewInits, Error> {
    let mut fetched = NewInits::new();
    for s in &playlist.segments {
        if after.is_some_and(|after| s.sequence <= after) {
            continue;
        }
        if let Some(init) = &s.init
            && !known.contains(init)
            && !fetched.iter().any(|(i, _)| i == init)
        {
            match fetch::fetch_init(http, init, cancel).await {
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                result => fetched.push((init.clone(), result)),
            }
        }
    }
    Ok(fetched)
}

/// 合并输入：按（录制次数, 不连续段）分组，只合并各轨都有的组；其余记为漏段（`Unmergeable`）。
/// `current` 之外的各次录制中序号的空洞记为 `Unknown` 漏段（当次的漏段已在录制时记下）。
pub(crate) fn merge_input(
    files: &[Vec<SegmentFile>],
    layout: &Layout,
    current: Option<u32>,
) -> Result<(Vec<DiscontinuityGroup>, Vec<Missed>), Error> {
    type Key = (u32, u64);
    let by_group: Vec<BTreeMap<Key, Vec<&SegmentFile>>> = files
        .iter()
        .map(|track| {
            let mut groups: BTreeMap<Key, Vec<&SegmentFile>> = BTreeMap::new();
            for file in track {
                groups
                    .entry((file.name.session, file.name.discontinuity))
                    .or_default()
                    .push(file);
            }
            groups
        })
        .collect();
    let common: BTreeSet<Key> = by_group
        .first()
        .map(|first| {
            first
                .keys()
                .filter(|k| by_group.iter().all(|g| g.contains_key(k)))
                .copied()
                .collect()
        })
        .unwrap_or_default();

    let mut missed = Vec::new();
    for (track, groups) in by_group.iter().enumerate() {
        for (key, files) in groups {
            if !common.contains(key) {
                missed.extend(files.iter().map(|f| Missed {
                    session: f.name.session,
                    track,
                    first: f.name.sequence,
                    last: f.name.sequence,
                    reason: MissReason::Unmergeable,
                }));
            }
        }
    }
    for (track, track_files) in files.iter().enumerate() {
        missed.extend(holes(track, track_files, current));
    }

    let groups = common
        .iter()
        .map(|key| {
            let tracks = by_group
                .iter()
                .enumerate()
                .map(|(track, groups)| {
                    let files = &groups[key];
                    let init = files[0].name.init;
                    if files.iter().any(|f| f.name.init != init) {
                        return Err(Error::WorkDir {
                            path: layout.root().to_path_buf(),
                            problem: WorkDirProblem::Corrupt(format!(
                                "第 {track} 条轨第 {} 次录制不连续段 {} 内的分片引用了不同的 init 段",
                                key.0, key.1
                            )),
                        });
                    }
                    Ok(TrackSegments {
                        init: init.map(|index| layout.init(track, index)),
                        segments: files.iter().map(|f| f.path.clone()).collect(),
                    })
                })
                .collect::<Result<_, _>>()?;
            Ok(DiscontinuityGroup { tracks })
        })
        .collect::<Result<_, Error>>()?;
    Ok((groups, missed))
}

/// 一条轨在 `current` 之外各次录制中，相邻两个已完成分片之间缺的序号。
fn holes(track: usize, files: &[SegmentFile], current: Option<u32>) -> Vec<Missed> {
    files
        .windows(2)
        .filter_map(|pair| {
            let (a, b) = (&pair[0].name, &pair[1].name);
            let gap_start = a.sequence.checked_add(1)?;
            (a.session == b.session && Some(a.session) != current && b.sequence > gap_start).then(
                || Missed {
                    session: a.session,
                    track,
                    first: gap_start,
                    last: b.sequence - 1,
                    reason: MissReason::Unknown,
                },
            )
        })
        .collect()
}

/// 漏段按录制次数、轨道、序号排序，序号相接且原因相同的合成一个区间。
pub(crate) fn merge_ranges(mut missed: Vec<Missed>) -> Vec<Missed> {
    missed.sort_by_key(|m| (m.session, m.track, m.first));
    let mut merged: Vec<Missed> = Vec::new();
    for m in missed {
        match merged.last_mut() {
            Some(prev)
                if (prev.session, prev.track) == (m.session, m.track)
                    && prev.reason == m.reason
                    && prev.last.checked_add(1) == Some(m.first) =>
            {
                prev.last = m.last;
            }
            _ => merged.push(m),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missed(session: u32, track: usize, range: (u64, u64), reason: MissReason) -> Missed {
        Missed {
            session,
            track,
            first: range.0,
            last: range.1,
            reason,
        }
    }

    #[test]
    fn adjacent_ranges_with_the_same_reason_merge() {
        let not_found = MissReason::Failed(HttpError::Status(404));
        let merged = merge_ranges(vec![
            missed(0, 0, (5, 5), not_found.clone()),
            missed(0, 0, (3, 4), not_found.clone()),
            missed(0, 0, (6, 6), MissReason::Expired),
            missed(0, 1, (7, 7), not_found.clone()),
            missed(1, 0, (7, 7), not_found.clone()),
            missed(0, 0, (u64::MAX, u64::MAX), not_found.clone()),
        ]);
        assert_eq!(
            merged,
            vec![
                missed(0, 0, (3, 5), not_found.clone()),
                missed(0, 0, (6, 6), MissReason::Expired),
                missed(0, 0, (u64::MAX, u64::MAX), not_found.clone()),
                missed(0, 1, (7, 7), not_found.clone()),
                missed(1, 0, (7, 7), not_found),
            ]
        );
    }

    #[test]
    fn discontinuity_offset_converts_within_u64() {
        assert_eq!(to_session_number(0, 3), Some(3));
        assert_eq!(to_session_number(5, -5), Some(0));
        assert_eq!(to_session_number(0, -1), None);
        assert_eq!(to_session_number(u64::MAX, 1), None);
    }

    #[test]
    fn file_name_ignores_host_path_prefix_and_query() {
        let a = Url::parse("https://edge1.cdn/tok1/v/seg100.ts?sig=1").unwrap();
        let b = Url::parse("https://edge9.cdn/tok2/v/seg100.ts?sig=2").unwrap();
        assert_eq!(file_name(&a), file_name(&b));
        assert_eq!(file_name(&a), "seg100.ts");
    }
}
