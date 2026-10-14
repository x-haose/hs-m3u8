//! 写出 MP4：按第 0 组建立输出流，逐组平移时间戳、交错写出，写完回读核对包数。

use std::path::Path;

use ffmpeg::{Dictionary, Packet, Rational, Rescale, Rounding, codec, encoder, format};
use ffmpeg_next as ffmpeg;

use crate::compare::layout_of;
use crate::group::{OpenTrack, open_group, primed, references_of};
use crate::source::{Dropped, Source};
use crate::timing::{MICROS, align_wraps, group_offset, retime};
use crate::{Error, Numbered, Report, Shape, StreamReport, Streams, Unsupported, ffi};

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

pub(crate) fn write_verified(
    streams: &[Streams],
    groups: &[Numbered<'_>],
    part: &Path,
) -> Result<Report, Error> {
    let first = open_group(streams, groups[0], None)?;
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

    let sources = primed(groups[0].0, first, &layout, &[])?;
    let references = references_of(&sources, outs.len());
    write_group(groups[0].0, sources, &mut octx, &mut outs, margin_us)?;
    for &numbered in &groups[1..] {
        let opened = open_group(streams, numbered, Some(&layout))?;
        let sources = primed(numbered.0, opened, &layout, &references)?;
        write_group(numbered.0, sources, &mut octx, &mut outs, margin_us)?;
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
                skipped_before_keyframe: o.dropped.before_keyframe,
                discarded_corrupt_bytes: o.dropped.corrupt_bytes,
                duration_us: match (o.start_us, o.end_us) {
                    (Some(start), Some(end)) => u64::try_from(end - start).unwrap_or(0),
                    _ => 0,
                },
            })
            .collect(),
    })
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
                total.corrupt_bytes += dropped.corrupt_bytes;
                total.before_keyframe += dropped.before_keyframe;
            }
        }
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
