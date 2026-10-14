//! 下载任务的错误。
//!
//! 按调用方的处理方式分类：
//! - 改请求再试：[`Error::InvalidInput`]；[`Error::OutputExists`]（要求覆盖）；[`Error::OutputOccupied`]（换路径）；
//!   [`Error::Select`]（选轨偏好与来源不符）；[`Error::LiveNotEnabled`]（开启直播录制）；[`Unsupported::Mp4`]、
//!   [`Unsupported::Hls`]（改用另一种输出）；
//! - 来源本身不行，同样的请求再试也不会成功：[`Error::Playlist`]、[`Error::NotMediaPlaylist`]、[`Error::Unsupported`]
//!   的其余各项、[`Error::Integrity`]、[`Error::KeyLength`]、[`Error::ResponseTooLarge`]；
//! - 网络：[`Error::Http`]，看 [`HttpError::retryable`]；
//! - 源站给出没写完的内容：[`Error::Incomplete`]，可重试；
//! - 说明出在哪里、看包着的原因：[`Error::Segment`]、[`Error::Key`]（哪个分片或 key）、[`Error::LiveStalled`]（直播
//!   哪条轨停滞）、[`Error::Cleanup`]（失败后收拾也没做完，带残留列表）；
//! - 读写文件：[`Error::Io`]（Windows 上别的进程一直开着文件时可重试，其余不可）、[`Error::VolumeUnavailable`]（盘
//!   不在、共享连不上，接上后再试）；合并 MP4：[`Error::Remux`]；系统代理用不了：[`Error::SystemProxy`]（改用直连或
//!   指定代理地址）。除注明的外都不可重试；
//! - 任务目录：[`Error::WorkDir`]（按 [`WorkDirProblem`] 处理）、[`Error::NothingRecorded`]；
//! - 调用方的回调出错：[`Error::Hook`]；[`Error::Cancelled`]。
//!
//! 可否重试看 [`Error::retryable`]，服务器要求的等待看 [`Error::retry_after`]，两者都看包着的原因。
//!
//! 错误信息已包含原因，不经 `source()` 重复给出；地址只显示到路径，不含用户名、密码与查询串（常带凭据或令牌）。

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use url::Url;

use crate::hooks::{HookError, HookKind, Purpose};
use crate::ident::bare_url;
use crate::{Leftover, Mp4Unsupported, RefreshCause, StallCause};

