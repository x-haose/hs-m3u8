//! 合并的基本行为：从 0 起的时间线、音视频分离、不连续段首尾相接、各轨取哪些流、输入的读取与探测。

use std::path::Path;

use hs_m3u8_remux::{Control, Error, Shape, StreamKind, Streams, Unsupported, remux};

use crate::{Timeline, concat_track, fixtures, group, span, timelines, track, track_in, work_dir};

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
        &Control::default(),
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
            (stream.duration.as_micros() as f64 - read_back_us).abs() < 1000.0,
            "报告 {:?}，回读 {read_back_us} µs",
            stream.duration
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
        &Control::default(),
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
        &Control::default(),
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
        &Control::default(),
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
        &Control::default(),
    )
    .unwrap();

    assert_eq!(report.streams.len(), 2);
    assert!(output.exists());
}

#[test]
fn same_stream_kind_from_two_tracks_is_rejected() {
    let dir = work_dir("duplicate");
    let output = dir.join("out.mp4");

    let err = remux(
        &[Streams::All; 2],
        &[group([track("ts_a"), track("fmp4_a/audio")])],
        &output,
        &Control::default(),
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
        &Control::default(),
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
        &Control::default(),
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
        &Control::default(),
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::Unsupported(Unsupported::NoStreams)),
        "实际 {err:?}"
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
        &Control::default(),
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
        &Control::default(),
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

/// 缺片下标须升序、在 1 到分片数减 1 之间。
#[test]
fn invalid_gaps_are_rejected() {
    let dir = work_dir("invalid_gaps");
    let output = dir.join("out.mp4");
    for gaps in [vec![0], vec![2], vec![1, 1]] {
        let mut ts = track("ts_a");
        ts.gaps = gaps;
        let err = remux(
            &[Streams::All],
            &[group([ts])],
            &output,
            &Control::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::InvalidGaps { group: 0, track: 0 }),
            "实际 {err:?}"
        );
    }
}
