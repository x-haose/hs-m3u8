//! 合并完整文件：各输入格式与编码进 MP4 与 Matroska，各输入的时间线各自平移，报错、进度与取消。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ffmpeg_next as ffmpeg;
use hs_m3u8_remux::{
    Cancel, Codec, Container, Control, Error, FileInput, MediaReport, StreamKind, Streams,
    Unsupported, WriteProgress, merge_files,
};

use crate::{Timeline, earliest, fixtures, span, timelines, work_dir};

fn file(name: &str) -> PathBuf {
    fixtures().join("files").join(name)
}

fn input(name: &str, streams: Streams) -> FileInput {
    FileInput {
        path: file(name),
        streams,
    }
}

/// 输出文件名的扩展名。
fn extension(container: Container) -> &'static str {
    match container {
        Container::Mp4 => "mp4",
        Container::Matroska => "mkv",
    }
}

fn merge(dir: &Path, inputs: &[FileInput], container: Container) -> (PathBuf, MediaReport) {
    let output = dir.join(format!("out.{}", extension(container)));
    let report = merge_files(inputs, &output, container, &Control::default()).unwrap();
    (output, report)
}

/// 文件里各路流的编码名，按流的顺序。
fn codecs(path: &Path) -> Vec<&'static str> {
    ffmpeg::init().unwrap();
    let ictx = ffmpeg::format::input(path).unwrap();
    ictx.streams().map(|s| s.parameters().id().name()).collect()
}

/// 输出的各路流（先视频后音频）与各输入里取的那路流包数相同，没有丢包，从 0 开始、时长与输入相同（差不过一帧）。
fn assert_copied(output: &Path, report: &MediaReport, sources: [&Timeline; 2], what: &str) {
    let (video, audio) = timelines(output);
    for (stream, (out, source)) in [video, audio].iter().zip(sources).enumerate() {
        let r = &report.streams[stream];
        assert_eq!(r.packets, source.len() as u64, "{what} 第 {stream} 路");
        assert_eq!(out.len(), source.len(), "{what} 第 {stream} 路");
        assert_eq!(
            (
                r.skipped_without_dts,
                r.skipped_before_keyframe,
                r.discarded_corrupt_bytes
            ),
            (0, 0, 0),
            "{what} 第 {stream} 路"
        );
        assert!(
            earliest(out).abs() < 0.05,
            "{what} 第 {stream} 路从 {}",
            earliest(out)
        );
        assert!(
            (span(out) - span(source)).abs() < 0.05,
            "{what} 第 {stream} 路时长 {} 与输入 {}",
            span(out),
            span(source)
        );
    }
}

/// YouTube 那样分开的单文件分段 MP4（H.264）与 M4A（AAC）合成 MP4。
#[test]
fn fragmented_mp4_and_m4a_merge_into_mp4() {
    let dir = work_dir("files_youtube_mp4");
    let (output, report) = merge(
        &dir,
        &[
            input("video_h264_frag.mp4", Streams::Video),
            input("audio_aac.m4a", Streams::Audio),
        ],
        Container::Mp4,
    );
    assert_eq!(codecs(&output), ["h264", "aac"]);
    let video = timelines(&file("video_h264_frag.mp4")).0;
    let audio = timelines(&file("audio_aac.m4a")).1;
    assert_copied(&output, &report, [&video, &audio], "MP4");
}

/// WebM 里的 VP9、AV1 与 Opus 合进 MP4（样本描述带 vpcC、av1C、dOps）与 Matroska。
#[test]
fn webm_vp9_av1_and_opus_merge_into_mp4_and_matroska() {
    let audio = timelines(&file("audio_opus.webm")).1;
    for (name, codec, config) in [
        ("video_vp9.webm", "vp9", b"vpcC"),
        ("video_av1.webm", "av1", b"av1C"),
    ] {
        let video = timelines(&file(name)).0;
        for container in [Container::Mp4, Container::Matroska] {
            let dir = work_dir(&format!("files_{codec}_{}", extension(container)));
            let (output, report) = merge(
                &dir,
                &[
                    input(name, Streams::Video),
                    input("audio_opus.webm", Streams::Audio),
                ],
                container,
            );
            let what = format!("{codec} 进 {container}");
            assert_eq!(codecs(&output), [codec, "opus"], "{what}");
            assert_copied(&output, &report, [&video, &audio], &what);
            if container == Container::Mp4 {
                let data = std::fs::read(&output).unwrap();
                for b in [config, b"dOps"] {
                    assert!(data.windows(4).any(|w| w == b), "{what}");
                }
            }
        }
    }
}

