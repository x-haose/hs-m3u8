//! 组间、组内的变化：编码参数与解码配置（MP4 的一条轨只有一个样本描述），以及流布局（PID、分片格式）。

use ffmpeg_next as ffmpeg;
use hs_m3u8_remux::{
    Control, Error, Shape, StreamKind, Streams, TrackSegments, Unsupported, remux,
};

use crate::{check, group, packed_fixture, packed_segment, segs, track, work_dir};

#[test]
fn resolution_change_between_groups_is_rejected() {
    let dir = work_dir("params_changed");
    let output = dir.join("out.mp4");

    let err = remux(
        &[Streams::All],
        &[group([track("ts_a")]), group([track("ts_small")])],
        &output,
        &Control::default(),
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
        remux(&[Streams::Audio], &groups, &output, &Control::default()).unwrap();
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
        &Control::default(),
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
        &Control::default(),
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
            &Control::default(),
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
            &Control::default(),
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
        &Control::default(),
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
        &Control::default(),
    )
    .unwrap();

    // 一开始就有两路音频：取第一路，另一路一直列在节目里，不算换了 PID
    let two = track("ts_two_audio");
    let report = remux(
        &[Streams::All],
        &[group([two.clone()])],
        &output,
        &Control::default(),
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
        &Control::default(),
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
            &Control::default(),
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
