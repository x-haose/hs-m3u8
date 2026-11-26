//! 写出：按第 0 组（或各完整文件）建立输出流，交错写出平移到输出时间线上的包，写尾，回读核对包数。两种合并的
//! 编排见 [`crate::hls`] 与 [`crate::files`]。

use std::path::{Path, PathBuf};
use std::time::Duration;

use ffmpeg::{Dictionary, Packet, Rational, Rescale, Rounding, codec, encoder, format, media};
use ffmpeg_next as ffmpeg;

use crate::chain::{self, Halt, SegmentChain};
use crate::compare::Desc;
use crate::error::file_error;
use crate::ffi::{self, OpenFailure, OutputFile};
use crate::group::OpenTrack;
use crate::source::{Dropped, Mapping, Source};
use crate::subtitles::{self, Lane};
use crate::timing::{MICROS, SampleClock, retime};
use crate::{
    Cancel, Container, Error, FfmpegError, Label, MediaReport, Origin, Shape, StreamKind,
    StreamReport, Unsupported,
};

/// 一路输出流：第 0 组决定其编码参数，写出过程中累计统计。`_us` 结尾的时刻为输出时间线上的微秒。
struct OutStream {
    shape: Shape,
    /// mov_text：回读时不数封装器自己补的清屏空样本（见 [`count_packets`]）
    blank_samples: bool,
    /// 见 [`StreamReport::discarded_cues`]；字幕写完后由调用方计入
    discarded_cues: u64,
    /// 写头之后由封装器确定
    time_base: Rational,
    written: u64,
    dropped: Dropped,
    start_us: Option<i64>,
    end_us: Option<i64>,
    last_dts_us: Option<i64>,
    /// 写出的上一个包的 DTS，输出时间基
    last_dts: Option<i64>,
    /// 音频的输入时间基比采样点粗时按采样点接续（见 [`SampleClock`]），遇到这样的输入时建立
    clock: Option<SampleClock>,
}

impl OutStream {
    fn report(self) -> StreamReport {
        StreamReport {
            shape: self.shape,
            packets: self.written,
            skipped_without_dts: self.dropped.without_dts,
            skipped_before_keyframe: self.dropped.before_keyframe,
            discarded_corrupt_bytes: self.dropped.corrupt_bytes,
            discarded_cues: self.discarded_cues,
            duration: match (self.start_us, self.end_us) {
                (Some(start), Some(end)) => {
                    Duration::from_micros(u64::try_from(end - start).unwrap_or(0))
                }
                _ => Duration::ZERO,
            },
        }
    }
}

/// 正在写的输出：已建好输出流、写完头。
pub(crate) struct Muxer {
    file: OutputFile,
    container: Container,
    outs: Vec<OutStream>,
}

impl Muxer {
    /// 以 `container` 的格式打开输出，按第 0 组（或各完整文件）`first`（各轨选中的流为 `layout`）建立音视频输出流，其后
    /// 为 `subtitles` 里各轨（轨号）的字幕流，写上 `labels`（按轨号；合并完整文件时为 None，不写）并写头。
    pub(crate) fn open(
        path: &str,
        container: Container,
        cancel: &Cancel,
        first: &[OpenTrack],
        layout: &[Vec<Desc>],
        labels: Option<&[&Label]>,
        subtitles: &[usize],
    ) -> Result<Muxer, Error> {
        let mut file = ffi::open_output(path, container, cancel)
            .map_err(|failure| open_error(failure, path, cancel))?;
        add_streams(&mut file.octx, container, first)?;
        let codec = subtitles::Codec::of(container);
        for _ in subtitles {
            let mut ost = file
                .octx
                .add_stream(encoder::find(codec::Id::None))
                .map_err(|e| Error::Mux(e.into()))?;
            ffi::set_subtitle_parameters(&mut ost, codec);
            ost.set_time_base(SUBTITLE_TIME_BASE);
        }
        if let Some(labels) = labels {
            let owners: Vec<usize> = layout
                .iter()
                .enumerate()
                .flat_map(|(track, descs)| std::iter::repeat_n(track, descs.len()))
                .chain(subtitles.iter().copied())
                .collect();
            label_streams(&mut file.octx, container, &owners, labels);
        }
        write_header(&mut file, container)?;
        let media_outputs: usize = layout.iter().map(Vec::len).sum();
        for n in 0..subtitles.len() {
            let tb = file
                .octx
                .stream(media_outputs + n)
                .expect("输出流已创建")
                .time_base();
            assert_eq!(
                tb, SUBTITLE_TIME_BASE,
                "链接的 FFmpeg 的 matroska 与 mov 封装器沿用字幕流设的时间基"
            );
        }
        let shapes = layout
            .iter()
            .flatten()
            .map(|desc| desc.shape)
            .chain(subtitles.iter().map(|_| Shape::Subtitle {
                codec: codec.name(),
            }));
        let outs: Vec<OutStream> = shapes
            .enumerate()
            .map(|(out, shape)| OutStream {
                shape,
                blank_samples: matches!(shape, Shape::Subtitle { .. })
                    && codec == subtitles::Codec::MovText,
                discarded_cues: 0,
                time_base: file.octx.stream(out).expect("输出流已创建").time_base(),
                written: 0,
                dropped: Dropped::default(),
                start_us: None,
                end_us: None,
                last_dts_us: None,
                last_dts: None,
                clock: None,
            })
            .collect();
        Ok(Muxer {
            file,
            container,
            outs,
        })
    }

