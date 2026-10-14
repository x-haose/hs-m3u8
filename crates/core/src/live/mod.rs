//! 直播录制：按 RFC 8216 6.3.4 的节奏刷新各轨的媒体播放列表，新分片交给下载器，直到满足结束条件。
//!
//! 录制按会话组织：一个会话是一段时间线连续的录制，分片文件名带会话编号，合并按（会话, 不连续段）分组，
//! 组内保留原时间戳（缺失的分片处时间线留空），组与组首尾相接。中断后再次运行时先判定接着上一个会话录还是
//! 另起一个（见 [`deciding`]），再开始录。
//!
//! 一个事件循环驱动整个录制：刷新、拉 init 段、核对续录的内容与之前录齐的组都是后台任务（见 [`tasks`]），完成后
//! 作为事件交回，循环从不等某一个网络请求；新录齐的组能否放进 MP4（见 [`Mp4Check`]）只读两组开头的分片，在循环里
//! 做完。拉到的播放列表先暂存在各轨，会话定下后逐份拉好 init 段、按顺序处理。
//! 定下结束原因后不再刷新，把暂存的处理完才结束；停止（[`crate::JobControl::stop`]）也是如此，但会话定下之前停止
//! 立即结束，不录新的分片。只录一条轨时，它的播放列表与之前刷新得到的接不上就另起会话接着录（见
//! [`crate::NewSession`]）。
//!
//! 轨道编号：第 0 条为所选变体（或来源本身的媒体播放列表），第 1 条（若有）为独立的音频 rendition。

mod deciding;
mod merge;
mod mp4_check;
mod session;
mod tasks;
mod track;
mod window;

use std::ops::ControlFlow;
use std::sync::Arc;

use hs_m3u8_hls as hls;
use hs_m3u8_remux::Streams;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) use self::merge::{merge_plan, report_missed};
use self::mp4_check::Mp4Check;
use self::session::Verdicts;
use self::tasks::{Done, Tasks};
use self::track::{Fetched, LiveTrack};
use self::window::{NewInits, Scope};
use crate::fetch::{Direct, Fetcher, Finished, ItemInfo, count_done};
use crate::hooks::Hooks;
use crate::http::Http;
use crate::request::LiveOptions;
use crate::resolve::ResolvedTrack;
use crate::workdir::{self, Stored, StoredInit, WorkDir};
use crate::{
    Error, LiveEnd, MissReason, Missed, Mp4Unsupported, NewSession, Progress, RefreshCause,
    SourceBreak, Stage, WorkDirProblem,
};

/// 本次运行的录制结果。
pub(crate) struct Outcome {
    pub end: LiveEnd,
    /// 本次运行中记下原因的缺失（未合并成区间）
    pub missed: Vec<Missed>,
    /// 本次运行录制中途另起的会话
    pub new_sessions: Vec<NewSession>,
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

/// 调用方的取消与停止信号。
struct Signals<'a> {
    cancel: &'a CancellationToken,
    stop: &'a CancellationToken,
}

/// 录制处在哪个阶段。
enum Phase {
    /// 续录时会话还没定下
    Deciding(Verdicts),
    /// 会话已定下，录进第 `session` 个会话
    Decided { session: u32 },
}

/// 等到的下一件事。
enum Event {
    Stop,
    /// 到了检查停滞的时刻（见 [`Recorder::stall_check_at`]）
    StallCheck,
    /// 有轨到了刷新的时刻
    Due,
    Done(Done),
    Downloaded(Finished),
}

/// 录制的状态，会话判定与录制两个阶段共用。
struct Recorder<'a> {
    http: Arc<Http>,
    hooks: Arc<dyn Hooks>,
    dir: &'a WorkDir,
    progress: &'a watch::Sender<Progress>,
    options: LiveOptions,
    tracks: Vec<LiveTrack>,
    /// 本次运行中记下原因的缺失
    missed: Vec<Missed>,
    /// 本次运行录制中途另起的会话
    new_sessions: Vec<NewSession>,
    tasks: Tasks,
    /// 核对内容用的直接拉取
    direct: Direct,
    phase: Phase,
    /// 定下的结束原因：不再刷新，处理完暂存的播放列表即结束
    ending: Option<LiveEnd>,
    /// 输出含 MP4 时录制中的核对；不输出 MP4 时为 None
    mp4: Option<Mp4Check>,
}

