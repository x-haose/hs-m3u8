//! 直播录制：按 RFC 8216 6.3.4 的节奏刷新各轨的媒体播放列表，新分片交给下载器，直到满足结束条件。
//!
//! 录制按会话组织：一个会话是一段时间线连续的录制，分片文件名带会话编号，合并按（会话, 不连续段）分组，
//! 组内保留原时间戳（缺失的分片处时间线留空），组与组首尾相接。中断后再次运行时接着上一个会话录还是另起一个，
//! 见 [`session`]。
//!
//! 轨道编号：第 0 条为所选变体（或来源本身的媒体播放列表），第 1 条（若有）为独立的音频 rendition。

mod merge;
mod session;
mod track;
mod window;

use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;

use hs_m3u8_hls::{self as hls, InitSection, MediaPlaylist, Segment};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) use self::merge::{merge_plan, report_missed};
use self::session::{Candidate, Deciding, Decision, Earlier};
use self::track::{LiveTrack, RefreshRequest};
use self::window::{NewInits, Processed, Scope, missable_http, newest_overlap};
use crate::fetch::{self, Fetcher, ItemId, record_done};
use crate::hooks::Hooks;
use crate::http::{Http, Permit};
use crate::request::LiveOptions;
use crate::resolve::{self, ResolvedTrack};
use crate::workdir::{self, JobRecord, Stored, StoredInit, WorkDir};
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

/// 本次运行录进哪个会话。
enum SessionState {
    Decided(u32),
    /// 续录：等各轨的第一份可用播放列表来判定
    Deciding(Deciding),
}

struct Recorder<'a> {
    http: Arc<Http>,
    hooks: Arc<dyn Hooks>,
    dir: &'a WorkDir,
    /// 当前请求的任务记录
    job: &'a JobRecord,
    progress: &'a watch::Sender<Progress>,
    options: LiveOptions,
    session: SessionState,
    tracks: Vec<LiveTrack>,
    missed: Vec<Missed>,
}