#[derive(thiserror::Error)]
pub enum Error {
    #[error("参数错误：{0}")]
    InvalidInput(String),
    /// 输出已存在，要求覆盖（[`crate::OutputOptions::overwrite`]）即可替换
    #[error("输出已存在：{}", .0.display())]
    OutputExists(PathBuf),
    /// 输出路径 `path` 用不了，覆盖也不会替换（以免路径给错时删掉别人的文件），换一个路径：要输出 MP4 而那里是目录，
    /// 要输出 HLS 而那里不是目录或目录里有不是本库写出的文件；或那里什么也没有，而最近的已存在的上级 `ancestor`
    /// 不是目录（是文件，或悬空的符号链接），建不出来。路径本身被占用时 `ancestor` 为 None
    #[error("{}", occupied_text(.path, .ancestor.as_deref()))]
    OutputOccupied {
        path: PathBuf,
        ancestor: Option<PathBuf>,
    },
    /// 内容不是合法的播放列表；用了不支持的特性是 [`Error::Unsupported`]，结尾截断的是 [`Error::Incomplete`]
    #[error("解析播放列表 {} 失败：{cause}", bare_url(.url))]
    Playlist {
        url: Box<Url>,
        cause: Box<hs_m3u8_hls::Malformed>,
    },
    /// 应为媒体播放列表的地址（变体、音频 rendition）返回了主播放列表
    #[error("{} 应为媒体播放列表，实际是主播放列表", bare_url(.url))]
    NotMediaPlaylist { url: Box<Url> },
    #[error("选轨失败：{0}")]
    Select(hs_m3u8_hls::SelectError),
    /// 来源是直播，而 [`crate::JobRequest::live`] 为 None
    #[error("来源是直播（播放列表没有 EXT-X-ENDLIST），请求没有开启直播录制")]
    LiveNotEnabled,
    #[error("不支持：{0}")]
    Unsupported(Unsupported),
    /// 请求失败；在最外层时是播放列表或 init 段的请求（分片与 key 的包在 [`Error::Segment`]、[`Error::Key`] 里）。
    /// `retry_after` 为服务器在 429/503 中要求（Retry-After）的最短等待
    #[error("请求 {} 失败：{kind}", bare_url(.url))]
    Http {
        url: Box<Url>,
        kind: HttpError,
        retry_after: Option<Duration>,
    },
    /// 某个分片最终失败；`cause` 为分片请求、回调、解密或校验的错误
    #[error("第 {track} 条轨分片 {sequence}（{}）失败：{}", bare_url(.url), cause_text(.cause, .url))]
    Segment {
        track: usize,
        sequence: u64,
        url: Box<Url>,
        cause: Box<Error>,
    },
    /// 取 key 失败；`cause` 为请求或回调的错误
    #[error("取 key {} 失败：{}", bare_url(.url), cause_text(.cause, .url))]
    Key { url: Box<Url>, cause: Box<Error> },
    /// 拉到的 key 不是 16 字节（`on_key` 回调没有改它）
    #[error("key {} 应为 16 字节，实际 {length} 字节", bare_url(.url))]
    KeyLength { url: Box<Url>, length: usize },
    /// 用途为 `purpose` 的请求 `url` 的响应体超过这类请求的上限 `limit` 字节，读到即停：地址多半指向了视频文件或
    /// 直播流，不是播放列表、init 段或 key。分片不设上限
    #[error("{}{} 的响应体超过 {limit} 字节", purpose_text(*.purpose), bare_url(.url))]
    ResponseTooLarge {
        url: Box<Url>,
        purpose: Purpose,
        limit: u64,
    },
    #[error("数据校验失败（{}）：{kind}", bare_url(.url))]
    Integrity { url: Box<Url>, kind: Integrity },
    /// 取到的内容没有写完，多为源站还在写这个文件，可重试：播放列表结尾截在多字节字符中间；分片或 init 段的响应
    /// 为空、fMP4 的 box 或样本数据超出了数据，或 TS、打包音频截在包、帧或标签中间（加密的看解出的内容）。分片与
    /// init 段在下载时已按 [`crate::RetryPolicy`] 重新取过
    #[error("{} 的内容没有写完", bare_url(.url))]
    Incomplete { url: Box<Url> },
    #[error("任务目录 {} 无法使用：{problem}", .path.display())]
    WorkDir {
        path: PathBuf,
        problem: WorkDirProblem,
    },
    #[error("直播没有录到可合并的分片")]
    NothingRecorded,
    /// 第 `track` 条轨长时间没有录到新分片，且不是直播正常结束（见 [`crate::StallCause`]）
    #[error("直播第 {track} 条轨停滞：{cause}")]
    LiveStalled { track: usize, cause: StallError },
    #[error("{hook}回调出错：{cause}")]
    Hook { hook: HookKind, cause: HookError },
    /// 读写文件 `path` 出错。Windows 上改名或删除时别的进程开着文件，先退避重试一小会儿，仍开着（如播放器开着要
    /// 替换的输出）时可重试（见 [`Error::retryable`]）：关掉它再试
    #[error("{action} {} 失败：{cause}", .path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        cause: io::Error,
    },
    /// 路径的根 `root`（Windows 的盘符、网络共享）读不到：盘不在、共享连不上或没有权限。根下的东西看不到不当作没有，
    /// 不据此删东西、丢记录
    #[error("读不到 {}：{cause}", .root.display())]
    VolumeUnavailable { root: PathBuf, cause: io::Error },
    /// 系统代理（[`crate::Proxy::System`]）用不了，值为说明：设置的值用不了（写错、用了 PAC 或自动发现），或读不出
    /// （系统的接口失败）。可改用 [`crate::Proxy::Direct`] 或 [`crate::Proxy::Url`]
    #[error("{0}")]
    SystemProxy(String),
    /// 合并 MP4 或下载中核对能否合并时失败：FFmpeg 初始化、读分片、写出或回读核对出错；内容放不进 MP4 是
    /// [`Unsupported::Mp4`]
    #[error("合并失败：{0}")]
    Remux(Box<hs_m3u8_remux::Error>),
    /// 写输出失败后收拾本次写出的、或任务开头收拾上次写输出留下的，没能做完：`failure` 为失败的原因（后者为
    /// [`Error::OutputOccupied`]：上次没能放回的旧输出，原处是别的东西或已没有记录），`leftovers` 为留下的东西（至少
    /// 一项）
    #[error("{failure}；{}", joined(.leftovers))]
    Cleanup {
        failure: Box<Error>,
        leftovers: Vec<Leftover>,
    },
    #[error("任务已取消")]
    Cancelled,
}

