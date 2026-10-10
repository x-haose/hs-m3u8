//! 下载任务的错误。
//!
//! 按调用方的处理方式分类：
//! - 调用方输入：[`Error::InvalidInput`]、[`Error::OutputExists`]、[`Error::OutputOccupied`]，改参数后再试；
//! - 来源内容：[`Error::Playlist`]、[`Error::NotMediaPlaylist`]、[`Error::Select`]、[`Error::Unsupported`]、
//!   [`Error::Integrity`]、[`Error::KeyLength`]，同样的请求再试也不会成功（其中 [`Unsupported::Live`] 开启直播
//!   录制即可）；
//! - 外部依赖：[`Error::Http`]（看 [`HttpError::retryable`]）、[`Error::Io`]；
//!   [`Error::Segment`]、[`Error::Key`] 说明出在哪个分片或 key，[`Error::LiveStalled`] 说明直播哪条轨停滞，
//!   可否重试看其原因（[`Error::retryable`]）；
//! - 任务目录：[`Error::WorkDir`]、[`Error::NothingRecorded`]；
//! - 回调：[`Error::Hook`]；合并：[`Error::Remux`]；[`Error::Cancelled`]；失败后清理也失败：[`Error::Cleanup`]。
//!
//! 错误信息已包含原因，不经 `source()` 重复给出；地址只显示到路径，不含用户名、密码与查询串（常带凭据或令牌）。

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use url::Url;

use crate::hooks::{HookError, HookKind};
use crate::ident::bare_url;
use crate::{Leftover, StallCause};

#[derive(thiserror::Error)]
pub enum Error {
    #[error("参数错误：{0}")]
    InvalidInput(String),
    /// 输出已存在，要求覆盖（[`crate::OutputOptions::overwrite`]）即可替换
    #[error("输出已存在：{}", .0.display())]
    OutputExists(PathBuf),
    /// 路径已被占用，覆盖也不会替换：要输出 MP4 而那里是目录，要输出 HLS 而那里不是目录、或目录里有不是本库
    /// 写出的文件，以免路径给错时删掉别人的文件；换一个路径
    #[error("路径已被占用，覆盖也不会替换：{}", .0.display())]
    OutputOccupied(PathBuf),
    /// 不是播放列表或语法错误
    #[error("解析播放列表 {} 失败：{cause}", bare_url(.url))]
    Playlist {
        url: Box<Url>,
        cause: Box<hs_m3u8_hls::Error>,
    },
    /// 应为媒体播放列表的地址（变体、音频 rendition）返回了主播放列表
    #[error("{} 应为媒体播放列表，实际是主播放列表", bare_url(.url))]
    NotMediaPlaylist { url: Box<Url> },
    #[error("选轨失败：{0}")]
    Select(hs_m3u8_hls::SelectError),
    #[error("不支持：{0}")]
    Unsupported(Unsupported),
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
    #[error("key {} 应为 16 字节，实际 {length} 字节", bare_url(.url))]
    KeyLength { url: Box<Url>, length: usize },
    #[error("数据校验失败（{}）：{kind}", bare_url(.url))]
    Integrity { url: Box<Url>, kind: Integrity },
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
    #[error("{action} {} 失败：{cause}", .path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        cause: io::Error,
    },
    #[error("合并失败：{0}")]
    Remux(Box<hs_m3u8_remux::Error>),
    /// 写输出失败后，收拾本次写出的也没做完：`failure` 为原来的失败，`leftovers` 为留下的东西（至少一项）
    #[error("{failure}；{}", joined(.leftovers))]
    Cleanup {
        failure: Box<Error>,
        leftovers: Vec<Leftover>,
    },
    #[error("任务已取消")]
    Cancelled,
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

impl From<hs_m3u8_remux::Error> for Error {
    fn from(error: hs_m3u8_remux::Error) -> Self {
        Error::Remux(Box::new(error))
    }
}

impl From<hs_m3u8_hls::SelectError> for Error {
    fn from(error: hs_m3u8_hls::SelectError) -> Self {
        Error::Select(error)
    }
}

impl Error {
    /// 取不到：404/410（已过期），或重试后仍失败的临时故障时返回原因；分片的失败看其原因。key、回调、校验等
    /// 失败为 None。
    pub(crate) fn missable(&self) -> Option<HttpError> {
        match self {
            Error::Http { kind, .. }
                if kind.retryable() || matches!(kind, HttpError::Status(404 | 410)) =>
            {
                Some(kind.clone())
            }
            Error::Segment { cause, .. } => cause.missable(),
            _ => None,
        }
    }

    /// 服务器在 429/503 中要求（Retry-After）的最短等待；其他错误为 None。
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// 外部依赖的临时故障，重试可能成功。
    pub fn retryable(&self) -> bool {
        match self {
            Error::Http { kind, .. } => kind.retryable(),
            Error::Segment { cause, .. } | Error::Key { cause, .. } => cause.retryable(),
            Error::Cleanup { failure, .. } => failure.retryable(),
            Error::LiveStalled { cause, .. } => match cause {
                StallError::RefreshFailed(error) => error.retryable(),
                StallError::RefreshPending => true,
                StallError::Unrecordable(kind) => kind.retryable(),
                StallError::TrackStopped(cause) => *cause == StallCause::NoNewSegments,
            },
            _ => false,
        }
    }
}