/// 录制：从 `tracks` 首次拉到的播放列表开始，直到满足结束条件，并等已列出的分片下完。
///
/// `job` 为当前请求的任务记录。目录里记录的来源地址（含查询串）与它不同时，有已录的分片就须接着上一个会话录才继续，
/// 并改为记录当前地址；接不上报 [`WorkDirProblem::SourceUnverified`]。
pub(crate) async fn record(
    ctx: Context<'_>,
    tracks: Vec<ResolvedTrack>,
    options: LiveOptions,
    job: &JobRecord,
) -> Result<Recording, Error> {
    let Context {
        http,
        hooks,
        dir,
        fetcher,
        progress,
        cancel,
        stop,
    } = ctx;
    progress.send_modify(|p| p.stage = Stage::Recording);
    let stored = dir.scan(tracks.len()).await?;
    count_stored(&stored, progress);
    let now = Instant::now();
    let live_tracks = tracks
        .iter()
        .zip(&stored.segments)
        .map(|(track, files)| {
            let recorded_us = files
                .iter()
                .map(|f| f.name.duration_us)
                .fold(0u64, u64::saturating_add);
            LiveTrack::new(track.url.clone(), &track.playlist, recorded_us, now)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let session = match stored
        .segments
        .iter()
        .flatten()
        .map(|f| f.name.session)
        .max()
    {
        Some(last) => {
            let earlier = stored
                .segments
                .iter()
                .map(|files| Earlier::of(files, last))
                .collect();
            SessionState::Deciding(Deciding::new(last, earlier))
        }
        None => {
            // 没有录过的分片：没有要核对的内容，地址变了直接改记
            if dir.url_changed(job) {
                dir.save(job).await?;
            }
            SessionState::Decided(0)
        }
    };
    let mut recorder = Recorder {
        http,
        hooks,
        dir,
        job,
        progress,
        options,
        session,
        tracks: live_tracks,
        missed: Vec::new(),
    };

    let refresh_cancel = cancel.child_token();
    let mut refreshes = JoinSet::new();
    let result = recorder
        .record(
            tracks,
            fetcher,
            &mut refreshes,
            &refresh_cancel,
            cancel,
            stop,
        )
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
    /// 处理首次拉到的播放列表，再按节奏刷新直到满足结束条件。
    async fn record(
        &mut self,
        tracks: Vec<ResolvedTrack>,
        fetcher: &mut Fetcher,
        refreshes: &mut JoinSet<Refreshed>,
        refresh_cancel: &CancellationToken,
        cancel: &CancellationToken,
        stop: &CancellationToken,
    ) -> Result<LiveEnd, Error> {
        let now = Instant::now();
        for (index, track) in tracks.into_iter().enumerate() {
            let window = self.tracks[index].window();
            let fetched = new_inits(
                &self.http,
                &track.playlist,
                &window.known_inits(),
                window.processed(),
                cancel,
            )
            .await?;
            if let Some(end) = self
                .apply(index, track.playlist, fetched, now, fetcher)
                .await?
            {
                return Ok(end);
            }
        }
        self.run(fetcher, refreshes, refresh_cancel, cancel, stop)
            .await
    }

    async fn run(
        &mut self,
        fetcher: &mut Fetcher,
        refreshes: &mut JoinSet<Refreshed>,
        refresh_cancel: &CancellationToken,
        cancel: &CancellationToken,
        stop: &CancellationToken,
    ) -> Result<LiveEnd, Error> {
        loop {
            if self.tracks.iter().all(LiveTrack::is_ended) {
                return Ok(LiveEnd::EndList);
            }
            let max_us = self.max_us();
            if self.tracks.iter().all(|t| t.finished(max_us)) {
                return Ok(LiveEnd::DurationReached);
            }
            let now = Instant::now();
            self.start_due_refreshes(now, refreshes, refresh_cancel);
            let next_due = self
                .tracks
                .iter()
                .filter(|t| !t.finished(max_us))
                .filter_map(LiveTrack::due_at)
                .min();
            let stall = self.next_stall();
            tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                _ = stop.cancelled() => return Ok(LiveEnd::Stopped),
                _ = sleep_until(stall.map(|(at, _)| at)), if stall.is_some() => {
                    let (_, track) = stall.expect("分支只在有停滞时刻时启用");
                    return self.stalled(track, Instant::now());
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

    fn max_us(&self) -> Option<u64> {
        self.options
            .max_duration
            .map(|max| u64::try_from(max.as_micros()).unwrap_or(u64::MAX))
    }

    fn start_due_refreshes(
        &mut self,
        now: Instant,
        refreshes: &mut JoinSet<Refreshed>,
        cancel: &CancellationToken,
    ) {
        let max_us = self.max_us();
        for (index, t) in self.tracks.iter_mut().enumerate() {
            if t.finished(max_us) || t.due_at().is_none_or(|at| at > now) {
                continue;
            }
            let request = t.start_refresh(now);
            refreshes.spawn(refresh(
                self.http.clone(),
                self.hooks.clone(),
                index,
                request,
                cancel.clone(),
            ));
        }
    }

    /// 第 `track` 条轨停滞时如何结束：看起来直播已结束时正常收尾，否则任务失败。
    fn stalled(&mut self, track: usize, now: Instant) -> Result<LiveEnd, Error> {
        let max_us = self.max_us();
        let others_listing = self
            .tracks
            .iter()
            .enumerate()
            .any(|(i, t)| i != track && !t.finished(max_us) && t.is_listing(now));
        match self.tracks[track].stall(now, others_listing) {
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
            .filter(|(_, t)| !t.finished(max_us))
            .filter_map(|(i, t)| Some((t.stall_at(self.options.stall_timeout)?, i)))
            .min()
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
                self.tracks[track].refreshed();
                self.apply(track, playlist, inits, started, fetcher).await
            }
            Err(e) if waitable_refresh_error(&e) => {
                self.tracks[track].refresh_failed(e, Instant::now());
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// 处理一份播放列表：会话已定时照常处理，续录判定期间作为候选。`started` 为这次加载开始的时刻。
    async fn apply(
        &mut self,
        track: usize,
        playlist: MediaPlaylist,
        fetched: NewInits,
        started: Instant,
        fetcher: &mut Fetcher,
    ) -> Result<Option<LiveEnd>, Error> {
        match self.session {
            SessionState::Decided(session) => {
                self.process(session, track, &playlist, fetched, started, fetcher)
                    .await
            }
            SessionState::Deciding(_) => {
                self.consider(track, playlist, fetched, started, fetcher)
                    .await
            }
        }
    }

    /// 续录判定期间的一份播放列表：没有分片时等下次刷新；否则核对能否接着上一个会话录，记为该轨的候选，
    /// 各轨都有候选后定下会话并处理暂存的播放列表。
    async fn consider(
        &mut self,
        track: usize,
        playlist: MediaPlaylist,
        fetched: NewInits,
        started: Instant,
        fetcher: &mut Fetcher,
    ) -> Result<Option<LiveEnd>, Error> {
        if playlist.segments.is_empty() && !playlist.ended {
            self.tracks[track].wait_unchanged(Instant::now());
            return Ok(None);
        }
        let fits = self.fits(track, &playlist, fetcher).await?;
        self.tracks[track].hold();
        let SessionState::Deciding(deciding) = &mut self.session else {
            unreachable!("只在判定会话期间调用");
        };
        deciding.offer(
            track,
            Candidate {
                playlist,
                fetched,
                started,
                fits,
            },
        );
        if !deciding.is_complete() {
            return Ok(None);
        }
        let decision = deciding.decide().ok_or_else(|| Error::WorkDir {
            path: self.dir.layout().root().to_path_buf(),
            problem: WorkDirProblem::Corrupt("会话编号已达上限".into()),
        })?;
        self.begin(decision, fetcher).await
    }

    /// 第 `track` 条轨能否接着上一个会话录：窗口与已录的分片重叠且一致，重叠中最新的分片内容也相同。
    async fn fits(
        &self,
        track: usize,
        playlist: &MediaPlaylist,
        fetcher: &Fetcher,
    ) -> Result<bool, Error> {
        let SessionState::Deciding(deciding) = &self.session else {
            unreachable!("只在判定会话期间调用");
        };
        let Some(earlier) = deciding.earlier(track) else {
            return Ok(false);
        };
        let Some(segment) = newest_overlap(earlier, playlist) else {
            return Ok(false);
        };
        let name = &earlier.segments()[&segment.sequence];
        let stored = self.dir.layout().segment(track, name);
        same_content(fetcher, track, segment, stored).await
    }

    /// 定下会话：地址变了时须接得上才继续并改记新地址；各轨按判定的起点重建窗口，再处理暂存的播放列表。
    async fn begin(
        &mut self,
        decision: Decision,
        fetcher: &mut Fetcher,
    ) -> Result<Option<LiveEnd>, Error> {
        if self.dir.url_changed(self.job) {
            if !decision.continued {
                return Err(Error::WorkDir {
                    path: self.dir.layout().root().to_path_buf(),
                    problem: WorkDirProblem::SourceUnverified,
                });
            }
            self.dir.save(self.job).await?;
        }
        let session = decision.session;
        self.session = SessionState::Decided(session);
        for (track, (start, candidate)) in decision.tracks.into_iter().enumerate() {
            self.tracks[track].begin(start);
            let Candidate {
                playlist,
                fetched,
                started,
                ..
            } = candidate;
            if let Some(end) = self
                .process(session, track, &playlist, fetched, started, fetcher)
                .await?
            {
                return Ok(Some(end));
            }
        }
        Ok(None)
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
        let init_failed = update
            .missed
            .iter()
            .filter(|m| matches!(m.reason, MissReason::InitFailed(_)))
            .count();
        let to_download = update.items.len() + init_failed;
        let missed: usize = update.missed.iter().map(Missed::count).sum();
        self.progress.send_modify(|p| {
            p.segments_total += to_download;
            p.segments_missed += missed;
        });
        self.missed.extend(update.missed);
        for item in update.items {
            fetcher.push(item);
        }
        Ok(None)
    }

    /// 处理一个分片的下载结果：取不到的记为缺失，其余失败上抛。
    fn on_finished(&mut self, id: ItemId, result: Result<u64, Error>) -> Result<(), Error> {
        match result {
            Ok(len) => {
                record_done(self.progress, len);
                self.tracks[id.track].segment_recorded(Instant::now());
                Ok(())
            }
            Err(e) => match missable_segment(&e) {
                Some(kind) => {
                    let SessionState::Decided(session) = self.session else {
                        unreachable!("会话定下之前不会排入下载");
                    };
                    self.tracks[id.track].segment_missed(kind.clone());
                    self.missed.push(Missed {
                        session,
                        track: id.track,
                        first: id.sequence,
                        last: id.sequence,
                        reason: MissReason::Failed(kind),
                    });
                    self.progress.send_modify(|p| p.segments_missed += 1);
                    Ok(())
                }
                None => Err(e),
            },
        }
    }
}

/// 重新下载 `segment` 并与目录中已存的 `stored` 逐字节比较。取不到（404/410、重试后仍失败）时无从核对，
/// 视为不同；其余失败（key、校验、回调）同下载失败一样使任务失败。
async fn same_content(
    fetcher: &Fetcher,
    track: usize,
    segment: &Segment,
    stored: PathBuf,
) -> Result<bool, Error> {
    let data = match fetcher.fetch(segment).await {
        Ok(data) => data,
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        Err(e) if missable_http(&e).is_some() => return Ok(false),
        Err(cause) => {
            return Err(Error::Segment {
                track,
                sequence: segment.sequence,
                url: Box::new(segment.uri.clone()),
                cause: Box::new(cause),
            });
        }
    };
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

/// 分片请求本身取不到时可记为缺失；key、回调、校验等失败不在此列。
fn missable_segment(error: &Error) -> Option<HttpError> {
    match error {
        Error::Segment { cause, .. } => missable_http(cause),
        _ => None,
    }
}

/// 刷新失败中可以等下次刷新的：取不到（含 404/410，直播结束时常见）、内容为空或语法错误（服务器没写完）。
/// 其余（401/403 等、内容不是播放列表、DRM、回调出错）使任务失败。
fn waitable_refresh_error(error: &Error) -> bool {
    match error {
        Error::Playlist { cause, .. } => {
            matches!(**cause, hls::Error::Syntax { .. } | hls::Error::Empty)
        }
        _ => missable_http(error).is_some(),
    }
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
    request: RefreshRequest,
    cancel: CancellationToken,
) -> Refreshed {
    let started = Instant::now();
    let result = async {
        let playlist = resolve::fetch_media(&http, &hooks, &request.url, &cancel).await?;
        let inits = new_inits(
            &http,
            &playlist,
            &request.known,
            &request.processed,
            &cancel,
        )
        .await?;
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
