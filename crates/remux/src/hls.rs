//! HLS 的合并：核对各不连续段组，逐组打开各轨、按全组共用的偏移平移、交错写出。

use std::path::Path;

use ffmpeg::Rescale;
use ffmpeg_next as ffmpeg;

use crate::chain::ffmpeg_path;
use crate::group::{layout_of, open_group, primed, references_of};
use crate::source::Source;
use crate::subtitles::{self, Lane};
use crate::timing::{MICROS, group_offset, wrap_shifts_us};
use crate::write::Muxer;
use crate::{
    Cancel, Container, Control, DiscontinuityGroup, Error, Label, Merged, Origin, Streams, Track,
    TrackSegments, Unsupported, WriteProgress, init, sync,
};

/// 把各不连续段组的分片复制进 `output`（`container` 的格式，MP4 时 moov 前置），写完回读核对每路流的包数并落盘。失败
/// 或取消时 `output` 上可能留有写了一半的文件，由调用方删除；已存在的 `output` 会被替换。`tracks[i]` 为第 i 条轨贡献的流
/// 与它的标注；字幕轨（[`Streams::Subtitles`]）的对齐见 `subtitles` 模块，`embed_subtitles` 为真时封进输出（Matroska 为
/// SubRip，MP4 为 mov_text），否则只在结果里给出对齐后的 cue。输出流的顺序：各音视频轨的流按轨号依次排开，其后是各字幕轨。
pub fn remux(
    tracks: &[Track],
    groups: &[DiscontinuityGroup],
    output: &Path,
    container: Container,
    embed_subtitles: bool,
    control: &Control,
) -> Result<Merged, Error> {
    init()?;
    let streams: Vec<Streams> = tracks.iter().map(|t| t.streams).collect();
    let groups: Vec<Numbered<'_>> = groups.iter().enumerate().collect();
    validate(&streams, &groups)?;
    let Some(path) = ffmpeg_path(output) else {
        return Err(Error::NonUtf8Path(output.to_path_buf()));
    };
    let merged = write_groups(tracks, &groups, path, container, embed_subtitles, control)?;
    sync(output)?;
    Ok(merged)
}

/// 核对能否合并进一个 `container` 格式的文件，不写输出，用于下载完全部分片之前尽早发现放不进的内容：各轨只给开头的
/// 分片。打开各组各音视频轨的输入、读到每路流的第一个包，核对编码受支持，视频不来自两条轨，各组的流种类、编码参数与
/// 解码配置与第 0 组相同，给出的分片有时间戳（打包音频与别的轨同组时），E-AC-3 的帧结构（MP4）。字幕轨不核对。
///
/// 只靠开头的分片判断不了的不报，留给合并：FFmpeg 打不开（如只有一两帧的 MP3）或探测不出参数（如开头没有这路流的包）
/// 的组跳过，第 0 组这样时其余各组只核对自身（编码、E-AC-3 的帧结构等），不与第 0 组比较；开头的分片里没有关键帧的组
/// 不核对逐帧的参数，流种类、编码参数与解码配置照样比较。通过不保证合并成功：组内中途的变化、后面的分片缺时间戳、
/// 时间戳不递增等要读到才知道。
///
/// `groups` 的各项为报错时用的组号（合并时这一组的下标）与这一组，按合并的顺序，第一项为第 0 组：可以只核对其中几组，
/// 如直播录制中拿第 0 组与新录到的一组核对。
pub fn check(
    streams: &[Streams],
    groups: &[(usize, &DiscontinuityGroup)],
    container: Container,
) -> Result<(), Error> {
    init()?;
    validate(streams, groups)?;
    let cancel = Cancel::never();
    let tracks = streams.len();
    // 第 0 组的流布局；它开头的分片预读判断不了（如没有关键帧）时逐包参照留空，流布局与参数照样拿来比较
    let first = match judgeable(open_group(streams, groups[0], None, container, &cancel))? {
        Some(first) => {
            let layout = layout_of(&first, tracks)?;
            let outputs = layout.iter().map(Vec::len).sum();
            let references = match judgeable(primed(first, &layout, &[]))? {
                Some(sources) => references_of(&sources, outputs),
                None => vec![None; outputs],
            };
            Some((layout, references))
        }
        None => None,
    };
    for &numbered in &groups[1..] {
        let layout = first.as_ref().map(|(layout, _)| layout.as_slice());
        judgeable(
            open_group(streams, numbered, layout, container, &cancel).and_then(
                |opened| match &first {
                    Some((layout, references)) => primed(opened, layout, references),
                    None => {
                        let own = layout_of(&opened, tracks)?;
                        primed(opened, &own, &[])
                    }
                },
            ),
        )?;
    }
    Ok(())
}

