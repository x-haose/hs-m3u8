//! 写出：按第 0 组（或各完整文件）建立输出流，交错写出平移到输出时间线上的包，写尾，回读核对包数。两种合并的
//! 编排见 [`crate::hls`] 与 [`crate::files`]。

use std::path::{Path, PathBuf};
use std::time::Duration;

use ffmpeg::{Dictionary, Packet, Rational, Rescale, Rounding, codec, encoder, format};
use ffmpeg_next as ffmpeg;

use crate::chain::{self, Halt, SegmentChain};
use crate::compare::Desc;
use crate::error::file_error;
use crate::ffi::{self, OpenFailure, OutputFile};
use crate::group::OpenTrack;
use crate::source::{Dropped, Mapping, Source};
use crate::timing::{MICROS, SampleClock, retime};
use crate::{
    Cancel, Container, Error, FfmpegError, MediaReport, Origin, Shape, StreamReport, Unsupported,
};

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
    /// 以 `container` 的格式打开输出，按第 0 组（或各完整文件）`first`（各轨选中的流为 `layout`）建立输出流并写头。
    pub(crate) fn open(
        path: &str,
        container: Container,
        cancel: &Cancel,
        first: &[OpenTrack],
        layout: &[Vec<Desc>],
    ) -> Result<Muxer, Error> {
        let mut file = ffi::open_output(path, container, cancel)
            .map_err(|failure| open_error(failure, path, cancel))?;
        add_streams(&mut file.octx, container, first)?;
        write_header(&mut file, container)?;
        let outs: Vec<OutStream> = layout
            .iter()
            .flatten()
            .enumerate()
            .map(|(out, desc)| OutStream {
                shape: desc.shape,
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

    /// 各路输出流已写出部分的最晚结束时刻与上一个包的 DTS，输出时间线上的微秒；还没写出时为 None。
    pub(crate) fn written_until(&self) -> Vec<(Option<i64>, Option<i64>)> {
        self.outs
            .iter()
            .map(|o| (o.end_us, o.last_dts_us))
            .collect()
    }

    /// 最粗的输出时间基的一个 tick，向上取整到微秒：换到输出时间基时的舍入不超过它的一半。
    pub(crate) fn coarsest_tick_us(&self) -> i64 {
        self.outs
            .iter()
            .map(|o| 1i64.rescale_with(o.time_base, MICROS, Rounding::Up))
            .max()
            .expect("至少有一路输出流")
    }

    /// 按加上各输入 [`Source::offset_us`] 之后的 DTS 交错写出各输入的包，直到都读完；`before` 在每个包写出之前调用，参数
    /// 为它所在的输入、包、映射与输出时间线上的 DTS（微秒），返回错误时中止。读完后把各输入丢弃的包计入输出流。
    pub(crate) fn interleave(
        &mut self,
        sources: &mut [Source],
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
    let found = count_packets(path, cancel)?;
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

/// 回读文件，按流下标统计包数。经分片链读（见 [`crate::chain`]）：读文件出错照原样报为 [`Error::Io`]，取消为
/// [`Error::Cancelled`]，其余为 [`Error::Reread`]（输出本身读不懂）。
fn count_packets(path: &Path, cancel: &Cancel) -> Result<Vec<u64>, Error> {
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
        counts[packet.stream()] += 1;
    }
}
