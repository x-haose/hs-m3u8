//! 直播录制：按 RFC 8216 6.3.4 的节奏刷新各轨的媒体播放列表，新分片交给下载器，直到满足结束条件。
//!
//! 录制按会话组织：一个会话是一段时间线连续的录制，分片文件名带会话编号，合并按（会话, 不连续段）分组，
//! 组内保留原时间戳（缺失的分片处时间线留空），组与组首尾相接。中断后再次运行时先判定接着上一个会话录还是
//! 另起一个（见 [`deciding`]），再开始录。
//!
//! 一个事件循环驱动整个录制：刷新、拉 init 段、核对续录的内容与之前录齐的组都是后台任务（见 [`tasks`]），完成后
//! 作为事件交回，循环从不等某一个网络请求；新录齐的组能否放进合并文件（见 [`MergeCheck`]）只读两组开头的分片，在循环里
//! 做完。拉到的播放列表先暂存在各轨，会话定下后逐份拉好 init 段、按顺序处理。
//! 定下结束原因后不再刷新，把暂存的处理完才结束；停止（[`crate::JobControl::stop`]）也是如此，但会话定下之前停止
//! 立即结束，不录新的分片。只有一条音视频轨时，它的播放列表与之前刷新得到的接不上就另起会话接着录（见
//! [`crate::NewSession`]）。
//!
//! 轨道编号：第 0 条为所选变体（或来源本身的媒体播放列表），其后依次是各独立音频、各字幕 rendition。

mod deciding;
mod merge;
mod merge_check;
mod session;
mod stall;
mod tails;
mod tasks;
mod track;
mod window;

use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use hs_m3u8_hls as hls;
use hs_m3u8_remux::{Container, Streams};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) use self::merge::{MergePlan, merge_plan, report_missed};
use self::merge_check::{Batch, MergeCheck};
use self::session::Verdicts;
use self::stall::Stall;
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
    Error, LiveEnd, MergeUnsupported, MissReason, Missed, NewSession, Progress, RefreshReason,
    SegmentCounts, SourceBreak, Stage, TailCause, WorkDirProblem,
};

/// 本次运行的录制结果。
pub(crate) struct Outcome {
    pub end: LiveEnd,
    /// 本次运行中记下原因的缺失（未合并成区间）
    pub missed: Vec<Missed>,
    /// 本次运行录制中途另起的会话
    pub new_sessions: Vec<NewSession>,
    /// 本次运行判出原因的尾部没录到的字幕轨：（会话, 轨号, 原因），见 [`crate::UnrecordedTail`]
    pub tails: Vec<(u32, usize, TailCause)>,
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
    /// 有字幕轨到了停滞开始的时刻（见 [`Recorder::tail_due`]）
    TailCheck,
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
    /// 输出含合并文件 时录制中的核对；不输出合并文件 时为 None
    merge: Option<MergeCheck>,
    /// 已察觉的停滞（见 [`Recorder::observe_stall`]）
    stall: Option<Stall>,
    /// 按轨号：这条轨是音视频轨（字幕轨为 false，不定录制的结束）
    media: Vec<bool>,
    /// 按轨号：这条字幕轨在录制中的会话里停滞过（之前的运行记下的，或本次满了停滞时限），置上时它的进展次数（见
    /// [`LiveTrack::progress`]）；之后还没有进展（见 [`Recorder::wants_tail`]）。停滞时置上，有进展才清：排进队列的分片
    /// 可能一个也下不成
    marked: Vec<Option<u64>>,
    /// 本次运行判出原因的尾部（会话, 轨号, 原因），见 [`Recorder::note_tail_causes`]
    tail_causes: Vec<(u32, usize, TailCause)>,
    /// 另起会话之前的会话里在途、之后才录到的字幕分片所在的（会话, 轨号）：那个会话因停滞记下的尾部不再算，由
    /// [`Recorder::update_tails`] 撤掉
    recovered: Vec<(u32, usize)>,
    /// 因接不上记下的旧会话尾部（会话, 轨号）：停之前排进的分片之后下完，不说明停之后的录到了，不撤
    broken_tails: Vec<(u32, usize)>,
    /// 按轨号：已写进任务目录的尾部（见 [`crate::workdir::WorkDir::set_tail`]）；由 [`Recorder::update_tails`] 改
    tails_written: Vec<bool>,
}

