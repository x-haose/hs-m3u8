//! 一条轨在一组里的输入。同一组的各输入读出的时间戳都在来源的同一条时间线上（TS 与打包音频为 33 位的 90 kHz
//! 时钟，回绕另行对齐；fMP4 为媒体时间），整组因此共用一个偏移。

use std::collections::HashMap;
use std::fs::File;
use std::ops::Range;
use std::path::{Path, PathBuf};

use ffmpeg::{Dictionary, Packet, format};
use ffmpeg_next as ffmpeg;

use crate::chain::{self, Halt, SegmentChain};
use crate::packed::Packed;
use crate::{Cancel, Error, Segment, TrackSegments, Unsupported, fmp4, standalone};

pub(crate) enum Input {
    /// TS 与 fMP4：init 段与分片按字节相连，作为一个输入读
    Chain {
        ictx: format::context::Input,
        /// 读分片文件出错时的路径与原因
        failure: chain::Failure,
        /// 有 init 段（fMP4）：编码参数都在 init 段里，不随帧携带
        init: bool,
        segments: SegmentMap,
    },
    Packed(Box<Packed>),
}

impl Input {
    /// 打开一条轨在一组里的分片。没有 init 段的分片须同一格式（见 [`Unsupported::FormatChangedInGroup`]），不是 TS
    /// 的为打包音频。`cancel` 取消后打开与读取以 [`Error::Cancelled`] 失败。
    pub(crate) fn open(
        segments: &TrackSegments,
        group: usize,
        track: usize,
        cancel: &Cancel,
    ) -> Result<Input, Error> {
        let init = segments.init.is_some();
        if !init {
            let heads = segments
                .segments
                .iter()
                .map(|segment| segment_head(&segment.path))
                .collect::<Result<Vec<_>, Error>>()?;
            if heads.iter().any(|h| h.format != heads[0].format) {
                return Err(Error::Unsupported(Unsupported::FormatChangedInGroup {
                    group,
                    track,
                }));
            }
            if heads[0].format != standalone::Format::Ts {
                return Packed::open(segments, &heads, group, track, cancel)
                    .map(|packed| Input::Packed(Box::new(packed)));
            }
        }
        let trex = segments.init.as_deref().map(init_trex).transpose()?;
        let paths = segments
            .init
            .iter()
            .chain(segments.segments.iter().map(|s| &s.path));
        let mut files = Vec::with_capacity(segments.segments.len() + 1);
        for path in paths {
            let len = match &trex {
                // 逐片核对要读遍各 box，每片之前查取消
                Some(_) if cancel.is_cancelled() => return Err(Error::Cancelled),
                Some(trex) => fmp4_len(path, trex)?,
                None => std::fs::metadata(path).map_err(read_error(path))?.len(),
            };
            files.push((path.clone(), 0..len));
        }
        let mut starts: Vec<u64> = files
            .iter()
            .scan(0u64, |at, (_, range)| {
                let start = *at;
                *at = at.saturating_add(range.end - range.start);
                Some(start)
            })
            .collect();
        if init {
            starts.remove(0);
        }
        // 第一个分片（fMP4 连同 init 段）的结尾
        let first_end = files
            .iter()
            .take(if init { 2 } else { 1 })
            .map(|(_, range)| range.end - range.start)
            .sum();
        let probe = Probe::first_segment(first_end, &segments.segments[0]);
        let (chain, failure) = new_chain(files, cancel)?;
        let ictx = open_chain(chain, &failure, probe, group, track)?;
        Ok(Input::Chain {
            ictx,
            failure,
            init,
            segments: SegmentMap::new(starts, segments),
        })
    }

    pub(crate) fn context(&self) -> &format::context::Input {
        match self {
            Input::Chain { ictx, .. } => ictx,
            Input::Packed(packed) => packed.context(),
        }
    }

    /// 读下一个包；读到末尾返回 false。
    pub(crate) fn read(
        &mut self,
        packet: &mut Packet,
        group: usize,
        track: usize,
    ) -> Result<bool, Error> {
        match self {
            Input::Chain { ictx, failure, .. } => read_packet(ictx, failure, packet, group, track),
            Input::Packed(packed) => packed.read(packet, group, track),
        }
    }

    /// 没有 init 段的分片（TS 与打包音频）：编码参数随帧携带，要逐包核对（见 [`crate::inband`]）；时间戳在 33 位的
    /// 90 kHz 时钟上，会回绕。
    pub(crate) fn standalone(&self) -> bool {
        !matches!(self, Input::Chain { init: true, .. })
    }

