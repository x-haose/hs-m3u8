//! 字幕轨：WebVTT 分片按 X-TIMESTAMP-MAP 对齐到画面，封进 Matroska（SubRip）或 MP4（mov_text），轨道写上标注。

use std::path::{Path, PathBuf};

use ffmpeg_next as ffmpeg;
use hs_m3u8_remux::{
    Container, Control, DiscontinuityGroup, Label, Segment, Shape, Streams, SubtitleMode, Track,
    TrackSegments,
};

use crate::{group, track, work_dir};

/// ts_a 第 0 组最早的 PTS（音频的第一个包），90 kHz：字幕的 MPEGTS 写它时，本地时刻即输出时刻。
const TS_A_START: u64 = 131_280;

/// 在 `dir` 里写两个字幕分片：第二片重写了跨片的第一条 cue（应去重）。
fn vtt_track(dir: &Path) -> TrackSegments {
    let header = format!("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:{TS_A_START},LOCAL:00:00:00.000\n\n");
    let first = "00:00.500 --> 00:01.500\n<b>一</b>\n\n00:00.800 --> 00:01.200\n二\n";
    let second =
        "00:00.500 --> 00:01.500\n<b>一</b>\n\n00:01.500 --> 00:01.900 line:0\n三 &amp; 四\n";
    let segments = [first, second]
        .iter()
        .enumerate()
        .map(|(i, body)| {
            let path = dir.join(format!("sub{i}.vtt"));
            std::fs::write(&path, format!("{header}{body}")).unwrap();
            Segment {
                path,
                duration_us: 1_000_000,
            }
        })
        .collect();
    TrackSegments {
        init: None,
        segments,
        gaps: Vec::new(),
    }
}

fn tracks() -> Vec<Track> {
    vec![
        Track {
            streams: Streams::All,
            label: Label::default(),
        },
        Track {
            streams: Streams::Subtitles,
            label: Label {
                language: Some("zh-Hans".into()),
                name: None,
                default: true,
                forced: false,
            },
        },
    ]
}

/// 输出里的字幕流。
struct SubtitleStream {
    codec: String,
    language: String,
    title: String,
    default: bool,
    /// (起点毫秒, 时长毫秒, 内容)
    packets: Vec<(i64, i64, Vec<u8>)>,
}

fn subtitle_stream(path: &Path) -> SubtitleStream {
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(path).unwrap();
    let stream = ictx
        .streams()
        .find(|s| s.parameters().medium() == ffmpeg::media::Type::Subtitle)
        .unwrap();
    let (index, tb) = (stream.index(), stream.time_base());
    let meta = |key| stream.metadata().get(key).unwrap_or_default().to_owned();
    let (language, title) = (meta("language"), meta("title"));
    let codec = stream.parameters().id().name().to_owned();
    let default = stream
        .disposition()
        .contains(ffmpeg::format::stream::Disposition::DEFAULT);
    let ms = |ts: i64| ts * 1000 * i64::from(tb.numerator()) / i64::from(tb.denominator());
    let mut packets = Vec::new();
    loop {
        let mut packet = ffmpeg::Packet::empty();
        if packet.read(&mut ictx).is_err() {
            break;
        }
        if packet.stream() == index {
            let data = packet.data().unwrap_or_default().to_vec();
            packets.push((ms(packet.pts().unwrap()), ms(packet.duration()), data));
        }
    }
    SubtitleStream {
        codec,
        language,
        title,
        default,
        packets,
    }
}

fn merged(container: Container, name: &str) -> (PathBuf, hs_m3u8_remux::Merged) {
    let dir = work_dir(name);
    let output = dir.join(name);
    let groups: Vec<DiscontinuityGroup> = vec![group([track("ts_a"), vtt_track(&dir)])];
    let merged = hs_m3u8_remux::remux(
        &tracks(),
        &groups,
        &output,
        container,
        SubtitleMode::Embed,
        &Control::default(),
    )
    .unwrap();
    (output, merged)
}

