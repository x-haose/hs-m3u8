//! 直播录制：按 RFC 8216 6.3.4 的节奏刷新各轨的媒体播放列表，新分片交给下载器，直到满足结束条件。
//!
//! 录制按会话组织：一个会话是一段时间线连续的录制，分片文件名带会话编号，合并按（会话, 不连续段）分组，
//! 组内保留原时间戳（缺失的分片处时间线留空），组与组首尾相接。中断后再次运行时先判定接着上一个会话录还是
//! 另起一个（见 [`deciding`]），再开始录。
//!
//! 一个事件循环驱动整个录制：刷新、拉 init 段、核对内容都是后台任务（见 [`tasks`]），完成后作为事件交回，
//! 循环从不等某一个网络请求。拉到的播放列表先暂存在各轨，会话定下后逐份拉好 init 段、按顺序处理。
//! 定下结束原因后不再刷新，把暂存的处理完才结束；停止（[`crate::Job::stop`]）也是如此，但会话定下之前停止
//! 立即结束，不录新的分片。
//!
//! 轨道编号：第 0 条为所选变体（或来源本身的媒体播放列表），第 1 条（若有）为独立的音频 rendition。

mod deciding;
mod merge;
mod session;
mod tasks;
mod track;
mod window;

use std::ops::ControlFlow;
use std::sync::Arc;

use hs_m3u8_hls as hls;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) use self::merge::{merge_plan, report_missed};
use self::session::Verdicts;
use self::tasks::{Done, Tasks};
use self::track::{Fetched, LiveTrack};
use self::window::{NewInits, Scope};
use crate::fetch::{Direct, Fetcher, Finished, ItemId, count_done};
use crate::hooks::Hooks;
use crate::http::Http;
use crate::request::LiveOptions;
use crate::resolve::ResolvedTrack;
use crate::workdir::{self, Stored, StoredInit, WorkDir};
use crate::{Error, LiveEnd, MissReason, Missed, Progress, Stage};

/// 本次运行的录制结果。
pub(crate) struct Outcome {
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
    /// 该轨停滞
    Stall(usize),
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
    tasks: Tasks,
    /// 核对内容用的直接拉取
    direct: Direct,
    phase: Phase,
    /// 定下的结束原因：不再刷新，处理完暂存的播放列表即结束
    ending: Option<LiveEnd>,
}

/// 录制：定下会话，从首次拉到的播放列表开始，直到满足结束条件，并等已列出的分片下完。
///
/// 目录里记录的完整来源地址与本次的不同时（见 [`WorkDir::url_changed`]），处理见 [`session::NewUrl`]。
pub(crate) async fn record(
    ctx: Context<'_>,
    tracks: Vec<ResolvedTrack>,
    options: LiveOptions,
) -> Result<Outcome, Error> {
    let (dir, progress) = (ctx.dir, ctx.progress);
    progress.send_modify(|p| p.stage = Stage::Recording);
    let stored = dir.scan(tracks.len()).await?;
    count_stored(&stored, progress);
    let now = Instant::now();
    // 目录里没有录过的分片时直接录第 0 个会话
    let phase = match Verdicts::new(&stored) {
        Some(verdicts) => Phase::Deciding(verdicts),
        None => Phase::Decided { session: 0 },
    };
    let mut recorder = Recorder {
        http: ctx.http,
        hooks: ctx.hooks,
        dir,
        progress,
        options,
        tracks: live_tracks(&tracks, &stored, now)?,
        missed: Vec::new(),
        tasks: Tasks::new(ctx.cancel),
        direct: ctx.fetcher.direct(),
        phase,
        ending: None,
    };
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
                    Phase::Deciding(_) => return Ok(LiveEnd::Stopped),
                    Phase::Decided { .. } => self.end(LiveEnd::Stopped),
                },
                Event::Stall(track) => self.on_stall(track).await?,
                Event::Due => {}
                Event::Done(done) => self.on_done(done, fetcher).await?,
                Event::Downloaded((id, result)) => self.on_finished(id, result)?,
            }
        }
    }

    /// 录制结束时的结束原因；还没结束时为 None。
    fn finished(&self) -> Option<LiveEnd> {
        if matches!(self.phase, Phase::Deciding(_)) {
            return None;
        }
        match &self.ending {
            Some(_) if self.tracks.iter().any(LiveTrack::has_held) => None,
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
            self.start_due_refreshes(Instant::now());
            let max_us = self.max_us();
            let next_due = self
                .tracks
                .iter()
                .filter(|t| t.needs_refresh(max_us))
                .filter_map(LiveTrack::due_at)
                .min();
            (self.next_stall(), next_due)
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
            _ = sleep_until(stall.map(|(at, _)| at)), if stall.is_some() => {
                Ok(Event::Stall(stall.expect("分支只在有停滞时刻时启用").1))
            }
            _ = sleep_until(next_due), if next_due.is_some() => Ok(Event::Due),
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
            self.tasks
                .refresh(self.http.clone(), self.hooks.clone(), index, request);
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
                // 结束中不再处理新拉到的
                if self.ending.is_some() {
                    return Ok(());
                }
                let (playlist, inits) = match result {
                    Ok(loaded) => loaded,
                    Err(e) if waitable_refresh_error(&e) => {
                        self.tracks[track].refresh_failed(e, Instant::now());
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                };
                self.tracks[track].refreshed(Instant::now());
                let fetched = Fetched { playlist, started };
                if self.tracks[track].is_undecided() {
                    return self.undecided(track, fetched).await;
                }
                match inits {
                    Some(inits) => self.process(track, fetched, inits, fetcher).await,
                    // 会话定下之前发起的刷新：暂存，与之前暂存的一起按顺序处理
                    None => {
                        self.hold(track, fetched);
                        Ok(())
                    }
                }
            }
            Done::Prepared {
                track,
                fetched,
                result,
            } => {
                self.process(track, fetched, result?, fetcher).await?;
                self.prepare_next(track);
                Ok(())
            }
            Done::Checked { track, result } => self.checked(track, result?).await,
        }
    }

    /// 会话定下后暂存第 `track` 条轨的一份播放列表，等轮到它时准备、处理。这次不录的轨不刷新，
    /// 结束中拉到的不暂存，所以只有要录的轨走到这里。
    fn hold(&mut self, track: usize, fetched: Fetched) {
        self.tracks[track].hold(fetched);
        self.prepare_next(track);
    }

    /// 第 `track` 条轨没有在途的刷新或准备时，开始准备最早暂存的播放列表。
    fn prepare_next(&mut self, track: usize) {
        if let Some((fetched, inits)) = self.tracks[track].start_preparing(Instant::now()) {
            self.tasks.prepare(self.http.clone(), track, fetched, inits);
        }
    }

    /// 按定下的会话处理第 `track` 条轨的一份播放列表：新 init 段落盘后再把新分片排入下载。服务器前后矛盾或
    /// 编码器重启时定下结束原因，这条轨暂存的不再处理。
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
            ControlFlow::Break(end) => {
                self.tracks[track].stop_processing();
                self.end(end);
                return Ok(());
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

    /// 录制中的会话编号。
    fn session(&self) -> u32 {
        match self.phase {
            Phase::Decided { session } => session,
            Phase::Deciding(_) => panic!("会话定下之后才处理播放列表、下载分片"),
        }
    }

    /// 一个分片的下载结果：取不到的记为缺失，其余失败上抛。
    fn on_finished(&mut self, id: ItemId, result: Result<u64, Error>) -> Result<(), Error> {
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
            session: self.session(),
            track: id.track,
            first: id.sequence,
            last: id.sequence,
            reason: MissReason::Failed(kind),
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
