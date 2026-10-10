//! 转封装：把 HLS 各条轨的分片原样复制（不重编码）进一个 MP4。
//!
//! 输入按不连续段组（两个 `EXT-X-DISCONTINUITY` 之间）组织，每组里每条轨一份分片列表，fMP4 另带 init 段。
//! - 组内：分片按字节顺序当作一个连续的输入读取，同一时刻只打开一个文件，不先拼成大文件。
//! - 组间：整组使用同一个时间偏移，保留组内各轨（如视频与独立音频 rendition）原有的相对时序；
//!   各组首尾相接，下一组从上一组所有流的最晚结束时刻之后开始。
//! - 每条轨按调用方指定的 [`Streams`] 贡献第一路视频和（或）第一路音频，没有这类流的轨不贡献（各轨合起来至少
//!   一路），未指定种类的流不进输出、不检查编码；同一类流只能来自一条轨；后续组的流布局与编码参数必须与第一组
//!   一致，组内每路流的解码时间戳不能往回跳。
//! - 只接受 H.264、HEVC 视频与 AAC 音频，与 FFmpeg 构建启用的组件一致。
//!
//! 输出写完回读核对每路流的包数并落盘；失败时输出上可能留有写了一半的文件。输出要么完整、要么不存在由调用方
//! 写到临时路径、成功后改名、失败时删除来保证。错误信息已包含原因，不经 `source()` 重复给出。

mod chain;
mod ffi;

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ffmpeg::format::context::StreamIo;
use ffmpeg::{Dictionary, Packet, Rational, Rescale, Rounding, codec, encoder, format, media};
use ffmpeg_next as ffmpeg;

use crate::chain::{SegmentChain, ffmpeg_path};

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

    fn accepts(self, id: codec::Id) -> bool {
        match self {
            StreamKind::Video => matches!(id, codec::Id::H264 | codec::Id::HEVC),
            StreamKind::Audio => id == codec::Id::AAC,
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
    },
    Audio {
        /// FFmpeg 的编码名，如 `aac`
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

/// 内容放不进 MP4：编码不受支持；后续组与第 0 组的流种类、编码参数不同，或组内解码时间戳往回跳（同一条 MP4 轨的
/// 内容须前后一致、时间戳递增）；或各轨都没有要取的流。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    #[error(
        "第 {group} 组第 {track} 条轨的{kind}编码 {codec} 放不进 MP4（只支持 H.264、HEVC、AAC）"
    )]
    Codec {
        group: usize,
        track: usize,
        kind: StreamKind,
        codec: &'static str,
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
    /// 往回跳多为来源在时间戳重新开始处漏标了 `EXT-X-DISCONTINUITY`，重复多为相邻分片首尾重复了一帧
    #[error(
        "第 {group} 组第 {track} 条轨的{kind}解码时间戳不递增（往回跳或重复），无法放进同一条 MP4 轨"
    )]
    DtsNotIncreasing {
        group: usize,
        track: usize,
        kind: StreamKind,
    },
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

/// 打开一条轨在一组里的全部分片（init 段在前），当作一个连续的输入。返回输入与记录文件错误的位置。
fn open_track(
    segments: &TrackSegments,
    group: usize,
    track: usize,
) -> Result<(format::context::Input, chain::Failure), Error> {
    let paths: Vec<PathBuf> = segments
        .init
        .iter()
        .chain(&segments.segments)
        .cloned()
        .collect();
    let (chain, failure) = SegmentChain::new(paths).map_err(|(path, cause)| Error::Io {
        action: "读取",
        path,
        cause,
    })?;
    let opened = StreamIo::from_read_seek(chain)
        .and_then(|io| format::input_from_stream(io, None, None))
        .map_err(|cause| {
            read_failure(&failure).unwrap_or(Error::OpenInput {
                group,
                track,
                cause: cause.into(),
            })
        })?;
    Ok((opened, failure))
}

/// 分片链记下的文件错误；FFmpeg 只拿到错误码，这里换回带路径的原始错误。
fn read_failure(failure: &chain::Failure) -> Option<Error> {
    let (path, cause) = failure.lock().unwrap_or_else(|p| p.into_inner()).take()?;
    Some(Error::Io {
        action: "读取",
        path,
        cause,
    })
}

/// 一条轨在某组中被选中的一路流。
struct Selected {
    index: usize,
    time_base: Rational,
    id: codec::Id,
    shape: Shape,
}