fn occupied_text(path: &Path, ancestor: Option<&Path>) -> String {
    match ancestor {
        None => format!("路径已被占用，覆盖也不会替换：{}", path.display()),
        Some(ancestor) => format!(
            "建不出 {}：上级 {} 不是目录",
            path.display(),
            ancestor.display()
        ),
    }
}

fn joined(leftovers: &[Leftover]) -> String {
    leftovers
        .iter()
        .map(Leftover::to_string)
        .collect::<Vec<_>>()
        .join("；")
}

/// 包装的错误里的原因；原因的地址显示出来（只到路径）与外层的相同时不再重复，回调改写路径或重定向后照常显示。
struct CauseText<'a> {
    cause: &'a Error,
    shown: &'a Url,
}

fn cause_text<'a>(cause: &'a Error, shown: &'a Url) -> CauseText<'a> {
    CauseText { cause, shown }
}

impl fmt::Display for CauseText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let same = |url: &Url| bare_url(url) == bare_url(self.shown);
        match self.cause {
            Error::Http { url, kind, .. } if same(url) => write!(f, "{kind}"),
            Error::Integrity { url, kind } if same(url) => write!(f, "数据校验失败：{kind}"),
            Error::Incomplete { url } if same(url) => write!(f, "内容没有写完"),
            Error::Unsupported(Unsupported::Fmp4BaseDataOffset { url, at }) if same(url) => {
                write!(f, "不支持：{}", base_data_offset_text(*at))
            }
            other => write!(f, "{other}"),
        }
    }
}

/// 与 `Display` 相同：派生的写法会带出完整地址（含查询串里的令牌），而 `unwrap`、`{:?}` 与日志都用它。
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// 同 [`Error`] 的 `Debug`。
impl fmt::Debug for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// 把文件操作的失败包成 [`Error::Io`]：`action` 为做的是什么（读取、删除……），`path` 为操作的路径。
pub(crate) fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Error {
    let path = path.to_path_buf();
    move |cause| Error::Io {
        action,
        path,
        cause,
    }
}

impl From<hs_m3u8_hls::SelectError> for Error {
    fn from(error: hs_m3u8_hls::SelectError) -> Self {
        Error::Select(error)
    }
}

impl Error {
    /// 请求取不到：404/410（已过期），或重试后仍失败的临时故障时返回原因；其余为 None。
    pub(crate) fn http_failure(&self) -> Option<HttpFailure> {
        match self {
            Error::Http {
                kind, retry_after, ..
            } if kind.retryable() || matches!(kind, HttpError::Status(404 | 410)) => {
                Some(HttpFailure {
                    kind: kind.clone(),
                    retry_after: *retry_after,
                })
            }
            _ => None,
        }
    }