/// Vorbis 放不进 MP4，报出编码与容器；放得进 Matroska。
#[test]
fn vorbis_goes_into_matroska_but_not_mp4() {
    assert!(!Container::Mp4.holds(Codec::Vorbis));
    assert!(Container::Matroska.holds(Codec::Vorbis));
    let dir = work_dir("files_vorbis");
    let inputs = [
        input("video_vp9.webm", Streams::Video),
        input("audio_vorbis.webm", Streams::Audio),
    ];
    let err = merge_files(
        &inputs,
        &dir.join("out.mp4"),
        Container::Mp4,
        &Control::default(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::Codec {
                group: 0,
                track: 1,
                kind: StreamKind::Audio,
                codec: "vorbis",
                container: Container::Mp4,
            })
        ),
        "{err:?}"
    );
    let (output, report) = merge(&dir, &inputs, Container::Matroska);
    assert_eq!(codecs(&output), ["vp9", "vorbis"]);
    let video = timelines(&file("video_vp9.webm")).0;
    let audio = timelines(&file("audio_vorbis.webm")).1;
    assert_copied(&output, &report, [&video, &audio], "Vorbis 进 Matroska");
}

/// Matroska 里的 FLAC 与 E-AC-3 合进 MP4（dfLa、dec3）。
#[test]
fn flac_and_eac3_from_matroska_merge_into_mp4() {
    let video = timelines(&file("video_h264_frag.mp4")).0;
    for (name, codec, config) in [
        ("audio_flac.mkv", "flac", b"dfLa"),
        ("audio_eac3.mkv", "eac3", b"dec3"),
    ] {
        let dir = work_dir(&format!("files_{codec}"));
        let (output, report) = merge(
            &dir,
            &[
                input("video_h264_frag.mp4", Streams::Video),
                input(name, Streams::Audio),
            ],
            Container::Mp4,
        );
        assert_eq!(codecs(&output), ["h264", codec]);
        let audio = timelines(&file(name)).1;
        assert_copied(&output, &report, [&video, &audio], codec);
        let data = std::fs::read(&output).unwrap();
        assert!(data.windows(4).any(|w| w == config), "{codec}");
    }
}

/// 一个 Matroska 里的带 B 帧的 H.264 与 AC-3 一起取：Matroska 只存 PTS，DTS 由 FFmpeg 推出，没有包因缺 DTS 丢掉。
#[test]
fn matroska_with_b_frames_merges_whole() {
    let dir = work_dir("files_mkv_b_frames");
    let name = "av_h264_ac3.mkv";
    let (output, report) = merge(&dir, &[input(name, Streams::All)], Container::Mp4);
    assert_eq!(codecs(&output), ["h264", "ac3"]);
    let (video, audio) = timelines(&file(name));
    assert_copied(&output, &report, [&video, &audio], name);
}

/// TS 的时间戳从 1.4 秒开始、M4A 的从 0 开始：各自平移，合出来都从 0 开始。TS 里的 H.264（参数集在码流里）也能进
/// Matroska。
#[test]
fn inputs_with_different_starts_each_begin_at_zero() {
    let ts = timelines(&file("av_h264_aac.ts")).0;
    assert!(earliest(&ts) > 1.0, "TS 样本应从 1.4 秒左右开始");
    let audio = timelines(&file("audio_aac.m4a")).1;
    for container in [Container::Mp4, Container::Matroska] {
        let dir = work_dir(&format!("files_ts_{}", extension(container)));
        let (output, report) = merge(
            &dir,
            &[
                input("av_h264_aac.ts", Streams::Video),
                input("audio_aac.m4a", Streams::Audio),
            ],
            container,
        );
        assert_copied(&output, &report, [&ts, &audio], &container.to_string());
    }
}

