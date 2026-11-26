//! 转封装：把 HLS 各条轨的分片原样复制（不重编码）进一个 MP4 或 Matroska 文件（[`remux`]），字幕轨由 WebVTT 生成；或把
//! 几个完整文件（如分开下载的视频与音频）合进一个（[`merge_files`]）。以下先说 HLS 的合并，完整文件的不同之处见
//! [`merge_files`]。
//!
//! 输入按不连续段组（两个 `EXT-X-DISCONTINUITY` 之间）组织，每组里每条轨一份分片列表，fMP4 另带 init 段。
//! - 组内：一条轨的分片按字节顺序当作一个连续的输入读取，同一时刻只打开一个文件，不先拼成大文件；打包音频（RFC
//!   8216 3.4，没有 init 段、不以 TS 的同步字节开头）只读各片里的音频帧，按开头 ID3 标签里的时间戳定时。TS 与打包音频的
//!   时间戳在 33 位的 90 kHz 时钟上，各输入的起点跨在回绕点附近两侧时按回绕对齐。
//! - 组间：整组使用同一个时间偏移，保留组内各轨（如视频与独立音频 rendition）原有的相对时序；
//!   各组首尾相接，下一组从上一组所有流的最晚结束时刻之后开始。
//! - 每条轨按调用方指定的 [`Streams`] 贡献第一路视频和（或）第一路音频，没有这类流的轨不贡献（各轨合起来至少
//!   一路），未指定种类的流不进输出、不检查编码；视频只能来自一条轨，音频可以来自几条轨、各成一路；字幕轨见
//!   [`Streams::Subtitles`] 与 [`remux`]；编码参数须探测得出；后续组的流布局、编码
//!   参数与解码配置必须与第一组一致，参数随帧携带的（TS 与打包音频）在组内也不能变，分片格式与 PID 也不能换；每路流
//!   的解码时间戳必须严格递增（组内不能往回跳或重复），相邻分片之间也不能往前跳得比声明的时长多出一截。TS 里损坏的
//!   音频包、打包音频里的损坏处跳过并计数（见 [`StreamReport::discarded_corrupt_bytes`]）；每组开头解不出的视频包
//!   不写出并计数（见 [`StreamReport::skipped_before_keyframe`]）。
//! - 只接受输出容器能放（见 [`Container::holds`]）、FFmpeg 构建启用了解复用与解析的编码，TS 与裸音频（含打包音频）
//!   里只收其中能逐帧读出参数的（报 [`Unsupported::Codec`] 时列出）；MP4 封装器另有的限制（MP3 的采样率、E-AC-3 的帧
//!   结构）见 [`Unsupported`] 的各项。
//!
//! 音频的输入时间基比一个采样点粗（如 Matroska 与部分 fMP4 的 1 毫秒）、输出时间基更细时，帧长已知的编码（链接的
//! FFmpeg 里为 AAC）按采样点接续，每个样本的时长都是整帧（同 ffmpeg 命令行复制音频时的做法），HLS 与合并文件都如此。
//!
//! 输出写完回读核对每路流的包数（mov_text 不数封装器自己补的清屏空样本）并落盘。[`remux`] 失败或取消（见 [`Cancel`]）时输出上可能留有写了一半的文件，输出
//! 要么完整、要么不存在由调用方写到临时路径、成功后改名、失败时删除来保证；[`merge_files`] 自己写临时文件、成功后
//! 换上。错误信息已包含原因，不经 `source()` 重复给出。
//!
//! 依照链接的 FFmpeg 的这些内部行为（各处注释写作「链接的 FFmpeg …」）：movenc 的 mov_init（MP3 最低采样率）与
//! handle_eac3（E-AC-3 的帧结构），mov 解复用器的 mov_read_tfhd、mov_read_trun（fMP4 样本的位置），mp3dec 的
//! mp3_parse_vbr_tags（Info/Xing 帧），ADTS、MPEG 音频、AC-3 的帧头解析与 adts_aac_probe、ff_mpa_bitrate_tab，各解复用器
//! 的名称与裸 AC-3 解复用器的时间基，封装器对 AVFMT_TS_NONSTRICT 的 DTS 检查。

mod chain;
mod codecs;
mod compare;
mod error;
mod ffi;
mod files;
pub mod fmp4;
mod frame;
mod group;
mod hls;
mod inband;
mod input;
mod nal;
mod packed;
mod source;
pub mod standalone;
mod subtitles;
mod timing;
mod write;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ffmpeg::media;
use ffmpeg_next as ffmpeg;

pub use crate::codecs::{Codec, Container};
pub use crate::error::{Error, FfmpegError, Origin, Unsupported};
pub use crate::files::{FileInput, merge_files};
pub use crate::hls::{check, remux};

