//! 转封装：把 HLS 各条轨的分片原样复制（不重编码）进一个 MP4。
//!
//! 输入按不连续段组（两个 `EXT-X-DISCONTINUITY` 之间）组织，每组里每条轨一份分片列表，fMP4 另带 init 段。
//! - 组内：一条轨的分片按字节顺序当作一个连续的输入读取，同一时刻只打开一个文件，不先拼成大文件；打包音频（RFC
//!   8216 3.4，没有 init 段、不以 TS 的同步字节开头）只读各片里的音频帧，按开头 ID3 标签里的时间戳定时。TS 与打包音频的
//!   时间戳在 33 位的 90 kHz 时钟上，各输入的起点跨在回绕点附近两侧时按回绕对齐。
//! - 组间：整组使用同一个时间偏移，保留组内各轨（如视频与独立音频 rendition）原有的相对时序；
//!   各组首尾相接，下一组从上一组所有流的最晚结束时刻之后开始。
//! - 每条轨按调用方指定的 [`Streams`] 贡献第一路视频和（或）第一路音频，没有这类流的轨不贡献（各轨合起来至少
//!   一路），未指定种类的流不进输出、不检查编码；同一类流只能来自一条轨；后续组的流布局与编码参数必须与第一组
//!   一致，参数随帧携带的（TS 与打包音频）在组内也不能变；每路流的解码时间戳必须严格递增（组内不能往回跳或
//!   重复）。
//! - 只接受 H.264、HEVC、AV1、VP9 视频与 AAC、AC-3、E-AC-3、MP3、FLAC、Opus、ALAC 音频，与 FFmpeg 构建启用的组件
//!   一致；TS 与打包音频里只收其中能逐帧读出参数的 H.264、HEVC、AAC、AC-3、E-AC-3、MP3。MP3 的采样率须不低于
//!   16 kHz，TS 与打包音频里的 E-AC-3 每帧须为 6 块、只有一个独立子流（MP4 封装器的限制）。
//!
//! 输出写完回读核对每路流的包数并落盘；失败时输出上可能留有写了一半的文件。输出要么完整、要么不存在由调用方
//! 写到临时路径、成功后改名、失败时删除来保证。错误信息已包含原因，不经 `source()` 重复给出。

mod chain;
mod ffi;
mod frame;
mod inband;
mod input;
mod packed;
pub mod standalone;

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ffmpeg::{Dictionary, Packet, Rational, Rescale, Rounding, codec, encoder, format, media};
use ffmpeg_next as ffmpeg;

use crate::chain::ffmpeg_path;
use crate::inband::{Inband, Mismatch};
use crate::input::Input;

/// 时间戳在组间换算时使用的公共时间基（微秒）。
const MICROS: Rational = Rational(1, 1_000_000);

/// 一个不连续段组里某条轨的分片，按播放顺序；fMP4 时 `init` 是该组使用的 init 段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSegments {
    pub init: Option<PathBuf>,
    pub segments: Vec<PathBuf>,
}

/// 一个不连续段组；`tracks` 的顺序在各组之间一致，下标即轨道号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscontinuityGroup {
    pub tracks: Vec<TrackSegments>,
}

/// 输出流的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    Audio,
}

impl StreamKind {
    fn medium(self) -> media::Type {
        match self {
            StreamKind::Video => media::Type::Video,
            StreamKind::Audio => media::Type::Audio,
        }
    }
}

impl fmt::Display for StreamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StreamKind::Video => "视频",
            StreamKind::Audio => "音频",
        })
    }
}

/// 一条轨在输出中贡献哪些种类的流，各组相同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Streams {
    All,
    Video,
    Audio,
}

impl Streams {
    fn wants(self, kind: StreamKind) -> bool {
        match self {
            Streams::All => true,
            Streams::Video => kind == StreamKind::Video,
            Streams::Audio => kind == StreamKind::Audio,
        }
    }
}

/// 一路流的编码参数；放进同一条 MP4 轨的内容必须前后一致，不同不连续段组的同一条轨之间按此比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Video {
        /// FFmpeg 的编码名，如 `h264`、`hevc`
        codec: &'static str,
        width: u32,
        height: u32,
        /// FFmpeg 的像素格式名（色度采样与位深），如 `yuv420p`、`yuv420p10le`；探测不出时为 None
        pixel_format: Option<&'static str>,
    },
    Audio {
        /// FFmpeg 的编码名，如 `aac`、`ac3`、`eac3`、`mp3`
        codec: &'static str,
        /// Hz
        sample_rate: u32,
        channels: u32,
    },
}

impl Shape {
    pub fn kind(&self) -> StreamKind {
        match self {
            Shape::Video { .. } => StreamKind::Video,
            Shape::Audio { .. } => StreamKind::Audio,
        }
    }

    pub fn codec(&self) -> &'static str {
        match self {
            Shape::Video { codec, .. } | Shape::Audio { codec, .. } => codec,
        }
    }
}

