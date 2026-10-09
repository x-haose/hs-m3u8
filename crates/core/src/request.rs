//! 任务配置与校验。

use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use url::Url;

use crate::hooks::{Hooks, NoHooks};
use crate::{Error, hls};

/// 一个下载任务的配置。用 [`JobRequest::new`] 取默认值后按需修改字段。
#[derive(Clone)]
pub struct JobRequest {
    /// 主播放列表或媒体播放列表的地址
    pub url: Url,
    /// 输出的 MP4 路径；所在目录不存在时在合并前创建
    pub output: PathBuf,
    /// 任务目录；None 时为 `<output>.hsdl`。只能是空目录、不存在的目录或本库建立的任务目录
    pub work_dir: Option<PathBuf>,
    /// 附加到所有请求的请求头
    pub headers: Vec<(String, String)>,
    pub preference: hls::Preference,
    /// 本任务同时下载的分片与 init 段数
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
    /// 直播的录制方式；None 时拒绝直播（[`crate::Unsupported::Live`]）
    pub live: Option<LiveOptions>,
    pub hooks: Arc<dyn Hooks>,
}

/// 默认并发：与常见下载器同一量级；站点限流时由调用方调低。
const DEFAULT_CONCURRENCY: NonZeroUsize = NonZeroUsize::new(16).unwrap();

impl JobRequest {
    pub fn new(url: Url, output: PathBuf) -> Self {
        JobRequest {
            url,
            output,
            work_dir: None,
            headers: Vec::new(),
            preference: hls::Preference::default(),
            concurrency: DEFAULT_CONCURRENCY,
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

    /// 实际使用的任务目录：`work_dir`，未指定时为 `<output>.hsdl`。
    pub fn resolved_work_dir(&self) -> PathBuf {
        self.work_dir.clone().unwrap_or_else(|| {
            let mut name = self.output.clone().into_os_string();
            name.push(".hsdl");
            PathBuf::from(name)
        })
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        let invalid = |message: String| Err(Error::InvalidInput(message));
        if !matches!(self.url.scheme(), "http" | "https") {
            return invalid(format!("只支持 http/https 地址：{}", self.url));
        }
        for (name, value) in &self.headers {
            check_header(name, value).map_err(Error::InvalidInput)?;
        }
        let t = self.timeouts;
        if t.connect.is_zero() || t.read_idle.is_zero() || t.request.is_zero() {
            return invalid("超时不能为 0".into());
        }
        if let Some(live) = self.live {
            if live.stall_timeout.is_zero() {
                return invalid("直播的 stall_timeout 不能为 0".into());
            }
            if live.max_duration.is_some_and(|d| d.is_zero()) {
                return invalid("直播的 max_duration 不能为 0".into());
            }
        }
        if self.output.file_name().is_none() {
            return invalid(format!("输出路径没有文件名：{}", self.output.display()));
        }
        check_output(&self.output, self.overwrite)
    }
}

/// 请求头名与值能否放进 HTTP 请求；不能时返回原因。
pub(crate) fn check_header(name: &str, value: &str) -> Result<(), String> {
    reqwest::header::HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| format!("请求头名不合法：{name:?}"))?;
    reqwest::header::HeaderValue::from_str(value)
        .map_err(|_| format!("请求头 {name} 的值不合法"))?;
    Ok(())
}

/// 不允许覆盖时，输出文件必须不存在。
pub(crate) fn check_output(output: &Path, overwrite: bool) -> Result<(), Error> {
    let exists = output.try_exists().map_err(|cause| Error::Io {
        action: "检查",
        path: output.to_path_buf(),
        cause,
    })?;
    if exists && !overwrite {
        return Err(Error::OutputExists(output.to_path_buf()));
    }
    Ok(())
}

/// 直播录制方式。
///
/// 录制从当前播放列表里的全部分片开始，按 RFC 8216 6.3.4 的节奏刷新，直到所有轨出现 EXT-X-ENDLIST、
/// 调用 [`crate::Job::stop`]、各轨都录满 `max_duration`、任一轨连续 `stall_timeout` 没有新分片，
/// 或服务器的播放列表前后矛盾（序号回退、同一序号换了分片）。结束后把已列出的分片下完再合并，
/// 结束原因见 [`crate::LiveEnd`]。
///
/// 同一次录制内，窗口已滑过或取不到（404/410、重试后仍失败的临时故障）的分片记为漏段，录制继续；
/// 成片保留原时间戳，漏段处时间线留空，各轨同步不受影响。其他失败（如 403、key 失败、校验失败）使任务失败。
///
/// 任务中断后用同样的请求再次运行，按 `resume` 继续录制或只合并已录到的部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveOptions {
    /// 每条轨已排入下载的分片声明时长之和达到此值后不再排入；各轨都达到即结束。None 不限，不能为 0
    pub max_duration: Option<Duration>,
    /// 任一轨持续这么久没有新分片（含刷新失败、刷新未返回）即结束录制，不能为 0；
    /// 大到无法表示的时长视为不限
    pub stall_timeout: Duration,
    pub resume: Resume,
}

/// 默认 stall_timeout：直播结束时播放列表多半直接 404 或不再更新，等一分钟足以排除临时故障。
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(60);

impl Default for LiveOptions {
    fn default() -> Self {
        LiveOptions {
            max_duration: None,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
            resume: Resume::Continue,
        }
    }
}

/// 任务目录里已有本来源的直播录制时怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    /// 继续录制：新的录制与之前的首尾相接（中断期间的内容不在输出中），最后一并合并
    Continue,
    /// 不联网，只把已录到的分片合并成输出；没有可合并的分片时报 [`Error::NothingRecorded`]
    MergeOnly,
}

/// 单个请求的重试策略：可重试的失败按指数退避重试。
///
/// 默认 8 次、0.5 秒起、上限 15 秒：一次请求最坏约 45 秒后放弃，足以越过常见的短时故障。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// 总尝试次数（含首次）
    pub attempts: NonZeroU32,
    /// 第 n 次重试前等待 base_delay × 2^(n-1)
    pub base_delay: Duration,
    /// 指数等待的上限；实际等待再乘以 [0.75, 1.25) 的随机抖动。
    /// 服务器在 429/503 中要求（Retry-After）的等待超过此值时不再重试，直接失败
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            attempts: NonZeroU32::new(8).unwrap(),
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(15),
        }
    }
}

/// 超时，都不能为 0。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    /// 两次读到数据之间的最长间隔
    pub read_idle: Duration,
    /// 单次尝试从发出到读完的上限；默认 600 秒，几 MB 的分片在几十 KB/s 时仍能读完
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
