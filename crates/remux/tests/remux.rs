//! remux 的编解码测试：用 tests/fixtures/media 下按 1 秒切好的合成 HLS 分片，核对输出的包数、时间线与 MP4 结构。

use std::path::{Path, PathBuf};

use ffmpeg_next as ffmpeg;
use hs_m3u8_remux::{
    Cancel, DiscontinuityGroup, Error, Segment, Shape, StreamKind, Streams, TrackSegments,
    Unsupported, remux,
};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media")
}

/// 每个测试独立的空目录。
fn work_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 目录下一条轨的分片：有 init.mp4 时作为 init 段，seg<N>.* 按 N 排序。
fn track_in(dir: &Path) -> TrackSegments {
    let init = dir.join("init.mp4");
    let mut segments: Vec<(u32, PathBuf)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.strip_prefix("seg")?.parse().ok()?;
            Some((stem, p))
        })
        .collect();
    segments.sort();
    let init = init.exists().then_some(init);
    TrackSegments {
        segments: segments
            .into_iter()
            .map(|(_, p)| declared(init.as_deref(), p))
            .collect(),
        init,
        gaps: Vec::new(),
    }
}

/// 分片 `path` 带上声明的时长（EXTINF）：取它的实际时长，即各路流从最早的呈现时刻到最晚的结束时刻，相当于来源写的
/// EXTINF 准确；fMP4 连同 `init` 读。读不出时长（不是完整的媒体）时为 0。
fn declared(init: Option<&Path>, path: PathBuf) -> Segment {
    let media = match init {
        Some(init) => {
            static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("declared");
            std::fs::create_dir_all(&dir).unwrap();
            let joined = dir.join(format!("{}-{n}.mp4", std::process::id()));
            let data = [std::fs::read(init).unwrap(), std::fs::read(&path).unwrap()].concat();
            std::fs::write(&joined, data).unwrap();
            joined
        }
        None => path.clone(),
    };
    let duration_us = media_span(&media).map_or(0, |span| (span * 1e6).round() as u64);
    Segment { path, duration_us }
}

/// 没有 init 段的各分片（TS 与打包音频），带上声明的时长（见 [`declared`]）。
fn segs(paths: impl IntoIterator<Item = PathBuf>) -> Vec<Segment> {
    paths.into_iter().map(|path| declared(None, path)).collect()
}

/// 文件里各路流从最早的呈现时刻到最晚的结束时刻，秒；打不开或读不出包时为 None。
fn media_span(path: &Path) -> Option<f64> {
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(path).ok()?;
    let bases: Vec<f64> = ictx.streams().map(|s| f64::from(s.time_base())).collect();
    let (mut start, mut end) = (f64::INFINITY, f64::NEG_INFINITY);
    loop {
        let mut packet = ffmpeg::Packet::empty();
        if packet.read(&mut ictx).is_err() {
            break;
        }
        let tb = bases[packet.stream()];
        let Some(pts) = packet.pts().or(packet.dts()) else {
            continue;
        };
        start = start.min(pts as f64 * tb);
        end = end.max((pts + packet.duration()) as f64 * tb);
    }
    (end > start).then_some(end - start)
}

fn track(name: &str) -> TrackSegments {
    track_in(&fixtures().join(name))
}

/// 按顺序核对各组（组号即下标），见 [`hs_m3u8_remux::check`]。
fn check(streams: &[Streams], groups: &[DiscontinuityGroup]) -> Result<(), Error> {
    let numbered: Vec<(usize, &DiscontinuityGroup)> = groups.iter().enumerate().collect();
    hs_m3u8_remux::check(streams, &numbered)
}

fn group<const N: usize>(tracks: [TrackSegments; N]) -> DiscontinuityGroup {
    DiscontinuityGroup {
        tracks: tracks.into(),
    }
}

/// 一路流的包：(pts 秒, 结束时刻秒)，按文件中的顺序。
type Timeline = Vec<(f64, f64)>;

/// 文件中第一路视频与第一路音频的包时间线。
fn timelines(path: &Path) -> (Timeline, Timeline) {
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(path).unwrap();
    let first = |medium| {
        ictx.streams()
            .find(|s| s.parameters().medium() == medium)
            .map(|s| (s.index(), f64::from(s.time_base())))
    };
    let (video, audio) = (
        first(ffmpeg::media::Type::Video),
        first(ffmpeg::media::Type::Audio),
    );
    let (mut v, mut a) = (Vec::new(), Vec::new());
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut ictx) {
            Ok(()) => {
                let target = match (video, audio) {
                    (Some((i, tb)), _) if i == packet.stream() => Some((&mut v, tb)),
                    (_, Some((i, tb))) if i == packet.stream() => Some((&mut a, tb)),
                    _ => None,
                };
                if let Some((list, tb)) = target {
                    let pts = packet.pts().or(packet.dts()).unwrap() as f64 * tb;
                    list.push((pts, pts + packet.duration() as f64 * tb));
                }
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => panic!("读取 {} 失败: {e}", path.display()),
        }
    }
    (v, a)
}

/// 把一条轨的 init 段与分片按字节拼成一个文件，便于用 FFmpeg 直接读取输入的时间线。
fn concat_track(t: &TrackSegments, dir: &Path, name: &str) -> PathBuf {
    let mut bytes = Vec::new();
    for p in t.init.iter().chain(t.segments.iter().map(|s| &s.path)) {
        bytes.extend(std::fs::read(p).unwrap());
    }
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// 打包音频样本 `name`（packed 下的目录）的各分片与各自的时长（90 kHz 时钟的 tick）。分片是裸的音频帧，不带
/// ID3 标签。
fn packed_fixture(name: &str) -> Vec<(PathBuf, u64)> {
    track(&format!("packed/{name}"))
        .segments
        .into_iter()
        .map(|segment| {
            let ticks = (span(&timelines(&segment.path).1) * 90_000.0).round() as u64;
            (segment.path, ticks)
        })
        .collect()
}

/// 在 `dir` 下写一个打包音频分片：`src` 的音频帧前面加上 ID3v2.4 标签，其中的 PRIV 帧给出时间戳 `timestamp`
/// （90 kHz）；`timestamp` 为 None 时原样复制。
fn packed_segment(dir: &Path, src: &Path, timestamp: Option<u64>) -> PathBuf {
    let audio = std::fs::read(src).unwrap();
    let data = match timestamp {
        Some(timestamp) => {
            let owner = b"com.apple.streaming.transportStreamTimestamp\0";
            let frame_data = [owner.as_slice(), &timestamp.to_be_bytes()].concat();
            let syncsafe =
                |size: usize| [size >> 21, size >> 14, size >> 7, size].map(|b| (b & 0x7F) as u8);
            let frame = [
                b"PRIV".as_slice(),
                &syncsafe(frame_data.len()),
                &[0, 0],
                &frame_data,
            ]
            .concat();
            [
                b"ID3\x04\x00\x00".as_slice(),
                &syncsafe(frame.len()),
                &frame,
                &audio,
            ]
            .concat()
        }
        None => audio,
    };
    let name = format!(
        "{}-{}",
        timestamp.map_or("untimed".to_owned(), |t| t.to_string()),
        src.file_name().unwrap().to_str().unwrap()
    );
    let path = dir.join(name);
    std::fs::write(&path, data).unwrap();
    path
}

/// 文件开头 `frames` 个包（打包音频样本里即前几帧）的字节。
fn first_frames(path: &Path, frames: usize) -> Vec<u8> {
    let len: usize = packet_sizes(path)[..frames].iter().sum();
    std::fs::read(path).unwrap()[..len].to_vec()
}

/// 文件里各包的字节数，按读出的顺序（裸音频样本里即各帧）。
fn packet_sizes(path: &Path) -> Vec<usize> {
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(path).unwrap();
    let mut sizes = Vec::new();
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut ictx) {
            Ok(()) => sizes.push(packet.size()),
            Err(ffmpeg::Error::Eof) => return sizes,
            Err(e) => panic!("{e}"),
        }
    }
}

/// 文件里各路流的编码名，按流的顺序。
fn codecs(path: &Path) -> Vec<&'static str> {
    ffmpeg::init().unwrap();
    let ictx = ffmpeg::format::input(path).unwrap();
    ictx.streams().map(|s| s.parameters().id().name()).collect()
}

/// MP4 里 AC-3 样本描述（dac3）或 E-AC-3 样本描述（dec3，第一个独立子流）的 acmod 与 lfeon（ETSI TS 102 366
/// 附录 F）；别的编码为 None。
fn ac3_layout(path: &Path, codec: &str) -> Option<(u8, bool)> {
    let data = std::fs::read(path).unwrap();
    let payload = |name: &[u8; 4]| {
        let at = data.windows(4).position(|w| w == name).unwrap();
        data[at + 4..at + 8].to_vec()
    };
    match codec {
        // fscod 2 位、bsid 5 位、bsmod 3 位、acmod 3 位、lfeon 1 位……
        "ac3" => {
            let dac3 = payload(b"dac3");
            let bits = u32::from_be_bytes([0, dac3[0], dac3[1], dac3[2]]);
            Some(((bits >> 11 & 7) as u8, bits >> 10 & 1 == 1))
        }
        // data_rate 13 位、num_ind_sub 3 位，再是第一个独立子流：fscod 2 位、bsid 5 位、保留 1 位、asvc 1 位、
        // bsmod 3 位、acmod 3 位、lfeon 1 位……
        "eac3" => {
            let byte = payload(b"dec3")[3];
            Some((byte >> 1 & 7, byte & 1 == 1))
        }
        _ => None,
    }
}

/// 时间线上最早的呈现时刻，秒。
fn earliest(timeline: &Timeline) -> f64 {
    timeline.iter().map(|p| p.0).fold(f64::INFINITY, f64::min)
}

fn span(timeline: &Timeline) -> f64 {
    let start = timeline.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    let end = timeline
        .iter()
        .map(|p| p.1)
        .fold(f64::NEG_INFINITY, f64::max);
    end - start
}

/// 解码文件中的第一路视频：(送进解码器的包数, 解码器拒收的包数, 解出的帧数)。
fn decode_video(path: &Path) -> (usize, usize, usize) {
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(path).unwrap();
    let stream = ictx.streams().best(ffmpeg::media::Type::Video).unwrap();
    let index = stream.index();
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .unwrap()
        .decoder()
        .video()
        .unwrap();
    let mut frame = ffmpeg::frame::Video::empty();
    let (mut packets, mut rejected, mut frames) = (0, 0, 0);
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut ictx) {
            Ok(()) if packet.stream() == index => {
                packets += 1;
                if decoder.send_packet(&packet).is_err() {
                    rejected += 1;
                }
                while decoder.receive_frame(&mut frame).is_ok() {
                    frames += 1;
                }
            }
            Ok(()) => {}
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => panic!("读取 {} 失败: {e}", path.display()),
        }
    }
    decoder.send_eof().unwrap();
    while decoder.receive_frame(&mut frame).is_ok() {
        frames += 1;
    }
    (packets, rejected, frames)
}

