//! HLS 下载引擎：选轨、并发拉取分片、AES-128 解密与校验、可续传的任务目录、合并为 MP4。
//!
//! 一个任务把一个播放列表地址变成一个输出文件。任务目录（默认 `<输出>.hsdl`）保存 `job.json`
//! 与已完成的分片；分片先写 `.part`、落盘后原子改名，因此「最终文件存在」即「该分片完整」，
//! 中断后用同样的请求再次运行即可续传。播放列表变化（计划摘要不同）时拒绝续传。
//!
//! 直播（播放列表没有 EXT-X-ENDLIST）按 [`LiveOptions`] 录制，结束后合并录到的部分，见 [`LiveReport`]。
//!
//! 失败一律经 [`Job::wait`] 的 `Err` 返回；任务失败或取消时不生成输出文件，任务目录保留以便续传。

mod crypto;
mod error;
mod fetch;
mod http;
mod job;
mod live;
mod plan;
mod workdir;

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::{CancellationToken, DropGuard};

pub use error::{Error, HttpError, Integrity, Unsupported};
pub use hs_m3u8_hls as hls;
pub use hs_m3u8_remux::Report;
pub use url::Url;

use crate::http::Http;

/// 下载引擎；持有跨任务共享的在途请求上限。
#[derive(Clone)]
pub struct Engine {
    requests: Arc<Semaphore>,
}

impl Engine {
    /// `max_requests`：所有任务合计同时在途的 HTTP 请求数（播放列表、key、init 段与分片）。
    pub fn new(max_requests: NonZeroUsize) -> Self {
        Engine {
            requests: Arc::new(Semaphore::new(max_requests.get())),
        }
    }

    /// 校验请求并在当前 tokio 运行时上启动任务。参数错误与输出已存在时立即返回错误。
    pub fn start(&self, request: JobRequest) -> Result<Job, Error> {
        request.validate()?;
        let http = Http::new(&request, self.requests.clone())?;
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

/// 一个下载任务的配置。用 [`JobRequest::new`] 取默认值后按需修改字段。
#[derive(Clone)]
pub struct JobRequest {
    /// 主播放列表或媒体播放列表的地址
    pub url: Url,
    /// 输出的 MP4 路径；所在目录不存在时创建
    pub output: PathBuf,
    /// 任务目录；None 时为 `<output>.hsdl`。只能是空目录、不存在的目录或本库建立的任务目录
    pub work_dir: Option<PathBuf>,
    /// 附加到所有请求的请求头
    pub headers: Vec<(String, String)>,
    pub preference: hls::Preference,
    /// 本任务同时处理的分片与 init 段数
    pub concurrency: NonZeroUsize,
    pub retry: RetryPolicy,
    pub timeouts: Timeouts,
    /// HTTP 或 SOCKS 代理；None 时使用系统代理设置
    pub proxy: Option<Url>,
    /// 不校验 TLS 证书
    pub insecure: bool,
    /// 输出文件已存在时替换它
    pub overwrite: bool,
    /// 成功后保留任务目录
    pub keep_work_dir: bool,
    /// 直播的录制方式；None 时拒绝直播（[`Unsupported::Live`]）
    pub live: Option<LiveOptions>,
    pub hooks: Arc<dyn Hooks>,
}

impl JobRequest {
    pub fn new(url: Url, output: PathBuf) -> Self {
        JobRequest {
            url,
            output,
            work_dir: None,
            headers: Vec::new(),
            preference: hls::Preference::default(),
            concurrency: NonZeroUsize::new(16).expect("16 非零"),
            retry: RetryPolicy::default(),
            timeouts: Timeouts::default(),
            proxy: None,
            insecure: false,
            overwrite: false,
            keep_work_dir: false,
            live: Some(LiveOptions::default()),
            hooks: Arc::new(NoHooks),
        }
    }

    fn work_dir(&self) -> PathBuf {
        self.work_dir.clone().unwrap_or_else(|| {
            let mut name = self.output.clone().into_os_string();
            name.push(".hsdl");
            PathBuf::from(name)
        })
    }

    fn validate(&self) -> Result<(), Error> {
        if !matches!(self.url.scheme(), "http" | "https") {
            return Err(Error::InvalidInput(format!(
                "只支持 http/https 地址：{}",
                self.url
            )));
        }
        for (name, value) in &self.headers {
            reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::InvalidInput(format!("请求头名不合法：{name:?}")))?;
            reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| Error::InvalidInput(format!("请求头 {name} 的值不合法")))?;
        }
        if self.retry.attempts == 0 {
            return Err(Error::InvalidInput("重试次数至少为 1（含首次请求）".into()));
        }
        if self.live.is_some_and(|l| l.stall_timeout.is_zero()) {
            return Err(Error::InvalidInput("直播的 stall_timeout 不能为 0".into()));
        }
        if self.output.file_name().is_none() {
            return Err(Error::InvalidInput(format!(
                "输出路径没有文件名：{}",
                self.output.display()
            )));
        }
        check_output(&self.output, self.overwrite)
    }
}