/// 一个不连续段组里某条轨的分片，按播放顺序；fMP4 时 `init` 是该组使用的 init 段。fMP4 的 init 段与各分片须通过
/// [`fmp4::check`]、最后一个 box 的长度不写 0（见 [`fmp4::seal`]），合并时再核对一遍（见 [`Error::Fmp4`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSegments {
    pub init: Option<PathBuf>,
    pub segments: Vec<Segment>,
    /// 与前一个分片之间缺了分片（来源里有、没取到）的分片下标，升序，在 1 到分片数减 1 之间。打包音频片内损坏之后的
    /// 帧要按下一片的时间戳定时，下一片之前缺了分片时定不了（见 [`StreamReport::discarded_corrupt_bytes`]）
    pub gaps: Vec<usize>,
}

/// 一个分片：文件与播放列表为它声明的时长。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub path: PathBuf,
    /// EXTINF 声明的时长，微秒；相邻分片之间的时间戳推进按它核对（见 [`Unsupported::TimestampJump`]）
    pub duration_us: u64,
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
    Subtitle,
}

impl StreamKind {
    pub(crate) fn medium(self) -> media::Type {
        match self {
            StreamKind::Video => media::Type::Video,
            StreamKind::Audio => media::Type::Audio,
            StreamKind::Subtitle => media::Type::Subtitle,
        }
    }
}

impl fmt::Display for StreamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StreamKind::Video => "视频",
            StreamKind::Audio => "音频",
            StreamKind::Subtitle => "字幕",
        })
    }
}

/// 一条轨（或合并文件时一个输入）在输出中贡献哪些种类的流，各组相同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Streams {
    /// 第一路视频与第一路音频
    All,
    Video,
    Audio,
    /// 整条轨是 WebVTT 字幕分片（HLS 的 SUBTITLES rendition），不经 FFmpeg 读；只用于 [`remux`]
    Subtitles,
}

impl Streams {
    /// 要不要这条轨里 FFmpeg 读出的 `kind` 类流（字幕轨不经 FFmpeg，什么也不要）。
    pub(crate) fn wants(self, kind: StreamKind) -> bool {
        match self {
            Streams::All => kind != StreamKind::Subtitle,
            Streams::Video => kind == StreamKind::Video,
            Streams::Audio => kind == StreamKind::Audio,
            Streams::Subtitles => false,
        }
    }
}

/// 合并 HLS 时的一条轨：贡献哪些流，与写进输出各路流的标注。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub streams: Streams,
    pub label: Label,
}

/// 写进输出流的标注（MP4 的 mdhd 语言、udta 名称与 tkhd 启用标志，Matroska 的 Language、Name、FlagDefault 与
/// FlagForced）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Label {
    /// BCP 47 语言标签（如 `en`、`zh-Hans`），写进输出时换成 ISO 639-2（MP4 用术语代码、Matroska 用书目代码），认不出的
    /// 写 `und`；None 也写 `und`
    pub language: Option<String>,
    /// 轨道名称；None 而有语言时写语言标签原文（ISO 639-2 分不开简繁等，名称里留着）
    pub name: Option<String>,
    /// 播放器默认选它。各类流里都没有标的时，第一路视频与第一路音频为默认，字幕都不默认
    pub default: bool,
    /// 强制显示的字幕（只含画面里外语对白等）
    pub forced: bool,
}

/// [`remux`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    pub report: MediaReport,
    /// 各字幕轨（[`Streams::Subtitles`]，按轨号）换到输出时间线上的 cue，与封进输出的字幕（若封）对齐，用于另存为
    /// SRT / WebVTT 文件
    pub subtitles: Vec<Vec<hs_m3u8_subtitle::Cue>>,
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
    /// 本库由 WebVTT 生成的字幕流
    Subtitle {
        /// FFmpeg 的编码名：Matroska 里为 `subrip`，MP4 里为 `mov_text`
        codec: &'static str,
    },
}

impl Shape {
    /// 探测出了参数：视频有宽高，音频有采样率与声道数。
    pub(crate) fn determined(&self) -> bool {
        match *self {
            Shape::Video { width, height, .. } => width > 0 && height > 0,
            Shape::Audio {
                sample_rate,
                channels,
                ..
            } => sample_rate > 0 && channels > 0,
            Shape::Subtitle { .. } => true,
        }
    }

    pub fn kind(&self) -> StreamKind {
        match self {
            Shape::Video { .. } => StreamKind::Video,
            Shape::Audio { .. } => StreamKind::Audio,
            Shape::Subtitle { .. } => StreamKind::Subtitle,
        }
    }

    pub fn codec(&self) -> &'static str {
        match self {
            Shape::Video { codec, .. } | Shape::Audio { codec, .. } | Shape::Subtitle { codec } => {
                codec
            }
        }
    }
}

