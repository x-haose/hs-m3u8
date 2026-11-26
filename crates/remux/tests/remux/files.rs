//! 合并完整文件：各输入格式与编码进 MP4 与 Matroska，各输入的时间线各自平移，报错、进度与取消。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ffmpeg_next as ffmpeg;
use hs_m3u8_remux::{
    Cancel, Codec, Container, Control, Error, FileInput, MediaReport, Origin, StreamKind, Streams,
    Unsupported, WriteProgress, merge_files,
};

use crate::{Timeline, earliest, fixtures, span, timelines, track, work_dir};

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
                at: Origin::Input(1),
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
                at: Origin::Input(0),
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
            Error::DuplicateVideo {
                at: Origin::Input(1)
            }
        ),
        "{err:?}"
    );
    let err = run(&[input("audio.ac3", Streams::Audio)]);
    assert!(
        matches!(&err, Error::FileFormat { at: Origin::Input(0), path, format: Some(format) }
            if path == &file("audio.ac3") && format == "ac3"),
        "{err:?}"
    );
    // 认不出的格式（链接的 FFmpeg 没有它的解复用器，如 WAV）与乱码一样报格式不收
    let wav = dir.join("tone.wav");
    let header = [
        b"RIFF".as_slice(),
        &36u32.to_le_bytes(),
        b"WAVEfmt ",
        &16u32.to_le_bytes(),
        &[1, 0, 1, 0],
        &48_000u32.to_le_bytes(),
        &96_000u32.to_le_bytes(),
        &[2, 0, 16, 0],
        b"data",
        &0u32.to_le_bytes(),
    ]
    .concat();
    std::fs::write(&wav, header).unwrap();
    let garbage = dir.join("garbage.bin");
    std::fs::write(&garbage, [0x5Au8; 4096]).unwrap();
    for path in [wav, garbage] {
        let err = run(&[FileInput {
            path: path.clone(),
            streams: Streams::Audio,
        }]);
        assert!(
            matches!(&err, Error::FileFormat { at: Origin::Input(0), path: p, format: None } if *p == path),
            "{err:?}"
        );
    }
    // 认得出是 Matroska（EBML 头完整）、后面是乱码，打不开
    let broken = dir.join("broken.mkv");
    let head = &std::fs::read(file("audio_flac.mkv")).unwrap()[..40];
    std::fs::write(&broken, [head, &[0x5A; 4096]].concat()).unwrap();
    let err = run(&[FileInput {
        path: broken,
        streams: Streams::Audio,
    }]);
    assert!(
        matches!(
            err,
            Error::OpenInput {
                at: Origin::Input(0),
                ..
            }
        ),
        "{err:?}"
    );
}

/// 输出与某个输入是同一个文件（同一路径或硬链接）时不动它，报出是哪个输入。
#[test]
fn output_that_is_an_input_is_refused() {
    let dir = work_dir("files_output_is_input");
    let audio = dir.join("audio.m4a");
    std::fs::copy(file("audio_aac.m4a"), &audio).unwrap();
    let linked = dir.join("linked.mp4");
    std::fs::hard_link(&audio, &linked).unwrap();
    let original = std::fs::read(&audio).unwrap();
    let inputs = [
        input("video_h264_frag.mp4", Streams::Video),
        FileInput {
            path: audio.clone(),
            streams: Streams::Audio,
        },
    ];
    for output in [&audio, &linked] {
        let err = merge_files(&inputs, output, Container::Mp4, &Control::default()).unwrap_err();
        assert!(
            matches!(
                err,
                Error::OutputIsInput {
                    at: Origin::Input(1)
                }
            ),
            "{err:?}"
        );
    }
    assert_eq!(std::fs::read(&audio).unwrap(), original);
}

/// 进 MP4 的 E-AC-3 不论来自什么容器都核对帧结构：Matroska 里每帧 3 块的报放不进 MP4，进 Matroska 照常。
#[test]
fn eac3_with_short_frames_from_matroska_is_rejected_for_mp4() {
    let dir = work_dir("files_eac3_short");
    let inputs = [input("audio_eac3_short.mkv", Streams::Audio)];
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
            Error::Unsupported(Unsupported::Eac3ShortFrames {
                at: Origin::Input(0)
            })
        ),
        "{err:?}"
    );
    merge(&dir, &inputs, Container::Matroska);
}

