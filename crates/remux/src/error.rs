//! 合并与核对的错误。

use std::io;
use std::path::PathBuf;

use ffmpeg_next as ffmpeg;

use crate::codecs::{MP3_MIN_SAMPLE_RATE, supported_codecs};
use crate::{Shape, StreamKind, fmp4};

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
    #[error("第 {group} 组第 {track} 条轨没有分片")]
    EmptyTrack { group: usize, track: usize },
    #[error("第 {group} 组第 {track} 条轨的缺片下标须升序、在 1 到分片数减 1 之间")]
    InvalidGaps { group: usize, track: usize },
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
    #[error("打开第 {group} 组第 {track} 条轨失败：{cause}")]
    OpenInput {
        group: usize,
        track: usize,
        cause: FfmpegError,
    },
    #[error("第 {track} 条轨的{kind}流与前面的轨重复")]
    DuplicateKind { track: usize, kind: StreamKind },
    /// 打包音频要按包在输入里的字节位置找到所在的分片；FFmpeg 的裸音频解复用器都给出位置
    #[error(
        "读取第 {group} 组第 {track} 条轨（打包音频）时，包没有在输入里的字节位置，定不了属于哪个分片"
    )]
    UnplacedPacket { group: usize, track: usize },
    #[error("{0}")]
    Unsupported(Unsupported),
    #[error("创建输出 {path} 失败：{cause}")]
    OpenOutput { path: PathBuf, cause: FfmpegError },
    #[error("MP4 封装器不接受选项 {0:?}")]
    RejectedOptions(Vec<(String, String)>),
    #[error("读取第 {group} 组第 {track} 条轨失败：{cause}")]
    Read {
        group: usize,
        track: usize,
        cause: FfmpegError,
    },
    #[error("写入 MP4 失败：{0}")]
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
    /// 读分片、写输出或回读输出时文件出错（写输出时含 FFmpeg 写文件的错误，如盘满）
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

