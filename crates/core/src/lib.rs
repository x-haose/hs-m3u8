//! HLS 下载引擎：选轨、并发拉取分片、AES-128 解密与校验、可续传的任务目录、直播录制，输出为 MP4、可直接播放的
//! 本地 HLS 目录，或两者都要。
//!
//! 一个任务把一个播放列表地址变成它的输出（[`Target`]）。任务目录（默认与输出同级、名为输出的主名加 `.hsdl`）保存 `job.json`
//! 与已完成的分片；分片先写 `.part`、落盘后原子改名，因此「最终文件存在」即「该分片完整」。
//! 中断后用同样的请求再次运行：点播续传缺的分片（播放列表变了则拒绝），直播续录（见 [`LiveOptions`]）；
//! 不联网、只合并已录到的直播用 [`Engine::merge_recorded`]。
//!
//! 失败一律经 [`Job::wait`] 的 `Err` 返回；任务失败或取消时不生成输出，任务目录保留以便续传。
//!
//! 结果与错误里的轨道编号（`track`）：第 0 条为所选变体（来源本身是媒体播放列表时即它），第 1 条（若有）为
//! 独立的音频 rendition。

mod blocking;
mod crypto;
mod entries;
mod error;
mod fetch;
mod hooks;
mod http;
mod ident;
mod info;
mod job;
mod live;
mod output;
mod probe;
mod report;
mod request;
mod resolve;
mod selection;
mod verify;
mod vod;
mod workdir;

use std::num::NonZeroUsize;
use std::sync::Arc;

use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::{CancellationToken, DropGuard};

pub use error::{Error, HttpError, Integrity, JobType, StallError, Unsupported, WorkDirProblem};
pub use hooks::{HookError, HookKind, Hooks, NoHooks, Purpose, RequestParts};
pub use info::{AudioInfo, MasterInfo, Selected, TrackInfo, VariantInfo};
pub use output::{OutputOptions, Target};
pub use probe::Probe;
pub use report::{
    LiveEnd, LiveReport, MissReason, Missed, Mp4Output, Output, Progress, RefreshCause, Stage,
    StallCause,
};
pub use request::{
    HttpOptions, JobRequest, KeyOverride, LiveOptions, RetryPolicy, Source, Timeouts,
};
pub use url::Url;

/// 公开接口用到的合并报告与合并错误的类型。
pub mod remux {
    pub use hs_m3u8_remux::{Error, FfmpegError, Report, Shape, StreamKind, StreamReport};
}

/// 公开接口用到的播放列表解析与选轨类型。
pub mod hls {
    pub use hs_m3u8_hls::{
        AudioChoice, Error, Preference, Resolution, SelectError, SyntaxError, Unsupported,
        VariantChoice,
    };
}

pub(crate) use blocking::blocking;

use crate::http::{Http, OnReceived};

/// 下载引擎；持有跨任务共享的在途请求上限。
#[derive(Clone)]
pub struct Engine {
    requests: Arc<Semaphore>,
}

impl Engine {
    /// `max_requests`：所有任务合计同时在途的 key、分片与点播 init 段请求数。播放列表与直播刷新时新出现的
    /// init 段不占名额，以免直播刷新排在其他任务的大批下载之后。
    pub fn new(max_requests: NonZeroUsize) -> Self {
        Engine {
            requests: Arc::new(Semaphore::new(max_requests.get())),
        }
    }

    /// 拉取来源并按偏好选轨，不下载分片、不碰任务目录；请求与回调同下载。用于开始下载前列出可选的清晰度与
    /// 音轨、查看时长与是否直播。参数错误时立即返回错误；不在 tokio 运行时内调用会 panic。
    pub async fn probe(&self, source: &Source) -> Result<Probe, Error> {
        source.validate()?;
        let http = Http::new(
            &source.http,
            source.hooks.clone(),
            self.requests.clone(),
            None,
        )?;
        probe::probe(&http, source).await
    }

    /// 校验请求并在当前 tokio 运行时上启动任务；不在 tokio 运行时内调用会 panic。参数错误时立即返回错误，
    /// 不访问文件系统；输出已存在等要读文件系统的检查在任务开头、任何下载之前进行，结果经 [`Job::wait`] 返回。
    pub fn start(&self, request: JobRequest) -> Result<Job, Error> {
        request.validate()?;
        let (progress_tx, progress) = watch::channel(Progress::default());
        let http = Http::new(
            &request.source.http,
            request.source.hooks.clone(),
            self.requests.clone(),
            Some(count_received(progress_tx.clone())),
        )?;
        let cancel = CancellationToken::new();
        let stop = CancellationToken::new();
        let task = tokio::spawn(job::run(
            request,
            http,
            cancel.clone(),
            stop.clone(),
            progress_tx,
        ));
        Ok(Job::new(task, cancel, stop, progress))
    }

