//! 直播录制：按 RFC 8216 6.3.4 的节奏刷新各轨的媒体播放列表，新分片交给下载器，直到满足结束条件。
//! 合并输入只由任务目录中已完成的分片决定，因此中断后不联网也能合并。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hs_m3u8_hls::{ByteRange, InitSection, MediaPlaylist};
use hs_m3u8_remux::{DiscontinuityGroup, TrackSegments};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::fetch::{self, Fetcher, Item, ItemId};
use crate::http::Http;
use crate::plan::{self, Source};
use crate::workdir::{self, SegmentFile, WorkDir};
use crate::{
    Error, Hooks, HttpError, LiveEnd, LiveOptions, MissReason, Missed, Progress, Stage, Unsupported,
};

/// 刷新间隔的下限，防止 TARGETDURATION 为 0 时空转。
const MIN_REFRESH: Duration = Duration::from_millis(10);

/// 录制结束时的结果；`missed` 已按轨道与序号合并成区间。
pub(crate) struct Recording {
    pub end: LiveEnd,
    pub missed: Vec<Missed>,
}

/// 录制中的一条轨。
struct LiveTrack {
    url: Url,
    /// 刷新间隔的基准：TARGETDURATION，缺失时取播放列表中最长的分片时长
    target: Duration,
    /// 已排入下载的最大序号
    last: Option<u64>,
    /// 上一次播放列表中各分片的身份（去掉查询串的地址、字节范围），用于一致性检查
    previous: HashMap<u64, (String, Option<ByteRange>)>,
    /// 上一次播放列表引用的 init 段及其编号
    current_inits: Vec<(InitSection, usize)>,
    /// init 编号 → 内容；地址不同、内容相同的 init 段共用编号
    init_data: Vec<Vec<u8>>,
    /// 不连续段序号 → 该段所用的 init 编号
    group_inits: HashMap<u64, Option<usize>>,
    ended: bool,
    refreshing: bool,
    next_refresh: Instant,
}

/// 新分片引用的、上次播放列表里没有的 init 段及其内容。
type NewInits = Vec<(InitSection, Vec<u8>)>;

/// 一次刷新的结果。
struct Refreshed {
    track: usize,
    started: Instant,
    result: Result<(MediaPlaylist, NewInits), Error>,
}

/// 录制用到的共享对象。
pub(crate) struct Session<'a> {
    pub http: Arc<Http>,
    pub hooks: Arc<dyn Hooks>,
    pub dir: &'a WorkDir,
    pub fetcher: &'a mut Fetcher,
    pub progress: &'a watch::Sender<Progress>,
    pub cancel: &'a CancellationToken,
    /// 调用方要求停止录制
    pub stop: &'a CancellationToken,
}

struct Recorder<'a> {
    http: Arc<Http>,
    hooks: Arc<dyn Hooks>,
    dir: &'a WorkDir,
    progress: &'a watch::Sender<Progress>,
    cancel: &'a CancellationToken,
    stop: &'a CancellationToken,
    options: LiveOptions,
    tracks: Vec<LiveTrack>,
    missed: Vec<Missed>,
    /// 第 0 条轨已排入下载的分片声明时长之和，微秒
    recorded_us: u64,
    /// 最近一次发现新分片的时刻
    last_new: Instant,
    /// 最近一次刷新失败的原因
    last_error: Option<String>,
}

/// 录制直播，直到满足结束条件，并等在途分片完成。`sources` 为首次拉到的各轨播放列表。
pub(crate) async fn record(
    session: Session<'_>,
    sources: Vec<Source>,
    options: LiveOptions,
) -> Result<Recording, Error> {
    let Session {
        http,
        hooks,
        dir,
        fetcher,
        progress,
        cancel,
        stop,
    } = session;
    progress.send_modify(|p| p.stage = Stage::Recording);
    let now = Instant::now();
    let mut recorder = Recorder {
        http,
        hooks,
        dir,
        progress,
        cancel,
        stop,
        options,
        tracks: Vec::new(),
        missed: Vec::new(),
        recorded_us: 0,
        last_new: now,
        last_error: None,
    };
    for source in &sources {
        let target = target(&source.playlist).ok_or_else(|| {
            Error::Unsupported(Unsupported::NoTargetDuration(Box::new(source.url.clone())))
        })?;
        recorder.tracks.push(LiveTrack {
            url: source.url.clone(),
            target,
            last: None,
            previous: HashMap::new(),
            current_inits: Vec::new(),
            init_data: Vec::new(),
            group_inits: HashMap::new(),
            ended: false,
            refreshing: false,
            next_refresh: now,
        });
    }

    let mut refreshes = JoinSet::new();
    let result = async {
        // 首次拉到的播放列表按一次刷新处理
        for (track, source) in sources.into_iter().enumerate() {
            let inits = new_inits(&recorder.http, &source.playlist, &[], None, cancel).await?;
            recorder
                .apply(track, source.playlist, inits, now, fetcher)
                .await?;
        }
        recorder.run(fetcher, &mut refreshes).await
    }
    .await;
    refreshes.shutdown().await;
    let end = match result {
        Ok(end) => end,
        Err(e) => {
            fetcher.abort();
            // 在途的项随后以 Cancelled 结束，只等它们收尾，不再处理结果
            while fetcher.next().await.is_some() {}
            return Err(e);
        }
    };
    recorder.drain(fetcher).await?;
    Ok(Recording {
        end,
        missed: merge_ranges(recorder.missed),
    })
}