    /// 输出流 `output` 写出的包数与读到而没有写出的部分（各输入读完后才是全部）。
    pub(crate) fn outcome(&self, output: usize) -> (u64, Dropped) {
        let o = &self.outs[output];
        (o.written, o.dropped)
    }

    /// 输出流的路数。
    pub(crate) fn outputs(&self) -> usize {
        self.outs.len()
    }

    /// 各路输出流已写出部分的最晚结束时刻与上一个包的 DTS，输出时间线上的微秒；还没写出时、字幕流（不参与组间的偏移，
    /// 字幕跟着画面走）为 None。
    pub(crate) fn written_until(&self) -> Vec<(Option<i64>, Option<i64>)> {
        self.outs
            .iter()
            .map(|o| match o.shape.kind() {
                StreamKind::Subtitle => (None, None),
                _ => (o.end_us, o.last_dts_us),
            })
            .collect()
    }

    /// 音视频输出流里最粗的时间基的一个 tick，向上取整到微秒：换到输出时间基时的舍入不超过它的一半。
    pub(crate) fn coarsest_tick_us(&self) -> i64 {
        self.outs
            .iter()
            .filter(|o| o.shape.kind() != StreamKind::Subtitle)
            .map(|o| 1i64.rescale_with(o.time_base, MICROS, Rounding::Up))
            .max()
            .expect("至少有一路输出流")
    }