/// 来源用到了不支持的特性。
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    /// [`crate::JobRequest::live`] 为 None 时遇到直播
    #[error("直播（播放列表没有 EXT-X-ENDLIST）")]
    Live,
    #[error("播放列表 {} 没有分片", bare_url(.0))]
    EmptyPlaylist(Box<Url>),
    #[error("直播播放列表 {} 既没有 EXT-X-TARGETDURATION 也没有分片，无法确定刷新间隔", bare_url(.0))]
    NoTargetDuration(Box<Url>),
    #[error("DRM 加密（KEYFORMAT={keyformat}）")]
    Drm { keyformat: String },
    #[error("SAMPLE-AES 加密")]
    SampleAes,
    #[error("未知的加密方式 {0}")]
    KeyMethod(String),
    /// 第 `track` 条轨里没有 init 段的不连续段组排在有 init 段（fMP4）的组后面：EXT-X-MAP 一直作用到下一个
    /// EXT-X-MAP，前面的 init 段会被用在后面的组上，本地 HLS 无法表示，可只输出 MP4。直播续录时服务器从 fMP4
    /// 换成 TS 会这样
    #[error("第 {track} 条轨在 fMP4 的段之后又有不用 init 段的段，本地 HLS 无法表示")]
    HlsMixedInit { track: usize },
    /// 本地 HLS 的主播放列表必须写码率（BANDWIDTH），而某条轨的分片声明的时长都为 0 算不出，来源也没写
    #[error(
        "某条轨的分片声明的时长都为 0、来源也没写 BANDWIDTH，算不出本地 HLS 主播放列表必填的码率"
    )]
    HlsBandwidthUnknown,
    #[error("同一不连续段内 EXT-X-MAP 发生变化（第 {track} 条轨，不连续段 {discontinuity}）")]
    InitChangesWithinGroup { track: usize, discontinuity: u64 },
    #[error("各轨的不连续段不一致：第 0 条轨 {first:?}，第 {track} 条轨 {found:?}")]
    DiscontinuityMismatch {
        track: usize,
        first: Vec<u64>,
        found: Vec<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("HTTP 状态 {0}")]
    Status(u16),
    #[error("字节范围请求应返回 206，实际 {0}")]
    RangeIgnored(u16),
    #[error("超时")]
    Timeout,
    #[error("连接失败：{0}")]
    Connect(String),
    /// 请求无法构造，如回调给出了不合法的地址或请求头
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
            HttpError::RangeIgnored(_) | HttpError::InvalidRequest(_) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Integrity {
    /// 多为 key 不对；IV 不对不会使填充不合法
    #[error("解密后填充不合法（key 不对或数据损坏）")]
    Padding,
    #[error("密文长度 {0} 不是 16 的整数倍")]
    CipherLength(usize),
    #[error("字节范围应为 {expected} 字节，实际 {found}")]
    RangeLength { expected: u64, found: usize },
    #[error("不是 TS 或可识别的打包音频（AAC、MP3、AC-3、E-AC-3）（开头 {0}）")]
    UnrecognizedSegment(String),
    #[error("不是 fMP4（开头 {0}）")]
    NotFmp4(String),
}

/// 直播停滞的故障原因。
#[derive(Debug, thiserror::Error)]
pub enum StallError {
    /// 刷新播放列表一直失败（404/410 之外的可重试错误，或内容不完整）
    #[error("刷新播放列表一直失败，最近一次：{0}")]
    RefreshFailed(Box<Error>),
    /// 刷新请求超过一个目标时长仍未返回
    #[error("刷新播放列表的请求一直没有返回")]
    RefreshPending,
    /// 播放列表仍在列出新分片，但一个都没有下载成功；带最近一次取不到分片或 init 段的原因
    #[error("有新分片，但一个都没有下载成功，最近一次：{0}")]
    Unrecordable(HttpError),
    /// 这条轨看起来已结束（原因同 [`StallCause`]），而其他轨仍在出新分片；播放列表被删除时不可重试
    #[error("这条轨{}，其他轨仍在出新分片", stopped_text(*.0))]
    TrackStopped(StallCause),
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
    /// job.json、outputs.json 或分片文件无法识别
    #[error("内容无法识别：{0}")]
    Corrupt(String),
    /// 目录里记录的任务类型与本次不同：记录的是直播、本次是点播，多为中断期间直播结束了而请求没开启直播
    /// （[`crate::JobRequest::live`]），开启即可续录，或用 [`crate::Engine::merge_recorded`] 只合并；记录的是点播、
    /// 本次是直播（同一来源现在是直播），换一个任务目录
    #[error("记录的是{recorded}任务，本次是{current}任务")]
    KindMismatch { recorded: JobType, current: JobType },
    /// 只合并（[`crate::Engine::merge_recorded`]）只用于直播录制，目录里是点播的下载；点播用同样的请求正常运行即可续传
    #[error("目录里是点播的下载，只合并只用于直播录制")]
    NotLiveRecording,
    /// 来源（见 [`crate::Source::url`]）与记录的不同
    #[error("记录的是另一个来源或选轨偏好")]
    SourceMismatch,
    /// 来源的轨道（所选变体、有无独立音频）与记录的不同
    #[error("来源的轨道与记录的不同")]
    TracksMismatch,
    /// 点播：播放列表的分片与记录的不一致
    #[error("播放列表的分片与记录的不一致，不能续传")]
    PlanChanged,
    /// 记录的变体或音频 rendition 已不在主播放列表中。已录的直播可用 [`crate::Engine::merge_recorded`] 合并，
    /// 或换一个任务目录重新开始
    #[error("记录的变体或音频已不在主播放列表中")]
    SelectionGone,
    /// 直播：来源相同而完整地址与记录的不同（如换了令牌），且有轨接不上它之前录到的内容，无法确认是同一个直播。
    /// 可用原来的地址续录、用 [`crate::Engine::merge_recorded`] 合并已录的部分，或换一个任务目录重新录
    #[error("地址与记录的不同，且当前内容与已录的接不上，无法确认是同一个直播")]
    SourceUnverified,
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
