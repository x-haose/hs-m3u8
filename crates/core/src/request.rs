//! 任务配置与校验。

use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use url::Url;

use crate::hooks::{Hooks, NoHooks};
use crate::ident::bare_url;
use crate::{Error, hls};

/// 来源与访问它的方式；探测（[`crate::Engine::probe`]）与下载（[`JobRequest::source`]）共用。
#[derive(Clone)]
pub struct Source {
    /// 主播放列表或媒体播放列表的地址。它去掉用户名、密码、查询串与片段后，连同 `preference`，称为来源：
    /// 任务目录记录的来源不同即是另一个任务（[`crate::WorkDirProblem::SourceMismatch`]）；来源相同而完整地址
    /// 不同（如换了令牌）时，直播续录见 [`crate::Resume::Continue`]
    pub url: Url,
    /// 选轨偏好，属于来源（见 `url`）。任务目录里有已完成的分片时按记录的变体与音频找回同一条轨，不按偏好重新选
    pub preference: hls::Preference,
    pub http: HttpOptions,
    pub hooks: Arc<dyn Hooks>,
}

impl Source {
    pub fn new(url: Url) -> Self {
        Source {
            url,
            preference: hls::Preference::default(),
            http: HttpOptions::default(),
            hooks: Arc::new(NoHooks),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        if !matches!(self.url.scheme(), "http" | "https") {
            return Err(Error::InvalidInput(format!(
                "只支持 http/https 地址：{}",
                bare_url(&self.url)
            )));
        }
        self.http.validate()
    }
}

/// 发请求的方式。不实现 `Debug`：请求头常带 Cookie 与令牌，代理地址可能带口令。
#[derive(Clone, Default)]
pub struct HttpOptions {
    /// 附加到所有请求的请求头
    pub headers: Vec<(String, String)>,
    /// HTTP 或 SOCKS 代理；None 时使用系统代理设置
    pub proxy: Option<Url>,
    /// 不校验 TLS 证书
    pub insecure: bool,
    pub timeouts: Timeouts,
    pub retry: RetryPolicy,
}

impl HttpOptions {
    fn validate(&self) -> Result<(), Error> {
        for (name, value) in &self.headers {
            check_header(name, value).map_err(Error::InvalidInput)?;
        }
        let t = self.timeouts;
        if t.connect.is_zero() || t.read_idle.is_zero() || t.request.is_zero() {
            return Err(Error::InvalidInput("超时不能为 0".into()));
        }
        Ok(())
    }
}

/// 一个下载任务的配置。用 [`JobRequest::new`] 取默认值后按需修改字段。
#[derive(Clone)]
pub struct JobRequest {
    pub source: Source,
    /// 输出的 MP4 路径；所在目录不存在时在合并前创建
    pub output: PathBuf,
    /// 任务目录；None 时为 `<output>.hsdl`。只能是空目录、不存在的目录或本库建立的任务目录
    pub work_dir: Option<PathBuf>,
    /// 自定义 key；None 时按播放列表取 key
    pub key: Option<KeyOverride>,
    /// 本任务同时下载的分片数
    pub concurrency: NonZeroUsize,
    /// 输出文件已存在时替换它
    pub overwrite: bool,
    /// 成功后保留任务目录
    pub keep_work_dir: bool,
    /// 直播的录制方式；None 时拒绝直播（[`crate::Unsupported::Live`]）
    pub live: Option<LiveOptions>,
}

/// 自定义 key：加密的分片一律用它解密，不请求播放列表里的 key 地址，也不经过 [`crate::Hooks::on_key`]。用于
/// key 地址取不到、或返回的不是真正的 key，而调用方已知 key 的站点；不加密的分片不受影响。
/// 不实现 `Debug`：key 不进日志。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct KeyOverride {
    pub key: [u8; 16],
    /// 所有加密分片都用这个 IV；None 时用各分片自己的（播放列表写明的，或由媒体序号推出）
    pub iv: Option<[u8; 16]>,
}

/// 默认并发：与常见下载器同一量级；站点限流时由调用方调低。
const DEFAULT_CONCURRENCY: NonZeroUsize = NonZeroUsize::new(16).unwrap();

impl JobRequest {
    pub fn new(source: Source, output: PathBuf) -> Self {
        JobRequest {
            source,
            output,
            work_dir: None,
            key: None,
            concurrency: DEFAULT_CONCURRENCY,
            overwrite: false,
            keep_work_dir: false,
            live: Some(LiveOptions::default()),
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
        self.source.validate()?;
        let invalid = |message: String| Err(Error::InvalidInput(message));
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
/// 调用 [`crate::JobControl::stop`]、各轨都录满 `max_duration`、任一轨停滞，或服务器的播放列表前后矛盾
/// （序号回退、同一序号换了分片）。结束后把已拉到的播放列表处理完、已列出的分片下完再合并，结束原因见
/// [`crate::LiveEnd`]。
///
/// 停滞：任一轨持续 `stall_timeout` 没有录到新分片（有分片还在排队或下载时不算）。播放列表不再出新分片
/// 或已被删除（404/410），且其他轨也不再出，视为直播已结束，照常合并；刷新一直失败、一直不返回，
/// 仍在出新分片却一个都下载不成功，或只有这条轨不再出新分片，任务失败（[`crate::Error::LiveStalled`]），
/// 任务目录保留，可稍后续录。
///
/// 窗口已滑过或取不到（404/410、重试后仍失败的临时故障）的分片记为缺失，录制继续；成片保留原时间戳，
/// 缺失处时间线留空，各轨同步不受影响。其他失败（如 403、key 失败、校验失败）使任务失败。
///
/// 任务中断后用同样的请求再次运行，按 `resume` 续录或只合并已录到的部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveOptions {
    /// 每条轨的分片声明时长之和达到此值后不再录，各轨都达到即结束；计入之前各会话已录到的、本次排入下载的、
    /// 本次列出了但取不到的，不计窗口已滑过的（时长未知）与之前运行里缺失的。None 不限，不能为 0
    pub max_duration: Option<Duration>,
    /// 停滞判定的时长，不能为 0；实际至少等三个目标时长（目标时长比它还长时，正常的直播两次出新分片之间
    /// 也会超过它）。大到无法表示的时长视为不限
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
    /// 续录。各轨当前的窗口都与各自之前录到的接得上时，接着录，并补上窗口内之前没录完的分片，时间线连续；
    /// 否则另起一段，与之前的首尾相接（中断期间的内容不在输出中），之前录过的分片不重录。最后一并合并。
    /// 来源相同而完整地址与记录的不同（如换了令牌）时，要录的各轨都接得上才续录并改记新地址，否则报
    /// [`crate::WorkDirProblem::SourceUnverified`]；这次没有要录的轨（都已录满或已结束）时不改记，直接合并
    Continue,
    /// 不联网，只把已录到的分片合并成输出。目录须是同一来源（见 [`crate::Source::url`]）的直播录制：来源不同报
    /// [`crate::WorkDirProblem::SourceMismatch`]，是点播的下载报 [`crate::WorkDirProblem::NotLiveRecording`]；
    /// 没有可合并的分片时报 [`Error::NothingRecorded`]
    MergeOnly,
}

/// 单个请求的重试策略：可重试的失败按指数退避重试。
///
/// 默认 8 次、0.5 秒起、上限 15 秒：重试前的等待合计约 45 秒（随抖动在 34 到 57 秒之间，不含各次请求本身的
/// 耗时），足以越过常见的短时故障。
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