/// 录制：定下会话，从首次拉到的播放列表开始，直到满足结束条件，并等已列出的分片下完。要合并时 `merge` 为各轨的
/// 取流方式，录制中核对能否放进合并文件（见 [`MergeCheck`]），放不进的原因放进进度，录制照常进行。
///
/// 本次的记录有须核对的差别时（见 [`WorkDir::source_change`]），能否改记见 [`session`]。
pub(crate) async fn record(
    ctx: Context<'_>,
    tracks: Vec<ResolvedTrack>,
    options: LiveOptions,
    merge: Option<(Vec<Streams>, Container)>,
) -> Result<Outcome, Error> {
    let dir = ctx.dir;
    let stored = dir.scan(tracks.len()).await?;
    count_stored(&stored, ctx.progress);
    let now = Instant::now();
    // 按轨号：音视频轨为 true，字幕轨为 false
    let media: Vec<bool> = dir
        .record()
        .streams()
        .iter()
        .map(|&s| s != Streams::Subtitles)
        .collect();
    let phase = initial_phase(&stored, dir, media.clone())?;
    let (merge, batches) = match merge {
        Some((streams, container)) => {
            let (check, batches) = merge_check(streams, container, &stored, dir)?;
            (Some(check), batches)
        }
        None => (None, Vec::new()),
    };
    let states = live_tracks(&tracks, &stored, now)?;
    let mut recorder = Recorder::new(&ctx, options, states, phase, merge, media);
    recorder.start_merge_check(batches);
    recorder.announce_recording();
    let first = tracks
        .into_iter()
        .map(|t| Fetched {
            playlist: t.playlist,
            started: now,
        })
        .collect();
    let signals = Signals {
        cancel: ctx.cancel,
        stop: ctx.stop,
    };
    let result = match recorder.start(first).await {
        Ok(()) => recorder.run(&signals, ctx.fetcher).await,
        Err(e) => Err(e),
    };
    recorder.tasks.shutdown().await;
    recorder.publish_refresh_errors(false);
    recorder.settle(result, ctx.fetcher).await
}

/// 录制开始时的阶段：目录里有录过的分片时先判定接着哪个会话录（`media` 按轨号标出参与判定的音视频轨），没有时直接录
/// 第 0 个会话。
fn initial_phase(stored: &Stored, dir: &WorkDir, media: Vec<bool>) -> Result<Phase, Error> {
    let verdicts =
        Verdicts::new(stored, dir.source_change(), media).map_err(|reason| Error::WorkDir {
            path: dir.layout().root().to_path_buf(),
            problem: WorkDirProblem::Corrupt(reason),
        })?;
    Ok(match verdicts {
        Some(verdicts) => Phase::Deciding(verdicts),
        None => Phase::Decided { session: 0 },
    })
}

/// 录制中能否放进合并文件 的核对（`streams` 为各轨的取流方式），与核对目录里已录齐的组的各批（见 [`MergeCheck::new`]）。
fn merge_check(
    streams: Vec<Streams>,
    container: Container,
    stored: &Stored,
    dir: &WorkDir,
) -> Result<(MergeCheck, Vec<Batch>), Error> {
    let plan = merge_plan(&stored.segments, &streams, dir.layout())?;
    Ok(MergeCheck::new(streams, container, &plan, stored))
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
    let duration_us = stored.duration_us(0);
    progress.send_modify(|p| {
        p.segments = SegmentCounts::default();
        p.bytes = 0;
        for track in 0..stored.segments.len() {
            if let Some(t) = p.tracks.get_mut(track) {
                t.segments = SegmentCounts::default();
                t.bytes = 0;
            }
            let done = stored.segments[track].len();
            p.segments_of(track, |s| {
                s.done += done;
                s.total += done;
            });
            p.add_bytes(track, stored.track_bytes(track));
        }
        p.duration = Duration::from_micros(duration_us);
    });
}

