//! 直播录制：按 RFC 8216 6.3.4 的节奏刷新各轨的媒体播放列表，新分片交给下载器，直到满足结束条件。
//!
//! 录制按会话组织：一个会话是一段时间线连续的录制，分片文件名带会话编号，合并按（会话, 不连续段）分组，
//! 组内保留原时间戳（缺失的分片处时间线留空），组与组首尾相接。中断后再次运行时先判定接着上一个会话录还是
//! 另起一个（见 [`session`]），再开始录。
//!
//! 轨道编号：第 0 条为所选变体（或来源本身的媒体播放列表），第 1 条（若有）为独立的音频 rendition。

mod merge;
mod session;
mod track;
mod window;

use std::convert::Infallible;
use std::future::Future;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;

use hs_m3u8_hls::{self as hls, InitSection, MediaPlaylist};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) use self::merge::{merge_plan, report_missed};
use self::session::{Candidate, Deciding, Decision, NewUrl, Recorded, TrackPlan, Verdict};
use self::track::{LiveTrack, RefreshRequest};
use self::window::{NewInits, Processed, Scope, overlaps};
use crate::fetch::{self, Fetcher, ItemId, count_done};
use crate::hooks::Hooks;
use crate::http::{Http, Permit};
use crate::request::LiveOptions;
use crate::resolve::{self, ResolvedTrack};
use crate::workdir::{self, Stored, StoredInit, WorkDir};
use crate::{
    Error, HttpError, LiveEnd, MissReason, Missed, Progress, Stage, WorkDirProblem, blocking,
};

/// 本次运行的录制结果。
pub(crate) struct Recording {
    pub end: LiveEnd,
    /// 本次运行中记下原因的缺失（未合并成区间）
    pub missed: Vec<Missed>,
}

/// 录制用到的共享对象。
pub(crate) struct Context<'a> {
    pub http: Arc<Http>,
    pub hooks: Arc<dyn Hooks>,
    pub dir: &'a WorkDir,
    pub fetcher: &'a mut Fetcher,
    pub progress: &'a watch::Sender<Progress>,
    pub cancel: &'a CancellationToken,
    /// 调用方要求停止录制
    pub stop: &'a CancellationToken,
}

/// 一次刷新的结果。
struct Refreshed {
    track: usize,
    started: Instant,
    result: Result<(MediaPlaylist, NewInits), Error>,
}

/// 刷新拿到的播放列表。
struct Loaded {
    track: usize,
    playlist: MediaPlaylist,
    /// 要录的新分片引用的新 init 段；会话定下之前不拉，为空
    fetched: NewInits,
    /// 这次加载开始的时刻
    started: Instant,
}

/// 等到的下一件事；`D` 为下载的结果（判定会话期间没有下载，为 [`Infallible`]）。
enum Event<D> {
    Stop,
    /// 该轨停滞
    Stall(usize),
    /// 有轨到了刷新的时刻
    Due,
    Refreshed(Refreshed),
    Downloaded(D),
}

/// 录制的对象，会话判定与录制两个阶段共用。
struct Recorder<'a> {
    http: Arc<Http>,
    hooks: Arc<dyn Hooks>,
    dir: &'a WorkDir,
    progress: &'a watch::Sender<Progress>,
    options: LiveOptions,
    tracks: Vec<LiveTrack>,
    /// 本次运行中记下原因的缺失
    missed: Vec<Missed>,
    refreshes: JoinSet<Refreshed>,
    /// 刷新任务的取消令牌：录制结束时取消，等在途的回调返回
    refresh_cancel: CancellationToken,
    cancel: CancellationToken,
    stop: CancellationToken,
}