    /// 按加上各输入 [`Source::offset_us`] 之后的 DTS 交错写出各输入的包与各字幕轨的待写队列（`lanes`，合并完整文件时为空）里封进
    /// 输出的字幕包，直到都写完，时刻相同时先写音视频；`before` 在每个音视频包写出之前调用，参数为它所在的输入、包、映射
    /// 与输出时间线上的 DTS（微秒），返回错误时中止。读完后把各输入丢弃的包计入输出流。
    pub(crate) fn interleave(
        &mut self,
        sources: &mut [Source],
        lanes: &mut [Lane],
        mut before: impl FnMut(&mut Source, &Packet, Mapping, i64) -> Result<(), Error>,
    ) -> Result<(), Error> {
        loop {
            for source in sources.iter_mut() {
                if source.queue.is_empty()
                    && !source.finished
                    && let Some(item) = source.read_next()?
                {
                    source.queue.push_back(item);
                }
            }
            let next = sources
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.front_dts_us().map(|dts| (i, dts)))
                .min_by_key(|&(_, dts)| dts);
            let subtitle = lanes
                .iter()
                .enumerate()
                .filter(|(_, lane)| lane.output.is_some())
                .filter_map(|(l, lane)| lane.front_us().map(|us| (l, us)))
                .min_by_key(|&(_, us)| us);
            if let Some((l, start_us)) = subtitle
                && next.is_none_or(|(_, dts_us)| start_us < dts_us)
            {
                let lane = &mut lanes[l];
                let packet = lane
                    .queue
                    .pop_front()
                    .expect("front_us 只指向有待写包的通道");
                let (output, _) = lane.output.expect("只挑封进输出的通道");
                let at = lane.at();
                self.write_subtitle(packet, output, at)?;
                continue;
            }
            let Some((i, dts_us)) = next else { break };
            let source = &mut sources[i];
            let (packet, mapping) = source.queue.pop_front().expect("next 只指向有待写包的输入");
            before(source, &packet, mapping, dts_us)?;
            let (at, offset_us) = (source.at, source.offset_us);
            self.write_packet(packet, mapping, at, offset_us)?;
        }
        self.add_dropped(sources);
        Ok(())
    }

    /// 各输入读完：它们丢弃的包计入对应的输出流。
    fn add_dropped(&mut self, sources: &[Source]) {
        for source in sources {
            for (mapping, dropped) in source.map.iter().zip(&source.dropped) {
                if let Some(mapping) = mapping {
                    let total = &mut self.outs[mapping.output].dropped;
                    total.without_dts += dropped.without_dts;
                    total.corrupt_bytes += dropped.corrupt_bytes;
                    total.before_keyframe += dropped.before_keyframe;
                }
            }
        }
    }

    /// 把 `at` 处的输入的包平移 `offset_us`（微秒）到输出时间线上、写出。
    fn write_packet(
        &mut self,
        mut packet: Packet,
        mapping: Mapping,
        at: Origin,
        offset_us: i64,
    ) -> Result<(), Error> {
        if self.file.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let out = mapping.output;
        let o = &mut self.outs[out];
        let t = retime(&mut packet, mapping.time_base, o.time_base, offset_us);
        if let Some((rate, frame)) = mapping.frames
            && SampleClock::applies(mapping.time_base, o.time_base, rate)
        {
            let clock = o.clock.get_or_insert_with(|| SampleClock::new(rate, frame));
            let placed = clock.place(t.dts_us, mapping.time_base);
            let dts = clock.to(placed, o.time_base);
            let shift = dts - packet.dts().expect("retime 设了 DTS");
            packet.set_dts(Some(dts));
            packet.set_pts(packet.pts().map(|pts| pts + shift));
        }
        // 每路流的 DTS 在输出时间基下须递增：MP4 严格递增（同一条轨不能有两个样本同时解码），Matroska 的时间基为 1 毫秒，
        // 换算后相等的不算（链接的 FFmpeg 对带 AVFMT_TS_NONSTRICT 的封装器同样只拦往回跳的）。FFmpeg 的 MP4 封装器遇到
        // 不递增的只报「参数不合法」，上一个为 0 时还不查，这里一律拦下；组间的偏移保证跨组递增，不递增只会出在组内
        let dts = packet.dts().expect("retime 设了 DTS");
        let strict = self.container == Container::Mp4;
        if o.last_dts
            .is_some_and(|last| dts < last || (strict && dts == last))
        {
            return Err(Error::Unsupported(Unsupported::DtsNotIncreasing {
                at,
                kind: o.shape.kind(),
            }));
        }
        o.last_dts = Some(dts);
        o.start_us = Some(o.start_us.map_or(t.pts_us, |s| s.min(t.pts_us)));
        o.end_us = Some(o.end_us.map_or(t.end_us, |e| e.max(t.end_us)));
        o.last_dts_us = Some(t.dts_us);
        packet.set_position(-1);
        packet.set_stream(out);
        if let Err(e) = packet.write_interleaved(&mut self.file.octx) {
            return Err(self.file.write_error(e));
        }
        self.outs[out].written += 1;
        Ok(())
    }

    /// 把字幕包写到输出流 `out`；`at` 只用于报错时指出轨道。
    fn write_subtitle(
        &mut self,
        subtitle: subtitles::Packet,
        out: usize,
        at: Origin,
    ) -> Result<(), Error> {
        if self.file.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let o = &mut self.outs[out];
        let mut packet = Packet::copy(&subtitle.data);
        let dts = subtitle.start_us.rescale(MICROS, o.time_base);
        let duration = subtitle.duration_us.rescale(MICROS, o.time_base);
        // 字幕轨的待写包的时刻都在 1 毫秒的网格上，换到写头后的时间基（见 open 里的核对）不舍入，且起点不往回走（MP4 严格递增）
        let strict = self.container == Container::Mp4;
        assert!(
            o.last_dts
                .is_none_or(|last| dts > last || (!strict && dts == last)),
            "{at}的字幕包起点往回走了：生成字幕包时保证不会（见 subtitles 模块）"
        );
        packet.set_dts(Some(dts));
        packet.set_pts(Some(dts));
        packet.set_duration(duration);
        packet.set_flags(ffmpeg::packet::Flags::KEY);
        packet.set_stream(out);
        o.last_dts = Some(dts);
        o.start_us = Some(
            o.start_us
                .map_or(subtitle.start_us, |s| s.min(subtitle.start_us)),
        );
        let end_us = subtitle.start_us + subtitle.duration_us;
        o.end_us = Some(o.end_us.map_or(end_us, |e| e.max(end_us)));
        o.last_dts_us = Some(subtitle.start_us);
        if let Err(e) = packet.write_interleaved(&mut self.file.octx) {
            return Err(self.file.write_error(e));
        }
        self.outs[out].written += 1;
        Ok(())
    }

    /// 字幕流 `output` 在对齐与生成包时丢掉的 cue 数（见 [`StreamReport::discarded_cues`]）。
    pub(crate) fn set_discarded_cues(&mut self, output: usize, cues: u64) {
        self.outs[output].discarded_cues = cues;
    }

    /// 写尾（MP4 的 moov 前置时在这里把数据整体后移，FFmpeg 读写文件时同样轮询取消），关闭输出并检查关闭的结果，再回读
    /// `path` 核对每路流的包数。
    pub(crate) fn finish_verified(
        mut self,
        path: &str,
        cancel: &Cancel,
    ) -> Result<MediaReport, Error> {
        if let Err(e) = self.file.octx.write_trailer() {
            return Err(self.file.write_error(e));
        }
        if let Err(e) = self.file.close() {
            return Err(self.file.close_error(e));
        }
        verify(Path::new(path), &self.outs, cancel)?;
        Ok(MediaReport {
            streams: self.outs.into_iter().map(OutStream::report).collect(),
        })
    }
}

