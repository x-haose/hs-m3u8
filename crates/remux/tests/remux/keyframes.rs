//! 组开头解不出的视频：第一个关键帧之前的帧、开放 GOP 的前导帧，以及没有关键帧的组。

use std::path::Path;

use ffmpeg_next as ffmpeg;
use hs_m3u8_remux::{Control, Error, Streams, TrackSegments, Unsupported, remux};

use crate::{check, concat_track, group, timelines, track, work_dir};

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

/// 从 GOP 中间开始的组：第一个关键帧之前的视频包参考的帧不在输出里，不写出、计数，输出的视频每个包都解得出。
#[test]
fn video_before_the_first_keyframe_of_a_group_is_dropped_and_counted() {
    let dir = work_dir("before_keyframe");
    let output = dir.join("out.mp4");
    let mut cut = track("ts_cut");
    // 按字节切开的 TS：第二片从 GOP 中间开始
    cut.segments.remove(0);
    let v_in = timelines(&concat_track(&cut, &dir, "in.ts")).0;

    let report = remux(
        &[Streams::All],
        &[group([cut])],
        &output,
        &Control::default(),
    )
    .unwrap();

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
        &Control::default(),
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

/// H.264 开放 GOP、不带 recovery point SEI：从第二片起每片以非 IDR 的 I 帧开头，FFmpeg 不给它打关键帧标志，之后的帧
/// 参考它之前的帧。从第二片起的组没有关键帧，明确报错，不写出解不出的画面。
#[test]
fn an_h264_open_gop_without_recovery_points_has_no_keyframe() {
    let dir = work_dir("h264_open_gop");
    let gop = track("ts_h264_open_gop");
    let later = TrackSegments {
        segments: gop.segments[1..].to_vec(),
        ..gop.clone()
    };

    let err = remux(
        &[Streams::All],
        &[group([later])],
        &dir.join("out.mp4"),
        &Control::default(),
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::NoKeyframe { group: 0, track: 0 })
        ),
        "{err:?}"
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
        &Control::default(),
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
        &Control::default(),
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