/// 一路输出流的合并结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamReport {
    pub shape: Shape,
    /// 写入、且回读核对一致的包数
    pub packets: u64,
    /// 输入中没有 DTS、因而未写入的包数
    pub skipped_without_dts: u64,
    /// TS 里损坏而没有写入的音频包数：解复用器标为损坏（来源丢了 TS 包，或 PES 不完整，如直播缺片处）或读不出帧头
    /// 的。损坏的视频包照常写入，由解码器做错误隐藏（丢掉一帧视频，后面直到下一个关键帧都解不出）
    pub discarded_corrupt: u64,
    /// 输出时间线上从最早的呈现时刻到最晚的结束时刻，微秒；含组内时间戳间断（如缺失的分片）留下的空档
    pub duration_us: u64,
}

/// 合并结果，`streams` 按输出流下标排列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub streams: Vec<StreamReport>,
}

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
    #[error("第 {group} 组读不出任何视频或音频包")]
    EmptyGroup { group: usize },
    #[error("输出路径无法交给 FFmpeg（不是有效的 UTF-8 或含 NUL 字符）：{}", .0.display())]
    NonUtf8Path(PathBuf),
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
    #[error("{action} {path} 失败：{cause}")]
    Io {
        action: &'static str,
        path: PathBuf,
        cause: io::Error,
    },
}

/// 内容放不进 MP4：编码不受支持；后续组与第 0 组的流种类、编码参数不同，组内分片格式或编码参数变了，或组内解码
/// 时间戳不递增（同一条 MP4 轨的内容须前后一致、时间戳严格递增）；打包音频没有时间戳而要与其他轨对齐；或各轨都没有
/// 要取的流。
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
    /// 打包音频的第 `segment` 个分片（下标）在字节 `offset` 处读不出音频帧：既不是音频帧也不是 ID3 标签、帧或标签
    /// 不完整，或是自由格式（帧头不给码率）的 MP3
    #[error(
        "第 {group} 组第 {track} 条轨（打包音频）第 {segment} 个分片在字节 {offset} 处读不出音频帧（不是音频帧或 ID3 标签、帧不完整，或是自由格式的 MP3）"
    )]
    UnreadablePackedAudio {
        group: usize,
        track: usize,
        segment: usize,
        offset: u64,
    },
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
    /// 第 `track` 条轨是打包音频，这一组有分片没有 ID3 时间戳（RFC 8216 3.4 要求每片都有），而这一组还有别的轨：
    /// 不知道它与别的轨怎样对齐。只有它一条轨时按样本数从头接续
    #[error(
        "第 {group} 组第 {track} 条轨是打包音频，这一组有分片没有 ID3 时间戳，无法与其他轨对齐"
    )]
    UntimedPackedAudio { group: usize, track: usize },
    #[error("各轨都没有要取的视频或音频流，写不出 MP4")]
    NoStreams,
}

/// 把各不连续段组的分片复制进 `output`（MP4，moov 前置），写完回读核对每路流的包数并落盘。失败时 `output` 上
/// 可能留有写了一半的文件，由调用方删除；已存在的 `output` 会被替换。`streams[i]` 为第 i 条轨贡献的流种类。
pub fn remux(
    streams: &[Streams],
    groups: &[DiscontinuityGroup],
    output: &Path,
) -> Result<Report, Error> {
    init()?;
    validate(streams, groups)?;
    if ffmpeg_path(output).is_none() {
        return Err(Error::NonUtf8Path(output.to_path_buf()));
    }
    let report = write_verified(streams, groups, output)?;
    sync(output)?;
    Ok(report)
}

/// 只打开各组各轨的输入，核对能否合并进一个 MP4，不读包、不写输出：编码受支持，同类流不来自两条轨，各组的流种类与
/// 参数与第 0 组相同，打包音频有时间戳（与别的轨同组时）。这些都看 init 段与开头的帧，各轨只给开头的分片即可，
/// 用于下载完全部分片之前尽早发现放不进 MP4 的内容；通过不保证合并成功：组内中途换参数、时间戳不递增等要读到才知道。
pub fn check(streams: &[Streams], groups: &[DiscontinuityGroup]) -> Result<(), Error> {
    init()?;
    validate(streams, groups)?;
    let layout = layout_of(&open_group(streams, groups, 0, None)?)?;
    for group in 1..groups.len() {
        open_group(streams, groups, group, Some(&layout))?;
    }
    Ok(())
}

/// 落盘。Windows 上落盘要求写权限，所以以可写方式打开。
fn sync(path: &Path) -> Result<(), Error> {
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|cause| Error::Io {
            action: "落盘",
            path: path.to_path_buf(),
            cause,
        })
}

fn init() -> Result<(), Error> {
    static INIT: OnceLock<Result<(), FfmpegError>> = OnceLock::new();
    INIT.get_or_init(|| {
        ffmpeg::init()?;
        // 封装过程中的 info/warning（如 faststart 第二遍）不输出；错误经返回值上抛
        ffmpeg::log::set_level(ffmpeg::log::Level::Error);
        Ok(())
    })
    .clone()
    .map_err(Error::Init)
}

