//! remux 的编解码测试：用 tests/fixtures/media 下的合成样本，核对输出各流的编码、包数与 MP4 结构。

use std::path::{Path, PathBuf};

use ffmpeg_next as ffmpeg;
use hs_m3u8_remux::{Error, StreamKind, remux};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/media")
        .join(name)
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

/// 文件中第一路视频与第一路音频的包数（无该类流时为 None）。
fn packet_counts(path: &Path) -> (Option<u64>, Option<u64>) {
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(path).unwrap();
    let first = |medium| {
        ictx.streams()
            .find(|s| s.parameters().medium() == medium)
            .map(|s| s.index())
    };
    let (video, audio) = (
        first(ffmpeg::media::Type::Video),
        first(ffmpeg::media::Type::Audio),
    );
    let (mut v, mut a) = (0u64, 0u64);
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut ictx) {
            Ok(()) if Some(packet.stream()) == video => v += 1,
            Ok(()) if Some(packet.stream()) == audio => a += 1,
            Ok(()) => {}
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => panic!("读取 {} 失败: {e}", path.display()),
        }
    }
    (video.map(|_| v), audio.map(|_| a))
}

/// MP4 顶层 box 的类型序列。
fn top_level_boxes(path: &Path) -> Vec<[u8; 4]> {
    let data = std::fs::read(path).unwrap();
    let mut boxes = Vec::new();
    let mut offset = 0usize;
    while offset + 8 <= data.len() {
        let size = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = data[offset + 4..offset + 8].try_into().unwrap();
        let size = match size {
            1 => u64::from_be_bytes(data[offset + 8..offset + 16].try_into().unwrap()),
            0 => (data.len() - offset) as u64,
            n => n,
        };
        boxes.push(kind);
        offset += usize::try_from(size).unwrap();
    }
    boxes
}

#[test]
fn ts_h264_aac_copies_every_packet_with_moov_first() {
    let dir = work_dir("ts_h264_aac");
    let input = fixture("h264_aac.ts");
    let output = dir.join("out.mp4");

    let report = remux(&[&input], &output).unwrap();

    let (video, audio) = packet_counts(&input);
    let kinds: Vec<_> = report
        .streams
        .iter()
        .map(|s| (s.kind, s.codec, s.packets, s.skipped_without_dts))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (StreamKind::Video, "h264", video.unwrap(), 0),
            (StreamKind::Audio, "aac", audio.unwrap(), 0)
        ]
    );
    assert_eq!(packet_counts(&output), (video, audio));

    let boxes = top_level_boxes(&output);
    let pos = |name: &[u8; 4]| boxes.iter().position(|b| b == name).unwrap();
    assert!(
        pos(b"moov") < pos(b"mdat"),
        "moov 应前置，实际顺序 {boxes:?}"
    );
}

#[test]
fn split_video_and_audio_renditions_merge_into_one_file() {
    let dir = work_dir("split");
    let (video_in, audio_in) = (fixture("split_video.mp4"), fixture("split_audio.mp4"));
    let output = dir.join("out.mp4");

    let report = remux(&[&video_in, &audio_in], &output).unwrap();

    let expected = (packet_counts(&video_in).0, packet_counts(&audio_in).1);
    assert_eq!(packet_counts(&output), expected);
    let kinds: Vec<_> = report.streams.iter().map(|s| (s.kind, s.codec)).collect();
    assert_eq!(
        kinds,
        vec![(StreamKind::Video, "h264"), (StreamKind::Audio, "aac")]
    );
}

#[test]
fn hevc_output_is_tagged_hvc1() {
    let dir = work_dir("hevc");
    let input = fixture("hevc_aac.ts");
    let output = dir.join("out.mp4");

    remux(&[&input], &output).unwrap();

    assert_eq!(packet_counts(&output), packet_counts(&input));
    let data = std::fs::read(&output).unwrap();
    let has = |tag: &[u8]| data.windows(4).any(|w| w == tag);
    assert!(has(b"hvc1") && !has(b"hev1"), "HEVC 输出流应标 hvc1");
}

#[test]
fn timestamp_regression_fails_and_leaves_no_files() {
    let dir = work_dir("regression");
    // 同一段 TS 拼两遍：第二段的时间戳从头开始，相当于未处理的 EXT-X-DISCONTINUITY
    let once = std::fs::read(fixture("h264_aac.ts")).unwrap();
    let input = dir.join("twice.ts");
    std::fs::write(&input, [once.as_slice(), once.as_slice()].concat()).unwrap();
    let output = dir.join("out.mp4");

    let err = remux(&[&input], &output).unwrap_err();

    assert!(matches!(err, Error::Mux(_)), "应为封装错误，实际 {err:?}");
    assert!(!output.exists());
    assert!(!dir.join("out.mp4.part").exists());
}

#[test]
fn second_input_with_same_stream_kind_is_rejected() {
    let dir = work_dir("duplicate");
    let output = dir.join("out.mp4");

    let err = remux(
        &[fixture("h264_aac.ts"), fixture("split_audio.mp4")],
        &output,
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            Error::DuplicateKind {
                kind: StreamKind::Audio,
                ..
            }
        ),
        "实际 {err:?}"
    );
    assert!(!output.exists());
    assert!(!dir.join("out.mp4.part").exists());
}
