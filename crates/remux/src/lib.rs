//! 转封装：把 HLS 各条轨的分片原样复制（不重编码）进一个 MP4。
//!
//! 输入按不连续段组（两个 `EXT-X-DISCONTINUITY` 之间）组织，每组里每条轨一份分片列表，fMP4 另带 init 段。
//! - 组内：分片经 FFmpeg 的 concatf 协议按字节顺序读取，不先拼成大文件。
//! - 组间：整组使用同一个时间偏移，保留组内各轨（如视频与独立音频 rendition）原有的相对时序；
//!   各组首尾相接，下一组从上一组所有流的最晚结束时刻之后开始。
//! - 每条轨按调用方指定的 [`Streams`] 贡献第一路视频和（或）第一路音频，未指定种类的流不进输出、不检查编码；
//!   同一类流只能来自一条轨；后续组的流布局与编码参数必须与第一组一致。
//! - 只接受 H.264、HEVC 视频与 AAC 音频，与 FFmpeg 构建启用的组件一致。
//!
//! 输出先写 `<输出>.part`，写完回读核对每路流的包数后改名；任一步失败都删除临时文件并返回错误。
//! 已存在的输出文件会被替换。

mod ffi;

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ffmpeg::{Dictionary, Packet, Rational, Rescale, Rounding, codec, encoder, format, media};
use ffmpeg_next as ffmpeg;

pub use ffi::Shape;

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
pub struct Streams {
    pub video: bool,
    pub audio: bool,
}

impl Streams {
    pub const ALL: Streams = Streams {
        video: true,
        audio: true,
    };
    pub const VIDEO: Streams = Streams {
        video: true,
        audio: false,
    };
    pub const AUDIO: Streams = Streams {
        video: false,
        audio: true,
    };

    fn wants(self, kind: StreamKind) -> bool {
        match kind {
            StreamKind::Video => self.video,
            StreamKind::Audio => self.audio,
        }
    }
}

/// 一路输出流的合并结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamReport {
    pub kind: StreamKind,
    /// FFmpeg 的编码名，如 `h264`、`hevc`、`aac`
    pub codec: &'static str,
    /// 写入、且回读核对一致的包数
    pub packets: u64,
    /// 输入中没有 DTS、因而未写入的包数
    pub skipped_without_dts: u64,
}

/// 合并结果，`streams` 按输出流下标排列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub streams: Vec<StreamReport>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("FFmpeg 初始化失败: {0}")]
    Init(ffmpeg::Error),
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
    #[error("路径不是有效的 UTF-8：{0}")]
    NonUtf8Path(PathBuf),
    #[error("打开第 {group} 组第 {track} 条轨失败: {source}")]
    OpenInput {
        group: usize,
        track: usize,
        source: ffmpeg::Error,
    },
    #[error("第 {track} 条轨没有指定要取的视频或音频流")]
    NoStreams { track: usize },
    #[error("第 {track} 条轨的{kind}流与前面的轨重复")]
    DuplicateKind { track: usize, kind: StreamKind },
    #[error("第 {group} 组第 {track} 条轨的{kind}编码 {codec} 不受支持（只支持 H.264、HEVC、AAC）")]
    UnsupportedCodec {
        group: usize,
        track: usize,
        kind: StreamKind,
        codec: &'static str,
    },
    #[error("第 {group} 组第 {track} 条轨的流种类与第 0 组不同")]
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
    #[error("创建输出 {path} 失败: {source}")]
    OpenOutput {
        path: PathBuf,
        source: ffmpeg::Error,
    },
    #[error("MP4 封装器不接受选项 {0:?}")]
    RejectedOptions(Vec<(String, String)>),
    #[error("读取第 {group} 组第 {track} 条轨失败: {source}")]
    Read {
        group: usize,
        track: usize,
        source: ffmpeg::Error,
    },
    #[error("写入 MP4 失败: {0}")]
    Mux(ffmpeg::Error),
    #[error("回读输出 {path} 失败: {source}")]
    Reread {
        path: PathBuf,
        source: ffmpeg::Error,
    },
    #[error("输出回读到 {found} 路流，应为 {expected} 路")]
    VerifyStreams { expected: usize, found: usize },
    #[error("输出第 {stream} 路流回读到 {found} 个包，写入了 {written} 个")]
    VerifyPackets {
        stream: usize,
        written: u64,
        found: u64,
    },
    #[error("{action} {path} 失败: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("{cause}；清理临时文件 {path} 也失败: {source}")]
    Cleanup {
        cause: Box<Error>,
        path: PathBuf,
        source: io::Error,
    },
}

