//! 组内的时间戳：往回跳、往前跳与不算跳变的中断，以及 33 位时间戳的回绕。

use hs_m3u8_remux::{
    Control, Error, Segment, StreamKind, Streams, TrackSegments, Unsupported, remux,
};

use crate::{concat_track, earliest, group, timelines, track, work_dir};

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
        &Control::default(),
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
        &Control::default(),
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

/// 组内时间戳往前跳了约 1000 秒（来源在时间戳重新开始处漏标了 EXT-X-DISCONTINUITY）：照写会在输出里留下一大段空档，报
/// TimestampJump，报出的空档长度为跳过的时长减去上一片声明的时长。两片之间缺了分片时，跳过的可能就是缺的那几片的时长，
/// 不核对，照常合并；声明的时长不准而内容连续时不误报。
#[test]
fn a_forward_timestamp_jump_within_a_group_is_rejected() {
    let dir = work_dir("timestamp_jump");
    let output = dir.join("out.mp4");
    let jump = track("ts_jump");

    let err = remux(
        &[Streams::All],
        &[group([jump.clone()])],
        &output,
        &Control::default(),
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
            }) if (998_000_000..1_000_000_000).contains(&jump_us)
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
        &Control::default(),
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
        &Control::default(),
    )
    .unwrap();
}

/// 时间戳在一个分片里面往前跳了约 1000 秒（两段内容拼成一片、声明 2 秒）：同样报 TimestampJump，跳变处在第 0 片。
#[test]
fn a_timestamp_jump_inside_a_segment_is_rejected() {
    let dir = work_dir("timestamp_jump_inside");
    let jump = track("ts_jump");
    let joined = dir.join("joined.ts");
    let data: Vec<u8> = jump
        .segments
        .iter()
        .flat_map(|s| std::fs::read(&s.path).unwrap())
        .collect();
    std::fs::write(&joined, data).unwrap();
    let one = TrackSegments {
        segments: vec![Segment {
            path: joined,
            duration_us: 2_000_000,
        }],
        ..jump
    };

    let err = remux(
        &[Streams::All],
        &[group([one])],
        &dir.join("out.mp4"),
        &Control::default(),
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::TimestampJump {
                segment: 0,
                jump_us,
                ..
            }) if (999_000_000..1_001_000_000).contains(&jump_us)
        ),
        "{err:?}"
    );
}

/// 独立的音频轨单独断了约 2 秒（第 1、2 片不在、没标缺片），视频轨照常连续：一路流单独中断不是时间戳跳变，照常合并，
/// 音频中间留空档。
#[test]
fn one_stream_pausing_alone_is_not_a_timestamp_jump() {
    let dir = work_dir("one_stream_pausing");
    let video = track("ts_long");
    let audio = TrackSegments {
        segments: vec![video.segments[0].clone(), video.segments[3].clone()],
        ..video.clone()
    };

    let report = remux(
        &[Streams::Video, Streams::Audio],
        &[group([video, audio])],
        &dir.join("out.mp4"),
        &Control::default(),
    )
    .unwrap();

    assert_eq!(report.streams.len(), 2);
}

/// 帧稀疏的纯视频：两片各声明 1 秒，一帧在第一片开头、一帧在第二片末尾，相邻两帧隔了将近两片，没有别的流推着时钟
/// 走：内容连续，不是时间戳跳变，照常合并。
#[test]
fn sparse_frames_across_segments_are_not_a_timestamp_jump() {
    let dir = work_dir("sparse_frames");
    let sparse = track("ts_sparse");
    let declared = TrackSegments {
        segments: sparse
            .segments
            .iter()
            .map(|s| Segment {
                path: s.path.clone(),
                duration_us: 1_000_000,
            })
            .collect(),
        ..sparse
    };

    let report = remux(
        &[Streams::All],
        &[group([declared])],
        &dir.join("out.mp4"),
        &Control::default(),
    )
    .unwrap();

    assert_eq!(report.streams[0].packets, 2);
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
        &Control::default(),
    )
    .unwrap();

    let (v_out, a_out) = timelines(&output);
    let lead = earliest(&a_out) - earliest(&v_out);
    assert!(
        (lead - expected).abs() < 0.001,
        "音频应比视频晚 {expected} 秒开始，实际 {lead}"
    );
}