/// 视频一个关键帧都没有的文件（长 GOP 的 TS 截掉开头）：报没有关键帧，指出是哪个输入。
#[test]
fn video_without_keyframes_is_reported() {
    let dir = work_dir("files_no_keyframe");
    let headless = track("ts_long_gop").segments[1].path.clone();
    let err = merge_files(
        &[
            input("audio_aac.m4a", Streams::Audio),
            FileInput {
                path: headless,
                streams: Streams::Video,
            },
        ],
        &dir.join("out.mp4"),
        Container::Mp4,
        &Control::default(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Unsupported(Unsupported::NoKeyframe {
                at: Origin::Input(1)
            })
        ),
        "{err:?}"
    );
}

/// Matroska 的时间戳只精确到毫秒：48 kHz 的 AAC（一帧 21.333 毫秒）进 MP4 后按采样点接续，每个样本都是 1024 个采样点。
#[test]
fn audio_from_millisecond_timestamps_keeps_whole_frames() {
    let dir = work_dir("files_sample_clock");
    let (output, report) = merge(
        &dir,
        &[input("audio_aac.mkv", Streams::Audio)],
        Container::Mp4,
    );
    let audio = timelines(&output).1;
    assert_eq!(report.streams[0].packets, audio.len() as u64);
    let frame = 1024.0 / 48_000.0;
    for pair in audio.windows(2) {
        assert!(
            (pair[1].0 - pair[0].0 - frame).abs() < 1e-6,
            "相邻样本相隔 {} 秒",
            pair[1].0 - pair[0].0
        );
    }
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

/// 失败时原有的输出不变、临时文件删掉；上次留下的临时文件是指向别处的符号链接时先删掉它，不顺着写到别处。
#[test]
fn output_is_replaced_only_on_success() {
    let dir = work_dir("files_replace");
    let output = dir.join("out.mp4");
    let temp = dir.join("out.mp4.part");
    std::fs::write(&output, "old").unwrap();
    let headless = track("ts_long_gop").segments[1].path.clone();
    let err = merge_files(
        &[FileInput {
            path: headless,
            streams: Streams::Video,
        }],
        &output,
        Container::Mp4,
        &Control::default(),
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::Unsupported(Unsupported::NoKeyframe { .. })),
        "{err:?}"
    );
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "old");
    assert!(!temp.exists());

    #[cfg(unix)]
    {
        let victim = dir.join("victim");
        std::fs::write(&victim, "user file").unwrap();
        std::os::unix::fs::symlink(&victim, &temp).unwrap();
        merge(
            &dir,
            &[input("audio_aac.m4a", Streams::Audio)],
            Container::Mp4,
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "user file");
        assert!(std::fs::symlink_metadata(&temp).is_err());
        assert_ne!(std::fs::read(&output).unwrap(), b"old");
    }
}

/// 输出或输入是 FIFO 时不打开它等写方：输出照常换上，输入报不是普通文件。
#[cfg(unix)]
#[test]
fn fifos_do_not_block() {
    let dir = work_dir("files_fifo");
    let fifo = |name: &str| {
        let path = dir.join(name);
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: c 是 NUL 结尾的路径。
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        path
    };
    let output = fifo("out.mp4");
    merge_files(
        &[input("audio_aac.m4a", Streams::Audio)],
        &output,
        Container::Mp4,
        &Control::default(),
    )
    .unwrap();
    assert!(std::fs::metadata(&output).unwrap().is_file());

    let err = merge_files(
        &[FileInput {
            path: fifo("in.m4a"),
            streams: Streams::Audio,
        }],
        &dir.join("other.mp4"),
        Container::Mp4,
        &Control::default(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::Io { cause, .. } if cause.kind() == std::io::ErrorKind::InvalidInput),
        "{err:?}"
    );
}