/// MP4 顶层 box 的类型序列。
fn top_level_boxes(path: &Path) -> Vec<[u8; 4]> {
    let data = std::fs::read(path).unwrap();
    let mut boxes = Vec::new();
    let mut offset = 0usize;
    while offset + 8 <= data.len() {
        let size = u64::from(u32::from_be_bytes(
            data[offset..offset + 4].try_into().unwrap(),
        ));
        boxes.push(data[offset + 4..offset + 8].try_into().unwrap());
        let size = match size {
            1 => u64::from_be_bytes(data[offset + 8..offset + 16].try_into().unwrap()),
            0 => (data.len() - offset) as u64,
            n => n,
        };
        offset += usize::try_from(size).unwrap();
    }
    boxes
}

#[test]
fn ts_single_group_copies_every_packet_from_zero_with_moov_first() {
    let dir = work_dir("ts_single_group");
    let output = dir.join("out.mp4");
    let a = track("ts_a");

    let report = remux(
        &[Streams::All],
        &[group([a.clone()])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let (v_in, a_in) = timelines(&concat_track(&a, &dir, "in.ts"));
    let streams: Vec<_> = report
        .streams
        .iter()
        .map(|s| {
            (
                s.shape.kind(),
                s.shape.codec(),
                s.packets,
                s.skipped_without_dts,
            )
        })
        .collect();
    assert_eq!(
        streams,
        vec![
            (StreamKind::Video, "h264", v_in.len() as u64, 0),
            (StreamKind::Audio, "aac", a_in.len() as u64, 0)
        ]
    );
    let (v_out, a_out) = timelines(&output);
    assert_eq!((v_out.len(), a_out.len()), (v_in.len(), a_in.len()));
    assert!(matches!(
        report.streams[0].shape,
        Shape::Video {
            width: 320,
            height: 180,
            ..
        }
    ));
    assert!(matches!(
        report.streams[1].shape,
        Shape::Audio {
            sample_rate: 48_000,
            ..
        }
    ));
    // 报告的时长与回读的时间线一致（误差在一个输出时间基 tick 内）
    for (stream, timeline) in report.streams.iter().zip([&v_out, &a_out]) {
        let read_back_us = span(timeline) * 1e6;
        assert!(
            (stream.duration_us as f64 - read_back_us).abs() < 1000.0,
            "报告 {} µs，回读 {read_back_us} µs",
            stream.duration_us
        );
    }
    let start = v_out
        .iter()
        .chain(&a_out)
        .map(|p| p.0)
        .fold(f64::INFINITY, f64::min);
    // 有 B 帧的视频首个 DTS 早于 PTS：最早的呈现时刻仍是 0（误差在一个输出时间基 tick 内），开头不留空白
    assert!(start.abs() < 1e-4, "输出应从 0 开始，实际 {start}");

    let boxes = top_level_boxes(&output);
    let pos = |name: &[u8; 4]| boxes.iter().position(|b| b == name).unwrap();
    assert!(
        pos(b"moov") < pos(b"mdat"),
        "moov 应前置，实际顺序 {boxes:?}"
    );
}

#[test]
fn split_fmp4_renditions_merge_into_one_file() {
    let dir = work_dir("split_fmp4");
    let output = dir.join("out.mp4");
    let (video, audio) = (track("fmp4_a/video"), track("fmp4_a/audio"));

    let report = remux(
        &[Streams::Video, Streams::Audio],
        &[group([video.clone(), audio.clone()])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let v_in = timelines(&concat_track(&video, &dir, "v.mp4")).0;
    let a_in = timelines(&concat_track(&audio, &dir, "a.mp4")).1;
    let (v_out, a_out) = timelines(&output);
    assert_eq!((v_out.len(), a_out.len()), (v_in.len(), a_in.len()));
    let kinds: Vec<_> = report
        .streams
        .iter()
        .map(|s| (s.shape.kind(), s.shape.codec()))
        .collect();
    assert_eq!(
        kinds,
        vec![(StreamKind::Video, "h264"), (StreamKind::Audio, "aac")]
    );
}

#[test]
fn discontinuity_groups_are_laid_end_to_end() {
    let dir = work_dir("discontinuity");
    let output = dir.join("out.mp4");
    let (a, b) = (track("ts_a"), track("ts_b"));
    let (va, aa) = timelines(&concat_track(&a, &dir, "a.ts"));
    let (vb, ab) = timelines(&concat_track(&b, &dir, "b.ts"));

    remux(
        &[Streams::All],
        &[group([a.clone()]), group([b]), group([a])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let (v_out, a_out) = timelines(&output);
    assert_eq!(v_out.len(), 2 * va.len() + vb.len());
    assert_eq!(a_out.len(), 2 * aa.len() + ab.len());
    let expected = 2.0 * span(&va) + span(&vb);
    assert!(
        (span(&v_out) - expected).abs() < 0.1,
        "视频总时长 {} 应约为 {expected}",
        span(&v_out)
    );
}

#[test]
fn split_renditions_keep_their_relative_timing_across_discontinuities() {
    let dir = work_dir("split_discontinuity");
    let output = dir.join("out.mp4");
    let (va, aa) = (track("fmp4_a/video"), track("fmp4_a/audio"));
    let (vb, ab) = (track("fmp4_b/video"), track("fmp4_b/audio"));
    let va_in = timelines(&concat_track(&va, &dir, "va.mp4")).0;
    let aa_in = timelines(&concat_track(&aa, &dir, "aa.mp4")).1;
    let vb_in = timelines(&concat_track(&vb, &dir, "vb.mp4")).0;
    let ab_in = timelines(&concat_track(&ab, &dir, "ab.mp4")).1;

    remux(
        &[Streams::Video, Streams::Audio],
        &[group([va, aa]), group([vb, ab])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let (v_out, a_out) = timelines(&output);
    assert_eq!(
        (v_out.len(), a_out.len()),
        (va_in.len() + vb_in.len(), aa_in.len() + ab_in.len())
    );
    // 第 1 组的首个视频包与首个音频包之间的时间差，应与节目 B 输入中的一致（整组同偏移）
    let delta_in = vb_in[0].0 - ab_in[0].0;
    let delta_out = v_out[va_in.len()].0 - a_out[aa_in.len()].0;
    assert!(
        (delta_out - delta_in).abs() < 0.001,
        "组内音视频相对时序应保持：输入 {delta_in}，输出 {delta_out}"
    );
    // 第 1 组紧接第 0 组：不重叠，空隙不超过一个视频帧（40ms）。
    // 第 0 组的结束时刻用输入包的真实时长计算：MP4 以相邻 DTS 之差存样本时长，读回时组内末包的时长会被拉到下一组
    let end_of = |out: &[(f64, f64)], input: &Timeline| {
        out.iter()
            .zip(input)
            .map(|(o, i)| o.0 + (i.1 - i.0))
            .fold(f64::NEG_INFINITY, f64::max)
    };
    let group0_end =
        end_of(&v_out[..va_in.len()], &va_in).max(end_of(&a_out[..aa_in.len()], &aa_in));
    let group1_start = v_out[va_in.len()..]
        .iter()
        .chain(&a_out[aa_in.len()..])
        .map(|p| p.0)
        .fold(f64::INFINITY, f64::min);
    assert!(
        (group0_end - 0.001..=group0_end + 0.041).contains(&group1_start),
        "第 1 组起点 {group1_start} 应紧接第 0 组终点 {group0_end}"
    );
}

#[test]
fn hevc_output_is_tagged_hvc1() {
    let dir = work_dir("hevc");
    let output = dir.join("out.mp4");
    let hevc = track("ts_hevc");

    remux(
        &[Streams::All],
        &[group([hevc.clone()])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let (v_in, a_in) = timelines(&concat_track(&hevc, &dir, "in.ts"));
    let (v_out, a_out) = timelines(&output);
    assert_eq!((v_out.len(), a_out.len()), (v_in.len(), a_in.len()));
    let data = std::fs::read(&output).unwrap();
    let has = |tag: &[u8]| data.windows(4).any(|w| w == tag);
    assert!(has(b"hvc1") && !has(b"hev1"), "HEVC 输出流应标 hvc1");
}

#[test]
fn paths_with_quotes_and_spaces_are_read() {
    let dir = work_dir("quoted_paths").join("it's a dir");
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["seg0.ts", "seg1.ts"] {
        std::fs::copy(fixtures().join("ts_a").join(name), dir.join(name)).unwrap();
    }
    let output = dir.join("out file.mp4");

    let report = remux(
        &[Streams::All],
        &[group([track_in(&dir)])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    assert_eq!(report.streams.len(), 2);
    assert!(output.exists());
}

#[test]
fn timestamps_going_back_within_a_group_fail() {
    let dir = work_dir("regression");
    let output = dir.join("out.mp4");
    // 节目 B 接在 A 后面却放在同一组：时间戳回退，相当于漏标 EXT-X-DISCONTINUITY
    let mut mixed = track("ts_a");
    mixed.segments.extend(track("ts_b").segments);

    let err = remux(
        &[Streams::All],
        &[group([mixed])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::DtsNotIncreasing {
                group: 0,
                track: 0,
                ..
            })
        ),
        "实际 {err:?}"
    );
}

/// 相邻分片首尾重复了一帧：两帧的解码时间戳相同，同样放不进同一条 MP4 轨。
#[test]
fn a_frame_repeated_across_segments_fails() {
    let dir = work_dir("repeated_frame");
    let output = dir.join("out.mp4");

    let err = remux(
        &[Streams::Video],
        &[group([track("ts_repeat")])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::DtsNotIncreasing {
                group: 0,
                track: 0,
                kind: StreamKind::Video,
            })
        ),
        "实际 {err:?}"
    );
}

#[test]
fn same_stream_kind_from_two_tracks_is_rejected() {
    let dir = work_dir("duplicate");
    let output = dir.join("out.mp4");

    let err = remux(
        &[Streams::All; 2],
        &[group([track("ts_a"), track("fmp4_a/audio")])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            Error::DuplicateKind {
                track: 1,
                kind: StreamKind::Audio
            }
        ),
        "实际 {err:?}"
    );
}

/// 另一条轨提供音频时只取本轨的视频：本轨混着的音频不进输出，其编码（TS 里的 Opus，放不进 MP4）也不检查。
#[test]
fn unwanted_streams_are_dropped_without_codec_checks() {
    let dir = work_dir("unwanted_streams");
    let output = dir.join("out.mp4");
    let (muxed, audio) = (track("ts_opus"), track("fmp4_a/audio"));

    let report = remux(
        &[Streams::Video, Streams::Audio],
        &[group([muxed.clone(), audio.clone()])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let v_in = timelines(&concat_track(&muxed, &dir, "v.ts")).0;
    let a_in = timelines(&concat_track(&audio, &dir, "a.mp4")).1;
    let streams: Vec<_> = report
        .streams
        .iter()
        .map(|s| (s.shape.kind(), s.shape.codec(), s.packets))
        .collect();
    assert_eq!(
        streams,
        vec![
            (StreamKind::Video, "h264", v_in.len() as u64),
            (StreamKind::Audio, "aac", a_in.len() as u64)
        ]
    );
}

/// 只有音频的变体又选了独立音频：变体那条轨只取视频而没有，什么也不贡献，输出只有独立音频；各轨都没有要取的流
/// 时放不进 MP4。
#[test]
fn tracks_without_the_wanted_streams_contribute_nothing() {
    let dir = work_dir("nothing_wanted");
    let output = dir.join("out.mp4");
    let (audio_only, audio) = (track("fmp4_a/audio"), track("fmp4_b/audio"));

    let report = remux(
        &[Streams::Video, Streams::Audio],
        &[group([audio_only.clone(), audio.clone()])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let a_in = timelines(&concat_track(&audio, &dir, "a.mp4")).1;
    let streams: Vec<_> = report
        .streams
        .iter()
        .map(|s| (s.shape.kind(), s.packets))
        .collect();
    assert_eq!(streams, vec![(StreamKind::Audio, a_in.len() as u64)]);

    let err = remux(
        &[Streams::Video],
        &[group([audio_only])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::Unsupported(Unsupported::NoStreams)),
        "实际 {err:?}"
    );
}

#[test]
fn resolution_change_between_groups_is_rejected() {
    let dir = work_dir("params_changed");
    let output = dir.join("out.mp4");

    let err = remux(
        &[Streams::All],
        &[group([track("ts_a")]), group([track("ts_small")])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::ParamsChanged {
                group: 1,
                track: 0,
                kind: StreamKind::Video,
                ..
            })
        ),
        "实际 {err:?}"
    );
}

/// fMP4 的 AAC：AudioSpecificConfig 写法不同而等价（FFmpeg 的编码器显式写出没有 SBR，AudioToolbox 的省略），两种
/// 顺序都照常合并。
#[test]
fn equivalent_aac_configs_from_two_encoders_merge() {
    let dir = work_dir("equivalent_aac");
    let output = dir.join("out.mp4");
    for (first, second) in [
        ("fmp4_a/audio", "fmp4_aac_at"),
        ("fmp4_aac_at", "fmp4_a/audio"),
    ] {
        let groups = [group([track(first)]), group([track(second)])];
        check(&[Streams::Audio], &groups).unwrap();
        remux(&[Streams::Audio], &groups, &output, &Cancel::never()).unwrap();
    }
}

/// 视频的色度采样变了（4:2:0 与 4:4:4，宽高相同）：跨组与组内都报出。TS 的参数集随帧携带，FFmpeg 按带内的解得出，
/// 只认样本描述的解码器（如 AVFoundation）解不出。
#[test]
fn chroma_changes_are_rejected() {
    let dir = work_dir("chroma_changed");
    let output = dir.join("out.mp4");
    let changing = track("ts_chroma_change");
    let err = remux(
        &[Streams::Video],
        &[group([changing.clone()])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::ParamsChangedInGroup {
                group: 0,
                track: 0,
                kind: StreamKind::Video
            })
        ),
        "组内：实际 {err:?}"
    );

    let first = TrackSegments {
        init: None,
        segments: changing.segments[..1].to_vec(),
        gaps: Vec::new(),
    };
    let second = TrackSegments {
        init: None,
        segments: changing.segments[1..].to_vec(),
        gaps: Vec::new(),
    };
    let err = remux(
        &[Streams::Video],
        &[group([first]), group([second])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::ParamsChanged {
                group: 1,
                first: Shape::Video {
                    pixel_format: Some("yuv420p"),
                    ..
                },
                found: Shape::Video {
                    pixel_format: Some("yuv444p"),
                    ..
                },
                ..
            })
        ),
        "跨组：实际 {err:?}"
    );
}

/// 编码参数相同而解码配置不同的两组：fMP4 的 H.264 High 与 Baseline（avcC 不同）、TS 里的 AAC LC 与 HE-AAC（都解码为
/// 48 kHz 双声道，ADTS 帧头不同）、一组 fMP4 一组 TS 的 AAC。MP4 的样本描述由第 0 组的写成，用在后面的组上解不出。
#[test]
fn decoder_configs_differing_between_groups_are_rejected() {
    let dir = work_dir("config_changed");
    let output = dir.join("out.mp4");
    let cases = [
        (
            Streams::Video,
            "fmp4_alac",
            "fmp4_baseline",
            StreamKind::Video,
        ),
        (
            Streams::All,
            "ts_aac_stereo",
            "ts_he_aac",
            StreamKind::Audio,
        ),
        (
            Streams::All,
            "ts_he_aac",
            "ts_aac_stereo",
            StreamKind::Audio,
        ),
        (Streams::Audio, "fmp4_a/audio", "ts_a", StreamKind::Audio),
        (
            Streams::Audio,
            "fmp4_aac_stereo",
            "fmp4_he_aac",
            StreamKind::Audio,
        ),
    ];
    for (streams, first, second, kind) in cases {
        let err = remux(
            &[streams],
            &[group([track(first)]), group([track(second)])],
            &output,
            &Cancel::never(),
        )
        .unwrap_err();
        assert!(
            matches!(
                &err,
                Error::Unsupported(Unsupported::ConfigChanged { group: 1, track: 0, kind: found })
                    if *found == kind
            ),
            "{first} 接 {second}：实际 {err:?}"
        );
    }
}

/// fMP4 里的 AV1、VP9 视频与 Opus、FLAC、ALAC 音频原样复制，MP4 里的样本描述带各自的配置 box。
#[test]
fn av1_vp9_opus_flac_and_alac_are_copied() {
    let dir = work_dir("fmp4_codecs");
    for (name, expected, boxes) in [
        ("fmp4_av1_opus", ["av1", "opus"], [b"av1C", b"dOps"]),
        ("fmp4_vp9_flac", ["vp9", "flac"], [b"vpcC", b"dfLa"]),
        ("fmp4_alac", ["h264", "alac"], [b"avcC", b"alac"]),
    ] {
        let muxed = track(name);
        let output = dir.join(format!("{name}.mp4"));
        let report = remux(
            &[Streams::All],
            &[group([muxed.clone()])],
            &output,
            &Cancel::never(),
        )
        .unwrap();

        let (v_in, a_in) = timelines(&concat_track(&muxed, &dir, "in.mp4"));
        let found: Vec<(&str, u64)> = report
            .streams
            .iter()
            .map(|s| (s.shape.codec(), s.packets))
            .collect();
        assert_eq!(
            found,
            [
                (expected[0], v_in.len() as u64),
                (expected[1], a_in.len() as u64)
            ],
            "{name}"
        );
        assert_eq!(codecs(&output), expected, "{name}");
        let data = std::fs::read(&output).unwrap();
        for b in boxes {
            assert!(
                data.windows(4).any(|w| w == b),
                "{name} 的输出应有 {}",
                String::from_utf8_lossy(b)
            );
        }
    }
}

/// 组内中途换了编码参数：TS 的画面宽高与采样率在第二个分片变了，MP4 的一条轨只有一个样本描述。
#[test]
fn parameter_changes_within_a_group_are_rejected() {
    let dir = work_dir("params_in_group");
    let output = dir.join("out.mp4");
    let changed = track("ts_param_change");
    for (streams, kind) in [
        (Streams::Video, StreamKind::Video),
        (Streams::Audio, StreamKind::Audio),
    ] {
        let err = remux(
            &[streams],
            &[group([changed.clone()])],
            &output,
            &Cancel::never(),
        )
        .unwrap_err();
        assert!(
            matches!(
                &err,
                Error::Unsupported(Unsupported::ParamsChangedInGroup { group: 0, track: 0, kind: found })
                    if *found == kind
            ),
            "{kind}：实际 {err:?}"
        );
    }
}

/// 在第 `at` 次检查取消时（返回真之前）做 `act`、并返回它的结果的取消信号，另返回已检查的次数。
fn counting_cancel(
    act: impl Fn(usize) -> bool + Send + Sync + 'static,
) -> (Cancel, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let checks = Arc::new(AtomicUsize::new(0));
    let counted = checks.clone();
    let cancel = Cancel::new(move || act(counted.fetch_add(1, Ordering::Relaxed)));
    (cancel, checks)
}

/// 取消后合并在下一处检查以 Cancelled 停下：打开输入（含探测时读后面的数据）、写出每个包、FFmpeg 读写输出文件（含
/// moov 前置的第二遍）与回读核对，在哪一处检查时取消都是 Cancelled，不会报成别的错误。逐个检查点都试：音频晚 1.2 秒
/// 开始的 TS（探测要读到后面），与音视频分离的 fMP4。
#[test]
fn a_merge_cancelled_at_any_check_stops_as_cancelled() {
    use std::sync::atomic::Ordering;

    let dir = work_dir("cancelled");
    let output = dir.join("out.mp4");
    let cases = [
        (vec![Streams::All], vec![group([track("ts_late_audio")])]),
        (
            vec![Streams::Video, Streams::Audio],
            vec![group([track("fmp4_a/video"), track("fmp4_a/audio")])],
        ),
    ];
    for (streams, groups) in &cases {
        let (cancel, checks) = counting_cancel(|_| false);
        remux(streams, groups, &output, &cancel).unwrap();
        let total = checks.load(Ordering::Relaxed);
        for from in 0..total {
            let (cancel, _) = counting_cancel(move |n| n >= from);
            let result = remux(streams, groups, &output, &cancel);
            assert!(
                matches!(result, Err(Error::Cancelled)),
                "第 {from} 次检查（共 {total} 次）起取消：{result:?}"
            );
        }
    }
}

/// 合并中途分片文件变短（在第 n 次检查时把第 2 片截掉一半）：报读文件出错，或在截短之前已读完这一片而照常合并出完整
/// 的输出；不会读了一半就当作读完，合并出缺内容的 MP4 而报成功。逐个检查点都试。
#[test]
fn a_segment_shrinking_during_the_merge_is_reported() {
    use std::sync::atomic::Ordering;

    let dir = work_dir("shrinking");
    let output = dir.join("out.mp4");
    let source = track("ts_long");
    let complete = {
        let report = remux(
            &[Streams::All],
            &[group([source.clone()])],
            &output,
            &Cancel::never(),
        )
        .unwrap();
        report.streams.iter().map(|s| s.packets).collect::<Vec<_>>()
    };
    let (cancel, checks) = counting_cancel(|_| false);
    remux(
        &[Streams::All],
        &[group([source.clone()])],
        &output,
        &cancel,
    )
    .unwrap();
    let total = checks.load(Ordering::Relaxed);
    for at in 0..total {
        let copy = dir.join(format!("copy{at}"));
        std::fs::create_dir_all(&copy).unwrap();
        let segments: Vec<Segment> = source
            .segments
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let path = copy.join(format!("seg{i}.ts"));
                std::fs::copy(&s.path, &path).unwrap();
                Segment {
                    path,
                    duration_us: s.duration_us,
                }
            })
            .collect();
        let victim = segments[2].path.clone();
        let (cancel, _) = counting_cancel(move |n| {
            if n == at {
                let file = std::fs::File::options().write(true).open(&victim).unwrap();
                let len = file.metadata().unwrap().len();
                file.set_len(len / 2).unwrap();
            }
            false
        });
        let shrinking = TrackSegments {
            segments,
            ..source.clone()
        };
        match remux(&[Streams::All], &[group([shrinking])], &output, &cancel) {
            Ok(report) => assert_eq!(
                report.streams.iter().map(|s| s.packets).collect::<Vec<_>>(),
                complete,
                "第 {at} 次检查时截短：照常合并的应完整"
            ),
            Err(Error::Io { .. }) => {}
            Err(other) => panic!("第 {at} 次检查时截短：应报读文件出错，实际 {other:?}"),
        }
    }
}

/// 写输出时盘满（Linux 的 /dev/full 每次写都报 ENOSPC）：报写文件出错、带上 errno 对应的种类，不报成封装器的错误。
#[cfg(target_os = "linux")]
#[test]
fn a_full_disk_while_writing_is_reported_as_a_file_error() {
    let err = remux(
        &[Streams::All],
        &[group([track("ts_long")])],
        Path::new("/dev/full"),
        &Cancel::never(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::Io { action: "写入", cause, .. }
            if cause.kind() == std::io::ErrorKind::StorageFull),
        "实际 {err:?}"
    );
}

/// 组内时间戳往前跳了约 1000 秒（来源在时间戳重新开始处漏标了 EXT-X-DISCONTINUITY）：照写会在输出里留下一大段空档，
/// 报 TimestampJump，报出的就是空档的长度。两片之间缺了分片时，跳过的可能就是缺的那几片的时长，不核对，照常合并；
/// 声明的时长不准而内容连续时不误报。
#[test]
fn a_forward_timestamp_jump_within_a_group_is_rejected() {
    let dir = work_dir("timestamp_jump");
    let output = dir.join("out.mp4");
    let jump = track("ts_jump");

    let err = remux(
        &[Streams::All],
        &[group([jump.clone()])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::TimestampJump {
                group: 0,
                track: 0,
                segment: 1,
                jump_us,
                ..
            }) if (999_000_000..1_001_000_000).contains(&jump_us)
        ),
        "{err:?}"
    );

    let missing = TrackSegments {
        gaps: vec![1],
        ..jump
    };
    remux(
        &[Streams::All],
        &[group([missing])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    // ts_long 每片实际 1 秒，只声明 0.3 秒
    let mut understated = track("ts_long");
    for segment in &mut understated.segments {
        segment.duration_us = 300_000;
    }
    remux(
        &[Streams::All],
        &[group([understated])],
        &output,
        &Cancel::never(),
    )
    .unwrap();
}

/// 从 GOP 中间开始的组：第一个关键帧之前的视频包参考的帧不在输出里，不写出、计数，输出的视频每个包都解得出。
#[test]
fn video_before_the_first_keyframe_of_a_group_is_dropped_and_counted() {
    let dir = work_dir("before_keyframe");
    let output = dir.join("out.mp4");
    let mut cut = track("ts_cut");
    // 按字节切开的 TS：第二片从 GOP 中间开始
    cut.segments.remove(0);
    let v_in = timelines(&concat_track(&cut, &dir, "in.ts")).0;

    let report = remux(&[Streams::All], &[group([cut])], &output, &Cancel::never()).unwrap();

    let video = &report.streams[0];
    assert!(video.skipped_before_keyframe > 0, "{video:?}");
    assert_eq!(
        video.packets + video.skipped_before_keyframe,
        v_in.len() as u64
    );
    assert_eq!(report.streams[1].skipped_before_keyframe, 0);
    let (packets, rejected, frames) = decode_video(&output);
    assert_eq!((packets, rejected, frames), (packets, 0, packets));
}

/// 开放 GOP 的 HEVC：第二组以 CRA 开头，它的前导帧（RASL）参考第一组的帧，在不连续处之后解不对：不写出、计数，第二组
/// 的画面接在第一组之后、不与它重叠。
#[test]
fn leading_pictures_of_a_group_starting_with_cra_are_dropped() {
    let dir = work_dir("open_gop");
    let output = dir.join("out.mp4");
    let gop = track("ts_hevc_open_gop");
    let first = TrackSegments {
        segments: gop.segments[..1].to_vec(),
        ..gop.clone()
    };
    let second = TrackSegments {
        segments: gop.segments[1..].to_vec(),
        ..gop.clone()
    };
    // 第二片里解码顺序在 CRA 之后、呈现时刻早于它的帧
    let second_in = timelines(&concat_track(&second, &dir, "second.ts")).0;
    let leading = second_in
        .iter()
        .skip(1)
        .filter(|p| p.0 < second_in[0].0)
        .count();
    assert!(leading > 0, "样本的第二片应以带前导帧的 CRA 开头");
    let first_in = timelines(&concat_track(&first, &dir, "first.ts")).0;

    let report = remux(
        &[Streams::All],
        &[group([first]), group([second])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    assert_eq!(
        report.streams[0].skipped_before_keyframe, leading as u64,
        "{report:?}"
    );
    let v_out = timelines(&output).0;
    let (group0, group1) = v_out.split_at(first_in.len());
    let group0_end = group0.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    let group1_start = group1.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    assert!(
        group1_start >= group0_end - 1e-6,
        "第二组从 {group1_start} 开始，第一组到 {group0_end} 结束"
    );
}

/// 音频在这一片第 8.5 秒才出现：探测按声明的时长读完第一片，探测得出音频；声明的时长不够时（这里为 0，按 FFmpeg 的
/// 默认值只探测 7 秒）报探测不出参数。
#[test]
fn probing_reads_through_the_declared_duration_of_the_first_segment() {
    let dir = work_dir("probe_first_segment");
    let output = dir.join("out.mp4");
    let late = track("ts_audio_late_in_segment");

    let report = remux(
        &[Streams::All],
        &[group([late.clone()])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let kinds: Vec<StreamKind> = report.streams.iter().map(|s| s.shape.kind()).collect();
    assert_eq!(kinds, [StreamKind::Video, StreamKind::Audio]);
    let mut undeclared = late;
    undeclared.segments[0].duration_us = 0;
    let err = remux(
        &[Streams::All],
        &[group([undeclared])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::ParamsUnknown {
                kind: StreamKind::Audio,
                ..
            })
        ),
        "{err:?}"
    );
}

/// H.264 开放 GOP、不带 recovery point SEI：从第二片起每片以非 IDR 的 I 帧开头，FFmpeg 不给它打关键帧标志。以它开头
/// 的组从这个 I 帧开始写，只去掉呈现在它之前、参考上一个 GOP 的帧，视频与音频一样长；这个 I 帧补上关键帧标志，MP4 的
/// 同步样本表（stss，只有视频轨有）恰好列出它。
#[test]
fn an_h264_open_gop_without_recovery_points_starts_at_its_intra_frame() {
    let dir = work_dir("h264_open_gop");
    let output = dir.join("out.mp4");
    let gop = track("ts_h264_open_gop");
    let later = TrackSegments {
        segments: gop.segments[1..].to_vec(),
        ..gop.clone()
    };
    let (v_in, a_in) = timelines(&concat_track(&later, &dir, "later.ts"));
    let leading = v_in.iter().skip(1).filter(|p| p.0 < v_in[0].0).count();

    let report = remux(
        &[Streams::All],
        &[group([later])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let video = &report.streams[0];
    assert_eq!(video.skipped_before_keyframe, leading as u64, "{video:?}");
    assert_eq!(video.packets, (v_in.len() - leading) as u64);
    let v_out = timelines(&output).0;
    assert!(
        (span(&v_out) - span(&a_in)).abs() < 0.2,
        "视频 {} 秒，音频 {} 秒",
        span(&v_out),
        span(&a_in)
    );
    // stss：版本与标志 4 字节、条目数 1、第 1 个样本
    let data = std::fs::read(&output).unwrap();
    let at = data
        .windows(4)
        .position(|w| w == b"stss")
        .expect("视频轨应有同步样本表");
    assert_eq!(
        &data[at + 4..at + 16],
        &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1]
    );
}

/// HEVC 闭合 GOP 的组以 IDR_W_RADL 开头，后面呈现在它之前的 RADL 只参考本 GOP、解得出：照常写入，一帧不丢。
#[test]
fn hevc_radl_pictures_at_the_start_of_a_group_are_kept() {
    let dir = work_dir("hevc_radl");
    let output = dir.join("out.mp4");
    let radl = track("ts_hevc_radl");
    let second = TrackSegments {
        segments: radl.segments[1..].to_vec(),
        ..radl.clone()
    };
    let v_in = timelines(&concat_track(&second, &dir, "second.ts")).0;
    assert!(
        v_in.iter().skip(1).any(|p| p.0 < v_in[0].0),
        "样本的第二片应以带前导帧的 IDR 开头"
    );

    let report = remux(
        &[Streams::All],
        &[group([second])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let video = &report.streams[0];
    assert_eq!(
        (video.packets, video.skipped_before_keyframe),
        (v_in.len() as u64, 0)
    );
    let (packets, rejected, frames) = decode_video(&output);
    assert_eq!((packets, rejected, frames), (packets, 0, packets));
}

/// 一组里视频一个能开始解码的帧都没有（长 GOP 按字节切开的后一半）：报 NoKeyframe，不静默写出一段没有画面的视频。
/// 下载前核对只拿到开头的分片，判断不了，不报。
#[test]
fn a_group_without_any_decodable_video_frame_is_rejected() {
    let dir = work_dir("no_keyframe");
    let output = dir.join("out.mp4");
    let gop = track("ts_long_gop");
    let tail = TrackSegments {
        segments: gop.segments[1..].to_vec(),
        ..gop
    };

    let err = remux(
        &[Streams::All],
        &[group([tail.clone()])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::NoKeyframe { group: 0, track: 0 })
        ),
        "{err:?}"
    );
    check(&[Streams::All], &[group([tail])]).unwrap();
}

/// 缺了一片、音频 PES 跨在缺片处的 TS：只丢缺片处拼坏的音频包并计数，其余照常合并；视频不丢。
#[test]
fn audio_damaged_at_a_missing_segment_is_dropped_and_counted() {
    let dir = work_dir("damaged_audio");
    let output = dir.join("out.mp4");
    let mut holed = track("ts_cut");
    holed.segments.remove(2);
    holed.gaps = vec![2];
    let (v_in, a_in) = timelines(&concat_track(&holed, &dir, "in.ts"));

    let report = remux(
        &[Streams::All],
        &[group([holed])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let [video, audio] = &report.streams[..] else {
        panic!("应有视频与音频两路流：{report:?}");
    };
    assert_eq!(
        (video.packets, video.discarded_corrupt_bytes),
        (v_in.len() as u64, 0)
    );
    // 缺片处前后两段拼成的只有一两个包；解复用器给整个 PES 打的损坏标志不作数
    let lost = a_in.len() as u64 - audio.packets;
    assert!(
        (1..=2).contains(&lost) && audio.discarded_corrupt_bytes > 0,
        "{audio:?}"
    );
}

/// 改写 TS 的音频（PID 0x101）：PES 长度写 0（不定长），连续计数器从 0 重新编号；`overstate` 为真时 PES 长度改为多报
/// 4 字节。数据都在：逐片独立生成的切片器常这样写，解复用器却给这些 PES 打上损坏标志。
fn rewrite_audio_pes(data: &[u8], overstate: bool) -> Vec<u8> {
    let mut out = data.to_vec();
    let mut cc = 0;
    for packet in out.chunks_mut(188) {
        let pid = u16::from(packet[1] & 0x1F) << 8 | u16::from(packet[2]);
        if pid != 0x101 || packet[3] & 0x10 == 0 {
            continue;
        }
        if !overstate {
            packet[3] = packet[3] & 0xF0 | cc;
            cc = (cc + 1) & 0x0F;
        }
        if packet[1] & 0x40 != 0 {
            // 有效载荷起点（跳过调整字段）：00 00 01、stream_id、PES_packet_length
            let start = if packet[3] & 0x20 != 0 {
                5 + usize::from(packet[4])
            } else {
                4
            };
            let len = u16::from_be_bytes([packet[start + 4], packet[start + 5]]);
            let len = if overstate { len + 4 } else { 0 };
            packet[start + 4..start + 6].copy_from_slice(&len.to_be_bytes());
        }
    }
    out
}

/// 音频 PES 不定长且各片的连续计数器从 0 重来、PES 长度多报几个字节：解复用器给整个 PES 打损坏标志，帧都完好，照常
/// 全部写入。
#[test]
fn audio_in_pes_flagged_corrupt_but_intact_is_kept() {
    let dir = work_dir("flagged_audio");
    let output = dir.join("out.mp4");
    let source = track("ts_long");
    let a_in = timelines(&concat_track(&source, &dir, "in.ts")).1;
    for overstate in [false, true] {
        let segments = source
            .segments
            .iter()
            .enumerate()
            .map(|(i, segment)| {
                let rewritten = dir.join(format!("{overstate}-{i}.ts"));
                let data = rewrite_audio_pes(&std::fs::read(&segment.path).unwrap(), overstate);
                std::fs::write(&rewritten, data).unwrap();
                Segment {
                    path: rewritten,
                    duration_us: segment.duration_us,
                }
            })
            .collect();
        let rewritten = TrackSegments {
            init: None,
            segments,
            gaps: Vec::new(),
        };

        let report = remux(
            &[Streams::All],
            &[group([rewritten])],
            &output,
            &Cancel::never(),
        )
        .unwrap();

        let audio = &report.streams[1];
        assert_eq!(
            (audio.packets, audio.discarded_corrupt_bytes),
            (a_in.len() as u64, 0),
            "多报长度 {overstate}"
        );
    }
}

/// 组内音频换了 PID（PMT 更新、漏标 EXT-X-DISCONTINUITY）：报错，不静默跳过新 PID 上的包；一开始就有两路音频时取
/// 第一路，不算换了 PID。
#[test]
fn a_pid_change_within_a_group_is_rejected() {
    let dir = work_dir("pid_change");
    let output = dir.join("out.mp4");

    let err = remux(
        &[Streams::All],
        &[group([track("ts_pid_change")])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::StreamChangedInGroup {
                group: 0,
                track: 0,
                kind: StreamKind::Audio
            })
        ),
        "实际 {err:?}"
    );
    // 只取视频时新的音频流与它无关
    remux(
        &[Streams::Video],
        &[group([track("ts_pid_change")])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    // 一开始就有两路音频：取第一路，另一路一直列在节目里，不算换了 PID
    let two = track("ts_two_audio");
    let report = remux(
        &[Streams::All],
        &[group([two.clone()])],
        &output,
        &Cancel::never(),
    )
    .unwrap();
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(&two.segments[0].path).unwrap();
    let first_audio = ictx
        .streams()
        .find(|s| s.parameters().medium() == ffmpeg::media::Type::Audio)
        .unwrap()
        .index();
    let mut packets = 0;
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut ictx) {
            Ok(()) => packets += u64::from(packet.stream() == first_audio),
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(report.streams[1].packets, packets);
}

/// 同一个 PID 中途从 AAC 换成 AC-3（漏标 EXT-X-DISCONTINUITY）：编码在组内变了。
#[test]
fn a_codec_switch_on_one_pid_is_rejected() {
    let dir = work_dir("pid_switch");
    let output = dir.join("out.mp4");
    let mut switched = track("ts_a");
    switched.segments.extend(track("ts_ac3").segments);

    let err = remux(
        &[Streams::Audio],
        &[group([switched])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::ParamsChangedInGroup {
                group: 0,
                track: 0,
                kind: StreamKind::Audio
            })
        ),
        "实际 {err:?}"
    );
}

/// 没有 init 段的一条轨在组内换了分片格式：打包音频从 AAC 换成 AC-3、TS 与打包音频前后相接，两个方向都报错，不跳过
/// 后面那种格式的分片。
#[test]
fn format_changes_within_a_group_are_rejected() {
    let dir = work_dir("format_in_group");
    let output = dir.join("out.mp4");
    let (aac, ac3) = (packed_fixture("aac"), packed_fixture("ac3"));
    let packed_aac = packed_segment(&dir, &aac[0].0, Some(900_000));
    let ts = track("ts_a").segments[0].path.clone();
    for segments in [
        vec![
            packed_aac.clone(),
            packed_segment(&dir, &ac3[1].0, Some(900_000 + aac[0].1)),
        ],
        vec![ts.clone(), packed_aac.clone()],
        vec![packed_aac, ts],
    ] {
        let audio = TrackSegments {
            init: None,
            segments: segs(segments),
            gaps: Vec::new(),
        };
        let err = remux(
            &[Streams::Audio],
            &[group([audio])],
            &output,
            &Cancel::never(),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                Error::Unsupported(Unsupported::FormatChangedInGroup { group: 0, track: 0 })
            ),
            "实际 {err:?}"
        );
    }
}

/// 各轨只给开头的分片。
fn head(name: &str) -> TrackSegments {
    let t = track(name);
    TrackSegments {
        init: t.init,
        segments: t.segments[..1].to_vec(),
        gaps: Vec::new(),
    }
}

/// 只核对：各轨开头的分片就能发现编码不受支持、流参数或解码配置与第 0 组不同、E-AC-3 的帧结构；能合并的通过，不写
/// 输出。
#[test]
fn check_finds_what_cannot_go_into_an_mp4_from_the_first_segments() {
    check(
        &[Streams::Video, Streams::Audio],
        &[
            group([head("fmp4_a/video"), head("fmp4_a/audio")]),
            group([head("fmp4_b/video"), head("fmp4_b/audio")]),
        ],
    )
    .unwrap();

    let err = check(&[Streams::All], &[group([head("ts_opus")])]).unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::Codec { codec: "opus", .. })
        ),
        "实际 {err:?}"
    );
    let err = check(
        &[Streams::All],
        &[group([head("ts_a")]), group([head("ts_small")])],
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::ParamsChanged { group: 1, .. })
        ),
        "实际 {err:?}"
    );
    let err = check(
        &[Streams::All],
        &[group([head("ts_aac_stereo")]), group([head("ts_he_aac")])],
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::ConfigChanged { group: 1, .. })
        ),
        "实际 {err:?}"
    );
    let err = check(&[Streams::Audio], &[group([head("ts_eac3_short")])]).unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::Eac3ShortFrames { group: 0, .. })
        ),
        "实际 {err:?}"
    );
}

/// 只靠开头的分片判断不了的，核对不报、留给合并：第一个分片里没有音频包（探测不出参数），打包 MP3 的第一个分片只有
/// 一帧（FFmpeg 要两帧相连才认）。整组合并照常成功；整组都探测不出参数时合并报 ParamsUnknown。第 0 组判断不了时其余
/// 各组照样核对自身。
#[test]
fn check_leaves_what_the_first_segment_cannot_tell_to_the_merge() {
    let dir = work_dir("check_undecided");
    let output = dir.join("out.mp4");
    let late = track("ts_late_audio");
    check(&[Streams::All], &[group([head("ts_late_audio")])]).unwrap();
    // 第 0 组判断不了时，其余各组照样核对自身
    let err = check(
        &[Streams::All],
        &[group([head("ts_late_audio")]), group([head("ts_opus")])],
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::Codec {
                group: 1,
                codec: "opus",
                ..
            })
        ),
        "实际 {err:?}"
    );
    remux(&[Streams::All], &[group([late])], &output, &Cancel::never()).unwrap();
    let err = remux(
        &[Streams::All],
        &[group([head("ts_late_audio")])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::ParamsUnknown {
                group: 0,
                track: 0,
                kind: StreamKind::Audio
            })
        ),
        "实际 {err:?}"
    );

    let segments = packed_fixture("mp3");
    let sliver = dir.join("sliver");
    std::fs::write(&sliver, first_frames(&segments[0].0, 1)).unwrap();
    let first = packed_segment(&dir, &sliver, Some(900_000));
    // 48 kHz 的 MPEG-1 Layer III 每帧 1152 个采样点，即 2160 个 90 kHz 的 tick
    let rest = packed_segment(&dir, &segments[1].0, Some(900_000 + 2160));
    let head = TrackSegments {
        init: None,
        segments: segs([first.clone()]),
        gaps: Vec::new(),
    };
    check(&[Streams::Audio], &[group([head])]).unwrap();
    let whole = TrackSegments {
        init: None,
        segments: segs([first, rest]),
        gaps: Vec::new(),
    };
    remux(
        &[Streams::Audio],
        &[group([whole])],
        &output,
        &Cancel::never(),
    )
    .unwrap();
}

/// fMP4 某一组开头的分片里没有样本（只有 styp）：宽高、采样率与声道数来自 init 段，视频的像素格式与 AAC 解出一帧后的
/// 参数探测不出，与第 0 组比不了，核对跳过这一组，不报参数或解码配置不同；init 段里的解码配置不同时照样报出。隐式
/// SBR 的 HE-AAC（配置里只写 LC 与 24 kHz 的核心采样率，解出帧才知道是 48 kHz）与单声道的 AAC（解出帧才知道有没有
/// PS）同样跳过，整组都给时与显式写法的照常合并；配置里的采样率加倍也对不上的照样报出。
#[test]
fn check_skips_what_a_head_without_samples_cannot_tell() {
    let dir = work_dir("check_head_without_samples");
    let styp_only = dir.join("styp.m4s");
    std::fs::write(&styp_only, b"\0\0\0\x18stypmsdh\0\0\0\0msdhmsix").unwrap();
    let without_samples = |init: PathBuf| TrackSegments {
        init: Some(init),
        segments: segs([styp_only.clone()]),
        gaps: Vec::new(),
    };
    let init = |name: &str| fixtures().join(name).join("init.mp4");
    // 显式 SBR 的配置 13 10 56 e5 98 去掉同步扩展，成为隐式 SBR
    let implicit = dir.join("implicit_sbr.mp4");
    let mut data = std::fs::read(init("fmp4_he_aac")).unwrap();
    let at = data
        .windows(5)
        .position(|w| w == [0x13, 0x10, 0x56, 0xe5, 0x98])
        .unwrap();
    data[at + 2..at + 5].fill(0);
    std::fs::write(&implicit, data).unwrap();
    for (streams, first, second) in [
        (Streams::Video, "fmp4_a/video", init("fmp4_b/video")),
        (Streams::Audio, "fmp4_a/audio", init("fmp4_aac_at")),
        (Streams::Audio, "fmp4_he_aac", init("fmp4_he_aac")),
        (Streams::Audio, "fmp4_he_aac", implicit.clone()),
    ] {
        let label = format!("{first} 接 {}", second.display());
        let groups = [group([head(first)]), group([without_samples(second)])];
        check(&[streams], &groups).unwrap_or_else(|e| panic!("{label}：{e:?}"));
    }
    let whole = TrackSegments {
        init: Some(implicit),
        ..track("fmp4_he_aac")
    };
    let groups = [group([track("fmp4_he_aac")]), group([whole])];
    remux(
        &[Streams::Audio],
        &groups,
        &dir.join("out.mp4"),
        &Cancel::never(),
    )
    .unwrap();

    let err = check(
        &[Streams::Video],
        &[
            group([head("fmp4_alac")]),
            group([without_samples(init("fmp4_baseline"))]),
        ],
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::ConfigChanged {
                group: 1,
                kind: StreamKind::Video,
                ..
            })
        ),
        "实际 {err:?}"
    );

    // 48 kHz 单声道 LC 的配置 11 88 改成 44.1 kHz 的 12 08：加倍是 88.2 kHz，与解出帧的 48 kHz 对不上
    let other_rate = dir.join("other_rate.mp4");
    let mut data = std::fs::read(init("fmp4_aac_at")).unwrap();
    let at = data
        .windows(7)
        .position(|w| w == [0x05, 0x80, 0x80, 0x80, 0x02, 0x11, 0x88])
        .unwrap();
    data[at + 5..at + 7].copy_from_slice(&[0x12, 0x08]);
    std::fs::write(&other_rate, data).unwrap();
    let groups = [
        group([head("fmp4_a/audio")]),
        group([without_samples(other_rate)]),
    ];
    let err = check(&[Streams::Audio], &groups).unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::ParamsChanged {
                group: 1,
                kind: StreamKind::Audio,
                found: Shape::Audio {
                    sample_rate: 44_100,
                    ..
                },
                ..
            })
        ),
        "实际 {err:?}"
    );
}