/// 录制：定下会话，从首次拉到的播放列表开始，直到满足结束条件，并等已列出的分片下完。
///
/// 目录里记录的完整来源地址与本次的不同时（见 [`WorkDir::url_changed`]），处理见 [`NewUrl`]。
pub(crate) async fn record(
    ctx: Context<'_>,
    tracks: Vec<ResolvedTrack>,
    options: LiveOptions,
) -> Result<Recording, Error> {
    let (dir, progress) = (ctx.dir, ctx.progress);
    progress.send_modify(|p| p.stage = Stage::Recording);
    let stored = dir.scan(tracks.len()).await?;
    count_stored(&stored, progress);
    let mut recorder = Recorder {
        http: ctx.http,
        hooks: ctx.hooks,
        dir,
        progress,
        options,
        tracks: live_tracks(&tracks, &stored, Instant::now())?,
        missed: Vec::new(),
        refreshes: JoinSet::new(),
        refresh_cancel: ctx.cancel.child_token(),
        cancel: ctx.cancel.clone(),
        stop: ctx.stop.clone(),
    };
    let fetcher = ctx.fetcher;
    let result = recorder.start(tracks, &stored, fetcher).await;
    recorder.join_refreshes().await;
    recorder.settle(result, fetcher).await
}

/// 各轨的录制状态；之前各会话已录到的时长计入 max_duration。
fn live_tracks(
    tracks: &[ResolvedTrack],
    stored: &Stored,
    now: Instant,
) -> Result<Vec<LiveTrack>, Error> {
    tracks
        .iter()
        .zip(&stored.segments)
        .map(|(track, files)| {
            let recorded_us = files
                .iter()
                .map(|f| f.name.duration_us)
                .fold(0u64, u64::saturating_add);
            LiveTrack::new(track.url.clone(), &track.playlist, recorded_us, now)
        })
        .collect()
}

/// 目录里已完成的分片与 init 段计入进度。
pub(crate) fn count_stored(stored: &Stored, progress: &watch::Sender<Progress>) {
    let done = stored.segments.iter().map(Vec::len).sum();
    let bytes = stored.segments.iter().flatten().map(|f| f.len).sum::<u64>() + stored.init_bytes;
    progress.send_modify(|p| {
        p.segments_done = done;
        p.segments_total = done;
        p.bytes = bytes;
    });
}