/// 不允许覆盖时，输出文件必须不存在。
fn check_output(output: &std::path::Path, overwrite: bool) -> Result<(), Error> {
    let exists = output.try_exists().map_err(|source| Error::Io {
        action: "检查",
        path: output.to_path_buf(),
        source,
    })?;
    if exists && !overwrite {
        return Err(Error::OutputExists(output.to_path_buf()));
    }
    Ok(())
}

/// 直播录制方式。
///
/// 录制从当前播放列表里的全部分片开始，按 RFC 8216 6.3.4 的节奏刷新，直到所有轨出现 EXT-X-ENDLIST、
/// 调用 [`Job::stop`]、达到 `max_duration`，或连续 `stall_timeout` 没有新分片。
/// 窗口已滑过或重试后仍取不到（404/410、超时、5xx）的分片记为漏段，录制继续；成片保留原时间戳，
/// 漏段处时间线留空，各轨同步不受影响。其他失败（如 403、校验失败）使任务失败。
/// 中断后用同样的请求再次运行时不联网，直接合并已录到的分片。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveOptions {
    /// 录到的时长（第 0 条轨已排入下载的分片声明时长之和）达到此值即停止；None 不限
    pub max_duration: Option<Duration>,
    /// 持续这么久没有新分片（含刷新失败）即认为直播已结束，不能为 0
    pub stall_timeout: Duration,
}

impl Default for LiveOptions {
    fn default() -> Self {
        LiveOptions {
            max_duration: None,
            stall_timeout: Duration::from_secs(60),
        }
    }
}

/// 单个请求的重试策略：可重试的失败按指数退避重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// 总尝试次数（含首次），至少 1
    pub attempts: u32,
    /// 第 n 次重试前等待 base_delay × 2^(n-1)，并在 ±25% 内随机抖动
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            attempts: 8,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(15),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    /// 两次读到数据之间的最长间隔
    pub read_idle: Duration,
    /// 单个请求从发出到读完的上限
    pub request: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            connect: Duration::from_secs(15),
            read_idle: Duration::from_secs(30),
            request: Duration::from_secs(600),
        }
    }
}

/// 请求的用途，回调据此区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Playlist,
    Key,
    Init,
    Segment,
}

/// 回调可修改的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestParts {
    pub purpose: Purpose,
    pub url: Url,
    /// 本次请求的请求头（已含 [`JobRequest::headers`]）
    pub headers: Vec<(String, String)>,
}

/// 站点适配回调。在阻塞线程池中执行；返回 `Err` 时任务失败，不重试。
/// 取消不会打断正在执行的回调，任务等它返回后才结束。
pub trait Hooks: Send + Sync {
    /// 改写拉到的播放列表文本。
    fn on_playlist(&self, url: &Url, text: String) -> Result<String, String> {
        let _ = url;
        Ok(text)
    }

    /// 发出请求前修改地址与请求头（如签名）。每次尝试（含重试）调用一次。
    fn on_request(&self, request: &mut RequestParts) -> Result<(), String> {
        let _ = request;
        Ok(())
    }