/// 一路输出流的合并结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamReport {
    pub shape: Shape,
    /// 写入、且回读核对一致的包数
    pub packets: u64,
    /// 输入中没有 DTS、因而未写入的包数。流开头缺 DTS 而带 PTS 与时长的包（Matroska 里有 B 帧时的开头几帧）按随后
    /// 第一个 DTS 倒推补上，不算在内
    pub skipped_without_dts: u64,
    /// 各组开头解不出、因而未写入的视频包数：第一个关键帧（随机访问点）之前的包，与参考上一个 GOP 的前导帧（HEVC 为
    /// RASL；其余编码为解码在关键帧之后、呈现在它之前的帧）。音频为 0
    pub skipped_before_keyframe: u64,
    /// 损坏而没有写入的音频字节数：TS 里读不出帧头或帧长与包长对不上的包（来源丢了 TS 包，或缺片处 PES 不完整，拼出
    /// 的包），打包音频里切帧时跳过的不完整的帧或标签、认不出的字节，以及片内损坏之后定不了时刻（这一片是最后一片，
    /// 或下一片之前缺了分片）的帧。视频包照常写入，由解码器做错误隐藏
    pub discarded_corrupt_bytes: u64,
    /// 输出时间线上从最早的呈现时刻到最晚的结束时刻；含组内时间戳间断（如缺失的分片）留下的空档
    pub duration: Duration,
}

/// 合并结果，`streams` 按输出流下标排列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaReport {
    pub streams: Vec<StreamReport>,
}

/// 调用方的取消信号：返回真之后，合并在下一处检查以 [`Error::Cancelled`] 结束。读分片、写出每个包、FFmpeg 每次读写
/// 输出文件（含 moov 前置时搬动数据的第二遍）与回读核对每个包时检查；最后的落盘不中断。
#[derive(Clone)]
pub struct Cancel(Arc<dyn Fn() -> bool + Send + Sync>);

impl Cancel {
    /// `cancelled` 会在 FFmpeg 的读写回调里被调用，要快、不阻塞，不得 panic（在 FFmpeg 的回调里 panic 会中止进程）；
    /// 一旦返回真须一直为真：各处检查到的取消与读写回调因取消而失败，都按取消报告。
    pub fn new(cancelled: impl Fn() -> bool + Send + Sync + 'static) -> Cancel {
        Cancel(Arc::new(cancelled))
    }

    /// 不会取消。
    pub fn never() -> Cancel {
        Cancel::new(|| false)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        (self.0)()
    }
}

impl fmt::Debug for Cancel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Cancel")
    }
}

impl Default for Cancel {
    /// 不会取消。
    fn default() -> Cancel {
        Cancel::never()
    }
}

/// 合并的进度，以新写完的量调用（只报大于 0 的），回读核对输出不计。在写包的线程上调用，要快、不阻塞。默认不报。
/// - [`remux`]：单位为文件。一组里一条轨的输入文件（init 段或分片；打包音频按分片算）写完若干个时报；各组各轨写完时
///   合计为它们的 init 段与分片数。按写出的包所在的分片推进：写到第 n 个分片的包时，它之前的分片与 init 段即算写完；
///   一组写完时补齐（含不取流的轨）。
/// - [`merge_files`]：单位为字节。按写出的包在所在文件里的字节位置推进；各文件写完时补齐，合计为各文件打开时的字节数。
#[derive(Clone, Default)]
pub struct WriteProgress(Option<Arc<dyn Fn(u64) + Send + Sync>>);

impl WriteProgress {
    pub fn new(written: impl Fn(u64) + Send + Sync + 'static) -> WriteProgress {
        WriteProgress(Some(Arc::new(written)))
    }

    pub(crate) fn add(&self, amount: u64) {
        if let (Some(written), 1..) = (&self.0, amount) {
            written(amount);
        }
    }
}

impl fmt::Debug for WriteProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WriteProgress")
    }
}

/// 调用方对合并的控制：取消与进度。默认不取消、不报进度。
#[derive(Clone, Debug, Default)]
pub struct Control {
    pub cancel: Cancel,
    pub progress: WriteProgress,
}

/// 落盘。Windows 上落盘要求写权限，所以以可写方式打开。
pub(crate) fn sync(path: &Path) -> Result<(), Error> {
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

pub(crate) fn init() -> Result<(), Error> {
    static INIT: OnceLock<Result<(), FfmpegError>> = OnceLock::new();
    INIT.get_or_init(|| {
        ffmpeg::init()?;
        // FFmpeg 的日志不输出：本库的失败经返回值上抛，带着出在哪一组哪条轨；FFmpeg 的消息里有的只是探测时的过程信息
        // （如解码器在参数集出现之前的报错，或判断不了的情形），写到进程的标准错误里，调用方既控制不了、也用不上
        ffmpeg::log::set_level(ffmpeg::log::Level::Quiet);
        Ok(())
    })
    .clone()
    .map_err(Error::Init)
}
