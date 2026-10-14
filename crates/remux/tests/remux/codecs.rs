//! 各编码能否放进 MP4：原样复制的与不收的。

use std::path::Path;

use ffmpeg_next as ffmpeg;
use hs_m3u8_remux::{Control, Error, StreamKind, Streams, TrackSegments, Unsupported, remux};

use crate::{
    concat_track, group, packed_fixture, packed_segment, segs, timelines, track, work_dir,
};

/// 文件里各路流的编码名，按流的顺序。
fn codecs(path: &Path) -> Vec<&'static str> {
    ffmpeg::init().unwrap();
    let ictx = ffmpeg::format::input(path).unwrap();
    ictx.streams().map(|s| s.parameters().id().name()).collect()
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
            &Control::default(),
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

/// TS 里每帧 3 块的 E-AC-3：MP4 封装器会把几帧并成一个样本、丢掉末尾凑不满的，放不进 MP4。
#[test]
fn eac3_with_fewer_than_six_blocks_per_frame_is_rejected() {
    let dir = work_dir("eac3_short");
    let output = dir.join("out.mp4");
    let err = remux(
        &[Streams::Audio],
        &[group([track("ts_eac3_short")])],
        &output,
        &Control::default(),
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
        &Control::default(),
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
        &Control::default(),
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
        &Control::default(),
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
            &Control::default(),
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
            &Control::default(),
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

#[test]
fn hevc_output_is_tagged_hvc1() {
    let dir = work_dir("hevc");
    let output = dir.join("out.mp4");
    let hevc = track("ts_hevc");

    remux(
        &[Streams::All],
        &[group([hevc.clone()])],
        &output,
        &Control::default(),
    )
    .unwrap();

    let (v_in, a_in) = timelines(&concat_track(&hevc, &dir, "in.ts"));
    let (v_out, a_out) = timelines(&output);
    assert_eq!((v_out.len(), a_out.len()), (v_in.len(), a_in.len()));
    let data = std::fs::read(&output).unwrap();
    let has = |tag: &[u8]| data.windows(4).any(|w| w == tag);
    assert!(has(b"hvc1") && !has(b"hev1"), "HEVC 输出流应标 hvc1");
}