fn validate(streams: &[Streams], groups: &[DiscontinuityGroup]) -> Result<(), Error> {
    let first = groups.first().ok_or(Error::NoGroups)?;
    if streams.len() != first.tracks.len() {
        return Err(Error::StreamsCount {
            expected: first.tracks.len(),
            found: streams.len(),
        });
    }
    for (group, g) in groups.iter().enumerate() {
        if g.tracks.len() != first.tracks.len() {
            return Err(Error::TrackCount {
                group,
                expected: first.tracks.len(),
                found: g.tracks.len(),
            });
        }
        for (track, t) in g.tracks.iter().enumerate() {
            if t.segments.is_empty() {
                return Err(Error::EmptyTrack { group, track });
            }
        }
    }
    Ok(())
}

/// 一条轨在某组中被选中的一路流。
struct Selected {
    index: usize,
    time_base: Rational,
    id: codec::Id,
    desc: Desc,
}

/// 一路流的编码参数与解码配置；MP4 的样本描述由第 0 组的写成，其余组须相同。
#[derive(Clone, PartialEq, Eq)]
struct Desc {
    shape: Shape,
    config: Config,
}

/// 一路流的解码配置。
#[derive(Clone, PartialEq, Eq)]
enum Config {
    /// 有 init 段（fMP4）：init 段里的解码配置（codecpar 的 extradata，如 avcC、esds 里的 AudioSpecificConfig），样本里
    /// 不带，按字节比较
    Init(Vec<u8>),
    /// 参数随帧携带（TS 与打包音频）：音频以第 0 组第一个包的帧头为准逐包核对（见 [`Inband`]）；视频的参数集在关键帧
    /// 前重复、解码器按带内的解码，只核对编码参数（宽高与像素格式，见 [`Shape`]）
    Inband,
}

/// 打开一条轨在一组里的分片，按 `wanted` 选出第一路视频与（或）第一路音频，并检查编码是否受支持。
fn open_selected(
    segments: &TrackSegments,
    wanted: Streams,
    group: usize,
    track: usize,
) -> Result<(Input, Vec<Selected>), Error> {
    let input = Input::open(segments, group, track)?;
    let mut selected = Vec::new();
    for kind in [StreamKind::Video, StreamKind::Audio] {
        if !wanted.wants(kind) {
            continue;
        }
        let Some(stream) = input
            .context()
            .streams()
            .find(|s| s.parameters().medium() == kind.medium())
        else {
            continue;
        };
        let id = stream.parameters().id();
        let shape = ffi::shape(&stream, kind);
        check_codec(id, &shape, input.standalone(), group, track).map_err(Error::Unsupported)?;
        let config = if input.standalone() {
            Config::Inband
        } else {
            Config::Init(ffi::extradata(&stream))
        };
        selected.push(Selected {
            index: stream.index(),
            time_base: stream.time_base(),
            id,
            desc: Desc { shape, config },
        });
    }
    Ok((input, selected))
}

/// MP4 能放的编码：FFmpeg 的编码 ID、流种类与报错时用的名称。参数随帧携带的输入（TS 与打包音频）要逐包核对参数
/// （MP4 的一条轨只有一个样本描述），只收其中能逐帧读出参数的（见 [`Inband::of`]）。
const MP4_CODECS: [(codec::Id, StreamKind, &str); 11] = [
    (codec::Id::H264, StreamKind::Video, "H.264"),
    (codec::Id::HEVC, StreamKind::Video, "HEVC"),
    (codec::Id::AV1, StreamKind::Video, "AV1"),
    (codec::Id::VP9, StreamKind::Video, "VP9"),
    (codec::Id::AAC, StreamKind::Audio, "AAC"),
    (codec::Id::AC3, StreamKind::Audio, "AC-3"),
    (codec::Id::EAC3, StreamKind::Audio, "E-AC-3"),
    (codec::Id::MP3, StreamKind::Audio, "MP3"),
    (codec::Id::FLAC, StreamKind::Audio, "FLAC"),
    (codec::Id::OPUS, StreamKind::Audio, "Opus"),
    (codec::Id::ALAC, StreamKind::Audio, "ALAC"),
];

/// MP4 封装器收的 MP3 最低采样率，Hz（FFmpeg 9.0.2 movenc 的 mov_init）。
const MP3_MIN_SAMPLE_RATE: u32 = 16_000;

/// 编码 `id` 能否放进 MP4；`standalone` 为输入没有 init 段（TS 与打包音频）。
fn accepts(id: codec::Id, standalone: bool) -> bool {
    MP4_CODECS.iter().any(|&(c, ..)| c == id) && (!standalone || Inband::of(id).is_some())
}

/// [`Unsupported::Codec`] 报错时列出的支持的编码。
fn supported_codecs() -> String {
    let list = |standalone: bool| {
        let names = |kind: StreamKind| {
            MP4_CODECS
                .iter()
                .filter(|&&(id, k, _)| k == kind && accepts(id, standalone))
                .map(|&(.., name)| name)
                .collect::<Vec<_>>()
                .join("、")
        };
        format!(
            "{} 视频与 {} 音频",
            names(StreamKind::Video),
            names(StreamKind::Audio)
        )
    };
    format!(
        "fMP4 里支持 {}，TS 与打包音频里支持 {}",
        list(false),
        list(true)
    )
}