/// 一个不连续段组与报错时用的组号。
pub(crate) type Numbered<'a> = (usize, &'a DiscontinuityGroup);

/// 只给开头的分片时判断不了的结果当作没有结果（见 [`check`]）：合并时有整组的数据，同样的问题照常报出。
fn judgeable<T>(result: Result<T, Error>) -> Result<Option<T>, Error> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(
            Error::OpenInput { .. }
            | Error::Unsupported(Unsupported::ParamsUnknown { .. } | Unsupported::NoKeyframe { .. }),
        ) => Ok(None),
        Err(e) => Err(e),
    }
}

/// 合并 HLS 的各组写到 `path`，写完回读核对每路流的包数。
fn write_groups(
    tracks: &[Track],
    groups: &[Numbered<'_>],
    path: &str,
    container: Container,
    embed_subtitles: bool,
    control: &Control,
) -> Result<Merged, Error> {
    let cancel = &control.cancel;
    let streams: Vec<Streams> = tracks.iter().map(|t| t.streams).collect();
    let first = open_group(&streams, groups[0], None, container, cancel)?;
    let layout = layout_of(&first, tracks.len())?;
    let labels: Vec<&Label> = tracks.iter().map(|t| &t.label).collect();
    let subtitle_tracks: Vec<usize> = (0..tracks.len())
        .filter(|&t| streams[t] == Streams::Subtitles)
        .collect();
    let embedded = if embed_subtitles {
        subtitle_tracks.len()
    } else {
        0
    };
    let codec = subtitles::Codec::of(container);
    let mut muxer = Muxer::open(
        path,
        container,
        cancel,
        &first,
        &layout,
        &labels,
        &subtitle_tracks[..embedded],
    )?;
    let media_outputs: usize = layout.iter().map(Vec::len).sum();
    let mut lanes: Vec<Lane> = subtitle_tracks
        .iter()
        .enumerate()
        .map(|(n, &track)| Lane::new(track, embed_subtitles.then_some((media_outputs + n, codec))))
        .collect();
    let sources = primed(first, &layout, &[])?;
    let references = references_of(&sources, media_outputs);
    let (group, tracks0) = groups[0];
    write_group(
        &mut muxer,
        group,
        sources,
        &mut lanes,
        &tracks0.tracks,
        &control.progress,
    )?;
    for &numbered in &groups[1..] {
        let opened = open_group(&streams, numbered, Some(&layout), container, cancel)?;
        let sources = primed(opened, &layout, &references)?;
        write_group(
            &mut muxer,
            numbered.0,
            sources,
            &mut lanes,
            &numbered.1.tracks,
            &control.progress,
        )?;
    }
    let report = muxer.finish_verified(path, cancel)?;
    Ok(Merged {
        report,
        subtitles: lanes.into_iter().map(|lane| lane.cues).collect(),
    })
}

/// 写出一组（各输入已读到每路流的第一个包）：各输入按回绕对齐（见 [`wrap_shifts_us`]），再按全组共用的偏移平移时间戳，
/// 字幕轨按同一偏移对齐（见 [`Lane::load_group`]），按 DTS 交错写出，写出前核对时间戳没有往前跳（见
/// [`Source::check_jump`]）。
fn write_group(
    muxer: &mut Muxer,
    group: usize,
    mut sources: Vec<Source>,
    lanes: &mut [Lane],
    tracks: &[TrackSegments],
    progress: &WriteProgress,
) -> Result<(), Error> {
    let wraps = wrap_shifts_us(&sources);
    let (offset_us, reference_us) = offset_of(muxer, group, &sources, &wraps)?;
    for (source, wrap_us) in sources.iter_mut().zip(wraps) {
        source.offset_us = wrap_us + offset_us;
    }
    for lane in lanes.iter_mut() {
        lane.load_group(group, &tracks[lane.track], reference_us, offset_us)?;
    }
    // 全组已写出的最大 DTS，输出时间线上的微秒
    let mut clock_us: Option<i64> = None;
    // 各轨已报为写完的文件数（见 [`WriteProgress`]）
    let mut written = vec![0u64; tracks.len()];
    muxer.interleave(&mut sources, lanes, |source, packet, mapping, dts_us| {
        source.check_jump(packet, mapping, dts_us, clock_us)?;
        if let Some(segment) = source.segment() {
            let track = source.at.index();
            let done = u64::from(tracks[track].init.is_some()) + segment as u64;
            progress.add(done.saturating_sub(written[track]));
            written[track] = written[track].max(done);
        }
        clock_us = Some(clock_us.map_or(dts_us, |clock| clock.max(dts_us)));
        Ok(())
    })?;
    for (track, done) in tracks.iter().zip(written) {
        let files = track.segments.len() as u64 + u64::from(track.init.is_some());
        progress.add(files - done);
    }
    Ok(())
}

/// 本组的时间偏移（见 [`group_offset`]）与全组最早的 PTS（输入时间线、已按回绕对齐，微秒），由各输出流本组的首个 DTS
/// 与全组最早的 PTS（各输入加上回绕对齐的量
/// `wraps_us` 之后的输入时间线，微秒，取自各输入已读出的包；队列内同一路流按解码顺序排列）算出。组间余量为最粗的
/// 输出时间基的一个 tick（向上取整到微秒）：换到输出时间基时的舍入不超过半个 tick，因此下一组的首个 DTS 一定大于
/// 上一组的末个 DTS。
fn offset_of(
    muxer: &Muxer,
    group: usize,
    sources: &[Source],
    wraps_us: &[i64],
) -> Result<(i64, i64), Error> {
    let mut first_dts_us: Vec<Option<i64>> = vec![None; muxer.outputs()];
    let mut min_pts_us: Option<i64> = None;
    for (source, &wrap_us) in sources.iter().zip(wraps_us) {
        for (packet, mapping) in &source.queue {
            let tb = mapping.time_base;
            let us = |ts: i64| ts.rescale(tb, MICROS) + wrap_us;
            let dts = us(packet.dts().expect("队列中的包都带 DTS"));
            let pts = packet.pts().map_or(dts, us);
            first_dts_us[mapping.output].get_or_insert(dts);
            min_pts_us = Some(min_pts_us.map_or(pts, |m| m.min(pts)));
        }
    }
    let min_pts_us = min_pts_us.ok_or(Error::EmptyGroup { group })?;
    let (ends_us, last_dts_us): (Vec<_>, Vec<_>) = muxer.written_until().into_iter().unzip();
    let offset_us = group_offset(
        &ends_us,
        &last_dts_us,
        &first_dts_us,
        min_pts_us,
        muxer.coarsest_tick_us(),
    );
    Ok((offset_us, min_pts_us))
}

fn validate(streams: &[Streams], groups: &[Numbered<'_>]) -> Result<(), Error> {
    let (_, first) = groups.first().ok_or(Error::NoGroups)?;
    if streams.len() != first.tracks.len() {
        return Err(Error::StreamsCount {
            expected: first.tracks.len(),
            found: streams.len(),
        });
    }
    for &(group, g) in groups {
        if g.tracks.len() != first.tracks.len() {
            return Err(Error::TrackCount {
                group,
                expected: first.tracks.len(),
                found: g.tracks.len(),
            });
        }
        for (track, t) in g.tracks.iter().enumerate() {
            if t.segments.is_empty() {
                return Err(Error::EmptyTrack {
                    at: Origin::Track { group, track },
                });
            }
            let ascending = t.gaps.windows(2).all(|w| w[0] < w[1]);
            if !ascending || t.gaps.iter().any(|&i| i == 0 || i >= t.segments.len()) {
                return Err(Error::InvalidGaps {
                    at: Origin::Track { group, track },
                });
            }
        }
    }
    Ok(())
}