    /// 打包音频丢弃的损坏字节：音频流的下标与字节数（见 [`Packed`]，读到末尾后才是全部）；其余输入为 None。
    pub(crate) fn discarded(&self) -> Option<(usize, u64)> {
        match self {
            Input::Packed(packed) => packed.discarded(),
            Input::Chain { .. } => None,
        }
    }

    /// 各分片在输入里的位置与声明的时长。
    pub(crate) fn segments(&self) -> &SegmentMap {
        match self {
            Input::Chain { segments, .. } => segments,
            Input::Packed(packed) => packed.segments(),
        }
    }

    /// 打包音频，而这一组有分片没有时间戳。
    pub(crate) fn untimed(&self) -> bool {
        matches!(self, Input::Packed(packed) if !packed.timed())
    }
}

/// 一条轨在一组里的各分片在输入里的位置与声明的时长：把读出的包按字节位置归到所在的分片，核对相邻分片之间的时间戳
/// 推进（见 [`crate::Unsupported::TimestampJump`]）。
pub(crate) struct SegmentMap {
    /// 各分片在输入里的起点，字节；升序，与分片一一对应
    starts: Vec<u64>,
    /// 各分片声明的时长，微秒
    durations_us: Vec<u64>,
    /// 与前一个分片之间缺了分片的分片下标（见 [`TrackSegments::gaps`]）
    gaps: Vec<usize>,
    /// 这一组里最长的声明时长，微秒
    longest_us: u64,
}

impl SegmentMap {
    /// `starts` 为 `segments` 的各分片在输入里的起点。
    pub(crate) fn new(starts: Vec<u64>, segments: &TrackSegments) -> SegmentMap {
        let durations_us: Vec<u64> = segments.segments.iter().map(|s| s.duration_us).collect();
        SegmentMap {
            starts,
            longest_us: durations_us.iter().copied().max().unwrap_or(0),
            durations_us,
            gaps: segments.gaps.clone(),
        }
    }

    /// 输入里第 `position` 字节所在的分片；位置未知（负数）或在第一个分片之前（fMP4 的 init 段里）时为 None。
    pub(crate) fn segment_at(&self, position: isize) -> Option<usize> {
        let position = u64::try_from(position).ok()?;
        self.starts
            .partition_point(|&start| start <= position)
            .checked_sub(1)
    }

    /// 第 `from` 片与第 `to` 片之间各片声明的时长之和，微秒（不含这两片；`from` 为 None 时从第 0 片算起；`from` 不
    /// 早于 `to` 时为 0）。其间有缺片（缺了的那几片时长不知道）时为 None，不核对。
    pub(crate) fn declared_between(&self, from: Option<usize>, to: usize) -> Option<u64> {
        let first = from.map_or(0, |from| from + 1);
        if self
            .gaps
            .iter()
            .any(|&gap| from.is_none_or(|from| from < gap) && gap <= to)
        {
            return None;
        }
        Some(self.durations_us[first.min(to)..to].iter().sum())
    }

    /// 输出里整组都没有内容的空档多长以内算正常：这一组里最长的声明时长，微秒。RFC 8216 要求 EXTINF 不超过目标时长
    /// （4.3.3.1），容差取一个目标时长量级；调用方只给各分片的声明时长（见 [`crate::Segment`]），没有目标时长，取最长的
    /// 代替。声明的时长都为 0 时为 None，判断不了，不核对。
    pub(crate) fn tolerance_us(&self) -> Option<u64> {
        (self.longest_us > 0).then_some(self.longest_us)
    }
}

/// fMP4 的 init 段 `path` 里各轨的默认样本长度（见 [`fmp4::trex_sample_sizes`]）。
fn init_trex(path: &Path) -> Result<HashMap<u32, u32>, Error> {
    let data = std::fs::read(path).map_err(read_error(path))?;
    fmp4::trex_sample_sizes(&data).map_err(|problem| Error::Fmp4 {
        path: path.to_path_buf(),
        problem,
    })
}