/// 编码能否放进 MP4；`standalone` 为输入没有 init 段。
fn check_codec(
    id: codec::Id,
    shape: &Shape,
    standalone: bool,
    group: usize,
    track: usize,
) -> Result<(), Unsupported> {
    if !accepts(id, standalone) {
        return Err(Unsupported::Codec {
            group,
            track,
            kind: shape.kind(),
            codec: id.name(),
        });
    }
    match *shape {
        Shape::Audio { sample_rate, .. }
            if id == codec::Id::MP3 && sample_rate < MP3_MIN_SAMPLE_RATE =>
        {
            Err(Unsupported::Mp3SampleRate {
                group,
                track,
                sample_rate,
            })
        }
        _ => Ok(()),
    }
}

/// 一路输入流对应的输出流。
#[derive(Clone, Copy)]
struct Mapping {
    output: usize,
    kind: StreamKind,
    /// 输入流的时间基
    time_base: Rational,
}

/// 一路输入流读到而没有写出的包数。
#[derive(Clone, Copy, Default)]
struct Dropped {
    /// 没有 DTS
    without_dts: u64,
    /// 解复用器标为损坏的音频包
    corrupt: u64,
}

struct Source {
    group: usize,
    track: usize,
    input: Input,
    /// 按输入流下标；None 表示该流不进输出
    map: Vec<Option<Mapping>>,
    /// 按输入流下标；进输出、参数随帧携带的流为 Some
    inband: Vec<Option<Inband>>,
    /// 按输入流下标
    dropped: Vec<Dropped>,
    /// 已读出、待写入的包及其映射；包一定带 DTS
    queue: VecDeque<(Packet, Mapping)>,
    finished: bool,
    /// 按回绕对齐时加到这个输入所有时间戳上的量，微秒（见 [`align_wraps`]）
    wrap_us: i64,
}

impl Source {
    /// `outputs[i]` 为 `selected[i]` 对应的输出流下标；`references` 见 [`references_of`]，第 0 组为空。
    fn new(
        group: usize,
        track: usize,
        input: Input,
        selected: &[Selected],
        outputs: &[usize],
        references: &[Option<Inband>],
    ) -> Self {
        let streams = input.context().nb_streams() as usize;
        let mut map = vec![None; streams];
        let mut inband = vec![None; streams];
        for (s, &output) in selected.iter().zip(outputs) {
            map[s.index] = Some(Mapping {
                output,
                kind: s.desc.shape.kind(),
                time_base: s.time_base,
            });
            if input.standalone() {
                inband[s.index] = Some(match references.get(output).cloned().flatten() {
                    Some(reference) => reference.for_next_group(),
                    None => Inband::of(s.id).expect("check_codec 只收能逐帧读出参数的编码"),
                });
            }
        }
        Source {
            group,
            track,
            input,
            map,
            inband,
            dropped: vec![Dropped::default(); streams],
            queue: VecDeque::new(),
            finished: false,
            wrap_us: 0,
        }
    }

    /// 读下一个要写出的包。跳过未映射流的包；损坏（解复用器标出或读不出帧头）的音频包与没有 DTS 的包记入
    /// `dropped`；参数随帧携带的包核对参数。读到末尾返回 `None`。
    fn read_next(&mut self) -> Result<Option<(Packet, Mapping)>, Error> {
        loop {
            let mut packet = Packet::empty();
            if !self.input.read(&mut packet, self.group, self.track)? {
                return Ok(None);
            }
            // TS 可能在文件中途出现新流，其下标超出建立映射时的流数
            let Some(mapping) = self.map.get(packet.stream()).copied().flatten() else {
                continue;
            };
            let dropped = &mut self.dropped[packet.stream()];
            if packet.is_corrupt() && mapping.kind == StreamKind::Audio {
                dropped.corrupt += 1;
                continue;
            }
            if packet.dts().is_none() {
                dropped.without_dts += 1;
                continue;
            }
            if let Some(check) = self.inband[packet.stream()].as_mut() {
                let stream = self
                    .input
                    .context()
                    .stream(packet.stream())
                    .expect("包所属的流存在");
                let (group, track) = (self.group, self.track);
                let unsupported = match check.check(packet.data().unwrap_or_default(), &stream) {
                    Ok(()) => None,
                    Err(Mismatch::Unreadable) => {
                        self.dropped[packet.stream()].corrupt += 1;
                        continue;
                    }
                    Err(Mismatch::Changed) => Some(Unsupported::ParamsChangedInGroup {
                        group,
                        track,
                        kind: mapping.kind,
                    }),
                    Err(Mismatch::Config) => Some(Unsupported::ConfigChanged {
                        group,
                        track,
                        kind: mapping.kind,
                    }),
                    Err(Mismatch::Eac3ShortFrames) => {
                        Some(Unsupported::Eac3ShortFrames { group, track })
                    }
                    Err(Mismatch::Eac3Substreams) => {
                        Some(Unsupported::Eac3Substreams { group, track })
                    }
                };
                if let Some(unsupported) = unsupported {
                    return Err(Error::Unsupported(unsupported));
                }
            }
            return Ok(Some((packet, mapping)));
        }
    }

