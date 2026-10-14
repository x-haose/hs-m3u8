//! 下载前只用各组开头的分片核对能否放进 MP4（见 `hs_m3u8_remux::check`）。

use std::path::PathBuf;

use hs_m3u8_remux::{
    Control, Error, Shape, StreamKind, Streams, TrackSegments, Unsupported, remux,
};

use crate::{
    check, first_frames, fixtures, group, packed_fixture, packed_segment, segs, track, work_dir,
};

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

/// 第 0 组开头的分片里没有关键帧（从 GOP 中间开始）：逐帧的参数核对留给合并，后面的组换了编码照样报出来。
#[test]
fn check_compares_with_a_first_group_that_has_no_keyframe_yet() {
    let long_gop = track("ts_long_gop");
    let mid_gop = TrackSegments {
        segments: long_gop.segments[1..].to_vec(),
        ..long_gop
    };
    let hevc = head("ts_hevc");

    let err = check(&[Streams::Video], &[group([mid_gop]), group([hevc])]).unwrap_err();

    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::ParamsChanged { group: 1, .. })
        ),
        "{err:?}"
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
    remux(
        &[Streams::All],
        &[group([late])],
        &output,
        &Control::default(),
    )
    .unwrap();
    let err = remux(
        &[Streams::All],
        &[group([head("ts_late_audio")])],
        &output,
        &Control::default(),
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
        &Control::default(),
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
        &Control::default(),
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
