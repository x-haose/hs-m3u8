//! 合并与核对的错误。

use std::fmt;
use std::io;
use std::path::PathBuf;

use ffmpeg_next as ffmpeg;

use crate::codecs::{MP3_MIN_SAMPLE_RATE, supported_codecs};
use crate::{Container, Shape, StreamKind, fmp4};

/// FFmpeg 返回的错误：AVERROR 码与对应说明。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}（AVERROR {code}）")]
pub struct FfmpegError {
    pub code: i32,
    pub message: String,
}

impl From<ffmpeg::Error> for FfmpegError {
    fn from(error: ffmpeg::Error) -> Self {
        FfmpegError {
            message: error.to_string(),
            code: i32::from(error),
        }
    }
}

/// 出错的输入在哪里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    /// HLS 的合并（[`crate::remux`]）：第 `group` 个不连续段组的第 `track` 条轨
    Track { group: usize, track: usize },
    /// 合并完整文件（[`crate::merge_files`]）：调用方给的第几个输入，从 0 起
    Input(usize),
}

impl Origin {
    /// 轨号或输入的下标。
    pub(crate) fn index(self) -> usize {
        match self {
            Origin::Track { track, .. } | Origin::Input(track) => track,
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Track { group, track } => write!(f, "第 {group} 组第 {track} 条轨"),
            Origin::Input(index) => write!(f, "第 {index} 个输入"),
        }
    }
}

/// 合并与核对的错误。指向某一个输入的以 `at`（[`Origin`]）指出是哪个；HLS 的合并为不连续段组里的轨，合并文件为
/// 第几个输入。只关乎一组的（如 [`Error::TrackCount`]）只带组号。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("FFmpeg 初始化失败：{0}")]
    Init(FfmpegError),
    #[error("至少需要一个不连续段组")]
    NoGroups,
    #[error("指定了 {found} 条轨的取流方式，第 0 组有 {expected} 条轨")]
    StreamsCount { expected: usize, found: usize },
    #[error("第 {group} 组有 {found} 条轨，第 0 组有 {expected} 条")]
    TrackCount {
        group: usize,
        expected: usize,
        found: usize,
    },
    #[error("{at}没有分片")]
    EmptyTrack { at: Origin },
    #[error("{at}的缺片下标须升序、在 1 到分片数减 1 之间")]
    InvalidGaps { at: Origin },
    #[error("合并文件至少需要一个输入")]
    NoInputs,
    /// 合并完整文件时，输出与这个输入是同一个文件（含经符号链接、硬链接）：换上输出时会把它替换掉
    #[error("输出与{at}是同一个文件")]
    OutputIsInput { at: Origin },
    /// 合并完整文件时，这个输入里没有调用方指定要取的这类流
    #[error("{at}里没有{kind}流")]
    MissingStream { at: Origin, kind: StreamKind },
    /// 合并完整文件时，这个输入的格式不收：`format` 为 FFmpeg 认出的格式（解复用器名），认不出时为 None
    #[error(
        "{at} {} 的格式{}不收，合并文件只收 MP4（含 M4A）、Matroska（含 WebM）、TS 与裸 AAC、MP3",
        .path.display(),
        .format.as_deref().map_or_else(|| "认不出，".to_owned(), |f| format!("是 {f}，"))
    )]
    FileFormat {
        at: Origin,
        path: PathBuf,
        format: Option<String>,
    },
    /// 合并完整文件时，这个输入里要取的这类流有，却没有一个包写出：没有包，或都没有 DTS。视频的包都在第一个关键帧
    /// 之前时报 [`Unsupported::NoKeyframe`]
    #[error("{at}的{kind}流没有能写出的包")]
    EmptyInput { at: Origin, kind: StreamKind },
    #[error("第 {group} 组读不出任何视频或音频包")]
    EmptyGroup { group: usize },
    #[error("输出路径无法交给 FFmpeg（不是有效的 UTF-8 或含 NUL 字符）：{}", .0.display())]
    NonUtf8Path(PathBuf),
    /// 没有 init 段的分片开头认不出格式（见 [`crate::standalone::read_head`]）
    #[error("{} 既不是 TS 也不是可识别的打包音频", .0.display())]
    Unrecognized(PathBuf),
    /// fMP4 的 init 段或分片通不过核对（见 [`fmp4::check`]）
    #[error("{} 的 fMP4 结构不对：{problem}", .path.display())]
    Fmp4 {
        path: PathBuf,
        problem: fmp4::Problem,
    },
    /// fMP4 的 init 段或分片从 `at` 起的最后一个 box 长度写为 0（到文件末尾）：接成一个输入读时会越过文件边界（见
    /// [`fmp4`]）
    #[error("{} 第 {at} 字节起的最后一个 box 长度写为 0（到文件末尾）", .path.display())]
    Fmp4OpenEnded { path: PathBuf, at: u64 },
    #[error("打开{at}失败：{cause}")]
    OpenInput { at: Origin, cause: FfmpegError },
    #[error("{at}的{kind}流与前面的重复")]
    DuplicateKind { at: Origin, kind: StreamKind },
    /// 打包音频要按包在输入里的字节位置找到所在的分片；FFmpeg 的裸音频解复用器都给出位置
    #[error("读取{at}（打包音频）时，包没有在输入里的字节位置，定不了属于哪个分片")]
    UnplacedPacket { at: Origin },
    #[error("{0}")]
    Unsupported(Unsupported),
    #[error("创建输出 {path} 失败：{cause}")]
    OpenOutput { path: PathBuf, cause: FfmpegError },
    #[error("封装器不接受选项 {0:?}")]
    RejectedOptions(Vec<(String, String)>),
    #[error("读取{at}失败：{cause}")]
    Read { at: Origin, cause: FfmpegError },
    #[error("写入输出失败：{0}")]
    Mux(FfmpegError),
    #[error("回读输出 {path} 失败：{cause}")]
    Reread { path: PathBuf, cause: FfmpegError },
    #[error("输出回读到 {found} 路流，应为 {expected} 路")]
    VerifyStreams { expected: usize, found: usize },
    #[error("输出第 {stream} 路流回读到 {found} 个包，写入了 {written} 个")]
    VerifyPackets {
        stream: usize,
        written: u64,
        found: u64,
    },
    /// 读分片或输入文件、写出或关闭输出、回读输出时文件出错（写出时含 FFmpeg 写文件的错误，如盘满）；合并文件时另有
    /// 读输出与临时文件的属性、删掉上次留下的临时文件、换上输出时的出错，及输入不是普通文件
    #[error("{action} {path} 失败：{cause}")]
    Io {
        action: &'static str,
        path: PathBuf,
        cause: io::Error,
    },
    /// 调用方取消了（见 [`crate::Cancel`]）
    #[error("合并已取消")]
    Cancelled,
}