    /// 读到每路映射流都至少有一个包排队（或读到末尾），用于确定本组各流的首个 DTS 与最早的 PTS。
    fn prime(&mut self) -> Result<(), Error> {
        let mapped: Vec<usize> = self.map.iter().flatten().map(|m| m.output).collect();
        while !self.finished
            && !mapped
                .iter()
                .all(|&out| self.queue.iter().any(|(_, m)| m.output == out))
        {
            match self.read_next()? {
                Some(item) => self.queue.push_back(item),
                None => self.finished = true,
            }
        }
        Ok(())
    }

    /// 队首的包对齐回绕后的 DTS，微秒。
    fn front_dts_us(&self) -> Option<i64> {
        let (packet, mapping) = self.queue.front()?;
        let dts = packet.dts().expect("队列中的包都带 DTS");
        Some(dts.rescale(mapping.time_base, MICROS) + self.wrap_us)
    }
}

/// 一路输出流：第 0 组决定其编码参数，写出过程中累计统计。`_us` 结尾的时刻为输出时间线上的微秒。
struct OutStream {
    shape: Shape,
    /// 写头之后由封装器确定
    time_base: Rational,
    written: u64,
    dropped: Dropped,
    start_us: Option<i64>,
    end_us: Option<i64>,
    last_dts_us: Option<i64>,
    /// 写出的上一个包的 DTS，输出时间基
    last_dts: Option<i64>,
}

fn write_verified(
    streams: &[Streams],
    groups: &[DiscontinuityGroup],
    part: &Path,
) -> Result<Report, Error> {
    let first = open_group(streams, groups, 0, None)?;
    let layout = layout_of(&first)?;
    let mut octx = format::output_as(part, "mp4").map_err(|cause| Error::OpenOutput {
        path: part.to_path_buf(),
        cause: cause.into(),
    })?;
    add_streams(&mut octx, &first)?;
    write_header(&mut octx)?;
    let mut outs: Vec<OutStream> = layout
        .iter()
        .flatten()
        .enumerate()
        .map(|(out, desc)| OutStream {
            shape: desc.shape,
            time_base: octx.stream(out).expect("输出流已创建").time_base(),
            written: 0,
            dropped: Dropped::default(),
            start_us: None,
            end_us: None,
            last_dts_us: None,
            last_dts: None,
        })
        .collect();
    // 组间余量：最粗的输出时间基的一个 tick（向上取整到微秒）。换到输出时间基时的舍入不超过半个 tick，
    // 因此下一组的首个 DTS 一定大于上一组的末个 DTS
    let margin_us = outs
        .iter()
        .map(|o| 1i64.rescale_with(o.time_base, MICROS, Rounding::Up))
        .max()
        .expect("第 0 组至少有一路输出流");

    let sources = primed(0, first, &layout, &[])?;
    let references = references_of(&sources, outs.len());
    write_group(0, sources, &mut octx, &mut outs, margin_us)?;
    for group in 1..groups.len() {
        let opened = open_group(streams, groups, group, Some(&layout))?;
        let sources = primed(group, opened, &layout, &references)?;
        write_group(group, sources, &mut octx, &mut outs, margin_us)?;
    }
    octx.write_trailer().map_err(|e| Error::Mux(e.into()))?;
    drop(octx);

    verify(part, &outs)?;
    Ok(Report {
        streams: outs
            .into_iter()
            .map(|o| StreamReport {
                shape: o.shape,
                packets: o.written,
                skipped_without_dts: o.dropped.without_dts,
                discarded_corrupt: o.dropped.corrupt,
                duration_us: match (o.start_us, o.end_us) {
                    (Some(start), Some(end)) => u64::try_from(end - start).unwrap_or(0),
                    _ => 0,
                },
            })
            .collect(),
    })
}

/// 一条轨在一组里打开的输入与选中的流。
struct OpenTrack {
    track: usize,
    input: Input,
    selected: Vec<Selected>,
}

/// 打开第 `group` 组的各轨并选流（见 [`open_selected`]）。`layout` 为第 0 组各轨选中的流（见 [`layout_of`]），其余组
/// 的流种类、参数与解码配置须与它相同；打开第 0 组时为 None。
fn open_group(
    streams: &[Streams],
    groups: &[DiscontinuityGroup],
    group: usize,
    layout: Option<&[Vec<Desc>]>,
) -> Result<Vec<OpenTrack>, Error> {
    let mut opened = Vec::new();
    for (track, segments) in groups[group].tracks.iter().enumerate() {
        let (input, selected) = open_selected(segments, streams[track], group, track)?;
        if let Some(layout) = layout {
            same_streams(&selected, &layout[track], group, track)?;
        }
        opened.push(OpenTrack {
            track,
            input,
            selected,
        });
    }
    check_timed(group, &opened)?;
    Ok(opened)
}

