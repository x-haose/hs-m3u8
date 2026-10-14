//! 任务配置与校验。

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use url::Url;

use crate::cookies::CookieStore;
use crate::hooks::{Hooks, NoHooks};
use crate::ident::bare_url;
use crate::output::{OutputOptions, ResolvedOutput};
use crate::{Error, hls};

/// 来源与访问它的方式；探测（[`crate::Engine::probe`]）与下载（[`JobRequest::source`]）共用。
#[derive(Clone)]
pub struct Source {
    /// 主播放列表或媒体播放列表的地址，续传时可以与上次的不同（换了令牌、主机或路径）。续传先核对这次取到的与已下载的
    /// 是同一份内容：点播每条轨已完成的字节数最多的至多 16 个不连续段组，各取已完成的分片里字节数最大的重新下载（取不到
    /// 的换次大的），与已存的逐字节比较，分片都已下完、地址也与上次相同时不核对；直播见 [`LiveOptions`]。不是时报
    /// [`crate::WorkDirProblem::SourceUnverified`]。点播的轨 init 段内容变了时，已完成的分片不沿用，重新下载
    pub url: Url,
    /// 选轨偏好。来源是主播放列表时，与任务目录记录的不同报 [`crate::WorkDirProblem::PreferenceChanged`]；任务目录里
    /// 有已完成的分片时按记录的变体与音频找回同一条轨，不按偏好重新选。按身份选的（`Matching`）续传时要给当初那个，
    /// 重新探测后再 `choice()` 得到的属性可能变了
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

    /// 校验地址（[`Source::check_url`]）、选轨偏好（[`Source::check_preference`]）与 `http`
    /// （[`HttpOptions::validate`]），不合要求时报 [`Error::InvalidInput`]。探测与开始任务时也会校验。
    pub fn validate(&self) -> Result<(), Error> {
        Self::check_url(&self.url)?;
        Self::check_preference(&self.preference)?;
        self.http.validate()
    }

    /// 来源地址只支持 http/https。
    pub fn check_url(url: &Url) -> Result<(), Error> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::InvalidInput(format!(
                "只支持 http/https 地址：{}",
                bare_url(url)
            )));
        }
        Ok(())
    }

    /// 音频语言不能有换行：选轨偏好的摘要按行编码，播放列表里的语言也不会有换行。
    pub fn check_preference(preference: &hls::Preference) -> Result<(), Error> {
        if let hls::AudioChoice::Language(language) = &preference.audio
            && language.contains(['\n', '\r'])
        {
            return Err(Error::InvalidInput(format!("音频语言不合法：{language:?}")));
        }
        Ok(())
    }
}

/// 发请求的方式。不实现 `Debug`：请求头常带 Cookie 与令牌，代理地址可能带口令（[`Proxy`] 同理）。
#[derive(Clone, Default)]
pub struct HttpOptions {
    /// 附加到请求的请求头。其中的 Cookie（`名=值; 名=值`）记进本任务的 Cookie 存储，作为只属于来源地址的主机的 Cookie：
    /// 只发往这个主机，服务器给这个主机下发同名的（不带 Domain 或 Domain 即这个主机）即被替换，给上级域下发的与它并存、
    /// 一同发出；Authorization 只发往与来源地址协议、主机、端口都相同的地址；其余的发往所有地址。跟随重定向时照样带上，
    /// 跳到别的主机（或端口、协议）时去掉 Authorization 与 Proxy-Authorization
    pub headers: Vec<(String, String)>,
    /// 调用方持有的 Cookie 存储，跨任务保存；None 时每个任务各用一个，任务结束即丢。给了它时 `headers` 里不能有 Cookie
    pub cookies: Option<Arc<dyn CookieStore>>,
    pub proxy: Proxy,
    /// 不校验 TLS 证书
    pub insecure: bool,
    pub timeouts: Timeouts,
    pub retry: RetryPolicy,
}

impl HttpOptions {
    /// 校验请求头、超时（[`Timeouts::validate`]）与代理，不合要求时报 [`Error::InvalidInput`]：请求头的名或值放不进
    /// HTTP 请求，是本库控制的头（Accept-Encoding、Range 与条件请求的 If-*），Cookie 不是 `名=值; 名=值` 的写法，或
    /// 给了 Cookie 存储又在请求头里带 Cookie。
    pub fn validate(&self) -> Result<(), Error> {
        for (name, value) in &self.headers {
            check_header(name, value).map_err(Error::InvalidInput)?;
            if name.eq_ignore_ascii_case("cookie") {
                if self.cookies.is_some() {
                    return Err(Error::InvalidInput(
                        "给了 Cookie 存储时 Cookie 经存储传，请求头里不能再带 Cookie".to_owned(),
                    ));
                }
                if cookie_pairs(value).any(|pair| pair.is_none()) {
                    return Err(Error::InvalidInput(
                        "请求头 Cookie 应为 `名=值; 名=值` 的写法".to_owned(),
                    ));
                }
            }
        }
        self.timeouts.validate()?;
        if let Proxy::Url(proxy) = &self.proxy {
            Proxy::check_url(proxy)?;
        }
        Ok(())
    }
}