/// 打开输出 `path` 失败：取消了为 [`Error::Cancelled`]；创建文件时的 errno 型错误为 [`Error::Io`]；其余为
/// [`Error::OpenOutput`]。
fn open_error(failure: OpenFailure, path: &str, cancel: &Cancel) -> Error {
    let cause = match failure {
        OpenFailure::Muxer(cause) => FfmpegError::from(cause),
        OpenFailure::File(_) if cancel.is_cancelled() => return Error::Cancelled,
        OpenFailure::File(cause) => {
            let cause = FfmpegError::from(cause);
            if let Some(cause) = file_error(&cause) {
                return Error::Io {
                    action: "创建",
                    path: PathBuf::from(path),
                    cause,
                };
            }
            cause
        }
    };
    Error::OpenOutput {
        path: PathBuf::from(path),
        cause,
    }
}

impl OutputFile {
    /// FFmpeg 写这个输出时的错误：取消了为 [`Error::Cancelled`]；输出的读写上下文记着写文件的错误（如盘满）为
    /// [`Error::Io`]；其余为封装器的错误 [`Error::Mux`]。
    fn write_error(&self, error: ffmpeg::Error) -> Error {
        if self.cancel.is_cancelled() {
            return Error::Cancelled;
        }
        let written = ffi::output_io_error(&self.octx)
            .and_then(|code| file_error(&FfmpegError::from(ffmpeg::Error::from(code))));
        match written {
            Some(cause) => Error::Io {
                action: "写入",
                path: self.path.clone(),
                cause,
            },
            None => Error::Mux(error.into()),
        }
    }

    /// 关闭这个输出时的错误（见 [`OutputFile::close`]）：取消了为 [`Error::Cancelled`]；errno 型的为 [`Error::Io`]；
    /// 其余为 [`Error::Mux`]。
    fn close_error(&self, error: ffmpeg::Error) -> Error {
        if self.cancel.is_cancelled() {
            return Error::Cancelled;
        }
        let error = FfmpegError::from(error);
        match file_error(&error) {
            Some(cause) => Error::Io {
                action: "关闭",
                path: self.path.clone(),
                cause,
            },
            None => Error::Mux(error),
        }
    }
}

/// 按第 0 组（或各完整文件）选中的流建立输出流，创建顺序即输出流下标（见 [`crate::group::layout_of`]）。
fn add_streams(
    octx: &mut format::context::Output,
    container: Container,
    first: &[OpenTrack],
) -> Result<(), Error> {
    for t in first {
        for s in &t.selected {
            let stream = t
                .input
                .context()
                .stream(s.index)
                .expect("select 返回的下标有效");
            let mut ost = octx
                .add_stream(encoder::find(codec::Id::None))
                .map_err(|e| Error::Mux(e.into()))?;
            ost.set_parameters(stream.parameters());
            ffi::set_codec_tag(&mut ost, s.id, container);
        }
    }
    Ok(())
}

/// 字幕流的时间基，一个 tick 为 [`subtitles::TICK_US`]。
const SUBTITLE_TIME_BASE: Rational = Rational(1, 1000);