/// 一条轨这一组选中的流与第 0 组的 `first` 种类、参数、解码配置都相同。
fn same_streams(
    selected: &[Selected],
    first: &[Desc],
    group: usize,
    track: usize,
) -> Result<(), Error> {
    if selected.len() != first.len()
        || selected
            .iter()
            .zip(first)
            .any(|(s, f)| s.desc.shape.kind() != f.shape.kind())
    {
        return Err(Error::Unsupported(Unsupported::LayoutChanged {
            group,
            track,
        }));
    }
    for (s, f) in selected.iter().zip(first) {
        let kind = f.shape.kind();
        if s.desc.shape != f.shape {
            return Err(Error::Unsupported(Unsupported::ParamsChanged {
                group,
                track,
                kind,
                first: f.shape,
                found: s.desc.shape,
            }));
        }
        if s.desc.config != f.config {
            return Err(Error::Unsupported(Unsupported::ConfigChanged {
                group,
                track,
                kind,
            }));
        }
    }
    Ok(())
}

/// 一组里有打包音频的轨没有时间戳时，只有它一条轨贡献流才能定时。
fn check_timed(group: usize, opened: &[OpenTrack]) -> Result<(), Error> {
    let contributing: Vec<&OpenTrack> = opened.iter().filter(|t| !t.selected.is_empty()).collect();
    match contributing.iter().find(|t| t.input.untimed()) {
        Some(untimed) if contributing.len() > 1 => {
            Err(Error::Unsupported(Unsupported::UntimedPackedAudio {
                group,
                track: untimed.track,
            }))
        }
        _ => Ok(()),
    }
}

/// 第 0 组各轨选中的流，下标为轨道号；依次排开即各路输出流。同一类流只能来自一条轨，各轨合起来至少一路。
fn layout_of(first: &[OpenTrack]) -> Result<Vec<Vec<Desc>>, Error> {
    let mut layout: Vec<Vec<Desc>> = Vec::new();
    for t in first {
        let duplicate = t.selected.iter().find(|s| {
            layout
                .iter()
                .flatten()
                .any(|d| d.shape.kind() == s.desc.shape.kind())
        });
        if let Some(s) = duplicate {
            return Err(Error::DuplicateKind {
                track: t.track,
                kind: s.desc.shape.kind(),
            });
        }
        layout.push(t.selected.iter().map(|s| s.desc.clone()).collect());
    }
    if layout.iter().all(Vec::is_empty) {
        return Err(Error::Unsupported(Unsupported::NoStreams));
    }
    Ok(layout)
}

/// 按第 0 组建立输出流，创建顺序即输出流下标（见 [`layout_of`]）。
fn add_streams(octx: &mut format::context::Output, first: &[OpenTrack]) -> Result<(), Error> {
    for t in first {
        for s in &t.selected {
            let stream = t
                .input
                .context()
                .stream(s.index)
                .expect("open_selected 返回的下标有效");
            let mut ost = octx
                .add_stream(encoder::find(codec::Id::None))
                .map_err(|e| Error::Mux(e.into()))?;
            ost.set_parameters(stream.parameters());
            ffi::set_codec_tag(&mut ost, s.id);
        }
    }
    Ok(())
}

/// 一组打开的各轨换成读包的输入，各自读到每路流的第一个包（见 [`Source::prime`]）：各轨选中的流按 `layout` 依次
/// 对应输出流；没有要取的流的轨不贡献，不读它的包。`references` 见 [`references_of`]，第 0 组为空。
fn primed(
    group: usize,
    opened: Vec<OpenTrack>,
    layout: &[Vec<Desc>],
    references: &[Option<Inband>],
) -> Result<Vec<Source>, Error> {
    let mut next = 0;
    let mut sources = Vec::new();
    for t in opened {
        let outputs: Vec<usize> = (next..next + t.selected.len()).collect();
        next += layout[t.track].len();
        if !t.selected.is_empty() {
            let mut source =
                Source::new(group, t.track, t.input, &t.selected, &outputs, references);
            source.prime()?;
            sources.push(source);
        }
    }
    Ok(sources)
}

/// 第 0 组读到第一个包之后各路输出流的逐包核对，下标为输出流；其余组的核对从它开始（见 [`Inband::for_next_group`]），
/// 第一个包就与第 0 组的比较。
fn references_of(sources: &[Source], outputs: usize) -> Vec<Option<Inband>> {
    let mut references = vec![None; outputs];
    for source in sources {
        for (mapping, inband) in source.map.iter().zip(&source.inband) {
            if let (Some(mapping), Some(inband)) = (mapping, inband) {
                references[mapping.output] = Some(inband.clone());
            }
        }
    }
    references
}

/// 写 MP4 头（moov 前置）；封装器不认识的选项视为错误。
fn write_header(octx: &mut format::context::Output) -> Result<(), Error> {
    let mut options = Dictionary::new();
    options.set("movflags", "+faststart");
    let rejected: Vec<(String, String)> = octx
        .write_header_with(options)
        .map_err(|e| Error::Mux(e.into()))?
        .iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    if rejected.is_empty() {
        Ok(())
    } else {
        Err(Error::RejectedOptions(rejected))
    }
}

