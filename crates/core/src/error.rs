//! 下载任务的错误。
//!
//! 按调用方的处理方式分类：
//! - 调用方输入：[`Error::InvalidInput`]、[`Error::OutputExists`]，改参数后再试；
//! - 来源内容：[`Error::Playlist`]、[`Error::NotMediaPlaylist`]、[`Error::Select`]、[`Error::Unsupported`]、
//!   [`Error::Integrity`]、[`Error::KeyLength`]，同样的来源再试也不会成功；
//! - 外部依赖：[`Error::Http`]（看 [`HttpError::retryable`]）、[`Error::Io`]；
//!   [`Error::Segment`]、[`Error::Key`] 说明出在哪个分片或 key，可否重试看其原因；
//! - 任务目录：[`Error::PlanChanged`]、[`Error::WorkDir`]、[`Error::NothingRecorded`]；
//! - 回调：[`Error::Hook`]；合并：[`Error::Remux`]；[`Error::Cancelled`]。
//!
//! 错误信息已包含原因，不再经 `source()` 链出同一段文字；地址只显示到路径，查询串（常带令牌）不显示。

use std::fmt;
use std::io;
use std::path::PathBuf;

use url::Url;

use crate::hooks::{HookError, HookKind};
use crate::plan::strip_query;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("参数错误：{0}")]
    InvalidInput(String),
    #[error("输出文件已存在：{}", .0.display())]
    OutputExists(PathBuf),
    /// 不是播放列表或语法错误
    #[error("解析播放列表 {} 失败：{cause}", strip_query(.url))]
    Playlist {
        url: Box<Url>,
        cause: Box<hs_m3u8_hls::Error>,
    },
    /// 应为媒体播放列表的地址（变体、音频 rendition）返回了主播放列表
    #[error("{} 应为媒体播放列表，实际是主播放列表", strip_query(.url))]
    NotMediaPlaylist { url: Box<Url> },
    #[error("选轨失败：{0}")]
    Select(hs_m3u8_hls::SelectError),
    #[error("不支持：{0}")]
    Unsupported(Unsupported),
    #[error("请求 {} 失败：{kind}", strip_query(.url))]
    Http { url: Box<Url>, kind: HttpError },
    /// 某个分片最终失败；`cause` 为分片请求、回调、解密或校验的错误
    #[error("第 {track} 条轨分片 {sequence}（{}）失败：{cause}", strip_query(.url))]
    Segment {
        track: usize,
        sequence: u64,
        url: Box<Url>,
        cause: Box<Error>,
    },
    /// 取 key 失败；`cause` 为请求或回调的错误
    #[error("取 key {} 失败：{cause}", strip_query(.url))]
    Key { url: Box<Url>, cause: Box<Error> },
    #[error("key {} 应为 16 字节，实际 {length} 字节", strip_query(.url))]
    KeyLength { url: Box<Url>, length: usize },
    #[error("数据校验失败（{}）：{kind}", strip_query(.url))]
    Integrity { url: Box<Url>, kind: Integrity },
    #[error("播放列表的分片与任务目录记录的不一致，不能续传")]
    PlanChanged,
    #[error("任务目录 {} 无法使用：{problem}", .path.display())]
    WorkDir {
        path: PathBuf,
        problem: WorkDirProblem,
    },
    #[error("直播没有录到可合并的分片")]
    NothingRecorded,
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
    #[error("任务已取消")]
    Cancelled,
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
    /// 外部依赖的临时故障，重试可能成功。
    pub fn retryable(&self) -> bool {
        match self {
            Error::Http { kind, .. } => kind.retryable(),
            Error::Segment { cause, .. } | Error::Key { cause, .. } => cause.retryable(),
            _ => false,
        }
    }
}

/// 来源用到了不支持的特性。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    /// [`crate::JobRequest::live`] 为 None 时遇到直播
    #[error("直播（播放列表没有 EXT-X-ENDLIST）")]
    Live,
    #[error("播放列表 {} 没有分片", strip_query(.0))]
    EmptyPlaylist(Box<Url>),
    #[error("直播播放列表 {} 既没有 EXT-X-TARGETDURATION 也没有分片，无法确定刷新间隔", strip_query(.0))]
    NoTargetDuration(Box<Url>),
    #[error("DRM 加密（KEYFORMAT={keyformat}）")]
    Drm { keyformat: String },
    #[error("SAMPLE-AES 加密")]
    SampleAes,
    #[error("未知的加密方式 {0}")]
    KeyMethod(String),
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
    #[error("解密后填充不合法（key 或 IV 不对）")]
    Padding,
    #[error("密文长度 {0} 不是 16 的整数倍")]
    CipherLength(usize),
    #[error("字节范围应为 {expected} 字节，实际 {found}")]
    RangeLength { expected: u64, found: usize },
    #[error("不是 TS、ADTS 或带 ID3 头的音频（开头 {0}）")]
    UnrecognizedSegment(String),
    #[error("不是 fMP4（开头 {0}）")]
    NotFmp4(String),
}

/// 任务目录不能使用的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkDirProblem {
    /// 不是本库建立的任务目录，不会去动它
    #[error("目录不为空，且没有 job.json")]
    NotEmpty,
    #[error("正被另一个任务使用")]
    Locked,
    /// job.json 或分片文件无法识别
    #[error("内容无法识别：{0}")]
    Corrupt(String),
    #[error("记录的是{recorded}任务，当前来源是{current}")]
    KindMismatch { recorded: JobType, current: JobType },
    /// 直播：来源地址（含查询串）或选轨偏好与记录的不同
    #[error("记录的是另一个直播来源或选轨偏好")]
    SourceMismatch,
    /// 直播：主播放列表的轨道布局（有无独立音频）与记录的不同
    #[error("来源的轨道布局与记录的不同")]
    StreamsMismatch,
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