impl<'a> Recorder<'a> {
    fn new(
        ctx: &Context<'a>,
        options: LiveOptions,
        tracks: Vec<LiveTrack>,
        phase: Phase,
        merge: Option<MergeCheck>,
        media: Vec<bool>,
    ) -> Recorder<'a> {
        let tailed = match phase {
            Phase::Decided { session } => tailed_in(ctx.dir, session, tracks.len()),
            Phase::Deciding(_) => vec![false; tracks.len()],
        };
        let marked = tailed
            .iter()
            .zip(&tracks)
            .map(|(&t, track)| t.then_some(track.progress()))
            .collect();
        if let Phase::Decided { session } = phase {
            ctx.progress.send_modify(|p| {
                p.session = Some(session);
                for (track, &t) in p.tracks.iter_mut().zip(&tailed) {
                    track.unrecorded_tail = t;
                }
            });
        }
        let tails_written = tailed;
        Recorder {
            http: ctx.http.clone(),
            hooks: ctx.hooks.clone(),
            dir: ctx.dir,
            progress: ctx.progress,
            options,
            tracks,
            missed: Vec::new(),
            new_sessions: Vec::new(),
            tasks: Tasks::new(ctx.cancel),
            direct: ctx.fetcher.direct(),
            phase,
            ending: None,
            merge,
            stall: None,
            media,
            marked,
            tail_causes: Vec::new(),
            recovered: Vec::new(),
            broken_tails: Vec::new(),
            tails_written,
        }
    }

    /// 在后台按顺序核对目录里已录齐的组（`batches`，见 [`MergeCheck::new`]）；不输出合并文件 或没有录齐的组时什么也不做。
    fn start_merge_check(&mut self, batches: Vec<Batch>) {
        if let Some(check) = &self.merge
            && !batches.is_empty()
        {
            let root = self.dir.layout().root().to_path_buf();
            self.tasks
                .check_merge(check.streams().to_vec(), check.container(), batches, root);
        }
    }

    /// 进度进入录制阶段，带上各轨当前的刷新失败。
    fn announce_recording(&self) {
        let refresh_errors = self.refresh_errors(true);
        self.progress.send_modify(|p| {
            p.stage = Stage::Recording;
            p.refresh_errors = refresh_errors;
        });
    }

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
            let event = self.next_event(signals, fetcher).await?;
            match event {
                Event::Stop => match self.phase {
                    Phase::Deciding(_) => {
                        return Ok(self.ending.clone().unwrap_or(LiveEnd::Stopped));
                    }
                    Phase::Decided { .. } => self.end(LiveEnd::Stopped),
                },
                Event::StallCheck => {
                    let now = Instant::now();
                    if let Some(stall) = self.observe_stall(now)
                        && self.judgeable(stall, now)
                    {
                        self.on_stall(stall, now).await?;
                    }
                }
                Event::Due => {}
                Event::TailCheck => {}
                Event::Done(done) => self.on_done(done, fetcher).await?,
                Event::Downloaded((id, result)) => {
                    let recorded = result.is_ok();
                    self.on_finished(id, result)?;
                    if let (true, Some(check)) = (recorded, &mut self.merge) {
                        let reason = check.recorded(id, self.dir.layout()).await?;
                        self.report_merge(reason);
                    }
                }
            }
            self.update_tails(Instant::now()).await?;
            self.publish_refresh_errors(true);
        }
    }

    /// 录制结束时的结束原因；还没结束时为 None。暂存的播放列表都处理完才算结束：判定期间拉到的候选可能
    /// 已带 ENDLIST，这条轨已算结束，候选还没处理。各条音视频轨都出现 ENDLIST（或都录满）即结束，字幕轨不看。
    fn finished(&self) -> Option<LiveEnd> {
        if matches!(self.phase, Phase::Deciding(_)) || self.tracks.iter().any(LiveTrack::has_held) {
            return None;
        }
        // 字幕轨不定录制的结束：有的服务器不给字幕播放列表补 ENDLIST
        let mut media = self
            .tracks
            .iter()
            .zip(&self.media)
            .filter(|(_, media)| **media)
            .map(|(t, _)| t);
        match &self.ending {
            Some(end) => Some(end.clone()),
            None if media.clone().all(LiveTrack::is_ended) => Some(LiveEnd::EndList),
            None if !media.any(|t| t.needs_refresh(self.max_us())) => {
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
    fn refresh_errors(&self, recording: bool) -> Vec<Option<RefreshReason>> {
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
    /// 刚录到的分片不被误判为停滞。醒来之后见 [`Recorder::after_wake`]。
    async fn next_event(
        &mut self,
        signals: &Signals<'_>,
        fetcher: &mut Fetcher,
    ) -> Result<Event, Error> {
        let ending = self.ending.is_some();
        // 会话定下之前停止立即结束，结束中也照样响应
        let stoppable = !ending || matches!(self.phase, Phase::Deciding(_));
        let tail_due = if ending { None } else { self.tail_due() };
        let (stall, next_due) = if ending {
            (None, None)
        } else {
            let now = Instant::now();
            let stall = self.observe_stall(now);
            let since = stall.map(|s| s.since);
            self.start_due_refreshes(now, since);
            let next_due = self.refreshing().filter_map(|(_, t)| t.due_at(since)).min();
            (self.stall_check_at(now, stall), next_due)
        };
        let downloads = async {
            match fetcher.next().await {
                Some(finished) => finished,
                None => std::future::pending().await,
            }
        };
        let event = tokio::select! {
            biased;
            _ = signals.cancel.cancelled() => Err(Error::Cancelled),
            done = self.tasks.next() => Ok(Event::Done(done)),
            finished = downloads => Ok(Event::Downloaded(finished)),
            _ = signals.stop.cancelled(), if stoppable => Ok(Event::Stop),
            _ = sleep_until(stall), if stall.is_some() => Ok(Event::StallCheck),
            _ = sleep_until(next_due), if next_due.is_some() => Ok(Event::Due),
            _ = sleep_until(tail_due), if tail_due.is_some() => Ok(Event::TailCheck),
        };
        self.after_wake(stall, Instant::now());
        event
    }

    fn max_us(&self) -> Option<u64> {
        self.options
            .max_duration
            .map(|max| u64::try_from(max.as_micros()).unwrap_or(u64::MAX))
    }

    /// 还要刷新的轨及其编号（见 [`LiveTrack::needs_refresh`]）。
    fn refreshing(&self) -> impl Iterator<Item = (usize, &LiveTrack)> {
        let max_us = self.max_us();
        self.tracks
            .iter()
            .enumerate()
            .filter(move |(_, t)| t.needs_refresh(max_us))
    }

    /// 发起到期的刷新；`stall_since` 见 [`LiveTrack::due_at`]。
    fn start_due_refreshes(&mut self, now: Instant, stall_since: Option<Instant>) {
        let max_us = self.max_us();
        for (index, t) in self.tracks.iter_mut().enumerate() {
            if !t.needs_refresh(max_us) || t.due_at(stall_since).is_none_or(|at| at > now) {
                continue;
            }
            let request = t.start_refresh(now);
            self.tasks.refresh(
                self.http.clone(),
                self.hooks.clone(),
                self.direct.clone(),
                index,
                request,
            );
        }
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
                let t = &mut self.tracks[track];
                if t.is_stopped() || t.is_stale(fetched.started, Instant::now()) {
                    return Ok(());
                }
                self.process(track, fetched, result?, fetcher).await?;
                self.prepare_next(track);
                Ok(())
            }
            Done::Checked { track, result } => self.checked(track, result?).await,
            Done::MergeChecked { result } => {
                let check = self.merge.as_mut().expect("只在输出合并文件时派出");
                let reason = check.judged(result)?;
                self.report_merge(reason);
                Ok(())
            }
        }
    }

    /// 核对出的放不进合并文件的原因放进进度；已有的不换。
    fn report_merge(&self, reason: Option<MergeUnsupported>) {
        if let Some(reason) = reason {
            self.progress.send_modify(|p| {
                p.merge_unsupported.get_or_insert(reason);
            });
        }
    }

    /// 第 `track` 条轨一次刷新的结果：可以再试的失败安排下次刷新，其余失败上抛。结束中拉到的、这条轨已不再处理（见
    /// [`LiveTrack::is_stopped`]）或属于重新开始之前（见 [`LiveTrack::is_stale`]）时的不再处理。
    /// 会话定下之前发起、定下之后才返回的，暂存起来排在之前暂存的后面（这次不录的轨不刷新，所以只有要录的轨）。
    async fn refreshed(
        &mut self,
        track: usize,
        started: Instant,
        result: Result<(hls::MediaPlaylist, Option<NewInits>), Error>,
        fetcher: &mut Fetcher,
    ) -> Result<(), Error> {
        if self.ending.is_some()
            || self.tracks[track].is_stopped()
            || self.tracks[track].is_stale(started, Instant::now())
        {
            return Ok(());
        }
        let (playlist, inits) = match result {
            Ok(loaded) => loaded,
            Err(e) => {
                let cause = match waitable_refresh_error(&e) {
                    Some(cause) => cause,
                    None if !self.media[track] => match e.subtitle_refresh_failure() {
                        Some(cause) => cause,
                        None => return Err(e),
                    },
                    None => return Err(e),
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
            self.tasks
                .prepare(self.direct.clone(), track, fetched, inits);
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
                return self
                    .source_broke(track, cause, Fetched { playlist, started })
                    .await;
            }
            ControlFlow::Continue(update) => update,
        };
        // init 段先落盘、再下载引用它的分片：中断后合并时，分片引用的 init 段一定存在
        for (fingerprint, data) in update.init_files {
            let len = data.len() as u64;
            let stored = workdir::store_init(self.dir.layout(), track, fingerprint, data).await?;
            if stored == StoredInit::Created {
                self.progress.send_modify(|p| p.add_bytes(track, len));
            }
        }
        let failed = update.init_failed.len();
        let gaps = update.gaps.len();
        let expired = update.expired.as_ref().map_or(0, Missed::count);
        let listed = update.items.len() + failed;
        self.progress.send_modify(|p| {
            p.segments_of(track, |s| {
                s.total += listed;
                s.failed += failed;
                s.gap += gaps;
                s.expired = s.expired.saturating_add(expired);
            });
        });
        self.missed.extend(update.expired);
        self.missed.extend(update.init_failed);
        self.missed.extend(update.gaps);
        for item in update.items {
            fetcher.push(item);
        }
        Ok(())
    }

    /// 第 `track` 条轨的播放列表 `fetched` 与之前刷新得到的接不上。字幕轨只停它自己（见 [`LiveTrack::stop_broken`]）；
    /// 在随画面进的新会话里还没录到分片的按新窗口重新开始（见 [`LiveTrack::restart`]）。它是唯一的音视频轨时另起会话接着
    /// 录（见 [`LiveTrack::restart`]），字幕轨跟着进新会话（见 [`LiveTrack::rejoin`]、[`LiveTrack::forgive_restart`]）；音视频轨不止一条时其他轨从哪里起与它对得上不知道，定下结束原因，这条轨暂存的不再处理。
    async fn source_broke(
        &mut self,
        track: usize,
        cause: SourceBreak,
        fetched: Fetched,
    ) -> Result<(), Error> {
        if !self.media[track] {
            let t = &mut self.tracks[track];
            if t.is_unrecorded_in_session() {
                t.restart(cause, fetched);
            } else {
                t.stop_broken(cause, Instant::now());
            }
            return Ok(());
        }
        if self.media.iter().filter(|&&m| m).count() > 1 {
            self.tracks[track].stop_processing();
            self.end(LiveEnd::Broken { track, cause });
            return Ok(());
        }
        let session = self
            .session()
            .checked_add(1)
            .ok_or_else(|| self.sessions_exhausted())?;
        // 同一次重启里先被认出回退而停了的字幕轨不算接不上；旧会话的尾部写定、判出原因，再换到新会话
        let mut rebuild = vec![false; self.tracks.len()];
        for subtitles in (0..self.tracks.len()).filter(|&t| !self.media[t]) {
            // 字幕轨与回退的这条音视频轨不是同一条
            let (low, high) = self.tracks.split_at_mut(subtitles.max(track));
            let (subtitle, media) = if subtitles < track {
                (&mut low[subtitles], &high[0])
            } else {
                (&mut high[0], &low[track])
            };
            rebuild[subtitles] = subtitle.forgive_restart(media);
        }
        let now = Instant::now();
        self.update_tails(now).await?;
        self.note_tail_causes(now);
        let old = self.session();
        for t in (0..self.tracks.len()).filter(|&t| self.tracks[t].is_broken()) {
            self.broken_tails.push((old, t));
        }
        self.decide(session);
        self.tracks[track].restart(cause, fetched);
        for ((t, &media), rebuild) in self.tracks.iter_mut().zip(&self.media).zip(rebuild) {
            if !media {
                t.rejoin(now, rebuild);
            }
        }
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

    /// 定下录制的会话，进度里随之写上。
    fn decide(&mut self, session: u32) {
        self.phase = Phase::Decided { session };
        let tailed = tailed_in(self.dir, session, self.tracks.len());
        self.marked = tailed
            .iter()
            .zip(&self.tracks)
            .map(|(&t, track)| t.then_some(track.progress()))
            .collect();
        self.tails_written.clone_from(&tailed);
        let tailed = &tailed;
        self.progress.send_modify(|p| {
            p.session = Some(session);
            for (track, &t) in p.tracks.iter_mut().zip(tailed) {
                track.unrecorded_tail = t;
            }
        });
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
                let current = id.name.session == self.session();
                self.tracks[id.track].segment_recorded(Instant::now(), current);
                if !self.media[id.track] && !current {
                    self.recovered.push((id.name.session, id.track));
                }
                return Ok(());
            }
            Err(error) => error,
        };
        let failure = if self.media[id.track] {
            error.missable()
        } else {
            error.subtitle_missable()
        };
        let Some(failure) = failure else {
            return Err(error);
        };
        let reason = MissReason::Failed(failure.kind());
        self.tracks[id.track].segment_missed(failure);
        self.missed.push(Missed {
            session: id.name.session,
            track: id.track,
            first: id.name.sequence,
            last: id.name.sequence,
            reason,
        });
        self.progress
            .send_modify(|p| p.segments_of(id.track, |s| s.failed += 1));
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
                if let Phase::Decided { .. } = self.phase {
                    self.settle_tails(Instant::now()).await?;
                }
                Ok(Outcome {
                    end,
                    missed: self.missed,
                    new_sessions: self.new_sessions,
                    tails: self.tail_causes,
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

/// 任务目录里第 `session` 个会话记为尾部没录到的轨，按轨号标出（共 `tracks` 条）。
fn tailed_in(dir: &WorkDir, session: u32, tracks: usize) -> Vec<bool> {
    let tails = dir.tails();
    (0..tracks)
        .map(|track| tails.binary_search(&(session, track)).is_ok())
        .collect()
}

/// 刷新失败中可以等下次刷新的及其原因：取不到（含 404/410，直播结束时常见）、内容为空、截断或语法错误（服务器
/// 没写完）。其余（401/403 等、内容不是播放列表、DRM、回调出错）为 None，使任务失败。
fn waitable_refresh_error(error: &Error) -> Option<RefreshReason> {
    match error {
        // 刷新只取播放列表，没写完的只会是它
        Error::Incomplete { .. } => Some(RefreshReason::Truncated),
        Error::Playlist { cause, .. } => match &**cause {
            hls::Malformed::Empty => Some(RefreshReason::Empty),
            hls::Malformed::Syntax { line, kind } => Some(RefreshReason::Syntax {
                line: *line,
                kind: kind.clone(),
            }),
            hls::Malformed::NotAPlaylist | hls::Malformed::NotUtf8 | hls::Malformed::Mixed => None,
        },
        _ => error.http_failure().map(RefreshReason::Http),
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