/// 内容放不进 MP4：编码不受支持或参数探测不出；后续组与第 0 组的流种类、编码参数或解码配置不同，组内分片格式、PID
/// 或编码参数变了，或组内解码时间戳不递增（同一条 MP4 轨的内容须前后一致、时间戳严格递增）；打包音频没有时间戳而要与
/// 其他轨对齐；或各轨都没有要取的流。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    #[error(
        "第 {group} 组第 {track} 条轨的{kind}编码 {codec} 放不进 MP4（{}）",
        supported_codecs()
    )]
    Codec {
        group: usize,
        track: usize,
        kind: StreamKind,
        codec: &'static str,
    },
    /// MP3 的采样率低于 16 kHz（MPEG-2.5 的 8、11.025、12 kHz）：MP4 封装器不收
    #[error(
        "第 {group} 组第 {track} 条轨的 MP3 采样率 {sample_rate} Hz 低于 {} Hz，放不进 MP4",
        MP3_MIN_SAMPLE_RATE
    )]
    Mp3SampleRate {
        group: usize,
        track: usize,
        /// Hz
        sample_rate: u32,
    },
    /// 选中的流探测不出编码或编码参数（视频的宽高、音频的采样率与声道数），或与第 0 组比较时没有确定的不同、而有项
    /// 比不了（视频的像素格式只有一边探测出，fMP4 里配置不同的 AAC 有一边没解出帧）：这一组的第一个分片里没有这路流
    /// 能解的数据（打开输入时探测读完第一个分片）
    #[error(
        "第 {group} 组第 {track} 条轨的{kind}流探测不出编码参数或解码配置（这一组的第一个分片里没有能解的数据），判断不了能否放进 MP4"
    )]
    ParamsUnknown {
        group: usize,
        track: usize,
        kind: StreamKind,
    },
    #[error("第 {group} 组第 {track} 条轨的流种类与第 0 组不同，无法放进同一个 MP4")]
    LayoutChanged { group: usize, track: usize },
    #[error(
        "第 {group} 组第 {track} 条轨的{kind}参数 {found:?} 与第 0 组 {first:?} 不同，无法放进同一条 MP4 轨"
    )]
    ParamsChanged {
        group: usize,
        track: usize,
        kind: StreamKind,
        first: Shape,
        found: Shape,
    },
    /// 编码参数相同而解码配置与第 0 组不同：fMP4 的为 init 段里的（如 H.264 的参数集），TS 与打包音频里的音频为帧头
    /// （如 AAC 的 profile 与采样率下标，LC 与 HE-AAC 的采样率都可以是 48 kHz）；或一组是 fMP4 而另一组是 TS 或打包
    /// 音频。MP4 的样本描述由第 0 组的写成，用在后面的组上解不出
    #[error(
        "第 {group} 组第 {track} 条轨的{kind}解码配置（如 AAC 的 profile、H.264 的参数集，或分片封装）与第 0 组不同，无法放进同一条 MP4 轨"
    )]
    ConfigChanged {
        group: usize,
        track: usize,
        kind: StreamKind,
    },
    /// TS 的一条轨在组内换了要取的那一路的 PID：PMT 更新后原来那路不再列出、换成了另一路（RFC 8216 要求这时标
    /// EXT-X-DISCONTINUITY）。不报的话新 PID 的包都不进输出
    #[error(
        "第 {group} 组第 {track} 条轨的{kind}流在组内换了 PID（PMT 更新后原来那路不再列出），无法放进同一条 MP4 轨"
    )]
    StreamChangedInGroup {
        group: usize,
        track: usize,
        kind: StreamKind,
    },
    /// 参数随帧携带的分片（TS 与打包音频）里，这路流在组内中途换了编码或编码参数（采样率、声道、画面宽高等）：MP4
    /// 的一条轨只有一个样本描述
    #[error("第 {group} 组第 {track} 条轨的{kind}编码参数在组内变了，无法放进同一条 MP4 轨")]
    ParamsChangedInGroup {
        group: usize,
        track: usize,
        kind: StreamKind,
    },
    /// 没有 init 段的一条轨在组内换了分片格式（TS 与打包音频，或打包音频的格式之间）：读取时只认开头那一种，其余的
    /// 分片被当作杂字节跳过
    #[error(
        "第 {group} 组第 {track} 条轨的分片格式在组内变了（TS 与打包音频，或打包音频的格式之间），无法放进同一条 MP4 轨"
    )]
    FormatChangedInGroup { group: usize, track: usize },
    /// TS 或打包音频里的 E-AC-3 每帧不足 6 块：MP4 封装器把几帧并成一个 6 块（1536 个采样点）的样本，末尾凑不满的
    /// 帧不写出
    #[error("第 {group} 组第 {track} 条轨的 E-AC-3 每帧不足 6 块，放不进 MP4")]
    Eac3ShortFrames { group: usize, track: usize },
    /// TS 或打包音频里的 E-AC-3 有多个独立子流：MP4 封装器不支持
    #[error("第 {group} 组第 {track} 条轨的 E-AC-3 有多个独立子流，放不进 MP4")]
    Eac3Substreams { group: usize, track: usize },
    /// 往回跳多为来源在时间戳重新开始处漏标了 `EXT-X-DISCONTINUITY`，重复多为相邻分片首尾重复了一帧
    #[error(
        "第 {group} 组第 {track} 条轨的{kind}解码时间戳不递增（往回跳或重复），无法放进同一条 MP4 轨"
    )]
    DtsNotIncreasing {
        group: usize,
        track: usize,
        kind: StreamKind,
    },
    /// 一路流进到后面的分片（中间没有缺片）时时间戳往前跳，输出里会留下比一个最长的声明时长（见
    /// [`crate::Segment::duration_us`]）还长的空档：多为来源在时间戳重新开始处漏标了 `EXT-X-DISCONTINUITY`
    #[error(
        "第 {group} 组第 {track} 条轨的{kind}时间戳在第 {segment} 个分片开头往前跳，输出里会留下 {jump_us} 微秒的空档，无法放进同一条 MP4 轨"
    )]
    TimestampJump {
        group: usize,
        track: usize,
        kind: StreamKind,
        /// 跳变后的第一个分片在这一组这条轨里的下标
        segment: usize,
        /// 空档的长度，微秒：与这路流上一个包的 DTS 之差，减去其间整片都没有这路流的包的那几片声明的时长
        jump_us: u64,
    },
    /// 这一组里这条轨的视频没有一个能开始解码的帧（关键帧；H.264 另认整帧都是 I slice 的帧）：各帧参考的帧都不在这一组
    /// 里，写进去解不出画面
    #[error(
        "第 {group} 组第 {track} 条轨的视频没有能开始解码的帧（关键帧或整帧 I 帧），无法放进 MP4"
    )]
    NoKeyframe { group: usize, track: usize },
    /// 第 `track` 条轨是打包音频，这一组有分片没有 ID3 时间戳（RFC 8216 3.4 要求每片都有），而这一组还有别的轨：
    /// 不知道它与别的轨怎样对齐。只有它一条轨时按样本数从头接续
    #[error(
        "第 {group} 组第 {track} 条轨是打包音频，这一组有分片没有 ID3 时间戳，无法与其他轨对齐"
    )]
    UntimedPackedAudio { group: usize, track: usize },
    #[error("各轨都没有要取的视频或音频流，写不出 MP4")]
    NoStreams,
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