    /// 分片或 init 段取不到：请求取不到（见 [`Error::http_failure`]），或重试后内容仍没写完；分片的失败看其原因。
    /// key、回调、校验等失败为 None。
    pub(crate) fn missable(&self) -> Option<FetchFailure> {
        match self {
            Error::Segment { cause, .. } => cause.missable(),
            Error::Incomplete { .. } => Some(FetchFailure::Incomplete),
            _ => self.http_failure().map(FetchFailure::Http),
        }
    }

    /// 服务器在 429/503 中要求（Retry-After）的最短等待，像 [`Error::retryable`] 一样看包着的原因；没有这样的
    /// 要求时为 None。
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Http { retry_after, .. } => *retry_after,
            Error::Segment { cause, .. } | Error::Key { cause, .. } => cause.retry_after(),
            Error::Cleanup { failure, .. } => failure.retry_after(),
            Error::LiveStalled { cause, .. } => cause.retry_after(),
            _ => None,
        }
    }

    /// 外部依赖的临时故障，重试可能成功。
    pub fn retryable(&self) -> bool {
        match self {
            Error::Http { kind, .. } => kind.retryable(),
            Error::Incomplete { .. } => true,
            Error::Segment { cause, .. } | Error::Key { cause, .. } => cause.retryable(),
            Error::Cleanup { failure, .. } => failure.retryable(),
            Error::LiveStalled { cause, .. } => cause.retryable(),
            Error::Io { cause, .. } => crate::busy::held_open(cause),
            _ => false,
        }
    }
}

/// 来源用到了不支持的特性，或内容放不进所选的输出。
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    #[error("播放列表 {} 没有分片", bare_url(.0))]
    EmptyPlaylist(Box<Url>),
    #[error("直播播放列表 {} 既没有 EXT-X-TARGETDURATION 也没有分片，无法确定刷新间隔", bare_url(.0))]
    NoTargetDuration(Box<Url>),
    /// 播放列表 `url` 第 `line` 行的 EXT-X-KEY 用了本库解不了的加密（DRM、SAMPLE-AES 等）
    #[error("播放列表 {} 第 {line} 行的 EXT-X-KEY：{what}", bare_url(.url))]
    Encryption {
        url: Box<Url>,
        line: usize,
        what: hs_m3u8_hls::Encryption,
    },
    /// 播放列表 `0` 里要取的分片或 init 段加密而所用的 EXT-X-KEY 没写 URI（违反 RFC 8216），取不到 key；已知 key 时用
    /// 自定义 key（[`crate::JobRequest::key`]）。下载与录制收到播放列表即核对，不等取分片
    #[error("播放列表 {} 的 EXT-X-KEY 没写 URI，取不到 key；已知 key 时可用自定义 key", bare_url(.0))]
    KeyWithoutUri(Box<Url>),
    /// 播放列表 `url` 第 `line` 行的 EXT-X-I-FRAMES-ONLY：只有关键帧的播放列表，供快进时预览，不是正常播放的内容；
    /// 来源应为主播放列表或正常的媒体播放列表
    #[error("播放列表 {} 第 {line} 行：EXT-X-I-FRAMES-ONLY，只有关键帧，不是正常播放的内容", bare_url(.url))]
    IFramesOnly { url: Box<Url>, line: usize },
    /// 播放列表 `url` 第 `line` 行的 EXT-X-SKIP：增量更新略去了前面的分片。服务器只在地址里请求了 `_HLS_skip` 时才
    /// 这样给，去掉这个参数即可
    #[error("播放列表 {} 第 {line} 行：EXT-X-SKIP，增量更新略去了前面的分片，去掉地址里的 _HLS_skip 参数即可", bare_url(.url))]
    DeltaUpdate { url: Box<Url>, line: usize },
    /// 内容放不进 MP4，可只输出本地 HLS（[`crate::Target::Hls`]）：它原样保留分片，不受这些限制。下到（直播为录到）
    /// 各组各轨的第一个分片时先核对其中判断得了的：编码，各组的流种类、编码参数与解码配置，E-AC-3 的帧结构；组内中途
    /// 的变化、后面分片里的问题，以及只靠第一个分片判断不了的，合并时读到才报。原因里的组号为合并时的：直播为各轨
    /// 都录到的组按会话、不连续段排列的下标
    #[error("{0}")]
    Mp4(Mp4Unsupported),
    /// 本地 HLS 表示不了这些内容，可只输出 MP4（[`crate::Target::Mp4`]）
    #[error("{0}")]
    Hls(HlsUnsupported),
    #[error("同一不连续段内 EXT-X-MAP 发生变化（第 {track} 条轨，不连续段 {discontinuity}）")]
    InitChangesWithinGroup { track: usize, discontinuity: u64 },
    #[error("各轨的不连续段不一致：第 0 条轨 {first:?}，第 {track} 条轨 {found:?}")]
    DiscontinuityMismatch {
        track: usize,
        first: Vec<u64>,
        found: Vec<u64>,
    },
    /// 点播没有一个不连续段合并得了：每段都有轨只有来源标了 EXT-X-GAP 的分片（合并要各轨都有内容），如某条轨全部
    /// 标了 GAP。`0` 为第一段里这样一条轨的播放列表
    #[error("播放列表 {} 在第一个不连续段里只有 EXT-X-GAP，没有一段各轨都有内容、合并得了", bare_url(.0))]
    NoMergeableGroup(Box<Url>),
    /// fMP4 分片或 init 段 `url` 从第 `at` 字节起的 moof 用了绝对位置（tfhd 的 base_data_offset）：RFC 8216 3.3 要求
    /// 按 moof 相对寻址，分片单独存放、接起来合并时绝对位置都对不上
    #[error("{} {}", bare_url(.url), base_data_offset_text(*.at))]
    Fmp4BaseDataOffset { url: Box<Url>, at: u64 },
    /// init 段 `0`（EXT-X-MAP）是 TS 的节目表（PAT/PMT），本库只支持 fMP4 的 init 段
    #[error("init 段 {} 是 TS 的节目表，只支持 fMP4 的 init 段", bare_url(.0))]
    TsInit(Box<Url>),
}