/// 裸 AAC 与 MP3（带 ID3v2 与 Info 帧）与视频合并。
#[test]
fn raw_aac_and_mp3_merge_with_video() {
    let video = timelines(&file("video_vp9.webm")).0;
    for (name, codec) in [("audio.aac", "aac"), ("audio.mp3", "mp3")] {
        for container in [Container::Mp4, Container::Matroska] {
            let dir = work_dir(&format!("files_raw_{codec}_{}", extension(container)));
            let (output, report) = merge(
                &dir,
                &[
                    input("video_vp9.webm", Streams::Video),
                    input(name, Streams::Audio),
                ],
                container,
            );
            let what = format!("{name} 进 {container}");
            assert_eq!(codecs(&output), ["vp9", codec], "{what}");
            let audio = timelines(&file(name)).1;
            assert_copied(&output, &report, [&video, &audio], &what);
        }
    }
}

/// 调用方给的输入不对：没有输入、没有指定的流、同类流来自两个输入、格式不收或认不出。
#[test]
fn wrong_inputs_are_reported() {
    let dir = work_dir("files_wrong");
    let output = dir.join("out.mp4");
    let run = |inputs: &[FileInput]| {
        merge_files(inputs, &output, Container::Mp4, &Control::default()).unwrap_err()
    };
    assert!(matches!(run(&[]), Error::NoInputs));
    let err = run(&[input("video_vp9.webm", Streams::All)]);
    assert!(
        matches!(
            err,
            Error::MissingStream {
                track: 0,
                kind: StreamKind::Audio
            }
        ),
        "{err:?}"
    );
    let err = run(&[
        input("video_vp9.webm", Streams::Video),
        input("av_h264_ac3.mkv", Streams::All),
    ]);
    assert!(
        matches!(
            err,
            Error::DuplicateKind {
                track: 1,
                kind: StreamKind::Video
            }
        ),
        "{err:?}"
    );
    let err = run(&[input("audio.ac3", Streams::Audio)]);
    assert!(
        matches!(&err, Error::FileFormat { path, format } if path == &file("audio.ac3") && format == "ac3"),
        "{err:?}"
    );
    let garbage = dir.join("garbage.bin");
    std::fs::write(&garbage, [0x5Au8; 4096]).unwrap();
    let err = run(&[FileInput {
        path: garbage,
        streams: Streams::Audio,
    }]);
    assert!(
        matches!(
            err,
            Error::OpenInput {
                group: 0,
                track: 0,
                ..
            }
        ),
        "{err:?}"
    );
}

/// 进度以字节报，各次之和为各输入的字节数；取消后以 [`Error::Cancelled`] 结束。
#[test]
fn progress_counts_input_bytes_and_cancel_stops() {
    let dir = work_dir("files_progress");
    let inputs = [
        input("video_h264_frag.mp4", Streams::Video),
        input("audio_opus.webm", Streams::Audio),
    ];
    let total: u64 = inputs
        .iter()
        .map(|i| std::fs::metadata(&i.path).unwrap().len())
        .sum();
    let reports = Arc::new(Mutex::new(Vec::new()));
    let seen = reports.clone();
    let control = Control {
        progress: WriteProgress::new(move |bytes| seen.lock().unwrap().push(bytes)),
        ..Control::default()
    };
    merge_files(&inputs, &dir.join("out.mp4"), Container::Mp4, &control).unwrap();
    let reports = reports.lock().unwrap();
    assert!(reports.len() > 2, "应随写出逐步报出：{reports:?}");
    assert!(reports.iter().all(|&b| b > 0));
    assert_eq!(reports.iter().sum::<u64>(), total);

    // 写出一部分之后取消
    let written = Arc::new(AtomicU64::new(0));
    let counted = written.clone();
    let observed = written.clone();
    let control = Control {
        cancel: Cancel::new(move || observed.load(Ordering::Relaxed) > total / 3),
        progress: WriteProgress::new(move |bytes| {
            counted.fetch_add(bytes, Ordering::Relaxed);
        }),
    };
    let err = merge_files(
        &inputs,
        &dir.join("cancelled.mp4"),
        Container::Mp4,
        &control,
    )
    .unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert!(written.load(Ordering::Relaxed) < total);
}