impl Recorder<'_> {
    async fn run(
        &mut self,
        fetcher: &mut Fetcher,
        refreshes: &mut JoinSet<Refreshed>,
    ) -> Result<LiveEnd, Error> {
        let (cancel, stop) = (self.cancel, self.stop);
        loop {
            if self.tracks.iter().all(|t| t.ended) {
                return Ok(LiveEnd::EndList);
            }
            if self.max_reached() {
                return Ok(LiveEnd::MaxDuration);
            }
            let now = Instant::now();
            for (track, t) in self.tracks.iter_mut().enumerate() {
                if !t.ended && !t.refreshing && t.next_refresh <= now {
                    t.refreshing = true;
                    let known = t.current_inits.iter().map(|(i, _)| i.clone()).collect();
                    refreshes.spawn(refresh(
                        self.http.clone(),
                        self.hooks.clone(),
                        track,
                        t.url.clone(),
                        known,
                        t.last,
                        cancel.clone(),
                    ));
                }
            }
            let next_due = self
                .tracks
                .iter()
                .filter(|t| !t.ended && !t.refreshing)
                .map(|t| t.next_refresh)
                .min();
            let stall_at = self.last_new + self.options.stall_timeout;
            tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                _ = stop.cancelled() => return Ok(LiveEnd::Stopped),
                _ = tokio::time::sleep_until(stall_at) => {
                    return Ok(LiveEnd::Stalled { last_error: self.last_error.clone() });
                }
                _ = tokio::time::sleep_until(next_due.unwrap_or(stall_at)), if next_due.is_some() => {}
                Some(joined) = refreshes.join_next() => {
                    let refreshed = match joined {
                        Ok(refreshed) => refreshed,
                        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                        Err(_) => unreachable!("刷新任务只在录制结束后统一中止"),
                    };
                    self.on_refresh(refreshed, fetcher).await?;
                }
                Some((id, result)) = fetcher.next(), if !fetcher.is_idle() => {
                    self.on_finished(id, result)?;
                }
            }
        }
    }

    fn max_reached(&self) -> bool {
        self.options
            .max_duration
            .is_some_and(|max| u128::from(self.recorded_us) >= max.as_micros())
    }

    async fn on_refresh(
        &mut self,
        refreshed: Refreshed,
        fetcher: &mut Fetcher,
    ) -> Result<(), Error> {
        let Refreshed {
            track,
            started,
            result,
        } = refreshed;
        self.tracks[track].refreshing = false;
        match result {
            Ok((playlist, inits)) => self.apply(track, playlist, inits, started, fetcher).await,
            // 网络或播放列表内容的问题：等下次刷新，持续到 stall_timeout 即结束录制
            Err(e @ (Error::Http { .. } | Error::Playlist { .. })) => {
                self.last_error = Some(e.to_string());
                let t = &mut self.tracks[track];
                t.next_refresh = Instant::now() + t.target / 2;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// 处理一次刷新得到的播放列表：检查一致性、记录窗口滑过的漏段、把新分片排入下载。
    async fn apply(
        &mut self,
        track: usize,
        playlist: MediaPlaylist,
        fetched: NewInits,
        started: Instant,
        fetcher: &mut Fetcher,
    ) -> Result<(), Error> {
        let max_us = self
            .options
            .max_duration
            .map(|max| u64::try_from(max.as_micros()).unwrap_or(u64::MAX));
        let t = &mut self.tracks[track];
        for s in &playlist.segments {
            if let Some(previous) = t.previous.get(&s.sequence)
                && *previous != (plan::identity(&s.uri), s.byte_range)
            {
                return Err(Error::LiveSegmentChanged {
                    url: Box::new(t.url.clone()),
                    sequence: s.sequence,
                });
            }
        }
        if let Some(target) = target(&playlist) {
            t.target = target;
        }
        if let (Some(last), Some(first)) = (t.last, playlist.segments.first())
            && first.sequence > last + 1
        {
            self.missed.push(Missed {
                track,
                first: last + 1,
                last: first.sequence - 1,
                reason: MissReason::Expired,
            });
            let count = usize::try_from(first.sequence - last - 1).unwrap_or(usize::MAX);
            self.progress
                .send_modify(|p| p.segments_missed = p.segments_missed.saturating_add(count));
        }

        let mut changed = playlist.ended && !t.ended;
        let mut init_files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
        let mut current_inits: Vec<(InitSection, usize)> = Vec::new();
        let mut items = Vec::new();
        for s in &playlist.segments {
            let is_new = t.last.is_none_or(|last| s.sequence > last);
            let init = match &s.init {
                Some(init) => {
                    let known = t
                        .current_inits
                        .iter()
                        .chain(&current_inits)
                        .find(|(i, _)| i == init)
                        .map(|(_, index)| *index);
                    let index = match (known, is_new) {
                        (Some(index), _) => index,
                        (None, true) => {
                            let (_, data) = fetched
                                .iter()
                                .find(|(i, _)| i == init)
                                .expect("刷新时已拉取新分片引用的、此前未见过的 init 段");
                            let dir = self.dir;
                            init_number(t, data, |index| {
                                init_files.push((dir.init(track, index), data.clone()));
                            })
                        }
                        // 旧分片引用、上次又未见过的 init 段：分片不会再下载，不需要编号
                        (None, false) => continue,
                    };
                    if !current_inits.iter().any(|(i, _)| i == init) {
                        current_inits.push((init.clone(), index));
                    }
                    Some(index)
                }
                None => None,
            };
            if !is_new {
                continue;
            }
            if track == 0 && max_us.is_some_and(|max| self.recorded_us >= max) {
                break;
            }
            match t.group_inits.get(&s.discontinuity) {
                Some(&used) if used != init => {
                    return Err(Error::Unsupported(Unsupported::InitChangesWithinGroup {
                        track,
                        discontinuity: s.discontinuity,
                    }));
                }
                Some(_) => {}
                None => {
                    t.group_inits.insert(s.discontinuity, init);
                }
            }
            items.push(Item::Segment {
                track,
                segment: Box::new(s.clone()),
                path: self.dir.segment(track, s.sequence, s.discontinuity, init),
            });
            t.last = Some(s.sequence);
            if track == 0 {
                self.recorded_us += s.duration_us;
            }
            changed = true;
        }
        t.previous = playlist
            .segments
            .iter()
            .map(|s| (s.sequence, (plan::identity(&s.uri), s.byte_range)))
            .collect();
        t.current_inits = current_inits;
        t.ended = playlist.ended;
        // RFC 8216 6.3.4：有变化后从开始加载起至少等一个 target duration，没变化时等半个
        t.next_refresh = if changed {
            started + t.target
        } else {
            Instant::now() + t.target / 2
        };

        // init 段先落盘、再下载引用它的分片：中断后合并时，分片引用的 init 段一定存在
        for (path, data) in init_files {
            let len = data.len() as u64;
            workdir::write(path, data).await?;
            self.progress.send_modify(|p| p.bytes += len);
        }
        if !items.is_empty() {
            self.last_new = Instant::now();
            let count = items.len();
            self.progress.send_modify(|p| p.segments_total += count);
        }
        for item in items {
            fetcher.push(item);
        }
        Ok(())
    }

    /// 处理一项下载结果：取不到的分片记为漏段，其余失败上抛。
    fn on_finished(&mut self, id: ItemId, result: Result<u64, Error>) -> Result<(), Error> {
        match (id, result) {
            (_, Ok(len)) => {
                self.progress.send_modify(|p| {
                    p.bytes += len;
                    if matches!(id, ItemId::Segment { .. }) {
                        p.segments_done += 1;
                    }
                });
                Ok(())
            }
            (ItemId::Segment { track, sequence }, Err(e)) if missable(&e) => {
                self.missed.push(Missed {
                    track,
                    first: sequence,
                    last: sequence,
                    reason: MissReason::Failed(e.to_string()),
                });
                self.progress.send_modify(|p| p.segments_missed += 1);
                Ok(())
            }
            (_, Err(e)) => Err(e),
        }
    }

    /// 等在途与排队的分片完成。有失败时取消其余项并返回第一个失败。
    async fn drain(&mut self, fetcher: &mut Fetcher) -> Result<(), Error> {
        let mut outcome = Ok(());
        while let Some((id, result)) = fetcher.next().await {
            if let Err(e) = self.on_finished(id, result) {
                fetcher.abort();
                if matches!(outcome, Ok(()) | Err(Error::Cancelled)) {
                    outcome = Err(e);
                }
            }
        }
        outcome
    }
}

/// 新 init 段的编号：内容与已有的相同则共用，否则分配新编号并调用 `created`。
fn init_number(t: &mut LiveTrack, data: &[u8], created: impl FnOnce(usize)) -> usize {
    match t.init_data.iter().position(|d| d == data) {
        Some(index) => index,
        None => {
            t.init_data.push(data.to_vec());
            let index = t.init_data.len() - 1;
            created(index);
            index
        }
    }
}

/// 窗口滑过之外，取不到的分片也记为漏段：404/410（已过期），以及重试后仍失败的临时故障。
fn missable(error: &Error) -> bool {
    match error {
        Error::Segment { source, .. } => {
            source.retryable()
                || matches!(
                    **source,
                    Error::Http {
                        kind: HttpError::Status(404 | 410),
                        ..
                    }
                )
        }
        _ => false,
    }
}

/// 刷新间隔的基准；播放列表既没有 TARGETDURATION 也没有分片时为 None。
fn target(playlist: &MediaPlaylist) -> Option<Duration> {
    let us = playlist
        .target_duration_us
        .or_else(|| playlist.segments.iter().map(|s| s.duration_us).max())?;
    Some(Duration::from_micros(us).max(MIN_REFRESH))
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
        let playlist = plan::fetch_media(&http, &hooks, &url, &cancel).await?;
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

/// 序号大于 `after` 的分片引用、又不在 `known` 中的 init 段，逐个拉取。
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
            let data = fetch::fetch_init(http, init, cancel).await?;
            fetched.push((init.clone(), data));
        }
    }
    Ok(fetched)
}