/// 把各不连续段组的分片复制进 `output`（MP4，moov 前置）。`tracks[i]` 为第 i 条轨贡献的流种类。
pub fn remux(
    tracks: &[Streams],
    groups: &[DiscontinuityGroup],
    output: &Path,
) -> Result<Report, Error> {
    init()?;
    validate(tracks, groups)?;
    let part = with_suffix(output, ".part");
    write_verified(tracks, groups, &part)
        .and_then(|report| {
            std::fs::rename(&part, output).map_err(|source| Error::Io {
                action: "重命名",
                path: part.clone(),
                source,
            })?;
            Ok(report)
        })
        .map_err(|cause| discard(&part, cause))
}

fn init() -> Result<(), Error> {
    static INIT: OnceLock<Result<(), ffmpeg::Error>> = OnceLock::new();
    let result = *INIT.get_or_init(|| {
        ffmpeg::init()?;
        // 封装过程中的 info/warning（如 faststart 第二遍）不输出；错误经返回值上抛
        ffmpeg::log::set_level(ffmpeg::log::Level::Error);
        Ok(())
    });
    result.map_err(Error::Init)
}

fn validate(tracks: &[Streams], groups: &[DiscontinuityGroup]) -> Result<(), Error> {
    let first = groups.first().ok_or(Error::NoGroups)?;
    if tracks.len() != first.tracks.len() {
        return Err(Error::StreamsCount {
            expected: first.tracks.len(),
            found: tracks.len(),
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

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// 删除失败任务留下的临时文件；删除本身失败时把两个错误一并返回。
fn discard(path: &Path, cause: Error) -> Error {
    match std::fs::remove_file(path) {
        Ok(()) => cause,
        Err(e) if e.kind() == io::ErrorKind::NotFound => cause,
        Err(source) => Error::Cleanup {
            cause: Box::new(cause),
            path: path.to_path_buf(),
            source,
        },
    }
}

/// concatf 列表中的一行：单引号包裹的 `file:` URL。FFmpeg 按 av_get_token 解析，引号外的 `\` 是转义符，
/// 引号内原样保留，因此 Windows 路径的反斜杠必须在引号内；路径自身的单引号写成 `'\''`。
fn concatf_line(path: &Path) -> Result<String, Error> {
    let absolute = std::path::absolute(path).map_err(|source| Error::Io {
        action: "解析绝对路径",
        path: path.to_path_buf(),
        source,
    })?;
    let text = absolute
        .to_str()
        .ok_or_else(|| Error::NonUtf8Path(absolute.clone()))?;
    Ok(format!("'file:{}'\n", text.replace('\'', r"'\''")))
}

/// 用 concatf 打开一条轨在一组里的全部分片（init 段在前）。列表文件写在 `list` 处，打开后即删除。
fn open_track(
    segments: &TrackSegments,
    list: &Path,
    group: usize,
    track: usize,
) -> Result<format::context::Input, Error> {
    let mut content = String::new();
    for path in segments.init.iter().chain(&segments.segments) {
        content.push_str(&concatf_line(path)?);
    }
    std::fs::write(list, content).map_err(|source| Error::Io {
        action: "写入",
        path: list.to_path_buf(),
        source,
    })?;
    let list_url = list
        .to_str()
        .ok_or_else(|| Error::NonUtf8Path(list.to_path_buf()))?;
    let opened = format::input(&format!("concatf:{list_url}")).map_err(|source| Error::OpenInput {
        group,
        track,
        source,
    });
    match (opened, std::fs::remove_file(list)) {
        (Ok(ictx), Ok(())) => Ok(ictx),
        (Ok(_), Err(source)) => Err(Error::Io {
            action: "删除",
            path: list.to_path_buf(),
            source,
        }),
        (Err(cause), _) => Err(discard(list, cause)),
    }
}

/// 一条轨在某组中被选中的一路流。
struct Selected {
    kind: StreamKind,
    index: usize,
    time_base: Rational,
    id: codec::Id,
    shape: Shape,
}

/// 按 `wanted` 选出输入里第一路视频与（或）第一路音频，并检查编码是否受支持。
fn select_streams(
    ictx: &format::context::Input,
    wanted: Streams,
    group: usize,
    track: usize,
) -> Result<Vec<Selected>, Error> {
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
            return Err(Error::UnsupportedCodec {
                group,
                track,
                kind,
                codec: id.name(),
            });
        }
        selected.push(Selected {
            kind,
            index: stream.index(),
            time_base: stream.time_base(),
            id,
            shape: ffi::shape(&stream, kind),
        });
    }
    Ok(selected)
}

/// 一条轨在第 0 组确定的输出流。
struct TrackOutput {
    kind: StreamKind,
    out: usize,
    shape: Shape,
}

struct Source {
    group: usize,
    track: usize,
    ictx: format::context::Input,
    /// 输入流下标 → 输出流下标；None 表示该流不进输出
    map: Vec<Option<usize>>,
    time_bases: Vec<Rational>,
    /// 已读出、待写入的包及其输出流下标；包一定带 DTS
    queue: VecDeque<(Packet, usize)>,
    finished: bool,
}

impl Source {
    fn new(
        group: usize,
        track: usize,
        ictx: format::context::Input,
        selected: &[(usize, Rational, usize)],
    ) -> Self {
        let n = ictx.nb_streams() as usize;
        let mut map = vec![None; n];
        let mut time_bases = vec![Rational(0, 1); n];
        for &(index, time_base, out) in selected {
            map[index] = Some(out);
            time_bases[index] = time_base;
        }
        Source {
            group,
            track,
            ictx,
            map,
            time_bases,
            queue: VecDeque::new(),
            finished: false,
        }
    }

    /// 读下一个要写出的包。跳过未映射流的包；没有 DTS 的包计入 `skipped`。读到末尾返回 `None`。
    fn read_next(&mut self, skipped: &mut [u64]) -> Result<Option<(Packet, usize)>, Error> {
        loop {
            let mut packet = Packet::empty();
            match packet.read(&mut self.ictx) {
                Ok(()) => {}
                Err(ffmpeg::Error::Eof) => return Ok(None),
                Err(source) => {
                    return Err(Error::Read {
                        group: self.group,
                        track: self.track,
                        source,
                    });
                }
            }
            // TS 可能在文件中途出现新流，其下标超出建立映射时的流数
            let Some(out) = self.map.get(packet.stream()).copied().flatten() else {
                continue;
            };
            if packet.dts().is_none() {
                skipped[out] += 1;
                continue;
            }
            return Ok(Some((packet, out)));
        }
    }

    /// 读到每路映射流都至少有一个包排队（或读到末尾），用于确定本组各流的首个 DTS 与最早的 PTS。
    fn prime(&mut self, skipped: &mut [u64]) -> Result<(), Error> {
        let mapped: Vec<usize> = self.map.iter().flatten().copied().collect();
        while !self.finished
            && !mapped
                .iter()
                .all(|out| self.queue.iter().any(|(_, o)| o == out))
        {
            match self.read_next(skipped)? {
                Some(item) => self.queue.push_back(item),
                None => self.finished = true,
            }
        }
        Ok(())
    }

    fn front_dts_us(&self) -> Option<i64> {
        let (packet, _) = self.queue.front()?;
        let dts = packet.dts().expect("队列中的包都带 DTS");
        Some(dts.rescale(self.time_bases[packet.stream()], MICROS))
    }
}

/// 打开第 `group` 组的全部轨，按 `outputs` 建立流映射并检查与第 0 组一致。
fn open_group(
    tracks: &[Streams],
    groups: &[DiscontinuityGroup],
    group: usize,
    outputs: &[Vec<TrackOutput>],
    list: &Path,
) -> Result<Vec<Source>, Error> {
    let mut sources = Vec::new();
    for (track, segments) in groups[group].tracks.iter().enumerate() {
        let ictx = open_track(segments, list, group, track)?;
        let selected = select_streams(&ictx, tracks[track], group, track)?;
        let expected = &outputs[track];
        if selected.len() != expected.len()
            || selected.iter().zip(expected).any(|(s, e)| s.kind != e.kind)
        {
            return Err(Error::LayoutChanged { group, track });
        }
        let mut mapping = Vec::new();
        for (s, e) in selected.iter().zip(expected) {
            if s.shape != e.shape {
                return Err(Error::ParamsChanged {
                    group,
                    track,
                    kind: s.kind,
                    first: e.shape,
                    found: s.shape,
                });
            }
            mapping.push((s.index, s.time_base, e.out));
        }
        sources.push(Source::new(group, track, ictx, &mapping));
    }
    Ok(sources)
}

fn write_verified(
    tracks: &[Streams],
    groups: &[DiscontinuityGroup],
    part: &Path,
) -> Result<Report, Error> {
    let list = with_suffix(part, ".list");
    let mut octx = format::output_as(part, "mp4").map_err(|source| Error::OpenOutput {
        path: part.to_path_buf(),
        source,
    })?;

    // 第 0 组决定输出流：下标即输出流下标
    let mut kinds: Vec<StreamKind> = Vec::new();
    let mut codecs: Vec<&'static str> = Vec::new();
    let mut outputs: Vec<Vec<TrackOutput>> = Vec::new();
    let mut first_sources = Vec::new();
    for (track, segments) in groups[0].tracks.iter().enumerate() {
        let ictx = open_track(segments, &list, 0, track)?;
        let selected = select_streams(&ictx, tracks[track], 0, track)?;
        if selected.is_empty() {
            return Err(Error::NoStreams { track });
        }
        let mut track_outputs = Vec::new();
        let mut mapping = Vec::new();
        for s in &selected {
            if kinds.contains(&s.kind) {
                return Err(Error::DuplicateKind {
                    track,
                    kind: s.kind,
                });
            }
            let stream = ictx.stream(s.index).expect("select_streams 返回的下标有效");
            let mut ost = octx
                .add_stream(encoder::find(codec::Id::None))
                .map_err(Error::Mux)?;
            ost.set_parameters(stream.parameters());
            ffi::set_codec_tag(&mut ost, s.id);
            track_outputs.push(TrackOutput {
                kind: s.kind,
                out: ost.index(),
                shape: s.shape,
            });
            mapping.push((s.index, s.time_base, ost.index()));
            kinds.push(s.kind);
            codecs.push(s.id.name());
        }
        outputs.push(track_outputs);
        first_sources.push(Source::new(0, track, ictx, &mapping));
    }

    let mut options = Dictionary::new();
    options.set("movflags", "+faststart");
    let rejected: Vec<(String, String)> = octx
        .write_header_with(options)
        .map_err(Error::Mux)?
        .iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    if !rejected.is_empty() {
        return Err(Error::RejectedOptions(rejected));
    }

    let out_time_bases: Vec<Rational> = (0..kinds.len())
        .map(|i| octx.stream(i).expect("输出流已创建").time_base())
        .collect();
    // 组间余量：最粗的输出时间基的一个 tick（向上取整到微秒）。换到输出时间基时的舍入不超过半个 tick，
    // 因此下一组的首个 DTS 一定大于上一组的末个 DTS
    let margin_us = out_time_bases
        .iter()
        .map(|&tb| 1i64.rescale_with(tb, MICROS, Rounding::Up))
        .max()
        .expect("第 0 组至少有一路输出流");

    let mut written = vec![0u64; kinds.len()];
    let mut skipped = vec![0u64; kinds.len()];
    // 各输出流已写内容的呈现结束时刻与末个 DTS（输出时间线，微秒）
    let mut ends_us: Vec<Option<i64>> = vec![None; kinds.len()];
    let mut last_dts_us: Vec<Option<i64>> = vec![None; kinds.len()];

    let mut sources = Some(first_sources);
    for group in 0..groups.len() {
        let mut group_sources = match sources.take() {
            Some(s) => s,
            None => open_group(tracks, groups, group, &outputs, &list)?,
        };
        for source in &mut group_sources {
            source.prime(&mut skipped)?;
        }
        // 本组各输出流的首个 DTS 与全组最早的 PTS（输入时间线，微秒）；队列内同一路流按解码顺序排列
        let mut first_dts_us: Vec<Option<i64>> = vec![None; kinds.len()];
        let mut min_pts_us: Option<i64> = None;
        for source in &group_sources {
            for (packet, out) in &source.queue {
                let tb = source.time_bases[packet.stream()];
                let dts = packet
                    .dts()
                    .expect("队列中的包都带 DTS")
                    .rescale(tb, MICROS);
                let pts = packet.pts().map_or(dts, |pts| pts.rescale(tb, MICROS));
                first_dts_us[*out].get_or_insert(dts);
                min_pts_us = Some(min_pts_us.map_or(pts, |m| m.min(pts)));
            }
        }
        let min_pts_us = min_pts_us.ok_or(Error::EmptyGroup { group })?;
        let offset_us = group_offset(&ends_us, &last_dts_us, &first_dts_us, min_pts_us, margin_us);

        loop {
            for source in &mut group_sources {
                if source.queue.is_empty() && !source.finished {
                    match source.read_next(&mut skipped)? {
                        Some(item) => source.queue.push_back(item),
                        None => source.finished = true,
                    }
                }
            }
            // 同组各输入共用一个偏移，直接按原始 DTS 交错写出
            let next = group_sources
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.front_dts_us().map(|dts| (i, dts)))
                .min_by_key(|&(_, dts)| dts);
            let Some((i, _)) = next else { break };

            let source = &mut group_sources[i];
            let (mut packet, out) = source.queue.pop_front().expect("next 只指向有待写包的输入");
            let in_tb = source.time_bases[packet.stream()];
            let (dts_us, end_us) = retime(&mut packet, in_tb, out_time_bases[out], offset_us);
            ends_us[out] = Some(ends_us[out].map_or(end_us, |e| e.max(end_us)));
            last_dts_us[out] = Some(dts_us);
            packet.set_position(-1);
            packet.set_stream(out);
            packet.write_interleaved(&mut octx).map_err(Error::Mux)?;
            written[out] += 1;
        }
    }
    octx.write_trailer().map_err(Error::Mux)?;
    drop(octx);

    let found = count_packets(part)?;
    if found.len() != written.len() {
        return Err(Error::VerifyStreams {
            expected: written.len(),
            found: found.len(),
        });
    }
    for (stream, (&written, &found)) in written.iter().zip(&found).enumerate() {
        if written != found {
            return Err(Error::VerifyPackets {
                stream,
                written,
                found,
            });
        }
    }

    let streams = kinds
        .into_iter()
        .zip(codecs)
        .zip(written.into_iter().zip(skipped))
        .map(
            |((kind, codec), (packets, skipped_without_dts))| StreamReport {
                kind,
                codec,
                packets,
                skipped_without_dts,
            },
        )
        .collect();
    Ok(Report { streams })
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

/// 把包的时间戳平移 `offset_us` 并换到输出时间基；返回 (平移后的 DTS, 呈现结束时刻)，均为输出时间线上的微秒。
/// 先换成微秒再平移、最后一次换到输出时间基，只经过一次舍入。
fn retime(packet: &mut Packet, in_tb: Rational, out_tb: Rational, offset_us: i64) -> (i64, i64) {
    let shift = |ts: i64| ts.rescale(in_tb, MICROS) + offset_us;
    let dts_us = shift(packet.dts().expect("写出的包都带 DTS"));
    let pts_us = packet.pts().map(shift);
    let duration_us = packet.duration().rescale(in_tb, MICROS);
    packet.set_dts(Some(dts_us.rescale(MICROS, out_tb)));
    packet.set_pts(pts_us.map(|us| us.rescale(MICROS, out_tb)));
    packet.set_duration(packet.duration().rescale(in_tb, out_tb));
    (dts_us, pts_us.unwrap_or(dts_us).max(dts_us) + duration_us)
}

/// 回读文件，按流下标统计包数。
fn count_packets(path: &Path) -> Result<Vec<u64>, Error> {
    let reread = |source| Error::Reread {
        path: path.to_path_buf(),
        source,
    };
    let mut ictx = format::input(path).map_err(reread)?;
    let mut counts = vec![0u64; ictx.nb_streams() as usize];
    loop {
        let mut packet = Packet::empty();
        match packet.read(&mut ictx) {
            Ok(()) => counts[packet.stream()] += 1,
            Err(ffmpeg::Error::Eof) => return Ok(counts),
            Err(source) => return Err(reread(source)),
        }
    }
}
