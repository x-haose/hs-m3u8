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

/// 另一条轨提供音频时只取本轨的视频：本轨混着的音频不进输出，其编码（MP3）也不检查。
#[test]
fn unwanted_streams_are_dropped_without_codec_checks() {
    let dir = work_dir("unwanted_streams");
    let output = dir.join("out.mp4");
    let (muxed, audio) = (track("ts_mp3"), track("fmp4_a/audio"));

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

#[test]
fn mp3_audio_is_rejected() {
    let dir = work_dir("mp3");
    let output = dir.join("out.mp4");

    let err = remux(&[Streams::All], &[group([track("ts_mp3")])], &output).unwrap_err();

    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::Codec {
                kind: StreamKind::Audio,
                codec: "mp3",
                ..
            })
        ),
        "实际 {err:?}"
    );
}