/// 由已完成的分片得到合并输入：只合并各轨都有的不连续段，其余记为漏段（已合并成区间）。
pub(crate) fn merge_input(
    files: &[Vec<SegmentFile>],
    dir: &WorkDir,
) -> Result<(Vec<DiscontinuityGroup>, Vec<Missed>), Error> {
    let by_group: Vec<BTreeMap<u64, Vec<&SegmentFile>>> = files
        .iter()
        .map(|track| {
            let mut groups: BTreeMap<u64, Vec<&SegmentFile>> = BTreeMap::new();
            for file in track {
                groups.entry(file.discontinuity).or_default().push(file);
            }
            groups
        })
        .collect();
    let common: BTreeSet<u64> = by_group
        .first()
        .map(|first| {
            first
                .keys()
                .filter(|d| by_group.iter().all(|g| g.contains_key(d)))
                .copied()
                .collect()
        })
        .unwrap_or_default();

    let mut missed = Vec::new();
    for (track, groups) in by_group.iter().enumerate() {
        for (discontinuity, files) in groups {
            if !common.contains(discontinuity) {
                missed.extend(files.iter().map(|f| Missed {
                    track,
                    first: f.sequence,
                    last: f.sequence,
                    reason: MissReason::Unmergeable,
                }));
            }
        }
    }
    let groups = common
        .iter()
        .map(|discontinuity| {
            let tracks = by_group
                .iter()
                .enumerate()
                .map(|(track, groups)| {
                    let files = &groups[discontinuity];
                    let init = files[0].init;
                    if files.iter().any(|f| f.init != init) {
                        return Err(Error::WorkDir {
                            path: dir.root().to_path_buf(),
                            reason: format!(
                                "第 {track} 条轨不连续段 {discontinuity} 内的分片引用了不同的 init 段"
                            ),
                        });
                    }
                    Ok(TrackSegments {
                        init: init.map(|index| dir.init(track, index)),
                        segments: files.iter().map(|f| f.path.clone()).collect(),
                    })
                })
                .collect::<Result<_, _>>()?;
            Ok(DiscontinuityGroup { tracks })
        })
        .collect::<Result<_, Error>>()?;
    Ok((groups, merge_ranges(missed)))
}

/// 漏段按轨道与序号排序，序号相接且原因相同的合成一个区间。
pub(crate) fn merge_ranges(mut missed: Vec<Missed>) -> Vec<Missed> {
    missed.sort_by_key(|m| (m.track, m.first));
    let mut merged: Vec<Missed> = Vec::new();
    for m in missed {
        match merged.last_mut() {
            Some(prev)
                if prev.track == m.track
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