/// TS 里每帧 3 块的 E-AC-3：MP4 封装器会把几帧并成一个样本、丢掉末尾凑不满的，放不进 MP4。
#[test]
fn eac3_with_fewer_than_six_blocks_per_frame_is_rejected() {
    let dir = work_dir("eac3_short");
    let output = dir.join("out.mp4");
    let err = remux(
        &[Streams::Audio],
        &[group([track("ts_eac3_short")])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::Eac3ShortFrames { group: 0, track: 0 })
        ),
        "实际 {err:?}"
    );
}

/// TS 里的 Opus 不收（只收 fMP4 里的，TS 里读不出每帧的参数）；采样率低于 16 kHz 的 MP3 放不进，报出采样率。
#[test]
fn opus_in_ts_and_low_rate_mp3_audio_are_rejected() {
    let dir = work_dir("rejected_audio");
    let output = dir.join("out.mp4");

    let err = remux(
        &[Streams::All],
        &[group([track("ts_opus")])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::Codec {
                kind: StreamKind::Audio,
                codec: "opus",
                ..
            })
        ),
        "实际 {err:?}"
    );
    let err = remux(
        &[Streams::All],
        &[group([track("ts_mp3_8k")])],
        &output,
        &Cancel::never(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::Mp3SampleRate {
                sample_rate: 8000,
                ..
            })
        ),
        "实际 {err:?}"
    );
}