    /// 变换拉到的 key（如站点自定义的 key 加密）；结果须为 16 字节。`url` 为播放列表中的 key 地址。
    fn on_key(&self, url: &Url, data: Vec<u8>) -> Result<Vec<u8>, String> {
        let _ = url;
        Ok(data)
    }

    /// 在解密之前变换分片（如去掉伪装成图片的前缀字节）。`url` 为播放列表中的分片地址；init 段不经过此回调。
    fn on_segment(&self, url: &Url, data: Vec<u8>) -> Result<Vec<u8>, String> {
        let _ = url;
        Ok(data)
    }
}

/// 不做任何改动的回调。
pub struct NoHooks;

impl Hooks for NoHooks {}

/// 任务进度快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub stage: Stage,
    /// 各轨合计；含续传前已完成的
    pub segments_done: usize,
    /// 直播时为目前已发现的分片数，随录制增长
    pub segments_total: usize,
    /// 直播的漏段数（窗口已滑过或取不到的分片）；点播恒为 0
    pub segments_missed: usize,
    /// 任务目录中已完成的分片与 init 段的字节数（解密后）；含续传前已完成的
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stage {
    #[default]
    Resolving,
    Downloading,
    /// 直播录制中
    Recording,
    Merging,
    Done,
}

/// 下载结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub path: PathBuf,
    pub report: Report,
    /// 各轨分片数合计
    pub segments: usize,
    /// 同 [`Progress::bytes`]
    pub bytes: u64,
    /// 删除任务目录失败的原因（含路径）；输出文件不受影响，残留目录由调用方处理。
    /// 未删除（`keep_work_dir`）或删除成功时为 None
    pub cleanup_error: Option<String>,
    /// 直播的录制结果；点播为 None
    pub live: Option<LiveReport>,
}

/// 直播的录制结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveReport {
    pub end: LiveEnd,
    /// 漏段，按轨道与序号排列。中断后再次运行合并时，只含合并时发现的漏段
    pub missed: Vec<Missed>,
}

/// 录制结束的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEnd {
    /// 所有轨出现 EXT-X-ENDLIST
    EndList,
    /// 调用了 [`Job::stop`]
    Stopped,
    /// 达到 [`LiveOptions::max_duration`]
    MaxDuration,
    /// 连续 [`LiveOptions::stall_timeout`] 没有新分片；附最后一次刷新失败的原因（刷新都成功时为 None）
    Stalled { last_error: Option<String> },
    /// 上次录制被中断，本次只合并已录到的分片
    Interrupted,
}

/// 一段连续的漏段：第 `track` 条轨序号 `first..=last` 的分片不在输出中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missed {
    pub track: usize,
    pub first: u64,
    pub last: u64,
    pub reason: MissReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissReason {
    /// 两次刷新之间已滑出播放列表窗口，没有被列出过
    Expired,
    /// 列出了，但重试后仍未取到；附错误说明
    Failed(String),
    /// 所在不连续段不是每条轨都录到，无法合并
    Unmergeable,
}

/// 在阻塞线程池中执行 `f`。`f` panic 时原样传播；运行时关闭导致它被取消时返回 [`Error::Cancelled`]。
pub(crate) async fn blocking<T, F>(f: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => Ok(value),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => Err(Error::Cancelled),
    }
}

/// 在阻塞线程池中执行回调。
pub(crate) async fn run_hook<T, F>(
    hooks: &Arc<dyn Hooks>,
    purpose: Purpose,
    f: F,
) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce(&dyn Hooks) -> Result<T, String> + Send + 'static,
{
    let hooks = hooks.clone();
    blocking(move || f(hooks.as_ref()))
        .await?
        .map_err(|message| Error::Hook { purpose, message })
}

/// 运行中的任务。丢弃句柄即取消任务。
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

    /// 直播：停止录制，等在途分片完成后合并，[`Job::wait`] 返回录到的部分。点播不受影响。
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