/// 录制：定下会话，从首次拉到的播放列表开始，直到满足结束条件，并等已列出的分片下完。要合并 MP4 时 `mp4` 为各轨的
/// 取流方式，录制中核对能否放进 MP4（见 [`Mp4Check`]），放不进的原因放进进度，录制照常进行。
///
/// 目录里记录的完整来源地址与本次的不同时（见 [`WorkDir::url_changed`]），处理见 [`session::NewUrl`]。
pub(crate) async fn record(
    ctx: Context<'_>,
    tracks: Vec<ResolvedTrack>,
    options: LiveOptions,
    mp4: Option<Vec<Streams>>,
) -> Result<Outcome, Error> {
    let (dir, progress) = (ctx.dir, ctx.progress);
    let stored = dir.scan(tracks.len()).await?;
    count_stored(&stored, progress);
    let now = Instant::now();
    // 目录里没有录过的分片时直接录第 0 个会话
    let verdicts = Verdicts::new(&stored).map_err(|reason| Error::WorkDir {
        path: dir.layout().root().to_path_buf(),
        problem: WorkDirProblem::Corrupt(reason),
    })?;
    let phase = match verdicts {
        Some(verdicts) => Phase::Deciding(verdicts),
        None => Phase::Decided { session: 0 },
    };
    let (mp4, batches) = match mp4 {
        Some(streams) => {
            let plan = merge_plan(&stored.segments, dir.layout())?;
            let (check, batches) = Mp4Check::new(streams, &plan, &stored);
            (Some(check), batches)
        }
        None => (None, Vec::new()),
    };
    let mut recorder = Recorder {
        http: ctx.http,
        hooks: ctx.hooks,
        dir,
        progress,
        options,
        tracks: live_tracks(&tracks, &stored, now)?,
        missed: Vec::new(),
        new_sessions: Vec::new(),
        tasks: Tasks::new(ctx.cancel),
        direct: ctx.fetcher.direct(),
        phase,
        ending: None,
        mp4,
    };
    if let Some(check) = &recorder.mp4
        && !batches.is_empty()
    {
        let root = dir.layout().root().to_path_buf();
        recorder
            .tasks
            .check_mp4(check.streams().to_vec(), batches, root);
    }
    let refresh_errors = recorder.refresh_errors(true);
    progress.send_modify(|p| {
        p.stage = Stage::Recording;
        p.refresh_errors = refresh_errors;
    });
    let signals = Signals {
        cancel: ctx.cancel,
        stop: ctx.stop,
    };
    let fetcher = ctx.fetcher;
    let first = tracks
        .into_iter()
        .map(|t| Fetched {
            playlist: t.playlist,
            started: now,
        })
        .collect();
    let result = match recorder.start(first).await {
        Ok(()) => recorder.run(&signals, fetcher).await,
        Err(e) => Err(e),
    };
    recorder.tasks.shutdown().await;
    recorder.publish_refresh_errors(false);
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
        .enumerate()
        .map(|(i, track)| {
            LiveTrack::new(
                track.url.clone(),
                &track.playlist,
                stored.duration_us(i),
                now,
            )
        })
        .collect()
}

/// 目录里已完成的分片与 init 段计入进度。
pub(crate) fn count_stored(stored: &Stored, progress: &watch::Sender<Progress>) {
    let done = stored.segments.iter().map(Vec::len).sum();
    let bytes = stored.bytes();
    let duration_us = stored.duration_us(0);
    progress.send_modify(|p| {
        p.segments_done = done;
        p.segments_total = done;
        p.bytes = bytes;
        p.duration_us = duration_us;
    });
}