impl Recorder<'_> {
    /// 定下会话（目录里有录过的分片时先判定），处理暂存的播放列表，再按节奏刷新直到满足结束条件。
    /// 返回录进的会话与结束原因；会话定下之前就结束时会话为 None（没有排入任何下载）。
    async fn start(
        &mut self,
        first: Vec<ResolvedTrack>,
        stored: &Stored,
        fetcher: &mut Fetcher,
    ) -> Result<(Option<u32>, LiveEnd), Error> {
        let decision = match Deciding::new(stored) {
            Some(deciding) => match self.decide(deciding, first, fetcher).await? {
                ControlFlow::Break(end) => return Ok((None, end)),
                ControlFlow::Continue(decision) => decision,
            },
            None => {
                let playlists = first.into_iter().map(|t| t.playlist).collect();
                Decision::first(playlists, Instant::now())
            }
        };
        let session = decision.session;
        if let Some(end) = self.begin(decision, fetcher).await? {
            return Ok((Some(session), end));
        }
        let end = self.run(session, fetcher).await?;
        Ok((Some(session), end))
    }

    /// 续录时判定会话：刷新各轨直到都有候选。期间停止、停滞时结束（`Break`）。
    async fn decide(
        &mut self,
        mut deciding: Deciding,
        first: Vec<ResolvedTrack>,
        fetcher: &Fetcher,
    ) -> Result<ControlFlow<LiveEnd, Decision>, Error> {
        let now = Instant::now();
        for (track, first) in first.into_iter().enumerate() {
            if let ControlFlow::Break(end) = self
                .consider(&mut deciding, track, first.playlist, now, fetcher)
                .await?
            {
                return Ok(ControlFlow::Break(end));
            }
        }
        while !deciding.is_complete() {
            let event: Event<Infallible> = self.next_event(std::future::pending()).await?;
            match event {
                Event::Stop => return Ok(ControlFlow::Break(LiveEnd::Stopped)),
                Event::Stall(track) => {
                    return self.stalled(track, Instant::now()).map(ControlFlow::Break);
                }
                Event::Due => {}
                Event::Refreshed(refreshed) => {
                    let Some(loaded) = self.refresh_result(refreshed)? else {
                        continue;
                    };
                    if let ControlFlow::Break(end) = self
                        .consider(
                            &mut deciding,
                            loaded.track,
                            loaded.playlist,
                            loaded.started,
                            fetcher,
                        )
                        .await?
                    {
                        return Ok(ControlFlow::Break(end));
                    }
                }
                Event::Downloaded(never) => match never {},
            }
        }
        let decision = deciding.decide().ok_or_else(|| Error::WorkDir {
            path: self.dir.layout().root().to_path_buf(),
            problem: WorkDirProblem::Corrupt("会话编号已达上限".into()),
        })?;
        Ok(ControlFlow::Continue(decision))
    }

    /// 判定会话期间第 `track` 条轨的一份播放列表：已录满的轨不再录、不影响判定；没有分片时等下次刷新；
    /// 否则核对能否接着它最近的会话录，记为该轨的候选并暂停刷新。核对期间要求停止时为 `Break`。
    async fn consider(
        &mut self,
        deciding: &mut Deciding,
        track: usize,
        playlist: MediaPlaylist,
        started: Instant,
        fetcher: &Fetcher,
    ) -> Result<ControlFlow<LiveEnd>, Error> {
        let verdict = if self.tracks[track].is_full(self.max_us()) {
            Verdict::Full
        } else if playlist.segments.is_empty() && !playlist.ended {
            self.tracks[track].wait_unchanged(Instant::now());
            return Ok(ControlFlow::Continue(()));
        } else {
            let check = self.verify(deciding.recorded(track), track, &playlist, fetcher);
            match until_stopped(&self.stop, check).await? {
                Some(verdict) => verdict,
                None => return Ok(ControlFlow::Break(LiveEnd::Stopped)),
            }
        };
        self.tracks[track].hold(&playlist);
        deciding.offer(track, Candidate { playlist, started }, verdict);
        Ok(ControlFlow::Continue(()))
    }

    /// 第 `track` 条轨的窗口能否接着 `recorded` 录：重叠且一致，并且重叠的分片（从新到旧取第一个还取得到的）
    /// 重新下载后与已存的相同。重叠的分片都已取不到（404/410）时无从核对，按接不上算——它们也录不到，另起会话
    /// 不会重复；其余失败（临时故障、key、校验、回调）如实上抛。
    async fn verify(
        &self,
        recorded: Option<&Recorded>,
        track: usize,
        playlist: &MediaPlaylist,
        fetcher: &Fetcher,
    ) -> Result<Verdict, Error> {
        let Some(recorded) = recorded else {
            return Ok(Verdict::Differs);
        };
        for segment in overlaps(recorded, playlist) {
            let data = match fetcher.fetch(track, segment).await {
                Ok(data) => data,
                Err(e) if matches!(e.missable(), Some(HttpError::Status(404 | 410))) => continue,
                Err(e) => return Err(e),
            };
            let stored = self
                .dir
                .layout()
                .segment(track, &recorded.segments()[&segment.sequence]);
            return Ok(if same_file(stored, data).await? {
                Verdict::Matches
            } else {
                Verdict::Differs
            });
        }
        Ok(Verdict::Differs)
    }

    /// 定下会话：完整来源地址变了时按 [`NewUrl`] 处理；记下各轨的起点，再按起点建窗口，拉要录的分片引用的
    /// init 段，处理暂存的播放列表。已录满的轨不录。期间要求停止或服务器前后矛盾时返回结束原因。
    async fn begin(
        &mut self,
        decision: Decision,
        fetcher: &mut Fetcher,
    ) -> Result<Option<LiveEnd>, Error> {
        if self.dir.url_changed() {
            match decision.new_url {
                NewUrl::Adopt => self.dir.adopt_url().await?,
                NewUrl::Keep => {}
                NewUrl::Unverified => {
                    return Err(Error::WorkDir {
                        path: self.dir.layout().root().to_path_buf(),
                        problem: WorkDirProblem::SourceUnverified,
                    });
                }
            }
        }
        // 起点先于这个会话的分片落盘：续录时有分片的会话一定有起点
        for (track, plan) in decision.tracks.iter().enumerate() {
            if let TrackPlan::Record(start, _) = plan
                && let Some(start) = start.to_record()
            {
                self.dir
                    .record_start(track, decision.session, start)
                    .await?;
            }
        }
        let now = Instant::now();
        for (track, plan) in decision.tracks.into_iter().enumerate() {
            let TrackPlan::Record(start, candidate) = plan else {
                continue;
            };
            self.tracks[track].begin(start, now);
            let window = self.tracks[track].window();
            let known = window.known_inits();
            let inits = new_inits(
                &self.http,
                &candidate.playlist,
                &known,
                window.processed(),
                &self.cancel,
            );
            let Some(fetched) = until_stopped(&self.stop, inits).await? else {
                return Ok(Some(LiveEnd::Stopped));
            };
            let session = decision.session;
            let playlist = &candidate.playlist;
            if let Some(end) = self
                .process(
                    session,
                    track,
                    playlist,
                    fetched,
                    candidate.started,
                    fetcher,
                )
                .await?
            {
                return Ok(Some(end));
            }
        }
        Ok(None)
    }

    /// 录制第 `session` 个会话：按节奏刷新，新分片排入下载，直到满足结束条件。
    async fn run(&mut self, session: u32, fetcher: &mut Fetcher) -> Result<LiveEnd, Error> {
        loop {
            if self.tracks.iter().all(LiveTrack::is_ended) {
                return Ok(LiveEnd::EndList);
            }
            let max_us = self.max_us();
            if !self.tracks.iter().any(|t| t.needs_refresh(max_us)) {
                return Ok(LiveEnd::DurationReached);
            }
            let downloads = async {
                match fetcher.next().await {
                    Some(finished) => finished,
                    None => std::future::pending().await,
                }
            };
            match self.next_event(downloads).await? {
                Event::Stop => return Ok(LiveEnd::Stopped),
                Event::Stall(track) => return self.stalled(track, Instant::now()),
                Event::Due => {}
                Event::Refreshed(refreshed) => {
                    let Some(loaded) = self.refresh_result(refreshed)? else {
                        continue;
                    };
                    let Loaded {
                        track,
                        playlist,
                        fetched,
                        started,
                    } = loaded;
                    if let Some(end) = self
                        .process(session, track, &playlist, fetched, started, fetcher)
                        .await?
                    {
                        return Ok(end);
                    }
                }
                Event::Downloaded((id, result)) => self.on_finished(session, id, result)?,
            }
        }
    }

    /// 发起到期的刷新，等下一件事。`downloads` 为下一个下载结果。
    async fn next_event<D>(
        &mut self,
        downloads: impl Future<Output = D>,
    ) -> Result<Event<D>, Error> {
        let now = Instant::now();
        self.start_due_refreshes(now);
        let max_us = self.max_us();
        let next_due = self
            .tracks
            .iter()
            .filter(|t| t.needs_refresh(max_us))
            .filter_map(LiveTrack::due_at)
            .min();
        let stall = self.next_stall();
        tokio::select! {
            _ = self.cancel.cancelled() => Err(Error::Cancelled),
            _ = self.stop.cancelled() => Ok(Event::Stop),
            _ = sleep_until(stall.map(|(at, _)| at)), if stall.is_some() => {
                Ok(Event::Stall(stall.expect("分支只在有停滞时刻时启用").1))
            }
            _ = sleep_until(next_due), if next_due.is_some() => Ok(Event::Due),
            Some(joined) = self.refreshes.join_next() => match joined {
                Ok(refreshed) => Ok(Event::Refreshed(refreshed)),
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Err(_) => unreachable!("刷新任务只经协作取消结束，不被中止"),
            },
            downloaded = downloads => Ok(Event::Downloaded(downloaded)),
        }
    }

    fn max_us(&self) -> Option<u64> {
        self.options
            .max_duration
            .map(|max| u64::try_from(max.as_micros()).unwrap_or(u64::MAX))
    }

    fn start_due_refreshes(&mut self, now: Instant) {
        let max_us = self.max_us();
        for (index, t) in self.tracks.iter_mut().enumerate() {
            if !t.needs_refresh(max_us) || t.due_at().is_none_or(|at| at > now) {
                continue;
            }
            let request = t.start_refresh(now);
            self.refreshes.spawn(refresh(
                self.http.clone(),
                self.hooks.clone(),
                index,
                request,
                self.refresh_cancel.clone(),
            ));
        }
    }

    /// 刷新的结果：拿到播放列表时返回它；可以再试的失败安排下次刷新、返回 None；其余失败上抛。
    fn refresh_result(&mut self, refreshed: Refreshed) -> Result<Option<Loaded>, Error> {
        let Refreshed {
            track,
            started,
            result,
        } = refreshed;
        match result {
            Ok((playlist, fetched)) => {
                self.tracks[track].refreshed();
                Ok(Some(Loaded {
                    track,
                    playlist,
                    fetched,
                    started,
                }))
            }
            Err(e) if waitable_refresh_error(&e) => {
                self.tracks[track].refresh_failed(e, Instant::now());
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// 第 `track` 条轨停滞时如何结束：看起来直播已结束时正常收尾，否则任务失败。
    fn stalled(&mut self, track: usize, now: Instant) -> Result<LiveEnd, Error> {
        let max_us = self.max_us();
        let others_live = self
            .tracks
            .iter()
            .enumerate()
            .any(|(i, t)| i != track && t.needs_refresh(max_us) && t.is_live(now));
        match self.tracks[track].stall(now, others_live) {
            Ok(cause) => Ok(LiveEnd::Stalled { track, cause }),
            Err(cause) => Err(Error::LiveStalled { track, cause }),
        }
    }

    /// 最早停滞的轨及其时刻。
    fn next_stall(&self) -> Option<(Instant, usize)> {
        let max_us = self.max_us();
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.needs_refresh(max_us))
            .filter_map(|(i, t)| Some((t.stall_at(self.options.stall_timeout)?, i)))
            .min()
    }

    /// 处理第 `session` 个会话的一份播放列表：新 init 段落盘后再把新分片排入下载。
    async fn process(
        &mut self,
        session: u32,
        track: usize,
        playlist: &MediaPlaylist,
        fetched: NewInits,
        started: Instant,
        fetcher: &mut Fetcher,
    ) -> Result<Option<LiveEnd>, Error> {
        let scope = Scope {
            track,
            session,
            layout: self.dir.layout(),
            max_us: self.max_us(),
        };
        let now = Instant::now();
        let update = match self.tracks[track].apply(&scope, playlist, fetched, started, now)? {
            ControlFlow::Break(end) => return Ok(Some(end)),
            ControlFlow::Continue(update) => update,
        };
        // init 段先落盘、再下载引用它的分片：中断后合并时，分片引用的 init 段一定存在
        for (fingerprint, data) in update.init_files {
            let len = data.len() as u64;
            let stored = workdir::store_init(self.dir.layout(), track, fingerprint, data).await?;
            if stored == StoredInit::Created {
                self.progress.send_modify(|p| p.bytes += len);
            }
        }
        let failed = update.init_failed.len();
        let expired = update.expired.as_ref().map_or(0, Missed::count);
        let listed = update.items.len() + failed;
        self.progress.send_modify(|p| {
            p.segments_total += listed;
            p.segments_failed += failed;
            p.segments_expired = p.segments_expired.saturating_add(expired);
        });
        self.missed.extend(update.expired);
        self.missed.extend(update.init_failed);
        for item in update.items {
            fetcher.push(item);
        }
        Ok(None)
    }

    /// 处理第 `session` 个会话一个分片的下载结果：取不到的记为缺失，其余失败上抛。
    fn on_finished(
        &mut self,
        session: u32,
        id: ItemId,
        result: Result<u64, Error>,
    ) -> Result<(), Error> {
        let error = match result {
            Ok(len) => {
                count_done(self.progress, len);
                self.tracks[id.track].segment_recorded(Instant::now());
                return Ok(());
            }
            Err(error) => error,
        };
        let Some(kind) = error.missable() else {
            return Err(error);
        };
        self.tracks[id.track].segment_missed(kind.clone());
        self.missed.push(Missed {
            session,
            track: id.track,
            first: id.sequence,
            last: id.sequence,
            reason: MissReason::Failed(kind),
        });
        self.progress.send_modify(|p| p.segments_failed += 1);
        Ok(())
    }

    /// 取消刷新任务并等它们退出：协作取消，在途的回调返回后才结束，不直接中止。
    async fn join_refreshes(&mut self) {
        self.refresh_cancel.cancel();
        while let Some(joined) = self.refreshes.join_next().await {
            if let Err(e) = joined
                && e.is_panic()
            {
                std::panic::resume_unwind(e.into_panic());
            }
        }
    }

    /// 录制结束后收尾：正常结束时等已列出的分片下完；失败时取消其余下载、等它们退出。
    async fn settle(
        mut self,
        result: Result<(Option<u32>, LiveEnd), Error>,
        fetcher: &mut Fetcher,
    ) -> Result<Recording, Error> {
        match result {
            Ok((session, end)) => {
                if let Some(session) = session {
                    fetcher
                        .drain(|id, r| self.on_finished(session, id, r))
                        .await?;
                }
                Ok(Recording {
                    end,
                    missed: self.missed,
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
}

/// 等 `work` 完成；其间调用方要求停止时为 `Ok(None)`，`work` 随之取消。
async fn until_stopped<T>(
    stop: &CancellationToken,
    work: impl Future<Output = Result<T, Error>>,
) -> Result<Option<T>, Error> {
    tokio::select! {
        _ = stop.cancelled() => Ok(None),
        result = work => result.map(Some),
    }
}

/// `data` 与目录中已存的 `stored` 逐字节相同。
async fn same_file(stored: PathBuf, data: Vec<u8>) -> Result<bool, Error> {
    blocking(move || {
        std::fs::read(&stored)
            .map(|existing| existing == data)
            .map_err(|cause| Error::Io {
                action: "读取",
                path: stored,
                cause,
            })
    })
    .await?
}

/// 刷新失败中可以等下次刷新的：取不到（含 404/410，直播结束时常见）、内容为空或语法错误（服务器没写完）。
/// 其余（401/403 等、内容不是播放列表、DRM、回调出错）使任务失败。
fn waitable_refresh_error(error: &Error) -> bool {
    match error {
        Error::Playlist { cause, .. } => {
            matches!(**cause, hls::Error::Syntax { .. } | hls::Error::Empty)
        }
        _ => error.missable().is_some(),
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// 刷新一条轨：拉播放列表；会话已定时再拉要录的新分片引用的新 init 段。
async fn refresh(
    http: Arc<Http>,
    hooks: Arc<dyn Hooks>,
    track: usize,
    request: RefreshRequest,
    cancel: CancellationToken,
) -> Refreshed {
    let started = Instant::now();
    let result = async {
        let playlist = resolve::fetch_media(&http, &hooks, &request.url, &cancel).await?;
        let inits = match &request.processed {
            Some(processed) => {
                new_inits(&http, &playlist, &request.known, processed, &cancel).await?
            }
            None => NewInits::new(),
        };
        Ok((playlist, inits))
    }
    .await;
    Refreshed {
        track,
        started,
        result,
    }
}

/// 拉取要录的分片（见 [`Processed::is_new`]）引用、又不在 `known` 中的 init 段；各自的失败随结果返回，
/// 只有取消中止。不占引擎名额：它们是刷新的一部分，排在大批下载之后会让直播停滞。
async fn new_inits(
    http: &Http,
    playlist: &MediaPlaylist,
    known: &[InitSection],
    processed: &Processed,
    cancel: &CancellationToken,
) -> Result<NewInits, Error> {
    let mut fetched = NewInits::new();
    for s in &playlist.segments {
        if !processed.is_new(s.sequence) {
            continue;
        }
        if let Some(init) = &s.init
            && !known.contains(init)
            && !fetched.iter().any(|(i, _)| i == init)
        {
            match fetch::fetch_init(http, init, Permit::Exempt, cancel).await {
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                result => fetched.push((init.clone(), result)),
            }
        }
    }
    Ok(fetched)
}