/// 打开一条轨在一组里的分片，按 `wanted` 选出第一路视频与（或）第一路音频，并检查编码是否受支持。
fn open_selected(
    segments: &TrackSegments,
    wanted: Streams,
    group: usize,
    track: usize,
) -> Result<(format::context::Input, chain::Failure, Vec<Selected>), Error> {
    let (ictx, failure) = open_track(segments, group, track)?;
    let mut selected = Vec::new();
    for kind in [StreamKind::Video, StreamKind::Audio] {
        if !wanted.wants(kind) {
            continue;
        }
        let Some(stream) = ictx
            .streams()
            .find(|s| s.parameters().medium() == kind.medium())
        else {
            continue;
        };
        let id = stream.parameters().id();
        if !kind.accepts(id) {
            return Err(Error::Unsupported(Unsupported::Codec {
                group,
                track,
                kind,
                codec: id.name(),
            }));
        }
        selected.push(Selected {
            index: stream.index(),
            time_base: stream.time_base(),
            id,
            shape: ffi::shape(&stream, kind),
        });
    }
    Ok((ictx, failure, selected))
}

/// 一条轨在第 0 组确定的一路输出流。
struct TrackOutput {
    shape: Shape,
    out: usize,
}

/// 一路输入流对应的输出流。
#[derive(Clone, Copy)]
struct Mapping {
    output: usize,
    /// 输入流的时间基
    time_base: Rational,
}

struct Source {
    group: usize,
    track: usize,
    ictx: format::context::Input,
    /// 读分片文件出错时的路径与原因
    failure: chain::Failure,
    /// 按输入流下标；None 表示该流不进输出
    map: Vec<Option<Mapping>>,
    /// 已读出、待写入的包及其映射；包一定带 DTS
    queue: VecDeque<(Packet, Mapping)>,
    finished: bool,
}

impl Source {
    /// `outputs[i]` 为 `selected[i]` 对应的输出流下标。
    fn new(
        group: usize,
        track: usize,
        ictx: format::context::Input,
        failure: chain::Failure,
        selected: &[Selected],
        outputs: &[usize],
    ) -> Self {
        let mut map = vec![None; ictx.nb_streams() as usize];
        for (s, &output) in selected.iter().zip(outputs) {
            map[s.index] = Some(Mapping {
                output,
                time_base: s.time_base,
            });
        }
        Source {
            group,
            track,
            ictx,
            failure,
            map,
            queue: VecDeque::new(),
            finished: false,
        }
    }

    /// 读下一个要写出的包。跳过未映射流的包；没有 DTS 的包计入该输出流的 `skipped`。读到末尾返回 `None`。
    fn read_next(&mut self, outs: &mut [OutStream]) -> Result<Option<(Packet, Mapping)>, Error> {
        loop {
            let mut packet = Packet::empty();
            match packet.read(&mut self.ictx) {
                Ok(()) => {}
                Err(ffmpeg::Error::Eof) => return Ok(None),
                Err(cause) => {
                    return Err(read_failure(&self.failure).unwrap_or(Error::Read {
                        group: self.group,
                        track: self.track,
                        cause: cause.into(),
                    }));
                }
            }
            // TS 可能在文件中途出现新流，其下标超出建立映射时的流数
            let Some(mapping) = self.map.get(packet.stream()).copied().flatten() else {
                continue;
            };
            if packet.dts().is_none() {
                outs[mapping.output].skipped += 1;
                continue;
            }
            return Ok(Some((packet, mapping)));
        }
    }

    /// 读到每路映射流都至少有一个包排队（或读到末尾），用于确定本组各流的首个 DTS 与最早的 PTS。
    fn prime(&mut self, outs: &mut [OutStream]) -> Result<(), Error> {
        let mapped: Vec<usize> = self.map.iter().flatten().map(|m| m.output).collect();
        while !self.finished
            && !mapped
                .iter()
                .all(|&out| self.queue.iter().any(|(_, m)| m.output == out))
        {
            match self.read_next(outs)? {
                Some(item) => self.queue.push_back(item),
                None => self.finished = true,
            }
        }
        Ok(())
    }

    fn front_dts_us(&self) -> Option<i64> {
        let (packet, mapping) = self.queue.front()?;
        let dts = packet.dts().expect("队列中的包都带 DTS");
        Some(dts.rescale(mapping.time_base, MICROS))
    }
}

/// 一路输出流：第 0 组决定其编码参数，写出过程中累计统计。`_us` 结尾的时刻为输出时间线上的微秒。
struct OutStream {
    shape: Shape,
    /// 写头之后由封装器确定
    time_base: Rational,
    written: u64,
    skipped: u64,
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
    let mut octx = format::output_as(part, "mp4").map_err(|cause| Error::OpenOutput {
        path: part.to_path_buf(),
        cause: cause.into(),
    })?;
    let (layout, first_sources) = create_outputs(streams, &groups[0], &mut octx)?;
    write_header(&mut octx)?;
    let mut outs: Vec<OutStream> = layout
        .iter()
        .flatten()
        .map(|t| OutStream {
            shape: t.shape,
            time_base: octx.stream(t.out).expect("输出流已创建").time_base(),
            written: 0,
            skipped: 0,
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

    write_group(0, first_sources, &mut octx, &mut outs, margin_us)?;
    for group in 1..groups.len() {
        let sources = open_group(streams, groups, group, &layout)?;
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
                skipped_without_dts: o.skipped,
                duration_us: match (o.start_us, o.end_us) {
                    (Some(start), Some(end)) => u64::try_from(end - start).unwrap_or(0),
                    _ => 0,
                },
            })
            .collect(),
    })
}