/// fMP4 的 init 段或分片 `path` 的字节数：须通过核对（`trex` 为所用 init 段的，见 [`fmp4::check_reader`]，否则报
/// [`Error::Fmp4`]）、最后一个 box 的长度不写 0（否则报 [`Error::Fmp4OpenEnded`]）。
fn fmp4_len(path: &Path, trex: &HashMap<u32, u32>) -> Result<u64, Error> {
    let mut file = File::open(path).map_err(read_error(path))?;
    let boxes = fmp4::check_reader(&mut file, trex)
        .map_err(read_error(path))?
        .map_err(|problem| Error::Fmp4 {
            path: path.to_path_buf(),
            problem,
        })?;
    if let Some(open) = boxes.iter().find(|b| b.to_end) {
        return Err(Error::Fmp4OpenEnded {
            path: path.to_path_buf(),
            at: open.at,
        });
    }
    let last = boxes
        .last()
        .expect("铺满了文件的顶层 box 至少一个（空文件报不是 fMP4）");
    Ok(last.at + last.size)
}

/// 读文件 `path` 失败。
fn read_error(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |cause| Error::Io {
        action: "读取",
        path: path.to_path_buf(),
        cause,
    }
}

/// 没有 init 段的分片开头的信息（见 [`standalone::read_head`]）；认不出格式时报 [`Error::Unrecognized`]。
pub(crate) fn segment_head(path: &Path) -> Result<standalone::Head, Error> {
    let head = File::open(path)
        .and_then(|mut file| standalone::read_head(&mut file))
        .map_err(read_error(path))?;
    head.ok_or_else(|| Error::Unrecognized(path.to_path_buf()))
}

/// 把 `pieces`（文件与其中要读的字节区间）串成一个分片链，并返回记录读取中止原因的位置。
pub(crate) fn new_chain(
    pieces: Vec<(PathBuf, Range<u64>)>,
    cancel: &Cancel,
) -> Result<(SegmentChain, chain::Failure), Error> {
    SegmentChain::new(pieces, cancel.clone()).map_err(|(path, cause)| Error::Io {
        action: "读取",
        path,
        cause,
    })
}

/// 打开输入时 FFmpeg 探测流参数最多读多少：至少读完第一个分片，探测不出的参数即第一个分片里没有（见
/// [`crate::Unsupported::ParamsUnknown`]）。各流的参数都探测出后 FFmpeg 就停下；有流（含不进输出的，如节目列表里有、
/// 第一个分片里却没有数据的另一路音频）一直探测不出时读到上限为止，读过的包缓存在内存里：分片小时可能不止一片。
pub(crate) struct Probe {
    /// 字节，不小于 FFmpeg 的默认值
    bytes: u64,
    /// 微秒，不小于 FFmpeg 的默认值
    duration_us: u64,
}

impl Probe {
    /// FFmpeg 探测的默认字节数（libavformat options_table.h 的 probesize）。
    const DEFAULT_BYTES: u64 = 5_000_000;
    /// FFmpeg 探测的默认时长，微秒：没设 analyzeduration 时 TS 的流按 7 秒、其余按 5 秒（libavformat demux.c 的
    /// avformat_find_stream_info），取大的。
    const DEFAULT_DURATION_US: u64 = 7_000_000;

    /// 第一个分片在输入里到第 `end` 字节为止，`first` 为它声明的时长。
    pub(crate) fn first_segment(end: u64, first: &Segment) -> Probe {
        Probe {
            bytes: end.max(Probe::DEFAULT_BYTES),
            duration_us: first.duration_us.max(Probe::DEFAULT_DURATION_US),
        }
    }

    fn options(&self) -> Dictionary<'static> {
        let mut options = Dictionary::new();
        options.set("probesize", &self.bytes.to_string());
        options.set("analyzeduration", &self.duration_us.to_string());
        options
    }
}

/// 把分片链当作一个输入交给 FFmpeg 打开，探测读到 `probe` 为止（见 [`chain::open`]）。
pub(crate) fn open_chain(
    chain: SegmentChain,
    failure: &chain::Failure,
    probe: Probe,
    group: usize,
    track: usize,
) -> Result<format::context::Input, Error> {
    chain::open(chain, failure, Some(probe.options())).map_err(|halt| match halt {
        Halt::Stopped(stopped) => stopped.into_error("读取"),
        Halt::Ffmpeg(cause) => Error::OpenInput {
            group,
            track,
            cause: cause.into(),
        },
    })
}

/// 读下一个包；读到末尾返回 false（见 [`chain::read`]）。
pub(crate) fn read_packet(
    ictx: &mut format::context::Input,
    failure: &chain::Failure,
    packet: &mut Packet,
    group: usize,
    track: usize,
) -> Result<bool, Error> {
    chain::read(ictx, failure, packet).map_err(|halt| match halt {
        Halt::Stopped(stopped) => stopped.into_error("读取"),
        Halt::Ffmpeg(cause) => Error::Read {
            group,
            track,
            cause: cause.into(),
        },
    })
}