/// TS 里的 MP2 音频（与 MP3 同样的 MPEG 音频帧头，逐帧核对参数）原样复制进 MP4。
#[test]
fn mp2_audio_in_ts_is_copied() {
    let dir = work_dir("mp2_audio");
    let output = dir.join("out.mp4");
    let muxed = track("ts_mp2");

    let report = remux(
        &[Streams::All],
        &[group([muxed.clone()])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let a_in = timelines(&concat_track(&muxed, &dir, "in.ts")).1;
    assert_eq!(report.streams[1].packets, a_in.len() as u64);
    assert_eq!(report.streams[1].shape.codec(), "mp2");
    // MP4 里 MPEG 音频的对象类型不分 Layer（采样率高于 24 kHz 时为 0x6B，即 ISO/IEC 11172-3），FFmpeg 回读时按表认作
    // mp3；写进去的是 Layer II 的帧：帧头的 layer 位为 0b10
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(&output).unwrap();
    let audio = ictx
        .streams()
        .best(ffmpeg::media::Type::Audio)
        .unwrap()
        .index();
    let first = loop {
        let mut packet = ffmpeg::Packet::empty();
        packet.read(&mut ictx).unwrap();
        if packet.stream() == audio {
            break packet.data().unwrap().to_vec();
        }
    };
    assert_eq!(
        (first[0], first[1] & 0xE0, first[1] >> 1 & 0b11),
        (0xFF, 0xE0, 0b10)
    );
}

/// AC-3、E-AC-3（5.1 声道）、MP3 音频：TS 里的与打包音频都原样复制。回读输出核对各路流的编码，AC-3 与 E-AC-3 的
/// 样本描述（dac3、dec3）里是 5.1 声道。
#[test]
fn ac3_eac3_and_mp3_audio_are_copied() {
    let dir = work_dir("more_audio");
    let output = dir.join("out.mp4");
    let five_one = |codec: &str| (codec != "mp3").then_some((7, true));

    for (ts, packed, codec) in [
        ("ts_ac3", "ac3", "ac3"),
        ("ts_eac3", "eac3", "eac3"),
        ("ts_mp3", "mp3", "mp3"),
    ] {
        let muxed = track(ts);
        let report = remux(
            &[Streams::All],
            &[group([muxed.clone()])],
            &output,
            &Cancel::never(),
        )
        .unwrap();
        let a_in = timelines(&concat_track(&muxed, &dir, "in.ts")).1;
        assert_eq!(report.streams[1].packets, a_in.len() as u64, "{ts}");
        assert_eq!(codecs(&output), ["h264", codec], "{ts}");
        assert_eq!(ac3_layout(&output, codec), five_one(codec), "{ts}");

        let segments = packed_fixture(packed);
        let mut at = 900_000;
        let mut tagged = Vec::new();
        for (path, ticks) in &segments {
            tagged.push(packed_segment(&dir, path, Some(at)));
            at += ticks;
        }
        let audio = TrackSegments {
            init: None,
            segments: segs(tagged),
            gaps: Vec::new(),
        };
        let report = remux(
            &[Streams::Audio],
            &[group([audio])],
            &output,
            &Cancel::never(),
        )
        .unwrap();
        let packets: usize = segments
            .iter()
            .map(|(path, _)| timelines(path).1.len())
            .sum();
        assert_eq!(report.streams[0].packets, packets as u64, "{packed}");
        assert_eq!(codecs(&output), [codec], "{packed}");
        assert_eq!(ac3_layout(&output, codec), five_one(codec), "{packed}");
    }
}

/// 打包音频按各分片 ID3 里的时间戳定时：与 TS 视频的相对时刻和输入一致，缺失的分片处留出空档。
#[test]
fn packed_audio_follows_its_id3_timestamps() {
    let dir = work_dir("packed_timestamps");
    let output = dir.join("out.mp4");
    let video = track("ts_a");
    let v_in = timelines(&concat_track(&video, &dir, "v.ts")).0;
    // 音频比视频晚 0.1 秒开始，第二个分片缺失
    let start = ((earliest(&v_in) + 0.1) * 90_000.0).round() as u64;
    let segments = packed_fixture("aac");
    let audio = TrackSegments {
        init: None,
        segments: segs([
            packed_segment(&dir, &segments[0].0, Some(start)),
            packed_segment(
                &dir,
                &segments[2].0,
                Some(start + segments[0].1 + segments[1].1),
            ),
        ]),
        gaps: vec![1],
    };

    remux(
        &[Streams::Video, Streams::Audio],
        &[group([video, audio])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let (v_out, a_out) = timelines(&output);
    let lead = a_out[0].0 - earliest(&v_out);
    assert!(
        (lead - 0.1).abs() < 0.001,
        "音频应比视频晚 0.1 秒开始，实际 {lead}"
    );
    let first_part = timelines(&segments[0].0).1.len();
    let gap = a_out[first_part].0 - a_out[0].0;
    let expected = (segments[0].1 + segments[1].1) as f64 / 90_000.0;
    assert!(
        (gap - expected).abs() < 0.001,
        "第三个分片应从 {expected} 秒处开始，实际 {gap}"
    );
}

/// 打包音频的时间戳越过 33 位的回绕点时接着往后排，不当作往回跳。
#[test]
fn packed_audio_timestamps_wrap_around() {
    let dir = work_dir("packed_wrap");
    let output = dir.join("out.mp4");
    let segments = packed_fixture("aac");
    let wrap = 1u64 << 33;
    let audio = TrackSegments {
        init: None,
        segments: segs([
            packed_segment(&dir, &segments[0].0, Some(wrap - segments[0].1)),
            packed_segment(&dir, &segments[1].0, Some(0)),
        ]),
        gaps: Vec::new(),
    };

    let report = remux(
        &[Streams::Audio],
        &[group([audio])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let expected = (segments[0].1 + segments[1].1) as f64 / 90_000.0;
    let duration = report.streams[0].duration_us as f64 / 1e6;
    assert!(
        (duration - expected).abs() < 0.001,
        "应连续 {expected} 秒，实际 {duration}"
    );
}

/// 打包音频的末片只有一两帧：整条轨作为一个输入读，格式只在开头识别，短片照常读出，各帧都带时间戳。
#[test]
fn packed_audio_with_very_short_segments_is_read() {
    let dir = work_dir("packed_short");
    let output = dir.join("out.mp4");
    for name in ["aac", "ac3", "eac3", "mp3"] {
        let segments = packed_fixture(name);
        let first = &segments[0];
        let full: usize = timelines(&first.0).1.len();
        for frames in [1, 2] {
            let tail = dir.join(format!("{name}-tail{frames}"));
            std::fs::write(&tail, first_frames(&segments[1].0, frames)).unwrap();
            let audio = TrackSegments {
                init: None,
                segments: segs([
                    packed_segment(&dir, &first.0, Some(900_000)),
                    packed_segment(&dir, &tail, Some(900_000 + first.1)),
                ]),
                gaps: Vec::new(),
            };

            let report = remux(
                &[Streams::Audio],
                &[group([audio])],
                &output,
                &Cancel::never(),
            )
            .unwrap();

            let stream = &report.streams[0];
            assert_eq!(
                (stream.packets, stream.skipped_without_dts),
                ((full + frames) as u64, 0),
                "{name} 末片 {frames} 帧"
            );
            let a_out = timelines(&output).1;
            let tail_start = a_out[full].0 - a_out[0].0;
            let expected = first.1 as f64 / 90_000.0;
            assert!(
                (tail_start - expected).abs() < 0.001,
                "{name} 末片 {frames} 帧应从 {expected} 秒处开始，实际 {tail_start}"
            );
        }
    }
}

/// 不带时间戳 PRIV 帧的 ID3v2.4 标签（如定时元数据），约 640 字节。
fn metadata_tag() -> Vec<u8> {
    let data = [b"com.example.metadata\0".as_slice(), &[0x5A; 600]].concat();
    let syncsafe =
        |size: usize| [size >> 21, size >> 14, size >> 7, size].map(|b| (b & 0x7F) as u8);
    let frame = [b"PRIV".as_slice(), &syncsafe(data.len()), &[0, 0], &data].concat();
    [
        b"ID3\x04\x00\x00".as_slice(),
        &syncsafe(frame.len()),
        &frame,
    ]
    .concat()
}

/// 帧之间夹着 ID3 标签的打包音频：标签不进输出，下一个分片（相接或隔着缺失的一片）从它自己的时间戳处开始。
#[test]
fn packed_audio_with_id3_tags_between_frames_keeps_its_timing() {
    let dir = work_dir("packed_inline_id3");
    let output = dir.join("out.mp4");
    for name in ["aac", "ac3", "eac3", "mp3"] {
        let segments = packed_fixture(name);
        let frames: Vec<usize> = segments
            .iter()
            .map(|(path, _)| timelines(path).1.len())
            .collect();
        // 第一个分片是样本的前两片，中间夹一个标签
        let first = dir.join(format!("{name}-first"));
        let data = [
            std::fs::read(packed_segment(&dir, &segments[0].0, Some(900_000))).unwrap(),
            metadata_tag(),
            std::fs::read(&segments[1].0).unwrap(),
        ]
        .concat();
        std::fs::write(&first, data).unwrap();
        for gap in [0, 90_000] {
            let second_start = 900_000 + segments[0].1 + segments[1].1 + gap;
            let audio = TrackSegments {
                init: None,
                segments: segs([
                    first.clone(),
                    packed_segment(&dir, &segments[2].0, Some(second_start)),
                ]),
                gaps: Vec::new(),
            };

            let report = remux(
                &[Streams::Audio],
                &[group([audio])],
                &output,
                &Cancel::never(),
            )
            .unwrap();

            assert_eq!(
                report.streams[0].packets,
                frames.iter().sum::<usize>() as u64,
                "{name} 空档 {gap}"
            );
            let a_out = timelines(&output).1;
            let second = frames[0] + frames[1];
            let expected = (second_start - 900_000) as f64 / 90_000.0;
            let found = a_out[second].0 - a_out[0].0;
            assert!(
                (found - expected).abs() < 0.001,
                "{name} 空档 {gap}：第二个分片应从 {expected} 秒处开始，实际 {found}"
            );
            assert!(
                a_out[second - 1].0 - a_out[0].0 < expected - 0.01,
                "{name} 空档 {gap}：第一个分片的帧不应排到第二个分片的位置"
            );
        }
    }
}

/// 每片开头带 Info 帧、末尾带 ID3v1 标签的打包 MP3：两者都不进输出，各片照常接续。
#[test]
fn packed_mp3_info_frames_and_id3v1_tags_are_skipped() {
    let dir = work_dir("packed_mp3_info");
    let output = dir.join("out.mp4");
    let segments = packed_fixture("mp3_info");
    let id3v1 = [b"TAG".as_slice(), &[0x20; 125]].concat();
    let mut start = 900_000;
    let mut tagged = Vec::new();
    for (path, ticks) in &segments {
        let segment = packed_segment(&dir, path, Some(start));
        let data = [std::fs::read(&segment).unwrap(), id3v1.clone()].concat();
        std::fs::write(&segment, data).unwrap();
        tagged.push(segment);
        start += ticks;
    }
    let frames: usize = segments
        .iter()
        .map(|(path, _)| timelines(path).1.len())
        .sum();
    let audio = TrackSegments {
        init: None,
        segments: segs(tagged),
        gaps: Vec::new(),
    };

    let report = remux(
        &[Streams::Audio],
        &[group([audio])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    assert_eq!(report.streams[0].packets, frames as u64);
    let expected = (start - 900_000) as f64 / 90_000.0;
    let span = span(&timelines(&output).1);
    assert!(
        (span - expected).abs() < 0.03,
        "输出应长 {expected} 秒，实际 {span}"
    );
}

/// 打包音频损坏：中间一片丢了一帧（同步字坏了）、帧之间夹着杂字节，最后一片丢了一帧。中间一片损坏之后的帧照常写入、
/// 保持原来的时刻（按下一片的时间戳定，不从片头按样本数往后推）；最后一片损坏之后的帧定不了时刻，不写入。丢弃的
/// 损坏字节都计入。
#[test]
fn damage_in_packed_audio_keeps_the_timing_of_later_frames() {
    let dir = work_dir("packed_damage");
    let output = dir.join("out.mp4");
    let segments = packed_fixture("aac");
    let sizes: Vec<Vec<usize>> = segments
        .iter()
        .map(|(path, _)| packet_sizes(path))
        .collect();
    // 48 kHz 的 AAC 每帧 1024 个采样点，即 1920 个 90 kHz 的 tick
    let frame = 1920;
    let (t0, t1) = (900_000, 900_000 + segments[0].1);
    let t2 = t1 + segments[1].1;
    let damaged = |i: usize, lost: usize, junk_after: Option<usize>| {
        let mut data = std::fs::read(&segments[i].0).unwrap();
        let at: usize = sizes[i][..lost].iter().sum();
        data[at] = 0;
        if let Some(after) = junk_after {
            let at: usize = sizes[i][..after].iter().sum();
            data.splice(at..at, *b"junk");
        }
        let path = dir.join(format!("damaged{i}"));
        std::fs::write(&path, data).unwrap();
        path
    };
    let audio = TrackSegments {
        init: None,
        segments: segs([
            packed_segment(&dir, &segments[0].0, Some(t0)),
            packed_segment(&dir, &damaged(1, 10, Some(20)), Some(t1)),
            packed_segment(&dir, &damaged(2, 5, None), Some(t2)),
        ]),
        gaps: Vec::new(),
    };

    let report = remux(
        &[Streams::Audio],
        &[group([audio])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let stream = &report.streams[0];
    let (n0, n1) = (sizes[0].len(), sizes[1].len());
    assert_eq!(stream.packets, (n0 + n1 - 1 + 5) as u64);
    let lost_in_last: usize = sizes[2][5..].iter().sum();
    assert_eq!(
        stream.discarded_corrupt_bytes,
        (sizes[1][10] + 4 + lost_in_last) as u64
    );
    let a_out = timelines(&output).1;
    let ticks = |seconds: f64| ((seconds - a_out[0].0) * 90_000.0).round() as u64;
    // 中间一片：第 10 帧丢了，第 11 帧起各在原来的时刻
    assert_eq!(ticks(a_out[n0 + 10].0), t1 - t0 + 11 * frame);
    assert_eq!(
        ticks(a_out[n0 + n1 - 2].0),
        t1 - t0 + (n1 as u64 - 1) * frame
    );
    // 最后一片从它的时间戳处开始
    assert_eq!(ticks(a_out[n0 + n1 - 1].0), t2 - t0);
}

/// 打包音频片内损坏之后的帧：下一片之前缺了分片时定不了时刻，不写入、计入丢弃，不挪到缺口之后；下一片在、但一帧也
/// 切不出（开头的帧头声明 8191 字节，超出分片，之后没有同步点）时，照样结束在它的时间戳处。
#[test]
fn damage_before_a_gap_or_a_frameless_segment_in_packed_audio() {
    let dir = work_dir("packed_damage_gap");
    let output = dir.join("out.mp4");
    let segments = packed_fixture("aac");
    let sizes: Vec<Vec<usize>> = segments
        .iter()
        .map(|(path, _)| packet_sizes(path))
        .collect();
    // 48 kHz 的 AAC 每帧 1024 个采样点，即 1920 个 90 kHz 的 tick
    let frame = 1920;
    let (t0, t1) = (900_000, 900_000 + segments[0].1);
    let t2 = t1 + segments[1].1;
    let mut data = std::fs::read(&segments[0].0).unwrap();
    data[sizes[0][..10].iter().sum::<usize>()] = 0;
    let damaged = dir.join("damaged0");
    std::fs::write(&damaged, data).unwrap();
    let mut frameless = std::fs::read(&segments[1].0).unwrap()[..7].to_vec();
    frameless[3] |= 0x03;
    frameless[4] = 0xFF;
    frameless[5] |= 0xE0;
    frameless.resize(5000, 0);
    let frameless_path = dir.join("frameless1");
    std::fs::write(&frameless_path, frameless).unwrap();
    let (n0, n2) = (sizes[0].len(), sizes[2].len());
    // 损坏的分片读不出完整的时长，声明的时长取样本里对应那一片的
    let declared_as = |i: usize, path: PathBuf| Segment {
        path,
        duration_us: segments[i].1 * 100 / 9,
    };

    let gap = TrackSegments {
        init: None,
        segments: vec![
            declared_as(0, packed_segment(&dir, &damaged, Some(t0))),
            declared_as(2, packed_segment(&dir, &segments[2].0, Some(t2))),
        ],
        gaps: vec![1],
    };
    let report = remux(
        &[Streams::Audio],
        &[group([gap])],
        &output,
        &Cancel::never(),
    )
    .unwrap();
    assert_eq!(report.streams[0].packets, (10 + n2) as u64);
    let lost: usize = sizes[0][10..].iter().sum();
    assert_eq!(report.streams[0].discarded_corrupt_bytes, lost as u64);
    let a_out = timelines(&output).1;
    let ticks = |seconds: f64| ((seconds - a_out[0].0) * 90_000.0).round() as u64;
    assert_eq!(ticks(a_out[10].0), t2 - t0);

    let empty_next = TrackSegments {
        init: None,
        segments: vec![
            declared_as(0, packed_segment(&dir, &damaged, Some(t0))),
            declared_as(1, packed_segment(&dir, &frameless_path, Some(t1))),
            declared_as(2, packed_segment(&dir, &segments[2].0, Some(t2))),
        ],
        gaps: Vec::new(),
    };
    let report = remux(
        &[Streams::Audio],
        &[group([empty_next])],
        &output,
        &Cancel::never(),
    )
    .unwrap();
    assert_eq!(report.streams[0].packets, (n0 - 1 + n2) as u64);
    assert_eq!(
        report.streams[0].discarded_corrupt_bytes,
        (sizes[0][10] + 5000) as u64
    );
    let a_out = timelines(&output).1;
    let ticks = |seconds: f64| ((seconds - a_out[0].0) * 90_000.0).round() as u64;
    // 第 10 帧丢了，第 11 帧起各在原来的时刻；第 2 片从它的时间戳处开始
    assert_eq!(ticks(a_out[10].0), 11 * frame);
    assert_eq!(ticks(a_out[n0 - 2].0), (n0 as u64 - 1) * frame);
    assert_eq!(ticks(a_out[n0 - 1].0), t2 - t0);
}

/// 样本里 fMP4 分片的 trun 声明的样本数据恰好到分片末尾：mdat 的长度写 0 时少一个字节、或截在 mdat 之前，都看得出
/// 没写完。合并时同样核对：没写完（截在 mdat 里或 mdat 之前）、最后一个 box 的长度写 0 都报错，不越过文件边界把下一片
/// 读成这一片的样本。
#[test]
fn fmp4_structure_is_checked() {
    use hs_m3u8_remux::fmp4::{self, Problem};
    use std::io::Cursor;

    let dir = work_dir("fmp4_structure");
    for name in ["fmp4_a/video", "fmp4_a/audio", "fmp4_av1_opus", "fmp4_alac"] {
        let t = track(name);
        let init = std::fs::read(t.init.as_ref().unwrap()).unwrap();
        let trex = fmp4::trex_sample_sizes(&init).unwrap();
        assert_eq!(fmp4::check(&init, &trex), Ok(()), "{name}");
        let data = std::fs::read(&t.segments[0].path).unwrap();
        let boxes = fmp4::check_reader(&mut Cursor::new(&data), &trex)
            .unwrap()
            .unwrap();
        let mdat = *boxes.last().unwrap();
        assert_eq!(&mdat.kind, b"mdat", "{name}");
        let mut open = data.clone();
        open[mdat.at as usize..][..4].fill(0);
        let mut sealed = open.clone();
        assert_eq!(fmp4::seal(&mut sealed, &trex), Ok(()), "{name}");
        assert_eq!(sealed, data, "{name}");
        let incomplete = Err(Problem::Incomplete);
        assert_eq!(
            fmp4::check(&open[..open.len() - 1], &trex),
            incomplete,
            "{name}"
        );
        assert_eq!(
            fmp4::check(&data[..mdat.at as usize], &trex),
            incomplete,
            "{name}"
        );
    }

    let video = track("fmp4_a/video");
    let original = std::fs::read(&video.segments[0].path).unwrap();
    let trex =
        fmp4::trex_sample_sizes(&std::fs::read(video.init.as_ref().unwrap()).unwrap()).unwrap();
    let boxes = fmp4::check_reader(&mut Cursor::new(&original), &trex)
        .unwrap()
        .unwrap();
    let mdat = boxes.last().unwrap().at as usize;
    let mut open = original.clone();
    open[mdat..mdat + 4].fill(0);
    let truncated = original[..original.len() - 10].to_vec();
    let before_mdat = original[..mdat].to_vec();
    for (name, data) in [
        ("open", open),
        ("truncated", truncated),
        ("before_mdat", before_mdat),
    ] {
        let path = dir.join(format!("{name}.m4s"));
        std::fs::write(&path, data).unwrap();
        let segments = TrackSegments {
            init: video.init.clone(),
            segments: vec![
                declared(video.init.as_deref(), path.clone()),
                video.segments[1].clone(),
            ],
            gaps: Vec::new(),
        };
        let err = remux(
            &[Streams::Video],
            &[group([segments])],
            &dir.join("out.mp4"),
            &Cancel::never(),
        )
        .unwrap_err();
        let expected = match name {
            "open" => {
                matches!(&err, Error::Fmp4OpenEnded { path: p, at } if *p == path && *at == mdat as u64)
            }
            _ => {
                matches!(&err, Error::Fmp4 { path: p, problem: Problem::Incomplete } if *p == path)
            }
        };
        assert!(expected, "{name}：实际 {err:?}");
    }
}

/// 打包音频连着两片损坏，后一片从第一帧起就在损坏之后（开头的帧头声明 8191 字节，超出分片）：前一片损坏之后的帧结束在
/// 后一片的时间戳处，后一片的帧结束在再下一片的时间戳处；后一片是最后一片时它的帧定不了时刻，不写入、计入丢弃。
#[test]
fn consecutive_damaged_segments_in_packed_audio_are_timed_separately() {
    let dir = work_dir("packed_damage_consecutive");
    let output = dir.join("out.mp4");
    let segments = packed_fixture("aac");
    let sizes: Vec<Vec<usize>> = segments
        .iter()
        .map(|(path, _)| packet_sizes(path))
        .collect();
    // 48 kHz 的 AAC 每帧 1024 个采样点，即 1920 个 90 kHz 的 tick
    let frame = 1920;
    let (t0, t1) = (900_000, 900_000 + segments[0].1);
    let t2 = t1 + segments[1].1;
    let mut a = std::fs::read(&segments[0].0).unwrap();
    a[sizes[0][..10].iter().sum::<usize>()] = 0;
    let a_path = dir.join("a");
    std::fs::write(&a_path, a).unwrap();
    let seg1 = std::fs::read(&segments[1].0).unwrap();
    let mut oversized = seg1[..7].to_vec();
    oversized[3] |= 0x03;
    oversized[4] = 0xFF;
    oversized[5] |= 0xE0;
    let twenty: usize = sizes[1][..20].iter().sum();
    let b_path = dir.join("b");
    std::fs::write(&b_path, [oversized, seg1[..twenty].to_vec()].concat()).unwrap();
    let n0 = sizes[0].len();
    let lost_in_a = sizes[0][10] as u64 + 7;

    let three = TrackSegments {
        init: None,
        segments: segs([
            packed_segment(&dir, &a_path, Some(t0)),
            packed_segment(&dir, &b_path, Some(t1)),
            packed_segment(&dir, &segments[2].0, Some(t2)),
        ]),
        gaps: Vec::new(),
    };
    let report = remux(
        &[Streams::Audio],
        &[group([three])],
        &output,
        &Cancel::never(),
    )
    .unwrap();
    let n2 = sizes[2].len();
    assert_eq!(report.streams[0].packets, (n0 - 1 + 20 + n2) as u64);
    assert_eq!(report.streams[0].discarded_corrupt_bytes, lost_in_a);
    let a_out = timelines(&output).1;
    let ticks = |seconds: f64| ((seconds - a_out[0].0) * 90_000.0).round() as u64;
    assert_eq!(ticks(a_out[10].0), 11 * frame);
    assert_eq!(ticks(a_out[n0 - 2].0), (n0 as u64 - 1) * frame);
    assert_eq!(ticks(a_out[n0 - 1].0), t2 - t0 - 20 * frame);
    assert_eq!(ticks(a_out[n0 - 1 + 20].0), t2 - t0);

    let two = TrackSegments {
        init: None,
        segments: segs([
            packed_segment(&dir, &a_path, Some(t0)),
            packed_segment(&dir, &b_path, Some(t1)),
        ]),
        gaps: Vec::new(),
    };
    let report = remux(
        &[Streams::Audio],
        &[group([two])],
        &output,
        &Cancel::never(),
    )
    .unwrap();
    assert_eq!(report.streams[0].packets, (n0 - 1) as u64);
    assert_eq!(
        report.streams[0].discarded_corrupt_bytes,
        lost_in_a + twenty as u64
    );
}

/// 缺片下标须升序、在 1 到分片数减 1 之间。
#[test]
fn invalid_gaps_are_rejected() {
    let dir = work_dir("invalid_gaps");
    let output = dir.join("out.mp4");
    for gaps in [vec![0], vec![2], vec![1, 1]] {
        let mut ts = track("ts_a");
        ts.gaps = gaps;
        let err = remux(&[Streams::All], &[group([ts])], &output, &Cancel::never()).unwrap_err();
        assert!(
            matches!(err, Error::InvalidGaps { group: 0, track: 0 }),
            "实际 {err:?}"
        );
    }
}

/// 两个 TS 输入的起点跨在回绕点前 60 秒两侧：FFmpeg 读两个输入时展开的方向不同，读出的起点差 2^33，合并时对齐回来。
#[test]
fn ts_inputs_across_the_wrap_edge_stay_aligned() {
    let dir = work_dir("ts_wrap_edge");
    let output = dir.join("out.mp4");
    let (video, audio) = (track("ts_wrap"), track("ts_wrap_audio"));
    // 输入各自读出的起点（秒）：音频按回绕后的值读成负数
    let v_start = earliest(&timelines(&concat_track(&video, &dir, "v.ts")).0);
    let a_start = earliest(&timelines(&concat_track(&audio, &dir, "a.ts")).1);
    let wrap = (1u64 << 33) as f64 / 90_000.0;
    assert!(a_start < 0.0 && v_start > wrap / 2.0, "样本应跨在界线两侧");
    let expected = a_start + wrap - v_start;

    remux(
        &[Streams::Video, Streams::Audio],
        &[group([video, audio])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let (v_out, a_out) = timelines(&output);
    let lead = earliest(&a_out) - earliest(&v_out);
    assert!(
        (lead - expected).abs() < 0.001,
        "音频应比视频晚 {expected} 秒开始，实际 {lead}"
    );
}

/// 两条轨的起点跨在回绕点前 60 秒两侧：打包音频按 ID3 时间戳展开，与 TS 视频读出的起点差 2^33，合并时对齐回来。
#[test]
fn packed_audio_aligns_with_ts_across_the_wrap_edge() {
    let dir = work_dir("packed_wrap_edge");
    let output = dir.join("out.mp4");
    let video = track("ts_wrap");
    let v_in = timelines(&concat_track(&video, &dir, "v.ts")).0;
    let video_start = (earliest(&v_in) * 90_000.0).round() as u64;
    let start = video_start + 9_000;
    let edge = (1u64 << 33) - 60 * 90_000;
    assert!(video_start < edge && start >= edge, "样本应跨在界线两侧");
    let segments = packed_fixture("aac");
    let audio = TrackSegments {
        init: None,
        segments: segs([packed_segment(&dir, &segments[0].0, Some(start))]),
        gaps: Vec::new(),
    };

    remux(
        &[Streams::Video, Streams::Audio],
        &[group([video, audio])],
        &output,
        &Cancel::never(),
    )
    .unwrap();

    let (v_out, a_out) = timelines(&output);
    let lead = a_out[0].0 - earliest(&v_out);
    assert!(
        (lead - 0.1).abs() < 0.001,
        "音频应比视频晚 0.1 秒开始，实际 {lead}"
    );
}

/// 打包音频有分片没有 ID3 时间戳：与别的轨同组时不知道怎样对齐，放不进 MP4；只有它一条轨时按样本数从头接续。
#[test]
fn untimed_packed_audio_is_used_only_alone() {
    let dir = work_dir("packed_untimed");
    let output = dir.join("out.mp4");
    let segments = packed_fixture("aac");
    let untimed = TrackSegments {
        init: None,
        segments: segs(segments.iter().map(|(path, _)| path.clone())),
        gaps: Vec::new(),
    };
    // 只有第一片有时间戳，第二片前缺了一片
    let partly = TrackSegments {
        init: None,
        segments: segs([
            packed_segment(&dir, &segments[0].0, Some(900_000)),
            segments[2].0.clone(),
        ]),
        gaps: Vec::new(),
    };

    for audio in [&untimed, &partly] {
        let err = remux(
            &[Streams::Video, Streams::Audio],
            &[group([track("ts_a"), audio.clone()])],
            &output,
            &Cancel::never(),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                Error::Unsupported(Unsupported::UntimedPackedAudio { group: 0, track: 1 })
            ),
            "实际 {err:?}"
        );
    }

    let report = remux(
        &[Streams::Audio],
        &[group([untimed])],
        &output,
        &Cancel::never(),
    )
    .unwrap();
    let packets: usize = segments
        .iter()
        .map(|(path, _)| timelines(path).1.len())
        .sum();
    assert_eq!(report.streams[0].packets, packets as u64);
    // 有时间戳的那片也按样本数接续：缺的那片不留空档
    let report = remux(
        &[Streams::Audio],
        &[group([partly])],
        &output,
        &Cancel::never(),
    )
    .unwrap();
    let expected = (segments[0].1 + segments[2].1) as f64 / 90_000.0;
    let duration = report.streams[0].duration_us as f64 / 1e6;
    assert!(
        (duration - expected).abs() < 0.001,
        "应连续 {expected} 秒，实际 {duration}"
    );
}