/// 按第 0 组建立输出流（创建顺序即输出流下标），返回各轨的输出流与第 0 组已打开的输入。
fn create_outputs(
    streams: &[Streams],
    first: &DiscontinuityGroup,
    octx: &mut format::context::Output,
) -> Result<(Vec<Vec<TrackOutput>>, Vec<Source>), Error> {
    let mut layout: Vec<Vec<TrackOutput>> = Vec::new();
    let mut sources = Vec::new();
    for (track, segments) in first.tracks.iter().enumerate() {
        let (ictx, failure, selected) = open_selected(segments, streams[track], 0, track)?;
        let mut outputs = Vec::new();
        for s in &selected {
            let kind = s.shape.kind();
            if layout.iter().flatten().any(|o| o.shape.kind() == kind) {
                return Err(Error::DuplicateKind { track, kind });
            }
            let stream = ictx.stream(s.index).expect("open_selected 返回的下标有效");
            let mut ost = octx
                .add_stream(encoder::find(codec::Id::None))
                .map_err(|e| Error::Mux(e.into()))?;
            ost.set_parameters(stream.parameters());
            ffi::set_codec_tag(&mut ost, s.id);
            outputs.push(TrackOutput {
                shape: s.shape,
                out: ost.index(),
            });
        }
        // 没有要取的流的轨什么也不贡献，不读它的包
        if !outputs.is_empty() {
            let indices: Vec<usize> = outputs.iter().map(|o| o.out).collect();
            sources.push(Source::new(0, track, ictx, failure, &selected, &indices));
        }
        layout.push(outputs);
    }
    if layout.iter().all(Vec::is_empty) {
        return Err(Error::Unsupported(Unsupported::NoStreams));
    }
    Ok((layout, sources))
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

/// 打开第 `group` 组的全部轨，按 `layout` 建立流映射并检查与第 0 组一致。
fn open_group(
    streams: &[Streams],
    groups: &[DiscontinuityGroup],
    group: usize,
    layout: &[Vec<TrackOutput>],
) -> Result<Vec<Source>, Error> {
    let mut sources = Vec::new();
    for (track, segments) in groups[group].tracks.iter().enumerate() {
        let (ictx, failure, selected) = open_selected(segments, streams[track], group, track)?;
        let expected = &layout[track];
        if selected.len() != expected.len()
            || selected
                .iter()
                .zip(expected)
                .any(|(s, e)| s.shape.kind() != e.shape.kind())
        {
            return Err(Error::Unsupported(Unsupported::LayoutChanged {
                group,
                track,
            }));
        }
        for (s, e) in selected.iter().zip(expected) {
            if s.shape != e.shape {
                return Err(Error::Unsupported(Unsupported::ParamsChanged {
                    group,
                    track,
                    kind: s.shape.kind(),
                    first: e.shape,
                    found: s.shape,
                }));
            }
        }
        if !expected.is_empty() {
            let indices: Vec<usize> = expected.iter().map(|e| e.out).collect();
            sources.push(Source::new(
                group, track, ictx, failure, &selected, &indices,
            ));
        }
    }
    Ok(sources)
}

/// 写出一组：按全组共用的偏移平移时间戳，各输入按 DTS 交错写出。
fn write_group(
    group: usize,
    mut sources: Vec<Source>,
    octx: &mut format::context::Output,
    outs: &mut [OutStream],
    margin_us: i64,
) -> Result<(), Error> {
    for source in &mut sources {
        source.prime(outs)?;
    }
    // 本组各输出流的首个 DTS 与全组最早的 PTS（输入时间线，微秒）；队列内同一路流按解码顺序排列
    let mut first_dts_us: Vec<Option<i64>> = vec![None; outs.len()];
    let mut min_pts_us: Option<i64> = None;
    for source in &sources {
        for (packet, mapping) in &source.queue {
            let tb = mapping.time_base;
            let dts = packet
                .dts()
                .expect("队列中的包都带 DTS")
                .rescale(tb, MICROS);
            let pts = packet.pts().map_or(dts, |pts| pts.rescale(tb, MICROS));
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
                match source.read_next(outs)? {
                    Some(item) => source.queue.push_back(item),
                    None => source.finished = true,
                }
            }
        }
        // 同组各输入共用一个偏移，直接按原始 DTS 交错写出
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
        let t = retime(&mut packet, mapping.time_base, o.time_base, offset_us);
        // MP4 封装器要求每路流的 DTS 在输出时间基下严格递增，不满足时只报「参数不合法」；组间的偏移保证跨组递增，
        // 不递增只会出在组内
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
    Ok(())
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