    /// 不联网，只把任务目录中已录到的直播分片写成输出；用于中断后不再续录、或来源已取不到的录制。与
    /// [`Engine::start`] 一样返回任务句柄：进度给出已录的分片数、字节数与时长，阶段先为 [`Stage::Preparing`]、
    /// 写出时为 [`Stage::Writing`]；开始写出前可取消，停止不起作用。参数错误时立即返回错误；不在 tokio 运行时内调用会 panic。
    ///
    /// 任务目录见 [`OutputOptions::resolved_work_dir`]。目录里是点播的下载时报
    /// [`WorkDirProblem::NotLiveRecording`]（点播用同样的请求再次运行即可续传）；目录不存在或没有已完成的分片时报
    /// [`Error::NothingRecorded`]。结果的 [`LiveReport::end`] 为 None。
    pub fn merge_recorded(&self, output: OutputOptions) -> Result<Job, Error> {
        output.validate()?;
        let (progress_tx, progress) = watch::channel(Progress::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(job::merge_recorded(output, cancel.clone(), progress_tx));
        Ok(Job::new(task, cancel, CancellationToken::new(), progress))
    }
}

/// 读到的响应体字节计入 [`Progress::received`]。
fn count_received(progress: watch::Sender<Progress>) -> OnReceived {
    Box::new(move |len| progress.send_modify(|p| p.received += len))
}

/// 运行中的任务：等结果用 [`Job::wait`]，取消、停止与读进度用 [`Job::control`]。丢弃句柄即取消任务；已开始写出
/// （[`Stage::Writing`]）的任务仍会在后台写完。
pub struct Job {
    control: JobControl,
    task: JoinHandle<Result<Output, Error>>,
    _guard: DropGuard,
}

impl Job {
    fn new(
        task: JoinHandle<Result<Output, Error>>,
        cancel: CancellationToken,
        stop: CancellationToken,
        progress: watch::Receiver<Progress>,
    ) -> Self {
        Job {
            control: JobControl {
                cancel: cancel.clone(),
                stop,
                progress,
            },
            task,
            _guard: cancel.drop_guard(),
        }
    }

    /// 控制句柄；与本句柄分开持有，等结果期间也能取消、停止。
    pub fn control(&self) -> JobControl {
        self.control.clone()
    }

    /// 等待任务结束。任务内部 panic（不变量被违反）原样传播；运行时关闭导致任务被取消时返回 [`Error::Cancelled`]。
    pub async fn wait(self) -> Result<Output, Error> {
        match self.task.await {
            Ok(result) => result,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => Err(Error::Cancelled),
        }
    }
}

/// 任务的控制句柄，可克隆。只持有它不会让任务继续运行（见 [`Job`]）；任务结束后调用不起作用。
#[derive(Clone)]
pub struct JobControl {
    cancel: CancellationToken,
    stop: CancellationToken,
    progress: watch::Receiver<Progress>,
}

impl JobControl {
    /// 进度快照的接收端。每读到一块数据就更新一次，按时间显示的消费者应定时采样，不要长时间持有
    /// `borrow()` 的结果（会挡住下载）。任务结束（成功、失败或取消）后发送端关闭，最后一份快照仍读得到；
    /// [`Stage`] 没有失败状态，失败时停在原来的阶段，结果只能从 [`Job::wait`] 取得。
    pub fn progress(&self) -> watch::Receiver<Progress> {
        self.progress.clone()
    }

    /// 请求取消；[`Job::wait`] 随后返回 [`Error::Cancelled`]，已完成的分片保留在任务目录中。
    /// 开始写出（[`Stage::Writing`]）后不响应取消：已开始写出的任务照常完成。
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// 直播：停止刷新，把已拉到的播放列表处理完、已列出的分片下完后写出，[`Job::wait`] 返回录到的部分；续录时
    /// 会话还没定下就立即结束，不录新的分片。点播与只合并不受影响。
    pub fn stop(&self) {
        self.stop.cancel();
    }
}