fn base_data_offset_text(at: u64) -> String {
    format!(
        "第 {at} 字节起的 moof 用了绝对位置（tfhd 的 base_data_offset），HLS 要求按 moof 相对寻址"
    )
}

/// 本地 HLS 表示不了的内容。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HlsUnsupported {
    /// 第 `track` 条轨里没有 init 段的不连续段组排在有 init 段（fMP4）的组后面：EXT-X-MAP 一直作用到下一个
    /// EXT-X-MAP，前面的 init 段会被用在后面的组上。直播续录时服务器从 fMP4 换成 TS 会这样
    #[error("第 {track} 条轨在 fMP4 的段之后又有不用 init 段的段，本地 HLS 无法表示")]
    MixedInit { track: usize },
    /// 音视频分离时主播放列表必须写码率（BANDWIDTH），而某条轨的分片声明的时长都为 0 算不出，来源也没写
    #[error(
        "某条轨的分片声明的时长都为 0、来源也没写 BANDWIDTH，算不出本地 HLS 主播放列表必填的码率"
    )]
    BandwidthUnknown,
}

/// 取不到的请求：失败的原因，与服务器在 429/503 中要求（Retry-After）的最短等待。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}")]
pub struct HttpFailure {
    pub kind: HttpError,
    pub retry_after: Option<Duration>,
}

/// 取不到的分片或 init 段。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchFailure {
    /// 请求取不到：404/410，或重试后仍失败的临时故障
    #[error(transparent)]
    Http(HttpFailure),
    /// 重试后取到的内容仍没写完（见 [`Error::Incomplete`]）
    #[error("内容没有写完")]
    Incomplete,
}

impl FetchFailure {
    /// 失败的种类，不带服务器要求的等待（缺失的原因只记它，见 [`crate::MissReason`]）。
    pub fn kind(&self) -> FetchError {
        match self {
            FetchFailure::Http(failure) => FetchError::Http(failure.kind.clone()),
            FetchFailure::Incomplete => FetchError::Incomplete,
        }
    }

