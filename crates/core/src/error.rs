//! 下载任务的错误。

use std::io;
use std::path::PathBuf;

use url::Url;

use crate::Purpose;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("参数错误：{0}")]
    InvalidInput(String),
    #[error("输出文件已存在：{0}")]
    OutputExists(PathBuf),
    #[error("解析播放列表 {url} 失败：{source}")]
    Playlist {
        url: Box<Url>,
        source: Box<hs_m3u8_hls::Error>,
    },
    #[error("选轨失败：{0}")]
    Select(#[from] hs_m3u8_hls::SelectError),
    #[error("不支持：{0}")]
    Unsupported(Unsupported),
    #[error("请求 {url} 失败：{kind}")]
    Http { url: Box<Url>, kind: HttpError },
    #[error("分片 {sequence}（{url}）失败：{source}")]
    Segment {
        sequence: u64,
        url: Box<Url>,
        source: Box<Error>,
    },
    #[error("key {url} 应为 16 字节，实际 {length} 字节")]
    KeyLength { url: Box<Url>, length: usize },
    #[error("数据校验失败（{url}）：{kind}")]
    Integrity { url: Box<Url>, kind: Integrity },
    #[error("播放列表的分片与任务目录记录的不一致，不能续传")]
    PlanChanged,
    #[error("任务目录 {path} 无法使用：{reason}")]
    WorkDir { path: PathBuf, reason: String },
    #[error("{purpose:?} 回调出错：{message}")]
    Hook { purpose: Purpose, message: String },
    #[error("{action} {path} 失败：{source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("合并失败：{0}")]
    Remux(#[source] Box<hs_m3u8_remux::Error>),
    #[error("任务已取消")]
    Cancelled,
}

impl From<hs_m3u8_remux::Error> for Error {
    fn from(error: hs_m3u8_remux::Error) -> Self {
        Error::Remux(Box::new(error))
    }
}

impl Error {
    /// 外部依赖的临时故障，重试可能成功。
    pub fn retryable(&self) -> bool {
        match self {
            Error::Http { kind, .. } => kind.retryable(),
            Error::Segment { source, .. } => source.retryable(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    #[error("直播（播放列表没有 EXT-X-ENDLIST）")]
    Live,
    #[error("播放列表 {0} 没有分片")]
    EmptyPlaylist(Box<Url>),
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
    #[error("{0}")]
    Other(String),
}

impl HttpError {
    pub fn retryable(&self) -> bool {
        match self {
            HttpError::Status(code) => matches!(code, 408 | 429 | 500..=599),
            HttpError::Timeout | HttpError::Connect(_) | HttpError::Other(_) => true,
            HttpError::RangeIgnored(_) => false,
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
    #[error("不是 TS 或 ADTS 数据（开头 {0}）")]
    NotTs(String),
    #[error("不是 fMP4 分片（开头 {0}）")]
    NotFmp4(String),
}