/// 请求经不经代理、经哪个。
#[derive(Clone, Default, PartialEq, Eq)]
pub enum Proxy {
    /// 按系统代理设置（见 [`crate::system_proxy`]）：先看环境变量（规则同 curl），没设代理时看 macOS 的系统设置或
    /// Windows 的 Internet 选项，其他平台即直连。探测与开始任务时读取，任务中每隔几秒重读，设置改了之后的请求照新
    /// 设置走；值用不了（写错、用了 PAC 或自动发现）或读不出时报 [`Error::SystemProxy`]，可改用 `Direct` 或 `Url`
    #[default]
    System,
    /// 不经代理，系统设置了代理也直连
    Direct,
    /// 经这个 HTTP 或 SOCKS 代理，协议为 http、https、socks4、socks4a、socks5、socks5h
    Url(Url),
}

impl Proxy {
    /// 代理地址 HTTP 库用不用得上：协议为 http、https、socks4、socks4a、socks5、socks5h，且主机名合法。它用不上的
    /// 地址不报错、请求直连，所以先拒绝。
    pub fn check_url(url: &Url) -> Result<(), Error> {
        match crate::proxy::unusable(url) {
            Some(reason) => Err(Error::InvalidInput(reason)),
            None => Ok(()),
        }
    }
}

/// 一个下载任务的配置。用 [`JobRequest::new`] 取默认值后按需修改字段。
#[derive(Clone)]
pub struct JobRequest {
    pub source: Source,
    pub output: OutputOptions,
    /// 自定义 key；None 时按播放列表取 key
    pub key: Option<KeyOverride>,
    /// 本任务同时下载的分片数
    pub concurrency: NonZeroUsize,
    /// 直播的录制方式；None 时拒绝直播（[`Error::LiveNotEnabled`]）
    pub live: Option<LiveOptions>,
}

/// 自定义 key：加密的分片与 init 段一律用它解密，不请求播放列表里的 key 地址，也不经过 [`crate::Hooks::on_key`]。用于
/// key 地址取不到、没写（EXT-X-KEY 没有 URI）或返回的不是真正的 key，而调用方已知 key 的站点；不加密的分片不受影响。
/// 不实现 `Debug`：key 不进日志。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct KeyOverride {
    pub key: [u8; 16],
    /// 所有加密分片都用这个 IV；None 时用各分片自己的（播放列表写明的，或由媒体序号推出）。IV 只影响每个分片
    /// 解密后的头 16 字节，给错了校验发现不了、错的内容照样写进输出，只在确知时给
    pub iv: Option<[u8; 16]>,
}

impl JobRequest {
    /// `concurrency` 的默认值：与常见下载器同一量级；站点限流时由调用方调低。
    pub const DEFAULT_CONCURRENCY: NonZeroUsize = NonZeroUsize::new(16).unwrap();

    pub fn new(source: Source, output: OutputOptions) -> Self {
        JobRequest {
            source,
            output,
            key: None,
            concurrency: Self::DEFAULT_CONCURRENCY,
            live: Some(LiveOptions::default()),
        }
    }

    /// 校验请求，返回补全为绝对路径的输出配置（见 [`OutputOptions::resolve`]）。
    pub(crate) fn validate(&self) -> Result<ResolvedOutput, Error> {
        self.source.validate()?;
        if let Some(live) = self.live {
            live.validate()?;
        }
        self.output.resolve()
    }
}

/// 本库控制的请求头（`name` 为小写）及不能另给的原因：它们决定响应体是什么、怎么编码。从浏览器复制的请求头常带着。
fn controlled(name: &str) -> Option<&'static str> {
    match name {
        "accept-encoding" => Some("本库不解压，服务器按它压缩的内容读不了"),
        "range" => Some("要取的字节范围由播放列表决定"),
        "if-none-match" | "if-modified-since" | "if-match" | "if-unmodified-since" | "if-range" => {
            Some("条件请求会让服务器不返回内容（304、412）")
        }
        _ => None,
    }
}

/// Cookie 请求头 `value` 里的各项（`名=值`），按分号分开、去掉两边的空白；名为空或没有等号的项为 None。
pub(crate) fn cookie_pairs(value: &str) -> impl Iterator<Item = Option<&str>> {
    value
        .split(';')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            pair.split_once('=')
                .is_some_and(|(name, _)| !name.trim().is_empty())
                .then_some(pair)
        })
}

/// 请求头名与值能否放进 HTTP 请求、且不是本库控制的头；不能时返回原因。
pub(crate) fn check_header(name: &str, value: &str) -> Result<(), String> {
    let parsed = reqwest::header::HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| format!("请求头名不合法：{name:?}"))?;
    if let Some(why) = controlled(parsed.as_str()) {
        return Err(format!("请求头 {name} 由本库控制，不能另给：{why}"));
    }
    reqwest::header::HeaderValue::from_str(value)
        .map_err(|_| format!("请求头 {name} 的值不合法"))?;
    Ok(())
}

