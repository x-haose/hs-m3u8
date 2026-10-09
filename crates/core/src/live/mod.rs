//! 直播录制：按 RFC 8216 6.3.4 的节奏刷新各轨的媒体播放列表，新分片交给下载器，直到满足结束条件。
//!
//! 录制按会话组织：一个会话是一段时间线连续的录制，分片文件名带会话编号，合并按（会话, 不连续段）分组，
//! 组内保留原时间戳（缺失的分片处时间线留空），组与组首尾相接。中断后再次运行时，各轨的第一份播放列表
//! 都与上一个会话已录的分片重叠（同一序号、同一身份、编号一致）就接着该会话录，并补录窗口内没录完的分片；
//! 否则另起一个会话，之前已录过的序号跳过。
//!
//! 轨道编号：第 0 条为所选变体（或来源本身的媒体播放列表），第 1 条（若有）为独立的音频 rendition。

mod merge;
mod track;

use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use hs_m3u8_hls::{self as hls, InitSection, MediaPlaylist};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

pub(crate) use self::merge::{merge_plan, report_missed};
use self::track::{
    Earlier, LiveTrack, NewInits, Processed, Refresh, Scope, Start, missable_http, target,
};
use crate::fetch::{self, Fetcher, ItemId, record_done};
use crate::hooks::Hooks;
use crate::http::{Http, Priority};
use crate::request::LiveOptions;
use crate::resolve::{self, ResolvedTrack};
use crate::workdir::{self, JobRecord, SegmentFile, Stored, WorkDir};
use crate::{
    Error, HttpError, LiveEnd, MissReason, Missed, Progress, Stage, Unsupported, WorkDirProblem,
};

/// 两次刷新之间的最短间隔，防止 TARGETDURATION 为 0 或极小时空转。
const MIN_REFRESH: Duration = Duration::from_millis(100);

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

/// 本次运行接着哪个会话录、各轨从哪里开始。
struct Opening {
    session: u32,
    starts: Vec<Start>,
    /// 接着上一个会话录
    continued: bool,
}

struct Recorder<'a> {
    http: Arc<Http>,
    hooks: Arc<dyn Hooks>,
    dir: &'a WorkDir,
    progress: &'a watch::Sender<Progress>,
    options: LiveOptions,
    session: u32,
    tracks: Vec<LiveTrack>,
    missed: Vec<Missed>,
}