/// 写出一组（各输入已读到每路流的第一个包）：各输入按回绕对齐（见 [`align_wraps`]），再按全组共用的偏移平移时间戳，
/// 按 DTS 交错写出。
fn write_group(
    group: usize,
    mut sources: Vec<Source>,
    octx: &mut format::context::Output,
    outs: &mut [OutStream],
    margin_us: i64,
) -> Result<(), Error> {
    align_wraps(&mut sources);
    // 本组各输出流的首个 DTS 与全组最早的 PTS（输入时间线，微秒）；队列内同一路流按解码顺序排列
    let mut first_dts_us: Vec<Option<i64>> = vec![None; outs.len()];
    let mut min_pts_us: Option<i64> = None;
    for source in &sources {
        for (packet, mapping) in &source.queue {
            let tb = mapping.time_base;
            let us = |ts: i64| ts.rescale(tb, MICROS) + source.wrap_us;
            let dts = us(packet.dts().expect("队列中的包都带 DTS"));
            let pts = packet.pts().map_or(dts, us);
            first_dts_us[mapping.output].get_or_insert(dts);
            min_pts_us = Some(min_pts_us.map_or(pts, |m| m.min(pts)));
        }
    }
    let min_pts_us = min_pts_us.ok_or(Error::EmptyGroup { group })?;
    let ends_us: Vec<Option<i64>> = outs.iter().map(|o| o.end_us).collect();
    let last_dts_us: Vec<Option<i64>> = outs.iter().map(|o| o.last_dts_us).collect();
    let offset_us = group_offset(&ends_us, &last_dts_us, &first_dts_us, min_pts_us, margin_us);

    loop {
        for source in &mut sources {
            if source.queue.is_empty() && !source.finished {
                match source.read_next()? {
                    Some(item) => source.queue.push_back(item),
                    None => source.finished = true,
                }
            }
        }
        // 同组各输入共用一个偏移，按对齐回绕后的 DTS 交错写出
        let next = sources
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.front_dts_us().map(|dts| (i, dts)))
            .min_by_key(|&(_, dts)| dts);
        let Some((i, _)) = next else { break };

        let source = &mut sources[i];
        let (mut packet, mapping) = source.queue.pop_front().expect("next 只指向有待写包的输入");
        let out = mapping.output;
        let o = &mut outs[out];
        let t = retime(
            &mut packet,
            mapping.time_base,
            o.time_base,
            offset_us + source.wrap_us,
        );
        // 每路流的 DTS 在输出时间基下须严格递增（同一条 MP4 轨不能有两个样本同时解码）。FFmpeg 的封装器遇到不递增的
        // 只报「参数不合法」，上一个为 0 时还不查，这里一律拦下；组间的偏移保证跨组递增，不递增只会出在组内
        let dts = packet.dts().expect("retime 设了 DTS");
        if o.last_dts.is_some_and(|last| dts <= last) {
            return Err(Error::Unsupported(Unsupported::DtsNotIncreasing {
                group,
                track: source.track,
                kind: o.shape.kind(),
            }));
        }
        o.last_dts = Some(dts);
        o.start_us = Some(o.start_us.map_or(t.pts_us, |s| s.min(t.pts_us)));
        o.end_us = Some(o.end_us.map_or(t.end_us, |e| e.max(t.end_us)));
        o.last_dts_us = Some(t.dts_us);
        packet.set_position(-1);
        packet.set_stream(out);
        packet
            .write_interleaved(octx)
            .map_err(|e| Error::Mux(e.into()))?;
        o.written += 1;
    }
    for source in &sources {
        for (mapping, dropped) in source.map.iter().zip(&source.dropped) {
            if let Some(mapping) = mapping {
                let total = &mut outs[mapping.output].dropped;
                total.without_dts += dropped.without_dts;
                total.corrupt += dropped.corrupt;
            }
        }
    }
    Ok(())
}

/// 同一组的 TS 与打包音频在同一个 33 位的 90 kHz 时钟上，但各输入按自己的第一个时间戳展开回绕（FFmpeg 读每个 TS
/// 输入、本库读打包音频都如此），起点跨在回绕点附近两侧的两个输入会差 2^33（约 26.5 小时）。以第一个读出包的这类
/// 输入为准，其余这类输入的第一个 DTS 离它超过半个回绕周期时，整个输入平移整数个周期：同一组各轨的起点实际相距远
/// 小于半个周期。fMP4 的时间戳不回绕，不参与。
fn align_wraps(sources: &mut [Source]) {
    let firsts: Vec<Option<i64>> = sources
        .iter()
        .map(|s| s.front_dts_us().filter(|_| s.input.standalone()))
        .collect();
    let Some(&base) = firsts.iter().flatten().next() else {
        return;
    };
    for (source, first) in sources.iter_mut().zip(firsts) {
        if let Some(first) = first {
            source.wrap_us = wrap_shift_us(first - base);
        }
    }
}