/// 内容放不进输出（MP4 或 Matroska）：编码不受支持或参数探测不出；后续组与第 0 组的流种类、编码参数或解码配置不同，
/// 组内分片格式、PID 或编码参数变了，或组内时间戳不递增、往前跳（同一条轨的内容须前后一致、时间戳递增且连续）；一组的
/// 视频没有能开始解码的帧；打包音频没有时间戳而要与其他轨对齐；或各轨都没有要取的流。HLS 的合并只输出 MP4。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    #[error(
        "{at}的{kind}编码 {codec} 放不进 {container}（{}）",
        supported_codecs(*container)
    )]
    Codec {
        at: Origin,
        kind: StreamKind,
        /// FFmpeg 的编码名，如 `vorbis`
        codec: &'static str,
        container: Container,
    },
    /// MP3 的采样率低于 16 kHz（MPEG-2.5 的 8、11.025、12 kHz）：MP4 封装器不收
    #[error(
        "{at}的 MP3 采样率 {sample_rate} Hz 低于 {} Hz，放不进 MP4",
        MP3_MIN_SAMPLE_RATE
    )]
    Mp3SampleRate {
        at: Origin,
        /// Hz
        sample_rate: u32,
    },
    /// 选中的流探测不出编码或编码参数（视频的宽高、音频的采样率与声道数），或与第 0 组比较时没有确定的不同、而有项
    /// 比不了（视频的像素格式只有一边探测出，fMP4 里配置不同的 AAC 有一边没解出帧）：这一组的第一个分片里没有这路流
    /// 能解的数据（打开输入时探测读完第一个分片；完整文件探测读到 FFmpeg 的默认上限）
    #[error(
        "{at}的{kind}流探测不出编码参数或解码配置（分片的这一组第一个分片、或文件开头探测的范围里没有能解的数据），判断不了能否放进输出"
    )]
    ParamsUnknown { at: Origin, kind: StreamKind },
    #[error("{at}的流种类与第 0 组不同，无法放进同一个 MP4")]
    LayoutChanged { at: Origin },
    #[error("{at}的{kind}参数 {found:?} 与第 0 组 {first:?} 不同，无法放进同一条 MP4 轨")]
    ParamsChanged {
        at: Origin,
        kind: StreamKind,
        first: Shape,
        found: Shape,
    },
    /// 编码参数相同而解码配置与第 0 组不同：fMP4 的为 init 段里的（如 H.264 的参数集），TS 与打包音频里的音频为帧头
    /// （如 AAC 的 profile 与采样率下标，LC 与 HE-AAC 的采样率都可以是 48 kHz）；或一组是 fMP4 而另一组是 TS 或打包
    /// 音频。MP4 的样本描述由第 0 组的写成，用在后面的组上解不出
    #[error(
        "{at}的{kind}解码配置（如 AAC 的 profile、H.264 的参数集，或分片封装）与第 0 组不同，无法放进同一条 MP4 轨"
    )]
    ConfigChanged { at: Origin, kind: StreamKind },
    /// TS 的一条轨在组内（或一个 TS 文件中途）换了要取的那一路的 PID：PMT 更新后原来那路不再列出、换成了另一路
    /// （RFC 8216 要求这时标 EXT-X-DISCONTINUITY）。不报的话新 PID 的包都不进输出
    #[error("{at}的{kind}流中途换了 PID（PMT 更新后原来那路不再列出），无法放进输出的同一条轨")]
    StreamChangedInGroup { at: Origin, kind: StreamKind },
    /// 参数随帧携带的输入（TS 与裸音频）里，这路流在组内（或文件）中途换了编码或编码参数（采样率、声道、画面宽高
    /// 等）：输出的一条轨只有一个样本描述
    #[error("{at}的{kind}编码参数中途变了，无法放进输出的同一条轨")]
    ParamsChangedInGroup { at: Origin, kind: StreamKind },
    /// 没有 init 段的一条轨在组内换了分片格式（TS 与打包音频，或打包音频的格式之间）：读取时只认开头那一种，其余的
    /// 分片被当作杂字节跳过
    #[error(
        "{at}的分片格式在组内变了（TS 与打包音频，或打包音频的格式之间），无法放进同一条 MP4 轨"
    )]
    FormatChangedInGroup { at: Origin },
    /// E-AC-3 每帧不足 6 块，输出为 MP4：MP4 封装器把几帧并成一个 6 块（1536 个采样点）的样本，末尾凑不满的帧不写出
    #[error("{at}的 E-AC-3 每帧不足 6 块，放不进 MP4")]
    Eac3ShortFrames { at: Origin },
    /// E-AC-3 有多个独立子流，输出为 MP4：MP4 封装器不支持
    #[error("{at}的 E-AC-3 有多个独立子流，放不进 MP4")]
    Eac3Substreams { at: Origin },
    /// 往回跳多为来源在时间戳重新开始处漏标了 `EXT-X-DISCONTINUITY`，重复多为相邻分片首尾重复了一帧。MP4 要求严格
    /// 递增；Matroska 的时间基为 1 毫秒，换算后相等的不算
    #[error("{at}的{kind}解码时间戳不递增（往回跳或重复），无法放进输出的同一条轨")]
    DtsNotIncreasing { at: Origin, kind: StreamKind },
    /// 组内时间戳往前跳，输出里会留下整组都没有内容的空档，扣掉这条轨上一个包所在的片（跨片时）之后仍比一个最长的
    /// 声明时长（见 [`crate::Segment::duration_us`]）还长：多为来源在时间戳重新开始处漏标了 `EXT-X-DISCONTINUITY`。
    /// 一路流单独中断不算；其间有缺片时不核对
    #[error(
        "{at}的{kind}时间戳在第 {segment} 个分片里往前跳，输出里会留下至少 {jump_us} 微秒的空档，无法放进同一条 MP4 轨"
    )]
    TimestampJump {
        at: Origin,
        kind: StreamKind,
        /// 跳变后的第一个包所在的分片在这一组这条轨里的下标
        segment: usize,
        /// 空档至少多长，微秒：与全组已写出的最大 DTS 之差，减去这条轨上一个包所在的片与其间各片声明的时长
        jump_us: u64,
    },
    /// 这一组里这条轨（或这个文件）的视频没有关键帧（随机访问点；不带 recovery point SEI 的 H.264 开放 GOP 里非 IDR
    /// 的 I 帧不算：之后的帧可能参考它之前的帧）：写进去解不出画面
    #[error("{at}的视频没有关键帧，无法放进输出")]
    NoKeyframe { at: Origin },
    /// 这条轨是打包音频，这一组有分片没有 ID3 时间戳（RFC 8216 3.4 要求每片都有），而这一组还有别的轨：不知道它与
    /// 别的轨怎样对齐。只有它一条轨时按样本数从头接续
    #[error("{at}是打包音频，这一组有分片没有 ID3 时间戳，无法与其他轨对齐")]
    UntimedPackedAudio { at: Origin },
    #[error("各轨都没有要取的视频或音频流，写不出输出")]
    NoStreams,
}