/// 录制：从 `tracks` 首次拉到的播放列表开始，直到满足结束条件，并等已列出的分片下完。
///
/// `job` 为当前请求的任务记录。目录里记录的来源地址（含查询串）与它不同时，只有能接着上一个会话录才继续，
/// 并改为记录当前地址；否则报 [`WorkDirProblem::SourceUnverified`]。
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
    let opening = open_session(&stored.segments, &tracks, dir)?;
    if dir.previous().is_some_and(|recorded| recorded != job) {
        if !opening.continued {
            return Err(Error::WorkDir {
                path: dir.layout().root().to_path_buf(),
                problem: WorkDirProblem::SourceUnverified,
            });
        }
        dir.save(job).await?;
    }

    let now = Instant::now();
    let mut live_tracks = Vec::with_capacity(tracks.len());
    for ((track, start), files) in tracks.iter().zip(opening.starts).zip(&stored.segments) {
        let target = target(&track.playlist).ok_or_else(|| {
            Error::Unsupported(Unsupported::NoTargetDuration(Box::new(track.url.clone())))
        })?;
        let recorded_us = files
            .iter()
            .map(|f| f.name.duration_us)
            .fold(0u64, u64::saturating_add);
        live_tracks.push(LiveTrack::new(
            track.url.clone(),
            target,
            start,
            recorded_us,
            now,
        ));
    }
    let mut recorder = Recorder {
        http,
        hooks,
        dir,
        progress,
        options,
        session: opening.session,
        tracks: live_tracks,
        missed: Vec::new(),
    };

    let refresh_cancel = cancel.child_token();
    let mut refreshes = JoinSet::new();
    let result = async {
        // 首次拉到的播放列表按一次刷新处理
        for (index, track) in tracks.into_iter().enumerate() {
            let t = &recorder.tracks[index];
            let fetched = new_inits(
                &recorder.http,
                &track.playlist,
                &t.known_inits(),
                t.processed(),
                cancel,
            )
            .await?;
            if let Some(end) = recorder
                .apply(index, &track.playlist, fetched, now, fetcher)
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

/// 决定本次运行的会话：各轨的第一份播放列表都接得上上一个会话已录的分片时接着录，否则另起一个。
fn open_session(
    stored: &[Vec<SegmentFile>],
    tracks: &[ResolvedTrack],
    dir: &WorkDir,
) -> Result<Opening, Error> {
    let Some(last) = stored.iter().flatten().map(|f| f.name.session).max() else {
        return Ok(Opening {
            session: 0,
            starts: tracks.iter().map(|_| Start::Fresh).collect(),
            continued: false,
        });
    };
    let earlier: Vec<Option<Earlier>> = stored
        .iter()
        .map(|files| Earlier::of(files, last))
        .collect();
    let fits: Vec<bool> = earlier
        .iter()
        .zip(tracks)
        .map(|(e, t)| e.as_ref().is_some_and(|e| e.continues_into(&t.playlist)))
        .collect();
    if fits.iter().all(|&fit| fit) {
        return Ok(Opening {
            session: last,
            starts: earlier
                .into_iter()
                .map(|e| Start::Continue(e.expect("接得上的轨都有之前的分片")))
                .collect(),
            continued: true,
        });
    }
    let session = last.checked_add(1).ok_or_else(|| Error::WorkDir {
        path: dir.layout().root().to_path_buf(),
        problem: WorkDirProblem::Corrupt("会话编号已达上限".into()),
    })?;
    let starts = earlier
        .into_iter()
        .zip(fits)
        .map(|(e, fit)| match e {
            Some(e) if fit => Start::After { through: e.last() },
            _ => Start::Fresh,
        })
        .collect();
    Ok(Opening {
        session,
        starts,
        continued: false,
    })
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
                return Ok(LiveEnd::DurationReached);
            }
            let now = Instant::now();
            self.start_due_refreshes(now, refreshes, refresh_cancel);
            let next_due = self
                .tracks
                .iter()
                .filter(|t| !self.finished(t))
                .filter_map(|t| match t.refresh {
                    Refresh::Due(at) => Some(at),
                    Refresh::InFlight { .. } | Refresh::Ended => None,
                })
                .min();
            let stall = self.next_stall();
            tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                _ = stop.cancelled() => return Ok(LiveEnd::Stopped),
                _ = sleep_until(stall.map(|(at, _)| at)), if stall.is_some() => {
                    let (_, track) = stall.expect("分支只在有停滞时刻时启用");
                    return match self.tracks[track].stall(Instant::now()) {
                        Ok(cause) => Ok(LiveEnd::Stalled { track, cause }),
                        Err(cause) => Err(Error::LiveStalled { track, cause }),
                    };
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
        matches!(t.refresh, Refresh::Ended) || self.max_us().is_some_and(|max| t.recorded_us >= max)
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
        let due: Vec<usize> = (0..self.tracks.len())
            .filter(|&i| {
                let t = &self.tracks[i];
                !self.finished(t) && matches!(t.refresh, Refresh::Due(at) if at <= now)
            })
            .collect();
        for index in due {
            let t = &mut self.tracks[index];
            t.refresh = Refresh::InFlight { started: now };
            refreshes.spawn(refresh(
                self.http.clone(),
                self.hooks.clone(),
                index,
                t.url.clone(),
                t.known_inits(),
                t.processed().clone(),
                cancel.clone(),
            ));
        }
    }

    /// 最早停滞的轨及其时刻。
    fn next_stall(&self) -> Option<(Instant, usize)> {
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| !self.finished(t))
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
                self.tracks[track].last_error = None;
                self.apply(track, &playlist, inits, started, fetcher).await
            }
            Err(e) if waitable_refresh_error(&e) => {
                let t = &mut self.tracks[track];
                let asked = match &e {
                    Error::Http { retry_after, .. } => retry_after.unwrap_or_default(),
                    _ => Duration::ZERO,
                };
                t.refresh =
                    Refresh::Due(Instant::now() + (t.target / 2).max(MIN_REFRESH).max(asked));
                t.last_error = Some(e);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// 处理一份播放列表：安排下次刷新，新 init 段落盘后再把新分片排入下载。`started` 为这次加载开始的时刻。
    async fn apply(
        &mut self,
        track: usize,
        playlist: &MediaPlaylist,
        fetched: NewInits,
        started: Instant,
        fetcher: &mut Fetcher,
    ) -> Result<Option<LiveEnd>, Error> {
        let scope = Scope {
            track,
            session: self.session,
            layout: self.dir.layout(),
            max_us: self.max_us(),
        };
        let t = &mut self.tracks[track];
        let update = match t.update(&scope, playlist, fetched)? {
            ControlFlow::Break(end) => return Ok(Some(end)),
            ControlFlow::Continue(update) => update,
        };
        let now = Instant::now();
        t.refresh = if update.ended {
            Refresh::Ended
        } else {
            // RFC 8216 6.3.4：有变化后从开始加载起至少等一个 target duration，没变化时等半个
            let at = if update.changed {
                started + t.target
            } else {
                now + t.target / 2
            };
            Refresh::Due(at.max(now + MIN_REFRESH))
        };

        // init 段先落盘、再下载引用它的分片：中断后合并时，分片引用的 init 段一定存在
        for (_, data) in update.init_files {
            let len = data.len() as u64;
            let (_, created) = workdir::store_init(self.dir.layout(), track, data).await?;
            if created {
                self.progress.send_modify(|p| p.bytes += len);
            }
        }
        let (scheduled, missed) = (update.items.len(), update.missed.len());
        self.progress.send_modify(|p| {
            p.segments_total += scheduled;
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
                self.tracks[id.track].last_recorded = Instant::now();
                Ok(())
            }
            Err(e) => match missable_segment(&e) {
                Some(kind) => {
                    self.missed.push(Missed {
                        session: self.session,
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
    url: Url,
    known: Vec<InitSection>,
    processed: Processed,
    cancel: CancellationToken,
) -> Refreshed {
    let started = Instant::now();
    let result = async {
        let playlist = resolve::fetch_media(&http, &hooks, &url, &cancel).await?;
        let inits = new_inits(&http, &playlist, &known, &processed, &cancel).await?;
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
            match fetch::fetch_init(http, init, Priority::Urgent, cancel).await {
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                result => fetched.push((init.clone(), result)),
            }
        }
    }
    Ok(fetched)
}