    pub fn retryable(&self) -> bool {
        match self {
            FetchFailure::Http(failure) => failure.kind.retryable(),
            FetchFailure::Incomplete => true,
        }
    }

    /// 服务器在 429/503 中要求（Retry-After）的最短等待；没有这样的要求时为 None。
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            FetchFailure::Http(failure) => failure.retry_after,
            FetchFailure::Incomplete => None,
        }
    }
}

/// 分片或 init 段取不到的原因的种类。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// 请求取不到：404/410，或重试后仍失败的临时故障
    #[error(transparent)]
    Http(HttpError),
    /// 重试后取到的内容仍没写完（见 [`Error::Incomplete`]）
    #[error("内容没有写完")]
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("HTTP 状态 {0}")]
    Status(u16),
    #[error("字节范围请求应返回 206，实际 {0}")]
    RangeIgnored(u16),
    /// 字节范围请求返回了 206，Content-Range 却不是请求的那一段（多为 CDN 缓存 206 响应时没按 Range 区分）；值为
    /// 收到的 Content-Range，没有时为 None
    #[error("字节范围请求返回的是另一段（Content-Range：{}）", .0.as_deref().unwrap_or("无"))]
    RangeMismatch(Option<String>),
    #[error("超时")]
    Timeout,
    /// TLS 证书校验失败（自签名、过期、与域名不符等）；确认来源可信时可设 [`crate::HttpOptions::insecure`]
    #[error("证书校验失败：{0}")]
    Certificate(String),
    /// 跟随重定向失败（循环、次数过多）
    #[error("重定向失败：{0}")]
    Redirect(String),
    /// 连不上服务器或代理
    #[error("连接失败：{0}")]
    Connect(String),
    /// 代理要求认证或拒绝了这次连接，出在代理的设置或策略上：HTTP 代理对 CONNECT 回 407，或 SOCKS 代理认证失败、
    /// 不允许或不支持这次连接。要检查代理设置与口令。CONNECT 回了别的非成功状态、SOCKS 代理报目标连不上的算连接失败
    #[error("代理拒绝：{0}")]
    Proxy(String),
    /// 请求无法构造：地址能解析，HTTP 库却不接受（如超过 65534 字节，播放列表里的与回调改出的都可能）。回调改出的
    /// 地址不是 http/https、或请求头不合法时报 [`Error::Hook`]
    #[error("请求不合法：{0}")]
    InvalidRequest(String),
    /// 读取响应体等其他传输错误
    #[error("{0}")]
    Transport(String),
}

