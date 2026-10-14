//! 写出 MP4：按第 0 组建立输出流，逐组平移时间戳、交错写出，写完回读核对包数。

use std::path::{Path, PathBuf};

use ffmpeg::{Dictionary, Packet, Rational, Rescale, Rounding, codec, encoder, format};
use ffmpeg_next as ffmpeg;

use crate::compare::Desc;
use crate::error::file_error;
use crate::ffi::{self, OpenFailure, OutputFile};
use crate::group::{OpenTrack, layout_of, open_group, primed, references_of};
use crate::source::{Dropped, Mapping, Source};
use crate::timing::{MICROS, align_wraps, group_offset, retime};
use crate::{
    Cancel, Error, FfmpegError, Numbered, Report, Shape, StreamReport, Streams, Unsupported,
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
}

impl OutStream {
    fn report(self) -> StreamReport {
        StreamReport {
            shape: self.shape,
            packets: self.written,
            skipped_without_dts: self.dropped.without_dts,
            skipped_before_keyframe: self.dropped.before_keyframe,
            discarded_corrupt_bytes: self.dropped.corrupt_bytes,
            duration_us: match (self.start_us, self.end_us) {
                (Some(start), Some(end)) => u64::try_from(end - start).unwrap_or(0),
                _ => 0,
            },
        }
    }
}

/// 合并各组写到 `path`（MP4，moov 前置），写完回读核对每路流的包数。
pub(crate) fn write_verified(
    streams: &[Streams],
    groups: &[Numbered<'_>],
    path: &str,
    cancel: &Cancel,
) -> Result<Report, Error> {
    let first = open_group(streams, groups[0], None, cancel)?;
    let layout = layout_of(&first)?;
    let mut muxer = Muxer::open(path, cancel, &first, &layout)?;
    let sources = primed(groups[0].0, first, &layout, &[])?;
    let references = references_of(&sources, muxer.outs.len());
    muxer.write_group(groups[0].0, sources)?;
    for &numbered in &groups[1..] {
        let opened = open_group(streams, numbered, Some(&layout), cancel)?;
        let sources = primed(numbered.0, opened, &layout, &references)?;
        muxer.write_group(numbered.0, sources)?;
    }
    let outs = muxer.finish()?;
    verify(Path::new(path), &outs, cancel)?;
    Ok(Report {
        streams: outs.into_iter().map(OutStream::report).collect(),
    })
}

/// 正在写的 MP4：已建好输出流、写完头。
struct Muxer<'a> {
    file: OutputFile,
    path: &'a str,
    cancel: &'a Cancel,
    outs: Vec<OutStream>,
    /// 组间余量：最粗的输出时间基的一个 tick（向上取整到微秒）。换到输出时间基时的舍入不超过半个 tick，因此下一组的
    /// 首个 DTS 一定大于上一组的末个 DTS
    margin_us: i64,
}

