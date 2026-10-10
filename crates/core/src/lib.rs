//! HLS 下载引擎：选轨、并发拉取分片、AES-128 解密与校验、可续传的任务目录、直播录制、合并为 MP4。
//!
//! 一个任务把一个播放列表地址变成一个输出文件。任务目录（默认 `<输出>.hsdl`）保存 `job.json`
//! 与已完成的分片；分片先写 `.part`、落盘后原子改名，因此「最终文件存在」即「该分片完整」。
//! 中断后用同样的请求再次运行：点播续传缺的分片（播放列表变了则拒绝），直播按 [`Resume`] 继续录制或只合并。
//!
//! 失败一律经 [`Job::wait`] 的 `Err` 返回；任务失败或取消时不生成输出文件，任务目录保留以便续传。
//!
//! 结果与错误里的轨道编号（`track`）：第 0 条为所选变体（来源本身是媒体播放列表时即它），第 1 条（若有）为
//! 独立的音频 rendition。

mod blocking;
mod crypto;
mod error;
mod fetch;
mod hooks;
mod http;
mod ident;
mod job;
mod live;
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
pub use hs_m3u8_hls as hls;
pub use hs_m3u8_remux::{Report, Shape, StreamKind, StreamReport};
pub use report::{LiveEnd, LiveReport, MissReason, Missed, Output, Progress, Stage, StallCause};
pub use request::{HttpOptions, JobRequest, LiveOptions, Resume, RetryPolicy, Source, Timeouts};
pub use url::Url;

pub(crate) use blocking::blocking;

use crate::http::Http;

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

    /// 校验请求并在当前 tokio 运行时上启动任务；不在 tokio 运行时内调用会 panic。
    /// 参数错误与输出已存在时立即返回错误。
    pub fn start(&self, request: JobRequest) -> Result<Job, Error> {
        request.validate()?;
        let http = Http::new(
            &request.source.http,
            request.source.hooks.clone(),
            self.requests.clone(),
        )?;
        let cancel = CancellationToken::new();
        let stop = CancellationToken::new();
        let (progress_tx, progress) = watch::channel(Progress::default());
        let task = tokio::spawn(job::run(
            request,
            http,
            cancel.clone(),
            stop.clone(),
            progress_tx,
        ));
        Ok(Job {
            cancel: cancel.clone(),
            stop,
            progress,
            task,
            _guard: cancel.drop_guard(),
        })
    }
}

/// 运行中的任务。丢弃句柄即取消任务；已进入合并阶段的任务仍会在后台完成合并并写出输出。
pub struct Job {
    cancel: CancellationToken,
    stop: CancellationToken,
    progress: watch::Receiver<Progress>,
    task: JoinHandle<Result<Output, Error>>,
    _guard: DropGuard,
}

impl Job {
    pub fn progress(&self) -> watch::Receiver<Progress> {
        self.progress.clone()
    }

    /// 请求取消；[`Job::wait`] 随后返回 [`Error::Cancelled`]，已完成的分片保留在任务目录中。
    /// 合并阶段不响应取消：已进入合并的任务照常完成。
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// 直播：停止刷新，把已拉到的播放列表处理完、已列出的分片下完后合并，[`Job::wait`] 返回录到的部分；续录时
    /// 会话还没定下就立即结束，不录新的分片。点播不受影响。
    pub fn stop(&self) {
        self.stop.cancel();
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