impl Unsupported {
    /// 出在哪个输入；与具体输入无关的（[`Unsupported::NoStreams`]）为 None。
    pub fn at(&self) -> Option<Origin> {
        use Unsupported as U;
        match self {
            U::Codec { at, .. }
            | U::Mp3SampleRate { at, .. }
            | U::ParamsUnknown { at, .. }
            | U::LayoutChanged { at }
            | U::ParamsChanged { at, .. }
            | U::ConfigChanged { at, .. }
            | U::StreamChangedInGroup { at, .. }
            | U::ParamsChangedInGroup { at, .. }
            | U::FormatChangedInGroup { at }
            | U::Eac3ShortFrames { at }
            | U::Eac3Substreams { at }
            | U::DtsNotIncreasing { at, .. }
            | U::TimestampJump { at, .. }
            | U::NoKeyframe { at }
            | U::UntimedPackedAudio { at } => Some(*at),
            U::NoStreams => None,
        }
    }
}

/// FFmpeg 读写文件出错时给的 AVERROR（负的 errno）换成 [`io::Error`]；不是 errno 的（FFmpeg 自己的错误码，取值远在 errno
/// 之外）为 None。Unix 上 errno 即系统的；Windows 上 FFmpeg 经 C 运行库读写文件，errno 是 MSVC C 运行库的取值，与
/// Windows 的系统错误码不是一套编号，按取值换成对应的种类，说明取 FFmpeg 的。
pub(crate) fn file_error(error: &FfmpegError) -> Option<io::Error> {
    let errno = error.code.checked_neg().filter(|e| (1..4096).contains(e))?;
    #[cfg(unix)]
    {
        Some(io::Error::from_raw_os_error(errno))
    }
    #[cfg(windows)]
    {
        let kind = match errno {
            libc::ENOENT => io::ErrorKind::NotFound,
            libc::EACCES => io::ErrorKind::PermissionDenied,
            libc::EEXIST => io::ErrorKind::AlreadyExists,
            libc::ENOSPC => io::ErrorKind::StorageFull,
            libc::EFBIG => io::ErrorKind::FileTooLarge,
            libc::EROFS => io::ErrorKind::ReadOnlyFilesystem,
            libc::ENOMEM => io::ErrorKind::OutOfMemory,
            libc::EINVAL => io::ErrorKind::InvalidInput,
            libc::EBUSY => io::ErrorKind::ResourceBusy,
            libc::EPIPE => io::ErrorKind::BrokenPipe,
            _ => io::ErrorKind::Other,
        };
        Some(io::Error::new(kind, error.message.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_codes_from_ffmpeg_become_io_errors() {
        let full = FfmpegError {
            code: -28,
            message: "No space left on device".to_owned(),
        };
        assert_eq!(
            file_error(&full).map(|e| e.kind()),
            Some(io::ErrorKind::StorageFull)
        );
        // AVERROR_INVALIDDATA（FFERRTAG('I','N','D','A')）是 FFmpeg 自己的错误码，不是 errno
        let invalid = FfmpegError {
            code: -1_094_995_529,
            message: "Invalid data found when processing input".to_owned(),
        };
        assert!(file_error(&invalid).is_none());
    }
}
