//! remux 的编解码测试：用 tests/fixtures/media 下按 1 秒切好的合成 HLS 分片，核对输出的包数、时间线与 MP4 结构。

use std::path::{Path, PathBuf};

use ffmpeg_next as ffmpeg;
use hs_m3u8_remux::{
    DiscontinuityGroup, Error, Shape, StreamKind, Streams, TrackSegments, Unsupported, remux,
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
    TrackSegments {
        init: init.exists().then_some(init),
        segments: segments.into_iter().map(|(_, p)| p).collect(),
    }
}

fn track(name: &str) -> TrackSegments {
    track_in(&fixtures().join(name))
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
    for p in t.init.iter().chain(&t.segments) {
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
        .map(|path| {
            let ticks = (span(&timelines(&path).1) * 90_000.0).round() as u64;
            (path, ticks)
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
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(path).unwrap();
    let mut len = 0;
    for _ in 0..frames {
        let mut packet = ffmpeg::Packet::empty();
        packet.read(&mut ictx).unwrap();
        len += packet.size();
    }
    std::fs::read(path).unwrap()[..len].to_vec()
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

    let report = remux(&[Streams::All], &[group([a.clone()])], &output).unwrap();

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
    assert!(
        (0.0..0.1).contains(&start),
        "输出应从 0 附近开始，实际 {start}"
    );

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

    remux(&[Streams::All], &[group([hevc.clone()])], &output).unwrap();

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

    let report = remux(&[Streams::All], &[group([track_in(&dir)])], &output).unwrap();

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

    let err = remux(&[Streams::All], &[group([mixed])], &output).unwrap_err();

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

    let err = remux(&[Streams::Video], &[group([track("ts_repeat")])], &output).unwrap_err();

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

/// 另一条轨提供音频时只取本轨的视频：本轨混着的音频不进输出，其编码（MP2）也不检查。
#[test]
fn unwanted_streams_are_dropped_without_codec_checks() {
    let dir = work_dir("unwanted_streams");
    let output = dir.join("out.mp4");
    let (muxed, audio) = (track("ts_mp2"), track("fmp4_a/audio"));

    let report = remux(
        &[Streams::Video, Streams::Audio],
        &[group([muxed.clone(), audio.clone()])],
        &output,
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
    )
    .unwrap();

    let a_in = timelines(&concat_track(&audio, &dir, "a.mp4")).1;
    let streams: Vec<_> = report
        .streams
        .iter()
        .map(|s| (s.shape.kind(), s.packets))
        .collect();
    assert_eq!(streams, vec![(StreamKind::Audio, a_in.len() as u64)]);

    let err = remux(&[Streams::Video], &[group([audio_only])], &output).unwrap_err();
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

/// fMP4 里的 AV1、VP9 视频与 Opus、FLAC、ALAC 音频原样复制，MP4 里的样本描述带各自的配置 box。
#[test]
fn av1_vp9_opus_flac_and_alac_are_copied() {
    let dir = work_dir("fmp4_codecs");
    for (name, codecs, boxes) in [
        ("fmp4_av1_opus", ["av1", "opus"], [b"av1C", b"dOps"]),
        ("fmp4_vp9_flac", ["vp9", "flac"], [b"vpcC", b"dfLa"]),
        ("fmp4_alac", ["h264", "alac"], [b"avcC", b"alac"]),
    ] {
        let muxed = track(name);
        let output = dir.join(format!("{name}.mp4"));
        let report = remux(&[Streams::All], &[group([muxed.clone()])], &output).unwrap();

        let (v_in, a_in) = timelines(&concat_track(&muxed, &dir, "in.mp4"));
        let found: Vec<(&str, u64)> = report
            .streams
            .iter()
            .map(|s| (s.shape.codec(), s.packets))
            .collect();
        assert_eq!(
            found,
            [
                (codecs[0], v_in.len() as u64),
                (codecs[1], a_in.len() as u64)
            ],
            "{name}"
        );
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

/// 组内中途换了编码参数：TS 的画面宽高与采样率、打包音频的编码都在第二个分片变了，MP4 的一条轨只有一个样本描述。
#[test]
fn parameter_changes_within_a_group_are_rejected() {
    let dir = work_dir("params_in_group");
    let output = dir.join("out.mp4");
    let changed = track("ts_param_change");
    for (streams, kind) in [
        (Streams::Video, StreamKind::Video),
        (Streams::Audio, StreamKind::Audio),
    ] {
        let err = remux(&[streams], &[group([changed.clone()])], &output).unwrap_err();
        assert!(
            matches!(
                &err,
                Error::Unsupported(Unsupported::ParamsChangedInGroup { group: 0, track: 0, kind: found })
                    if *found == kind
            ),
            "{kind}：实际 {err:?}"
        );
    }

    let (aac, ac3) = (packed_fixture("aac"), packed_fixture("ac3"));
    let audio = TrackSegments {
        init: None,
        segments: vec![
            packed_segment(&dir, &aac[0].0, Some(900_000)),
            packed_segment(&dir, &ac3[1].0, Some(900_000 + aac[0].1)),
        ],
    };
    let err = remux(&[Streams::Audio], &[group([audio])], &output).unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::ParamsChangedInGroup {
                kind: StreamKind::Audio,
                ..
            })
        ),
        "实际 {err:?}"
    );
}

/// 只核对：各轨开头的分片就能发现编码不受支持、流参数与第 0 组不同；能合并的通过，不写输出。
#[test]
fn check_finds_what_cannot_go_into_an_mp4_from_the_first_segments() {
    let head = |name: &str| {
        let t = track(name);
        TrackSegments {
            init: t.init,
            segments: t.segments[..1].to_vec(),
        }
    };
    hs_m3u8_remux::check(
        &[Streams::Video, Streams::Audio],
        &[
            group([head("fmp4_a/video"), head("fmp4_a/audio")]),
            group([head("fmp4_b/video"), head("fmp4_b/audio")]),
        ],
    )
    .unwrap();

    let err = hs_m3u8_remux::check(&[Streams::All], &[group([head("ts_mp2")])]).unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::Codec { codec: "mp2", .. })
        ),
        "实际 {err:?}"
    );
    let err = hs_m3u8_remux::check(
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

/// MP2 音频放不进 MP4；采样率低于 16 kHz 的 MP3 也放不进，报出采样率。
#[test]
fn mp2_and_low_rate_mp3_audio_are_rejected() {
    let dir = work_dir("rejected_audio");
    let output = dir.join("out.mp4");

    let err = remux(&[Streams::All], &[group([track("ts_mp2")])], &output).unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::Codec {
                kind: StreamKind::Audio,
                codec: "mp2",
                ..
            })
        ),
        "实际 {err:?}"
    );
    let err = remux(&[Streams::All], &[group([track("ts_mp3_8k")])], &output).unwrap_err();
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

/// AC-3、E-AC-3、MP3 音频：TS 里的与打包音频都原样复制，MP4 里的样本描述分别带 dac3、dec3 与 esds。
#[test]
fn ac3_eac3_and_mp3_audio_are_copied() {
    let dir = work_dir("more_audio");
    let output = dir.join("out.mp4");

    for (ts, packed, codec, sample_entry) in [
        ("ts_ac3", "ac3", "ac3", b"dac3"),
        ("ts_eac3", "eac3", "eac3", b"dec3"),
        ("ts_mp3", "mp3", "mp3", b"esds"),
    ] {
        let muxed = track(ts);
        let report = remux(&[Streams::All], &[group([muxed.clone()])], &output).unwrap();
        let a_in = timelines(&concat_track(&muxed, &dir, "in.ts")).1;
        assert_eq!(
            (report.streams[1].shape.codec(), report.streams[1].packets),
            (codec, a_in.len() as u64),
            "{ts}"
        );
        assert!(
            std::fs::read(&output)
                .unwrap()
                .windows(4)
                .any(|w| w == sample_entry),
            "{ts} 的输出应有 {}",
            String::from_utf8_lossy(sample_entry)
        );

        let segments = packed_fixture(packed);
        let mut at = 900_000;
        let mut tagged = Vec::new();
        for (path, ticks) in &segments {
            tagged.push(packed_segment(&dir, path, Some(at)));
            at += ticks;
        }
        let audio = TrackSegments {
            init: None,
            segments: tagged,
        };
        let report = remux(&[Streams::Audio], &[group([audio])], &output).unwrap();
        let packets: usize = segments
            .iter()
            .map(|(path, _)| timelines(path).1.len())
            .sum();
        assert_eq!(
            (report.streams[0].shape.codec(), report.streams[0].packets),
            (codec, packets as u64),
            "{packed}"
        );
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
        segments: vec![
            packed_segment(&dir, &segments[0].0, Some(start)),
            packed_segment(
                &dir,
                &segments[2].0,
                Some(start + segments[0].1 + segments[1].1),
            ),
        ],
    };

    remux(
        &[Streams::Video, Streams::Audio],
        &[group([video, audio])],
        &output,
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
        segments: vec![
            packed_segment(&dir, &segments[0].0, Some(wrap - segments[0].1)),
            packed_segment(&dir, &segments[1].0, Some(0)),
        ],
    };

    let report = remux(&[Streams::Audio], &[group([audio])], &output).unwrap();

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
                segments: vec![
                    packed_segment(&dir, &first.0, Some(900_000)),
                    packed_segment(&dir, &tail, Some(900_000 + first.1)),
                ],
            };

            let report = remux(&[Streams::Audio], &[group([audio])], &output).unwrap();

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
        segments: vec![packed_segment(&dir, &segments[0].0, Some(start))],
    };

    remux(
        &[Streams::Video, Streams::Audio],
        &[group([video, audio])],
        &output,
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
        segments: segments.iter().map(|(path, _)| path.clone()).collect(),
    };
    // 只有第一片有时间戳，第二片前缺了一片
    let partly = TrackSegments {
        init: None,
        segments: vec![
            packed_segment(&dir, &segments[0].0, Some(900_000)),
            segments[2].0.clone(),
        ],
    };

    for audio in [&untimed, &partly] {
        let err = remux(
            &[Streams::Video, Streams::Audio],
            &[group([track("ts_a"), audio.clone()])],
            &output,
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

    let report = remux(&[Streams::Audio], &[group([untimed])], &output).unwrap();
    let packets: usize = segments
        .iter()
        .map(|(path, _)| timelines(path).1.len())
        .sum();
    assert_eq!(report.streams[0].packets, packets as u64);
    // 有时间戳的那片也按样本数接续：缺的那片不留空档
    let report = remux(&[Streams::Audio], &[group([partly])], &output).unwrap();
    let expected = (segments[0].1 + segments[2].1) as f64 / 90_000.0;
    let duration = report.streams[0].duration_us as f64 / 1e6;
    assert!(
        (duration - expected).abs() < 0.001,
        "应连续 {expected} 秒，实际 {duration}"
    );
}