impl HttpError {
    pub fn retryable(&self) -> bool {
        match self {
            HttpError::Status(code) => matches!(code, 408 | 429 | 500..=599),
            HttpError::Timeout | HttpError::Connect(_) | HttpError::Transport(_) => true,
            HttpError::RangeIgnored(_)
            | HttpError::RangeMismatch(_)
            | HttpError::Certificate(_)
            | HttpError::Redirect(_)
            | HttpError::Proxy(_)
            | HttpError::InvalidRequest(_) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Integrity {
    /// 多为 key 不对（IV 不对不会使填充不合法）；解出的是截断的媒体数据时报 [`Error::Incomplete`]
    #[error("解密后填充不合法（key 不对或数据损坏）")]
    Padding,
    /// 整块部分解出的不是截断的媒体数据（如源站返回的是错误页面、完整的内容后面多了字节）；是的报
    /// [`Error::Incomplete`]
    #[error("密文长度 {0} 不是 16 的整数倍")]
    CipherLength(usize),
    /// `found` 为读到的字节数：超出 `expected` 时读到即停，响应体可能更长
    #[error("字节范围应为 {expected} 字节，读到 {found}")]
    RangeLength { expected: u64, found: usize },
    #[error("不是 TS 或可识别的打包音频（AAC、MP3、AC-3、E-AC-3）（开头 {0}）")]
    UnrecognizedSegment(String),
    #[error("不是 fMP4（开头 {0}）")]
    NotFmp4(String),
    /// fMP4 从第 `at` 字节起的顶层 box 的长度字段 `size` 小于 box 头的长度（8 字节，64 位长度时 16 字节）。没写完的
    /// 报 [`Error::Incomplete`]
    #[error("fMP4 第 {at} 字节起的 box 长度 {size} 小于 box 头")]
    Fmp4BoxTooSmall { at: u64, size: u64 },
    /// fMP4 从第 `at` 字节起末尾的 `len` 个字节不是完整的 box：不足 8 个凑不成 box 头，或类型认不出而长度写 0 或超出了
    /// 数据。多为多出的字节（如 CDN 加了换行或填充、回调没去干净）；trun 声明的样本数据也超出了数据时报
    /// [`Error::Incomplete`]
    #[error("fMP4 第 {at} 字节起末尾的 {len} 个字节不是完整的 box")]
    Fmp4TrailingBytes { at: u64, len: u64 },
    /// fMP4 从第 `at` 字节起的顶层 box（moof 或 moov）内容不合法：子 box 超出、字段不全，或算出的样本位置溢出
    #[error("fMP4 第 {at} 字节起的 box 内容不合法")]
    Fmp4Malformed { at: u64 },
    /// fMP4 最后一个顶层 box 的长度写为 0（到数据末尾），实际的 `size` 字节写不进 32 位的长度字段：存盘前要改写成实际
    /// 长度（合并时一条轨的分片接成一个输入读）
    #[error(
        "fMP4 第 {at} 字节起的最后一个 box 长度写为 0，实际 {size} 字节超过 4 GiB，改写不了长度字段"
    )]
    Fmp4OpenBoxTooLarge { at: u64, size: u64 },
}

/// 直播停滞的故障原因。
#[derive(Debug, thiserror::Error)]
pub enum StallError {
    /// 刷新播放列表一直失败（404/410 之外的取不到，或内容不完整），带最近一次的原因
    #[error("刷新播放列表一直失败，最近一次：{0}")]
    RefreshFailed(RefreshCause),
    /// 刷新播放列表（或为新分片拉 init 段）的请求，在察觉停滞之后又过了一个目标时长仍未返回
    #[error("刷新播放列表的请求一直没有返回")]
    RefreshPending,
    /// 停滞开始之后播放列表仍列出了新分片，但一个都没有下载成功；带最近一次取不到分片或 init 段的原因
    #[error("有新分片，但一个都没有下载成功，最近一次：{0}")]
    Unrecordable(FetchFailure),
    /// 这条轨看起来已结束（原因同 [`StallCause`]），而其他轨仍在出新分片；播放列表被删除时不可重试
    #[error("这条轨{}，其他轨仍在出新分片", stopped_text(*.0))]
    TrackStopped(StallCause),
}

impl StallError {
    fn retryable(&self) -> bool {
        match self {
            StallError::RefreshFailed(cause) => cause.retryable(),
            StallError::RefreshPending => true,
            StallError::Unrecordable(failure) => failure.retryable(),
            StallError::TrackStopped(cause) => *cause == StallCause::NoNewSegments,
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            StallError::RefreshFailed(cause) => cause.retry_after(),
            StallError::Unrecordable(failure) => failure.retry_after(),
            StallError::RefreshPending | StallError::TrackStopped(_) => None,
        }
    }
}

/// 请求用途在错误信息里的叫法。
fn purpose_text(purpose: Purpose) -> &'static str {
    match purpose {
        Purpose::Playlist => "播放列表 ",
        Purpose::Key => "key ",
        Purpose::Init => "init 段 ",
        Purpose::Segment => "分片 ",
    }
}

fn stopped_text(cause: StallCause) -> String {
    match cause {
        StallCause::NoNewSegments => "不再出新分片".to_owned(),
        StallCause::PlaylistGone(status) => format!("的播放列表已被删除（HTTP {status}）"),
    }
}