/// 给各输出流写上标注：第 i 路流属于第 `owners[i]` 条轨，标注为 `labels[owners[i]]`（见 [`Label`]）。每类流里都没有标默认
/// 的时，第一路视频与第一路音频为默认。
fn label_streams(
    octx: &mut format::context::Output,
    container: Container,
    owners: &[usize],
    labels: &[&Label],
) {
    let kinds: Vec<media::Type> = octx.streams().map(|s| s.parameters().medium()).collect();
    let flagged = |kind: media::Type| {
        kinds
            .iter()
            .zip(owners)
            .any(|(&k, &owner)| k == kind && labels[owner].default)
    };
    let mut first_seen: Vec<media::Type> = Vec::new();
    for (index, (&kind, &owner)) in kinds.iter().zip(owners).enumerate() {
        let label = labels[owner];
        let first = !first_seen.contains(&kind);
        if first {
            first_seen.push(kind);
        }
        let default = label.default || (first && kind != media::Type::Subtitle && !flagged(kind));
        let mut metadata = Dictionary::new();
        let code = label
            .language
            .as_deref()
            .and_then(crate::iso639::iso639_2)
            .map_or("und", |(bibliographic, terminology)| match container {
                Container::Matroska => bibliographic,
                Container::Mp4 => terminology,
            });
        metadata.set("language", code);
        if let Some(title) = label.name.as_deref().or(label.language.as_deref()) {
            metadata.set("title", title);
        }
        let mut ost = octx.stream_mut(index).expect("下标在流数以内");
        ost.set_metadata(metadata);
        ffi::set_disposition(&mut ost, default, label.forced);
    }
}

/// 写头（MP4 的 moov 前置）；封装器不认识的选项视为错误。
fn write_header(file: &mut OutputFile, container: Container) -> Result<(), Error> {
    let mut options = Dictionary::new();
    if container == Container::Mp4 {
        options.set("movflags", "+faststart");
    }
    let written = file.octx.write_header_with(options).map(|rejected| {
        rejected
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect::<Vec<(String, String)>>()
    });
    let rejected = written.map_err(|e| file.write_error(e))?;
    if rejected.is_empty() {
        Ok(())
    } else {
        Err(Error::RejectedOptions(rejected))
    }
}

/// 回读输出，核对每路流的包数与写入的一致。
fn verify(path: &Path, outs: &[OutStream], cancel: &Cancel) -> Result<(), Error> {
    let blanks: Vec<bool> = outs.iter().map(|o| o.blank_samples).collect();
    let found = count_packets(path, &blanks, cancel)?;
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

/// 回读文件，按流下标统计包数；`skip_blank[i]` 为真的流（mov_text）不数清屏的空样本：链接的 FFmpeg 的 movenc 在字幕样本
/// 之间与末尾自己补上（mov_write_packet 与 mov_write_trailer），补几个取决于各路流的包的先后，本库写出的都有文字。经分
/// 片链读（见 [`crate::chain`]）：读文件出错照原样报为 [`Error::Io`]，取消为 [`Error::Cancelled`]，其余为
/// [`Error::Reread`]（输出本身读不懂）。
fn count_packets(path: &Path, skip_blank: &[bool], cancel: &Cancel) -> Result<Vec<u64>, Error> {
    let file_failed = |cause| Error::Io {
        action: "回读",
        path: path.to_path_buf(),
        cause,
    };
    let len = std::fs::metadata(path).map_err(file_failed)?.len();
    let (chain, failure) = SegmentChain::new(vec![(path.to_path_buf(), 0..len)], cancel.clone())
        .map_err(|(_, cause)| file_failed(cause))?;
    let halted = |halt| match halt {
        Halt::Stopped(stopped) => stopped.into_error("回读"),
        Halt::Ffmpeg(cause) => Error::Reread {
            path: path.to_path_buf(),
            cause: cause.into(),
        },
    };
    let mut ictx = chain::open(chain, &failure, None).map_err(halted)?;
    let mut counts = vec![0u64; ictx.nb_streams() as usize];
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut packet = Packet::empty();
        if !chain::read(&mut ictx, &failure, &mut packet).map_err(halted)? {
            return Ok(counts);
        }
        // 回读出的流比写出的多时没有这一项，照常计数，由 verify 报流数不符
        let blank = skip_blank.get(packet.stream()).is_some_and(|&skip| skip)
            && packet
                .data()
                .is_none_or(|data| data.iter().all(|&b| b == 0) && data.len() <= 2);
        if !blank {
            counts[packet.stream()] += 1;
        }
    }
}