/// 相距 `distance_us` 的两个时刻要相距不超过半个回绕周期，后者要加的整数个 33 位回绕周期（90 kHz），微秒。
fn wrap_shift_us(distance_us: i64) -> i64 {
    // 周期 2^33 / 90000 秒，即 2^33 × 100 / 9 微秒，按分数算：周期数四舍五入，平移量四舍五入到微秒
    let (num, den) = (i128::from(1i64 << 33) * 100, 9i128);
    let periods = (2 * i128::from(distance_us) * den + num).div_euclid(2 * num);
    let shift = (2 * periods * num + den).div_euclid(2 * den);
    i64::try_from(-shift).expect("i64 的时刻差不到 2^63 微秒，周期数有限")
}

/// 回读输出，核对每路流的包数与写入的一致。
fn verify(path: &Path, outs: &[OutStream]) -> Result<(), Error> {
    let found = count_packets(path)?;
    if found.len() != outs.len() {
        return Err(Error::VerifyStreams {
            expected: outs.len(),
            found: found.len(),
        });
    }
    for (stream, (o, &found)) in outs.iter().zip(&found).enumerate() {
        if o.written != found {
            return Err(Error::VerifyPackets {
                stream,
                written: o.written,
                found,
            });
        }
    }
    Ok(())
}

/// 本组的时间偏移（微秒），取两个下限中较大者：
/// - 呈现：本组最早的 PTS 不早于此前所有流的最晚结束时刻（第 0 组为 0）；
/// - 解码：每路流本组首个 DTS 严格大于该流上一组的末个 DTS，多加 `margin_us` 吸收换到输出时间基时的舍入；
///   此前没写过包的流，DTS 不小于 0。
///
/// 整组共用这一个偏移，组内各轨的相对时序保持原样；按呈现而非 DTS 对齐，避免 B 帧的解码提前量在每个组边界留下空隙。
fn group_offset(
    ends_us: &[Option<i64>],
    last_dts_us: &[Option<i64>],
    first_dts_us: &[Option<i64>],
    min_pts_us: i64,
    margin_us: i64,
) -> i64 {
    let presentation = ends_us.iter().flatten().max().copied().unwrap_or(0) - min_pts_us;
    let decode = last_dts_us
        .iter()
        .zip(first_dts_us)
        .filter_map(|(last, first)| match (last, first) {
            (Some(last), Some(first)) => Some(last - first + margin_us),
            (None, Some(first)) => Some(-first),
            (_, None) => None,
        })
        .max();
    decode.map_or(presentation, |d| presentation.max(d))
}

/// 平移后的包在输出时间线上的时刻，微秒。
struct Retimed {
    dts_us: i64,
    /// 没有 PTS 时取 DTS
    pts_us: i64,
    /// 呈现结束时刻：max(PTS, DTS) + 时长
    end_us: i64,
}

/// 把包的时间戳平移 `offset_us` 并换到输出时间基。先换成微秒再平移、最后一次换到输出时间基，只经过一次舍入。
fn retime(packet: &mut Packet, in_tb: Rational, out_tb: Rational, offset_us: i64) -> Retimed {
    let shift = |ts: i64| ts.rescale(in_tb, MICROS) + offset_us;
    let dts_us = shift(packet.dts().expect("写出的包都带 DTS"));
    let pts_us = packet.pts().map(shift);
    let duration_us = packet.duration().rescale(in_tb, MICROS);
    packet.set_dts(Some(dts_us.rescale(MICROS, out_tb)));
    packet.set_pts(pts_us.map(|us| us.rescale(MICROS, out_tb)));
    packet.set_duration(packet.duration().rescale(in_tb, out_tb));
    let pts_us = pts_us.unwrap_or(dts_us);
    Retimed {
        dts_us,
        pts_us,
        end_us: pts_us.max(dts_us) + duration_us,
    }
}

/// 回读文件，按流下标统计包数。
fn count_packets(path: &Path) -> Result<Vec<u64>, Error> {
    let reread = |cause: ffmpeg::Error| Error::Reread {
        path: path.to_path_buf(),
        cause: cause.into(),
    };
    let mut ictx = format::input(path).map_err(reread)?;
    let mut counts = vec![0u64; ictx.nb_streams() as usize];
    loop {
        let mut packet = Packet::empty();
        match packet.read(&mut ictx) {
            Ok(()) => counts[packet.stream()] += 1,
            Err(ffmpeg::Error::Eof) => return Ok(counts),
            Err(cause) => return Err(reread(cause)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_further_apart_than_half_a_wrap_are_shifted_by_whole_wraps() {
        // 回绕周期 2^33 / 90000 秒，约 95443717688.9 微秒
        let wrap = 95_443_717_689;
        assert_eq!(wrap_shift_us(0), 0);
        assert_eq!(wrap_shift_us(-75_000_000), 0);
        assert_eq!(wrap_shift_us(wrap / 2 - 1), 0);
        // 视频在回绕点前、音频在回绕点后：音频的起点读成了早约一个周期
        assert_eq!(wrap_shift_us(75_000_000 - wrap), wrap);
        assert_eq!(wrap_shift_us(wrap - 75_000_000), -wrap);
        assert_eq!(wrap_shift_us(2 * wrap), -2 * wrap);
    }
}