/// 直播录制方式。
///
/// 录制从当前播放列表里的全部分片开始，按 RFC 8216 6.3.4 的节奏刷新，直到所有轨出现 EXT-X-ENDLIST、
/// 调用 [`crate::JobControl::stop`]、各轨都录满 `max_duration`、任一轨停滞，或录的不止一条轨时某条轨的播放列表
/// 前后接不上（序号回退、同一序号换了分片；只录一条轨时另起会话接着录，见 [`crate::NewSession`]）。结束后把已拉到
/// 的播放列表处理完、已列出的分片下完再合并，结束原因见 [`crate::LiveEnd`]。
///
/// 停滞：任一轨持续 `stall_timeout` 没有录到新分片（有分片还在排队或下载时不算）。结论依据那之后各轨的刷新结果（请求在
/// 途的，察觉停滞后再等一个目标时长；服务器要求等待的照它的意思等，要等过察觉停滞后一个目标时长的以那次失败为据），进程
/// 被挂起醒来时不凭挂起前的情况下结论：这条轨的播放列表不再出新分片或已被删除（404/410），且其他轨也不再出，视为直播已
/// 结束，照常合并；有轨刷新失败或一直不返回，这条轨仍在出新分片却一个都下载不成功，或只有这条轨不再出新分片，任务失败
/// （[`crate::Error::LiveStalled`]），任务目录保留，可稍后续录。
///
/// 窗口已滑过或取不到（404/410、重试后仍失败的临时故障）的分片记为缺失，录制继续；成片保留原时间戳，
/// 缺失处时间线留空，各轨同步不受影响。其他失败（如 403、key 失败、校验失败）使任务失败。
///
/// 续录：任务中断后用同样的请求再次运行。至少一条轨当前的窗口与之前录到的接得上（重叠的分片重新下载后内容相同；已录满、
/// 这次不录的轨同样核对），且没有一条轨内容不同时，接着录：补上窗口内之前没录完的分片，时间线连续，之前没录到的轨从窗口
/// 起点并入；否则另起一段，与之前的首尾相接（中断期间的内容不在输出中），之前录过的分片不重录，另起的一段里有轨这次不录
/// （已录满或没有内容）时报 [`crate::WorkDirProblem::Unaligned`]。最后一并合并。地址与记录的不同时，接着录才改记新地
/// 址，另起一段报 [`crate::WorkDirProblem::SourceUnverified`]；这次没有要录的轨（都已录满或已结束）时不改记，直接合并。
/// 不联网、只合并已录到的部分用 [`crate::Engine::merge_recorded`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveOptions {
    /// 每条轨的分片声明时长之和达到此值后不再录，各轨都达到即结束；计入之前各会话已录到的、本次排入下载的、
    /// 本次列出了但取不到的，不计窗口已滑过的（时长未知）与之前运行里缺失的。None 不限，不能为 0
    pub max_duration: Option<Duration>,
    /// 停滞判定的时长，不能为 0；实际至少等三个目标时长（目标时长比它还长时，正常的直播两次出新分片之间
    /// 也会超过它）。大到无法表示的时长视为不限
    pub stall_timeout: Duration,
}

/// 默认 stall_timeout：直播结束时播放列表多半直接 404 或不再更新，等一分钟足以排除临时故障。
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(60);

impl LiveOptions {
    /// `stall_timeout` 与 `max_duration` 不能为 0，否则报 [`Error::InvalidInput`]。
    pub fn validate(&self) -> Result<(), Error> {
        if self.stall_timeout.is_zero() {
            return Err(Error::InvalidInput("直播的 stall_timeout 不能为 0".into()));
        }
        if self.max_duration.is_some_and(|d| d.is_zero()) {
            return Err(Error::InvalidInput("直播的 max_duration 不能为 0".into()));
        }
        Ok(())
    }
}

impl Default for LiveOptions {
    fn default() -> Self {
        LiveOptions {
            max_duration: None,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
        }
    }
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

impl Timeouts {
    /// 都不能为 0，否则报 [`Error::InvalidInput`]。
    pub fn validate(&self) -> Result<(), Error> {
        if self.connect.is_zero() || self.read_idle.is_zero() || self.request.is_zero() {
            return Err(Error::InvalidInput("超时不能为 0".into()));
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_the_library_controls_are_rejected_in_any_case() {
        for name in [
            "Accept-Encoding",
            "range",
            "If-None-Match",
            "IF-MODIFIED-SINCE",
        ] {
            assert!(check_header(name, "x").is_err(), "{name}");
        }
        for name in ["Referer", "Cookie", "Authorization", "X-Token"] {
            assert_eq!(check_header(name, "x"), Ok(()), "{name}");
        }
    }

    #[test]
    fn cookie_headers_split_into_name_value_pairs() {
        let pairs: Vec<_> = cookie_pairs(" a=1; b=x=y ;; c= ").collect();
        assert_eq!(pairs, [Some("a=1"), Some("b=x=y"), Some("c=")]);
        let pairs: Vec<_> = cookie_pairs("a=1; HttpOnly; =2").collect();
        assert_eq!(pairs, [Some("a=1"), None, None]);
    }
}