impl Recorder<'_> {
    /// 按节奏刷新、处理各种事件，直到结束，返回结束原因。
    async fn run(
        &mut self,
        signals: &Signals<'_>,
        fetcher: &mut Fetcher,
    ) -> Result<LiveEnd, Error> {
        loop {
            if let Some(end) = self.finished() {
                return Ok(end);
            }
            match self.next_event(signals, fetcher).await? {
                Event::Stop => match self.phase {
                    Phase::Deciding(_) => {
                        return Ok(self.ending.clone().unwrap_or(LiveEnd::Stopped));
                    }
                    Phase::Decided { .. } => self.end(LiveEnd::Stopped),
                },
                Event::StallCheck => {
                    if let Some(track) = self.judgeable_stall(Instant::now()) {
                        self.on_stall(track).await?;
                    }
                }
                Event::Due => {}
                Event::Done(done) => self.on_done(done, fetcher).await?,
                Event::Downloaded((id, result)) => {
                    let recorded = result.is_ok();
                    self.on_finished(id, result)?;
                    if let (true, Some(check)) = (recorded, &mut self.mp4) {
                        let reason = check.recorded(id, self.dir.layout()).await?;
                        self.report_mp4(reason);
                    }
                }
            }
            self.publish_refresh_errors(true);
        }
    }

    /// 录制结束时的结束原因；还没结束时为 None。暂存的播放列表都处理完才算结束：判定期间拉到的候选可能
    /// 已带 ENDLIST，这条轨已算结束，候选还没处理。
    fn finished(&self) -> Option<LiveEnd> {
        if matches!(self.phase, Phase::Deciding(_)) || self.tracks.iter().any(LiveTrack::has_held) {
            return None;
        }
        match &self.ending {
            Some(end) => Some(end.clone()),
            None if self.tracks.iter().all(LiveTrack::is_ended) => Some(LiveEnd::EndList),
            None if !self.tracks.iter().any(|t| t.needs_refresh(self.max_us())) => {
                Some(LiveEnd::DurationReached)
            }
            None => None,
        }
    }

    /// 定下结束原因；已有的不变。
    fn end(&mut self, end: LiveEnd) {
        self.ending.get_or_insert(end);
    }

    /// 进度里各轨的刷新失败，从各轨的状态推出：录制中、还会刷新的轨报它最近一次刷新失败的原因，其余为 None。
    fn refresh_errors(&self, recording: bool) -> Vec<Option<RefreshCause>> {
        let max_us = self.max_us();
        self.tracks
            .iter()
            .map(|t| {
                let refreshing = recording && self.ending.is_none() && t.needs_refresh(max_us);
                t.last_refresh_error().filter(|_| refreshing).cloned()
            })
            .collect()
    }

    /// 把 [`Recorder::refresh_errors`] 发布到进度。
    fn publish_refresh_errors(&self, recording: bool) {
        let causes = self.refresh_errors(recording);
        self.progress.send_if_modified(|p| {
            let changed = p.refresh_errors != causes;
            p.refresh_errors = causes;
            changed
        });
    }

    /// 发起到期的刷新，等下一件事。已完成的后台任务与下载先于停止、停滞处理：停止前已拉到的照常处理，
    /// 刚录到的分片不被误判为停滞。
    async fn next_event(
        &mut self,
        signals: &Signals<'_>,
        fetcher: &mut Fetcher,
    ) -> Result<Event, Error> {
        let ending = self.ending.is_some();
        // 会话定下之前停止立即结束，结束中也照样响应
        let stoppable = !ending || matches!(self.phase, Phase::Deciding(_));
        let (stall, next_due) = if ending {
            (None, None)
        } else {
            let now = Instant::now();
            let probe = self
                .next_stall()
                .map(|(since, _)| since)
                .filter(|&since| since <= now);
            self.start_due_refreshes(now, probe);
            let max_us = self.max_us();
            let next_due = self
                .tracks
                .iter()
                .filter(|t| t.needs_refresh(max_us))
                .filter_map(|t| t.due_at(probe))
                .min();
            (self.stall_check_at(now), next_due)
        };
        let downloads = async {
            match fetcher.next().await {
                Some(finished) => finished,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _ = signals.cancel.cancelled() => Err(Error::Cancelled),
            done = self.tasks.next() => Ok(Event::Done(done)),
            finished = downloads => Ok(Event::Downloaded(finished)),
            _ = signals.stop.cancelled(), if stoppable => Ok(Event::Stop),
            _ = sleep_until(stall), if stall.is_some() => Ok(Event::StallCheck),
            _ = sleep_until(next_due), if next_due.is_some() => Ok(Event::Due),
        }
    }

    fn max_us(&self) -> Option<u64> {
        self.options
            .max_duration
            .map(|max| u64::try_from(max.as_micros()).unwrap_or(u64::MAX))
    }

    /// 发起到期的刷新；`probe` 见 [`LiveTrack::due_at`]。
    fn start_due_refreshes(&mut self, now: Instant, probe: Option<Instant>) {
        let max_us = self.max_us();
        for (index, t) in self.tracks.iter_mut().enumerate() {
            if !t.needs_refresh(max_us) || t.due_at(probe).is_none_or(|at| at > now) {
                continue;
            }
            let request = t.start_refresh(now);
            self.tasks
                .refresh(self.http.clone(), self.hooks.clone(), index, request);
        }
    }

    /// 最早开始停滞的轨及其停滞开始的时刻（见 [`LiveTrack::stall_since`]）。
    fn next_stall(&self) -> Option<(Instant, usize)> {
        let max_us = self.max_us();
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.needs_refresh(max_us))
            .filter_map(|(i, t)| Some((t.stall_since(self.options.stall_timeout)?, i)))
            .min()
    }

    /// 下次检查停滞的时刻：最早开始停滞的轨停滞开始的时刻；已开始停滞而结论要等的，为让结论可下的最早的将来时刻
    /// （停滞的轨那次刷新、或其他轨在途的刷新满一个目标时长，见 [`Recorder::judgeable_stall`]），没有时为 None（等刷新
    /// 返回的事件）。只取将来的时刻：已过去的不会再让结论可下，取了会让循环空转。
    fn stall_check_at(&self, now: Instant) -> Option<Instant> {
        let (since, track) = self.next_stall()?;
        if since > now || self.judgeable_stall(now).is_some() {
            return Some(since.max(now));
        }
        let max_us = self.max_us();
        let own = self.tracks[track].pending_deadline(since);
        let others = self
            .tracks
            .iter()
            .enumerate()
            .filter(|&(i, t)| i != track && t.needs_refresh(max_us))
            .filter_map(|(_, t)| t.overdue_at());
        own.into_iter().chain(others).filter(|&at| at > now).min()
    }

    /// 此刻能对哪条停滞的轨下结论。结论要依据停滞开始之后的刷新结果，不能只看过了多久：进程被挂起（Windows 上睡眠时
    /// 单调时钟照走、调试器暂停）醒来时停滞已到，而期间一次刷新也没发，凭挂起前的情况会把仍在进行的直播判为已结束。
    /// 所以停滞的轨要有停滞开始之后发起、已返回的刷新（还没发起的由 [`LiveTrack::due_at`] 立即发起），或那之后发起的
    /// 刷新在途满了一个目标时长（结论为刷新没有返回）；其他还要刷新的轨（判断它们是否还在出新分片）同样要有那之后返回
    /// 的刷新，或者刷新在途已满一个目标时长（等不到它，按已知的判断）。
    fn judgeable_stall(&self, now: Instant) -> Option<usize> {
        let (since, track) = self.next_stall()?;
        if since > now {
            return None;
        }
        let stalled = &self.tracks[track];
        let own = stalled.observed_since(since)
            || stalled
                .pending_deadline(since)
                .is_some_and(|deadline| deadline <= now);
        let max_us = self.max_us();
        let others = self
            .tracks
            .iter()
            .enumerate()
            .filter(|&(i, t)| i != track && t.needs_refresh(max_us))
            .all(|(_, t)| t.observed_since(since) || t.refresh_overdue(now));
        (own && others).then_some(track)
    }

    /// 第 `track` 条轨停滞：看起来直播已结束时定下结束原因，否则任务失败。会话定下之前停滞的是还没拿到候选的轨，
    /// 直播看起来已结束时，还没拿到候选的轨都没有内容，这次都不录。
    async fn on_stall(&mut self, track: usize) -> Result<(), Error> {
        let now = Instant::now();
        let max_us = self.max_us();
        let others_live = self
            .tracks
            .iter()
            .enumerate()
            .any(|(i, t)| i != track && t.needs_refresh(max_us) && t.is_live(now));
        let cause = self.tracks[track]
            .stall(now, others_live)
            .map_err(|cause| Error::LiveStalled { track, cause })?;
        self.end(LiveEnd::Stalled { track, cause });
        if self.tracks[track].is_undecided() {
            self.drop_waiting().await?;
        }
        Ok(())
    }

    /// 一个后台任务完成。
    async fn on_done(&mut self, done: Done, fetcher: &mut Fetcher) -> Result<(), Error> {
        match done {
            Done::Refreshed {
                track,
                started,
                result,
            } => {
                self.tracks[track].refresh_returned();
                let handled = self.refreshed(track, started, result, fetcher).await;
                self.prepare_next(track);
                handled
            }
            Done::Prepared {
                track,
                fetched,
                result,
            } => {
                self.tracks[track].prepared();
                self.process(track, fetched, result?, fetcher).await?;
                self.prepare_next(track);
                Ok(())
            }
            Done::Checked { track, result } => self.checked(track, result?).await,
            Done::Mp4Checked { result } => {
                let check = self.mp4.as_mut().expect("只在输出 MP4 时派出");
                let reason = check.judged(result)?;
                self.report_mp4(reason);
                Ok(())
            }
        }
    }

    /// 核对出的放不进 MP4 的原因放进进度；已有的不换。
    fn report_mp4(&self, reason: Option<Mp4Unsupported>) {
        if let Some(reason) = reason {
            self.progress.send_modify(|p| {
                p.mp4_unsupported.get_or_insert(reason);
            });
        }
    }

    /// 第 `track` 条轨一次刷新的结果：可以再试的失败安排下次刷新，其余失败上抛；结束中拉到的不再处理。
    /// 会话定下之前发起、定下之后才返回的，暂存起来排在之前暂存的后面（这次不录的轨不刷新，所以只有要录的轨）。
    async fn refreshed(
        &mut self,
        track: usize,
        started: Instant,
        result: Result<(hls::MediaPlaylist, Option<NewInits>), Error>,
        fetcher: &mut Fetcher,
    ) -> Result<(), Error> {
        if self.ending.is_some() {
            return Ok(());
        }
        let (playlist, inits) = match result {
            Ok(loaded) => loaded,
            Err(e) => {
                let Some(cause) = waitable_refresh_error(&e) else {
                    return Err(e);
                };
                self.tracks[track].refresh_failed(cause, Instant::now());
                return Ok(());
            }
        };
        self.tracks[track].refreshed();
        let fetched = Fetched { playlist, started };
        if self.tracks[track].is_undecided() {
            return self.undecided(track, fetched).await;
        }
        match inits {
            Some(inits) => self.process(track, fetched, inits, fetcher).await,
            None => {
                self.tracks[track].hold(fetched);
                Ok(())
            }
        }
    }

    /// 第 `track` 条轨没有在途的刷新或准备时，开始准备最早暂存的播放列表。
    fn prepare_next(&mut self, track: usize) {
        if let Some((fetched, inits)) = self.tracks[track].start_preparing(Instant::now()) {
            self.tasks.prepare(self.http.clone(), track, fetched, inits);
        }
    }

    /// 按定下的会话处理第 `track` 条轨的一份播放列表：新 init 段落盘后再把新分片排入下载。与之前刷新得到的接不上时
    /// 见 [`Recorder::source_broke`]。
    async fn process(
        &mut self,
        track: usize,
        fetched: Fetched,
        inits: NewInits,
        fetcher: &mut Fetcher,
    ) -> Result<(), Error> {
        let session = self.session();
        let scope = Scope {
            track,
            session,
            layout: self.dir.layout(),
            max_us: self.max_us(),
        };
        let now = Instant::now();
        let Fetched { playlist, started } = fetched;
        let update = match self.tracks[track].apply(&scope, &playlist, inits, started, now)? {
            ControlFlow::Break(cause) => {
                return self.source_broke(track, cause, Fetched { playlist, started });
            }
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
        Ok(())
    }

    /// 第 `track` 条轨的播放列表 `fetched` 与之前刷新得到的接不上。只录这一条轨时另起会话接着录（见
    /// [`LiveTrack::restart`]）；录的不止一条轨时其他轨从哪里起与它对得上不知道，定下结束原因，这条轨暂存的不再处理。
    fn source_broke(
        &mut self,
        track: usize,
        cause: SourceBreak,
        fetched: Fetched,
    ) -> Result<(), Error> {
        if self.tracks.len() > 1 {
            self.tracks[track].stop_processing();
            self.end(LiveEnd::Broken { track, cause });
            return Ok(());
        }
        let session = self
            .session()
            .checked_add(1)
            .ok_or_else(|| self.sessions_exhausted())?;
        self.phase = Phase::Decided { session };
        self.tracks[track].restart(cause, fetched);
        self.new_sessions.push(NewSession { session, cause });
        Ok(())
    }

    /// 另起会话而会话编号已达上限：目录无法再用。
    fn sessions_exhausted(&self) -> Error {
        Error::WorkDir {
            path: self.dir.layout().root().to_path_buf(),
            problem: WorkDirProblem::Corrupt("会话编号已达上限".into()),
        }
    }

    /// 录制中的会话编号。
    fn session(&self) -> u32 {
        match self.phase {
            Phase::Decided { session } => session,
            Phase::Deciding(_) => panic!("会话定下之后才处理播放列表、下载分片"),
        }
    }

    /// 一个分片的下载结果：取不到的记为缺失，其余失败上抛。
    fn on_finished(&mut self, id: ItemInfo, result: Result<u64, Error>) -> Result<(), Error> {
        let error = match result {
            Ok(len) => {
                count_done(self.progress, id, len);
                self.tracks[id.track].segment_recorded(Instant::now());
                return Ok(());
            }
            Err(error) => error,
        };
        let Some(failure) = error.missable() else {
            return Err(error);
        };
        let reason = MissReason::Failed(failure.kind());
        self.tracks[id.track].segment_missed(failure);
        self.missed.push(Missed {
            session: self.session(),
            track: id.track,
            first: id.name.sequence,
            last: id.name.sequence,
            reason,
        });
        self.progress.send_modify(|p| p.segments_failed += 1);
        Ok(())
    }

    /// 录制结束后收尾：正常结束时等已列出的分片下完；失败时取消其余下载、等它们退出。
    async fn settle(
        mut self,
        result: Result<LiveEnd, Error>,
        fetcher: &mut Fetcher,
    ) -> Result<Outcome, Error> {
        match result {
            Ok(end) => {
                fetcher.drain(|id, r| self.on_finished(id, r)).await?;
                Ok(Outcome {
                    end,
                    missed: self.missed,
                    new_sessions: self.new_sessions,
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

/// 刷新失败中可以等下次刷新的及其原因：取不到（含 404/410，直播结束时常见）、内容为空、截断或语法错误（服务器
/// 没写完）。其余（401/403 等、内容不是播放列表、DRM、回调出错）为 None，使任务失败。
fn waitable_refresh_error(error: &Error) -> Option<RefreshCause> {
    match error {
        // 刷新只取播放列表，没写完的只会是它
        Error::Incomplete { .. } => Some(RefreshCause::Truncated),
        Error::Playlist { cause, .. } => match &**cause {
            hls::Malformed::Empty => Some(RefreshCause::Empty),
            hls::Malformed::Syntax { line, kind } => Some(RefreshCause::Syntax {
                line: *line,
                kind: kind.clone(),
            }),
            hls::Malformed::NotAPlaylist | hls::Malformed::NotUtf8 | hls::Malformed::Mixed => None,
        },
        _ => error.http_failure().map(RefreshCause::Http),
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