/// Matroska：每条 cue 一个 SubRip 包，重叠的照样重叠，跨片重复的只留一条；语言写书目代码，标题写原标签。
#[test]
fn subtitles_are_aligned_and_embedded_in_matroska() {
    let (output, merged) = merged(Container::Matroska, "sub.mkv");
    let shapes: Vec<Shape> = merged.report.streams.iter().map(|s| s.shape).collect();
    assert_eq!(shapes[2], Shape::Subtitle { codec: "subrip" });
    assert_eq!(merged.report.streams[2].packets, 3);
    let starts: Vec<i64> = merged.subtitles[0]
        .cues
        .iter()
        .map(|c| c.start_us)
        .collect();
    assert_eq!(starts, [500_000, 800_000, 1_500_000]);

    let SubtitleStream {
        codec,
        language,
        title,
        default,
        packets,
    } = subtitle_stream(&output);
    assert_eq!(
        (codec.as_str(), language.as_str(), title.as_str(), default),
        ("subrip", "chi", "zh-Hans", true)
    );
    assert_eq!(
        packets,
        [
            (500, 1000, b"<b>\xe4\xb8\x80</b>".to_vec()),
            (800, 400, "二".as_bytes().to_vec()),
            (1500, 400, "三 & 四".as_bytes().to_vec()),
        ]
    );
}

/// MP4：mov_text 同一时刻一个样本，重叠的合成一段；封装器在空档与末尾补清屏的空样本；语言写术语代码。
#[test]
fn subtitles_are_sliced_into_mov_text_for_mp4() {
    let (output, merged) = merged(Container::Mp4, "sub.mp4");
    assert_eq!(
        merged.report.streams[2].shape,
        Shape::Subtitle { codec: "mov_text" }
    );
    let SubtitleStream {
        codec,
        language,
        packets,
        ..
    } = subtitle_stream(&output);
    assert_eq!((codec.as_str(), language.as_str()), ("mov_text", "zho"));
    let sample = |text: &str| {
        let mut s = u16::try_from(text.len()).unwrap().to_be_bytes().to_vec();
        s.extend_from_slice(text.as_bytes());
        s
    };
    let timed: Vec<(i64, Vec<u8>)> = packets
        .into_iter()
        .filter(|(_, _, data)| data.len() > 2)
        .map(|(start, _, data)| (start, data))
        .collect();
    assert_eq!(
        timed,
        [
            (500, sample("一")),
            (800, sample("一\n二")),
            (1200, sample("一")),
            (1500, sample("三 & 四")),
        ]
    );
}

/// 不封进输出时只给出对齐后的 cue，输出里没有字幕流。
#[test]
fn subtitles_can_be_left_out_of_the_output() {
    let dir = work_dir("sub_separate");
    let output = dir.join("out.mkv");
    let groups = vec![group([track("ts_a"), vtt_track(&dir)])];
    let merged = hs_m3u8_remux::remux(
        &tracks(),
        &groups,
        &output,
        Container::Matroska,
        SubtitleMode::CuesOnly,
        &Control::default(),
    )
    .unwrap();
    assert_eq!(merged.report.streams.len(), 2);
    assert_eq!(merged.subtitles[0].cues.len(), 3);
}

/// 几条音频轨各成一路，语言换成 ISO 639-2，名称照写；没有标默认的时第一路音频为默认。
#[test]
fn audio_tracks_carry_their_labels() {
    let dir = work_dir("labels");
    let output = dir.join("labels.mkv");
    let audio = |language: &str, name: Option<&str>| Track {
        streams: Streams::Audio,
        label: Label {
            language: Some(language.into()),
            name: name.map(str::to_owned),
            ..Label::default()
        },
    };
    let tracks = [
        Track {
            streams: Streams::Video,
            label: Label::default(),
        },
        audio("de-AT", Some("Deutsch")),
        audio("yue", None),
    ];
    let groups = vec![group([track("ts_a"), track("ts_a"), track("ts_a")])];
    hs_m3u8_remux::remux(
        &tracks,
        &groups,
        &output,
        Container::Matroska,
        SubtitleMode::Embed,
        &Control::default(),
    )
    .unwrap();

    ffmpeg::init().unwrap();
    let ictx = ffmpeg::format::input(&output).unwrap();
    let labels: Vec<(String, String, bool)> = ictx
        .streams()
        .filter(|s| s.parameters().medium() == ffmpeg::media::Type::Audio)
        .map(|s| {
            let meta = |key| s.metadata().get(key).unwrap_or_default().to_owned();
            let default = s
                .disposition()
                .contains(ffmpeg::format::stream::Disposition::DEFAULT);
            (meta("language"), meta("title"), default)
        })
        .collect();
    assert_eq!(
        labels,
        [
            ("ger".into(), "Deutsch".into(), true),
            // 链接的 FFmpeg 读 Matroska 时语言为 und 的不给出
            (String::new(), "yue".into(), false)
        ]
    );
}