impl<'a> Muxer<'a> {
    /// 打开输出，按第 0 组 `first`（各轨选中的流为 `layout`）建立输出流并写头。
    fn open(
        path: &'a str,
        cancel: &'a Cancel,
        first: &[OpenTrack],
        layout: &[Vec<Desc>],
    ) -> Result<Muxer<'a>, Error> {
        let mut file =
            ffi::open_output(path, cancel).map_err(|failure| open_error(failure, path, cancel))?;
        add_streams(&mut file.octx, first)?;
        write_header(&mut file, path, cancel)?;
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
            })
            .collect();
        let margin_us = outs
            .iter()
            .map(|o| 1i64.rescale_with(o.time_base, MICROS, Rounding::Up))
            .max()
            .expect("第 0 组至少有一路输出流");
        Ok(Muxer {
            file,
            path,
            cancel,
            outs,
            margin_us,
        })
    }

    fn failed(&self, error: ffmpeg::Error) -> Error {
        write_error(&self.file, self.path, self.cancel, error)
    }

    /// 写出一组（各输入已读到每路流的第一个包）：各输入按回绕对齐（见 [`align_wraps`]），再按全组共用的偏移平移时间戳，
    /// 按 DTS 交错写出。
    fn write_group(&mut self, group: usize, mut sources: Vec<Source>) -> Result<(), Error> {
        align_wraps(&mut sources);
        let offset_us = self.offset_of(group, &sources)?;
        loop {
            for source in &mut sources {
                if source.queue.is_empty()
                    && !source.finished
                    && let Some(item) = source.read_next()?
                {
                    source.queue.push_back(item);
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
            let (packet, mapping) = source.queue.pop_front().expect("next 只指向有待写包的输入");
            let at = Placed {
                group,
                track: source.track,
                offset_us: offset_us + source.wrap_us,
            };
            self.write_packet(packet, mapping, at)?;
        }
        for source in &sources {
            for (mapping, dropped) in source.map.iter().zip(&source.dropped) {
                if let Some(mapping) = mapping {
                    let total = &mut self.outs[mapping.output].dropped;
                    total.without_dts += dropped.without_dts;
                    total.corrupt_bytes += dropped.corrupt_bytes;
                    total.before_keyframe += dropped.before_keyframe;
                }
            }
        }
        Ok(())
    }

    /// 本组的时间偏移（见 [`group_offset`]），由各输出流本组的首个 DTS 与全组最早的 PTS（输入时间线，微秒，取自各输入
    /// 已读出的包；队列内同一路流按解码顺序排列）算出。
    fn offset_of(&self, group: usize, sources: &[Source]) -> Result<i64, Error> {
        let mut first_dts_us: Vec<Option<i64>> = vec![None; self.outs.len()];
        let mut min_pts_us: Option<i64> = None;
        for source in sources {
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
        let ends_us: Vec<Option<i64>> = self.outs.iter().map(|o| o.end_us).collect();
        let last_dts_us: Vec<Option<i64>> = self.outs.iter().map(|o| o.last_dts_us).collect();
        Ok(group_offset(
            &ends_us,
            &last_dts_us,
            &first_dts_us,
            min_pts_us,
            self.margin_us,
        ))
    }

    /// 把包平移到输出时间线上、写出。
    fn write_packet(
        &mut self,
        mut packet: Packet,
        mapping: Mapping,
        at: Placed,
    ) -> Result<(), Error> {
        if self.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let out = mapping.output;
        let o = &mut self.outs[out];
        let t = retime(&mut packet, mapping.time_base, o.time_base, at.offset_us);
        // 每路流的 DTS 在输出时间基下须严格递增（同一条 MP4 轨不能有两个样本同时解码）。FFmpeg 的封装器遇到不递增的
        // 只报「参数不合法」，上一个为 0 时还不查，这里一律拦下；组间的偏移保证跨组递增，不递增只会出在组内
        let dts = packet.dts().expect("retime 设了 DTS");
        if o.last_dts.is_some_and(|last| dts <= last) {
            return Err(Error::Unsupported(Unsupported::DtsNotIncreasing {
                group: at.group,
                track: at.track,
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
            return Err(self.failed(e));
        }
        self.outs[out].written += 1;
        Ok(())
    }

    /// 写尾（moov 前置时在这里把数据整体后移，FFmpeg 读写文件时同样轮询取消），关闭输出。
    fn finish(mut self) -> Result<Vec<OutStream>, Error> {
        if let Err(e) = self.file.octx.write_trailer() {
            return Err(self.failed(e));
        }
        Ok(self.outs)
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

/// FFmpeg 写输出 `file`（路径 `path`）时的错误：取消了为 [`Error::Cancelled`]；输出的读写上下文记着写文件的错误（如
/// 盘满）为 [`Error::Io`]；其余为封装器的错误 [`Error::Mux`]。
fn write_error(file: &OutputFile, path: &str, cancel: &Cancel, error: ffmpeg::Error) -> Error {
    if cancel.is_cancelled() {
        return Error::Cancelled;
    }
    let written = ffi::output_io_error(&file.octx)
        .and_then(|code| file_error(&FfmpegError::from(ffmpeg::Error::from(code))));
    match written {
        Some(cause) => Error::Io {
            action: "写入",
            path: PathBuf::from(path),
            cause,
        },
        None => Error::Mux(error.into()),
    }
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

/// 写 MP4 头（moov 前置）；封装器不认识的选项视为错误。
fn write_header(file: &mut OutputFile, path: &str, cancel: &Cancel) -> Result<(), Error> {
    let mut options = Dictionary::new();
    options.set("movflags", "+faststart");
    let written = file.octx.write_header_with(options).map(|rejected| {
        rejected
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect::<Vec<(String, String)>>()
    });
    let rejected = written.map_err(|e| write_error(file, path, cancel, e))?;
    if rejected.is_empty() {
        Ok(())
    } else {
        Err(Error::RejectedOptions(rejected))
    }
}

/// 一个包在哪一组哪条轨，及它的输入要加的时间偏移（微秒）。
struct Placed {
    group: usize,
    track: usize,
    offset_us: i64,
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

/// 回读文件，按流下标统计包数；每读一个包查一次取消。读文件出错（FFmpeg 给的是 errno）为 [`Error::Io`]，其余为
/// [`Error::Reread`]。
fn count_packets(path: &Path, cancel: &Cancel) -> Result<Vec<u64>, Error> {
    let reread = |cause: ffmpeg::Error| {
        let cause = FfmpegError::from(cause);
        match file_error(&cause) {
            Some(cause) => Error::Io {
                action: "回读",
                path: path.to_path_buf(),
                cause,
            },
            None => Error::Reread {
                path: path.to_path_buf(),
                cause,
            },
        }
    };
    let mut ictx = format::input(path).map_err(reread)?;
    let mut counts = vec![0u64; ictx.nb_streams() as usize];
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut packet = Packet::empty();
        match packet.read(&mut ictx) {
            Ok(()) => counts[packet.stream()] += 1,
            Err(ffmpeg::Error::Eof) => return Ok(counts),
            Err(cause) => return Err(reread(cause)),
        }
    }
}