/// 任务目录不能使用的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkDirProblem {
    /// 不是本库建立的任务目录，不会去动它
    #[error("目录不为空，且没有 job.json")]
    NotEmpty,
    #[error("正被另一个任务使用")]
    Locked,
    /// job.json、outputs.json 或分片文件无法识别；是另一个格式版本写的为 [`WorkDirProblem::UnsupportedVersion`]
    #[error("内容无法识别：{0}")]
    Corrupt(String),
    /// 记录 `file`（job.json 或 outputs.json）是另一个格式版本写的：`found` 比本库支持的 `supported` 新时，用更新的
    /// 版本续传；旧时，用写下它的版本做完，或放弃任务（outputs.json 是另一个版本写的时放弃也不动它，见
    /// [`crate::Engine::discard`]）
    #[error("{file} 的格式版本 {found} 不受支持（支持 {supported}）")]
    UnsupportedVersion {
        file: &'static str,
        found: u32,
        supported: u32,
    },
    /// 目录里记录的任务类型与本次不同：记录的是直播、本次是点播，多为中断期间直播结束了而请求没开启直播
    /// （[`crate::JobRequest::live`]），开启即可续录，或用 [`crate::Engine::merge_recorded`] 只合并；记录的是点播、
    /// 本次是直播（同一地址现在是直播），换一个任务目录
    #[error("记录的是{recorded}任务，本次是{current}任务")]
    KindMismatch { recorded: JobType, current: JobType },
    /// 只合并（[`crate::Engine::merge_recorded`]）只用于直播录制，目录里是点播的下载；点播用同样的请求正常运行即可续传
    #[error("目录里是点播的下载，只合并只用于直播录制")]
    NotLiveRecording,
    /// 来源是主播放列表，选轨偏好与记录的不同：换了选轨即是另一个任务，换一个任务目录，或放弃任务
    /// （[`crate::Engine::discard`]）
    #[error("选轨偏好与记录的不同")]
    PreferenceChanged,
    /// 来源的轨道（所选变体、有无独立音频）与记录的不同；地址的种类与记录的不同（原来是主播放列表、这次是某一档的
    /// 媒体播放列表，或反过来）也是这样，换回同类的地址即可
    #[error("来源的轨道与记录的不同")]
    TracksMismatch,
    /// 点播：播放列表的分片与记录的不一致
    #[error("播放列表的分片与记录的不一致，不能续传")]
    PlanChanged,
    /// 记录的变体或音频 rendition 已不在主播放列表中。已录的直播可用 [`crate::Engine::merge_recorded`] 合并，
    /// 或换一个任务目录重新开始
    #[error("记录的变体或音频已不在主播放列表中")]
    SelectionGone,
    /// 核对不上这次取到的与已下载的是同一份内容：点播为核对的组里有已完成的分片重新下载后内容不同，或都已取不到（见
    /// [`crate::Source::url`]），多为另一个视频、同一视频的另一档或同一地址换了内容，同一地址也核对不上的多为来源每次
    /// 输出的字节不同（边转码边出），只能重新下载；直播为地址与记录的不同，且接不上之前录到的（有轨内容不同，或没有一条
    /// 轨核对一致，见 [`crate::LiveOptions`]）。记录不变：直播可用原来的地址续录、用 [`crate::Engine::merge_recorded`]
    /// 合并已录的部分；或换一个任务目录、放弃任务
    #[error("核对不上这次取到的与已下载的是同一份内容")]
    SourceUnverified,
    /// 直播续录：之前录到的接不上（要另起会话），而第 `track` 条轨这次不录（已录满 max_duration，或这次没有内容）：
    /// 另起的会话里它没有分片，各轨都有的组才合并得了，录到的合并不进去。可调大 max_duration 后续录、用
    /// [`crate::Engine::merge_recorded`] 合并已录的部分，或换一个任务目录重新录
    #[error("要另起会话，而第 {track} 条轨这次不录，录到的合并不进去")]
    Unaligned { track: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobType {
    Vod,
    Live,
}

impl fmt::Display for JobType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            JobType::Vod => "点播",
            JobType::Live => "直播",
        })
    }
}