/// 两个不连续段组：每组的字幕按自己的 X-TIMESTAMP-MAP 对齐到这一组的画面，第二组的 cue 落在第二组的起点之后；超出这一组
/// 声明时长的 cue 截到结尾，整条在组外的丢掉并计数，时间行写坏的块同样计数。
#[test]
fn subtitles_follow_each_discontinuity_group() {
    let dir = work_dir("sub_groups");
    let output = dir.join("groups.mkv");
    let vtt = |name: &str, body: &str| {
        let path = dir.join(name);
        let header = format!("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:{TS_A_START},LOCAL:00:00:00.000\n\n");
        std::fs::write(&path, format!("{header}{body}")).unwrap();
        TrackSegments {
            init: None,
            segments: vec![Segment {
                path,
                duration_us: 1_000_000,
            }],
            gaps: Vec::new(),
        }
    };
    let video = track("ts_a");
    let declared_us: u64 = video.segments.iter().map(|s| s.duration_us).sum();
    let last = declared_us / 1000;
    let clip_from = last - 100;
    let first = vtt(
        "g0.vtt",
        &format!(
            "00:00.500 --> 00:00.900\n一\n\n00:{:02}.{:03} --> 00:09.000\n截到结尾\n\n\
             00:09.000 --> 00:09.500\n组外\n\n00:01.000 --> 写坏了\n坏\n",
            clip_from / 1000,
            clip_from % 1000
        ),
    );
    let second = vtt("g1.vtt", "00:00.500 --> 00:00.900\n二\n");
    let groups = vec![group([video.clone(), first]), group([video, second])];
    let merged = hs_m3u8_remux::remux(
        &tracks(),
        &groups,
        &output,
        Container::Matroska,
        SubtitleMode::Embed,
        &Control::default(),
    )
    .unwrap();

    let subtitles = &merged.subtitles[0];
    let times: Vec<(i64, i64, &str)> = subtitles
        .cues
        .iter()
        .map(|c| (c.start_us / 1000, c.end_us / 1000, c.text.as_str()))
        .collect();
    let last = i64::try_from(last).unwrap();
    assert_eq!(times[0], (500, 900, "一"));
    assert_eq!(times[1], (last - 100, last, "截到结尾"));
    assert_eq!(times[2].2, "二");
    // 第二组从第一组的画面结尾开始（组间首尾相接；ts_a 的画面比声明的时长略短），字幕与它一起平移
    let second_start = times[2].0 - 500;
    assert!((2000..last).contains(&second_start), "{times:?}");
    assert_eq!(times.len(), 3);
    assert_eq!(subtitles.discarded_cues, 2);
    assert_eq!(merged.report.streams[2].discarded_cues, 2);
}

/// MP4 里几条音频各成一路：语言写 ISO 639-2 的术语代码；Matroska 的字幕写强制标志。
#[test]
fn labels_in_mp4_and_forced_subtitles_in_matroska() {
    let dir = work_dir("labels_mp4");
    let mp4 = dir.join("labels.mp4");
    let audio = |language: &str| Track {
        streams: Streams::Audio,
        label: Label {
            language: Some(language.into()),
            ..Label::default()
        },
    };
    let tracks = [
        Track {
            streams: Streams::Video,
            label: Label::default(),
        },
        audio("de"),
        audio("zh-Hant"),
    ];
    let groups = vec![group([track("ts_a"), track("ts_a"), track("ts_a")])];
    hs_m3u8_remux::remux(
        &tracks,
        &groups,
        &mp4,
        Container::Mp4,
        SubtitleMode::Embed,
        &Control::default(),
    )
    .unwrap();
    ffmpeg::init().unwrap();
    let ictx = ffmpeg::format::input(&mp4).unwrap();
    let languages: Vec<String> = ictx
        .streams()
        .filter(|s| s.parameters().medium() == ffmpeg::media::Type::Audio)
        .map(|s| s.metadata().get("language").unwrap_or_default().to_owned())
        .collect();
    assert_eq!(languages, ["deu", "zho"]);

    let mkv = dir.join("forced.mkv");
    let mut tracks = self::tracks();
    tracks[1].label.forced = true;
    let groups = vec![group([track("ts_a"), vtt_track(&dir)])];
    hs_m3u8_remux::remux(
        &tracks,
        &groups,
        &mkv,
        Container::Matroska,
        SubtitleMode::Embed,
        &Control::default(),
    )
    .unwrap();
    let ictx = ffmpeg::format::input(&mkv).unwrap();
    let subtitle = ictx
        .streams()
        .find(|s| s.parameters().medium() == ffmpeg::media::Type::Subtitle)
        .unwrap();
    assert!(
        subtitle
            .disposition()
            .contains(ffmpeg::format::stream::Disposition::FORCED)
    );
}
