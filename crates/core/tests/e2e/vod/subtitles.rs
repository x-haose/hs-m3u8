//! 多条音轨与字幕：探测时字幕能不能下，自动选到的下不了时跳过、点名的报错；合并为 MKV、另存字幕文件、本地 HLS 带
//! rendition。

use axum::http::StatusCode;
use hs_m3u8_core::hls::{RenditionChoice, Renditions};
use hs_m3u8_core::remux::{Container, Shape, StreamKind};
use hs_m3u8_core::{
    Error, FetchFailure, HttpErrorKind, MergedFile, Output, SubtitleFileFormat, SubtitleFormat,
    SubtitleProblem, SubtitleSupport, Target, TrackKind, Unsupported,
};

use crate::server::Server;
use crate::{engine, fixture, request, run, test_dir};

/// fmp4_a 的视频与音频（音频另放一份在 `a2/`），三条字幕：`zh` 为 WebVTT 文本（两片，第二片重写了跨片的 cue），`en` 的
/// 分片是 fMP4，`ja` 的媒体播放列表 `ja_missing` 时取不到（404），否则与 `zh` 用同样的分片。
fn put_tracks(server: &Server, ja_missing: bool) {
    for (kind, dir, count) in [("video", "v", 2), ("audio", "a1", 3), ("audio", "a2", 3)] {
        server.put(
            &format!("{dir}/init.mp4"),
            fixture(&format!("fmp4_a/{kind}/init.mp4")),
        );
        let mut playlist =
            format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"{dir}/init.mp4\"\n");
        for i in 0..count {
            server.put(
                &format!("{dir}/seg{i}.m4s"),
                fixture(&format!("fmp4_a/{kind}/seg{i}.m4s")),
            );
            playlist += &format!("#EXTINF:1,\n{dir}/seg{i}.m4s\n");
        }
        server.put(&format!("{dir}.m3u8"), playlist + "#EXT-X-ENDLIST\n");
    }
    let header = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n";
    server.put(
        "zh/0.vtt",
        format!("{header}00:00.200 --> 00:01.400\n<i>你好</i>\n"),
    );
    server.put(
        "zh/1.vtt",
        format!("{header}00:00.200 --> 00:01.400\n<i>你好</i>\n\n00:01.500 --> 00:01.900\n再见\n"),
    );
    server.put(
        "zh.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nzh/0.vtt\n#EXTINF:1,\nzh/1.vtt\n#EXT-X-ENDLIST\n",
    );
    server.put(
        "en.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"en/init.mp4\"\n#EXTINF:1,\nen/0.m4s\n\
         #EXTINF:1,\nen/1.m4s\n#EXT-X-ENDLIST\n",
    );
    if ja_missing {
        server.status("ja.m3u8", StatusCode::NOT_FOUND);
    } else {
        server.put(
            "ja.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nzh/0.vtt\n#EXTINF:1,\nzh/1.vtt\n#EXT-X-ENDLIST\n",
        );
    }
    server.put(
        "master.m3u8",
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"a1.m3u8\"\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Deutsch\",LANGUAGE=\"de\",URI=\"a2.m3u8\"\n\
         #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"简体\",LANGUAGE=\"zh-Hans\",URI=\"zh.m3u8\"\n\
         #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"en.m3u8\"\n\
         #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"日本語\",LANGUAGE=\"ja\",URI=\"ja.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=200000,RESOLUTION=320x180,CODECS=\"avc1.64000c,mp4a.40.2,wvtt\",AUDIO=\"aud\",SUBTITLES=\"subs\"\n\
         v.m3u8\n",
    );
}

/// 探测读字幕组里每一条的媒体播放列表：fMP4 的标为格式不支持，取不到的如实给出，都不让探测失败；默认选到的 fMP4 字幕
/// 下不了，列为跳过。
#[tokio::test(flavor = "multi_thread")]
async fn probe_reports_which_subtitles_can_be_downloaded() {
    let dir = test_dir("subtitles_probe");
    let server = Server::start().await;
    put_tracks(&server, true);
    let mut req = request(server.url("master.m3u8"), &dir);
    req.source.preference.subtitles = Renditions::Default;

    let probe = engine().probe(&req.source).await.unwrap();

    let master = probe.master.unwrap();
    let support: Vec<_> = master.subtitles.iter().map(|s| s.support.clone()).collect();
    assert_eq!(support[0], SubtitleSupport::Supported);
    assert_eq!(
        support[1],
        SubtitleSupport::Unsupported(SubtitleFormat::Fmp4 {
            codecs: vec!["wvtt".into()]
        })
    );
    assert!(
        matches!(
            &support[2],
            SubtitleSupport::Unreadable(SubtitleProblem::Fetch(FetchFailure::Http(failure)))
                if failure.kind == HttpErrorKind::Status(404)
        ),
        "{:?}",
        support[2]
    );
    let selected = &master.selected;
    assert!(selected.subtitles.is_empty());
    let skipped: Vec<_> = selected
        .skipped_subtitles
        .iter()
        .map(|s| s.language.as_deref())
        .collect();
    assert_eq!(skipped, [Some("en")]);
    assert_eq!(probe.tracks.len(), 2);
}

/// 点名选的字幕下不了时开始即报错：格式不支持报 SubtitleFormat，取不到报那次请求的错误；自动选到的取不到同样报那次请求的
/// 错误（外部故障如实上抛，不当成下不了去掉）。
#[tokio::test(flavor = "multi_thread")]
async fn subtitles_that_cannot_be_fetched_or_used_are_errors() {
    let dir = test_dir("subtitles_named");
    let server = Server::start().await;
    put_tracks(&server, true);
    let with = |name: &str, subtitles: Renditions| {
        let mut req = request(server.url("master.m3u8"), &dir.join(name));
        req.source.preference.subtitles = subtitles;
        req
    };
    let named =
        |language: &str| Renditions::These(vec![RenditionChoice::Language(language.into())]);

    let fmp4 = run(with("en", named("en"))).await.unwrap_err();
    assert!(
        matches!(fmp4, Error::Unsupported(Unsupported::SubtitleFormat { .. })),
        "{fmp4:?}"
    );
    let missing = run(with("ja", named("ja"))).await.unwrap_err();
    assert!(matches!(missing, Error::Http { .. }), "{missing:?}");
    let all = run(with("all", Renditions::All)).await.unwrap_err();
    assert!(matches!(all, Error::Http { .. }), "{all:?}");
}

/// SRT 里 `<主名>.<语言>.srt` 的各条 cue 的起点，毫秒。
fn srt_starts(path: &std::path::Path) -> Vec<u64> {
    let srt = std::fs::read_to_string(path).unwrap();
    srt.lines()
        .filter_map(|line| line.split_once(" --> "))
        .map(|(start, _)| {
            let (hms, ms) = start.split_once(',').unwrap();
            let parts: Vec<u64> = hms.split(':').map(|p| p.parse().unwrap()).collect();
            ((parts[0] * 60 + parts[1]) * 60 + parts[2]) * 1000 + ms.parse::<u64>().unwrap()
        })
        .collect()
}

/// 选全部音频与字幕，输出 MKV 与本地 HLS、另存 SRT：fMP4 的那条字幕跳过，合并文件里两路音频、两路字幕（跨片重复的 cue
/// 只留一条）；SRT 里各 cue 的间隔与原文相同、起点按画面的起点平移；本地 HLS 的主播放列表列出两条音频、两条字幕；进度
/// 按轨分开，字幕不计入合计。
#[tokio::test(flavor = "multi_thread")]
async fn all_tracks_are_merged_into_matroska_with_subtitle_files() {
    let dir = test_dir("subtitles_mkv");
    let server = Server::start().await;
    put_tracks(&server, false);
    let mut req = request(server.url("master.m3u8"), &dir);
    req.source.preference.audio = Renditions::All;
    req.source.preference.subtitles = Renditions::All;
    req.output.target = Target::Both {
        file: MergedFile {
            path: dir.join("out.mkv"),
            container: Container::Matroska,
        },
        hls: dir.join("out"),
    };
    req.output.subtitles.files = Some(SubtitleFileFormat::Srt);

    let job = engine().start(req).unwrap();
    let progress = job.control().progress();
    let output: Output = job.wait().await.unwrap();

    let merged = output.merged.unwrap();
    assert_eq!(merged.container, Container::Matroska);
    let kinds: Vec<StreamKind> = merged
        .report
        .streams
        .iter()
        .map(|s| s.shape.kind())
        .collect();
    assert_eq!(
        kinds,
        [
            StreamKind::Video,
            StreamKind::Audio,
            StreamKind::Audio,
            StreamKind::Subtitle,
            StreamKind::Subtitle
        ]
    );
    assert_eq!(
        (
            merged.report.streams[3].shape,
            merged.report.streams[3].packets
        ),
        (Shape::Subtitle { codec: "subrip" }, 2)
    );
    let files: Vec<_> = output
        .subtitle_files
        .iter()
        .map(|f| f.path.clone())
        .collect();
    assert_eq!(files, [dir.join("out.zh-Hans.srt"), dir.join("out.ja.srt")]);
    assert_eq!(
        (
            output.subtitle_files[0].cues,
            output.subtitle_files[0].discarded_cues
        ),
        (2, 0)
    );
    let srt = std::fs::read_to_string(&files[0]).unwrap();
    assert!(srt.contains("<i>你好</i>") && srt.contains("再见"), "{srt}");
    assert_eq!(srt.matches("你好").count(), 1, "{srt}");
    // 原文的 cue 起于 200 毫秒与 1500 毫秒；画面从媒体时间线上 100 毫秒以内开始，输出从它开始
    let starts = srt_starts(&files[0]);
    assert_eq!(starts[1] - starts[0], 1300, "{starts:?}");
    assert!((100..=200).contains(&starts[0]), "{starts:?}");

    let master = std::fs::read_to_string(dir.join("out/index.m3u8")).unwrap();
    assert_eq!(master.matches("TYPE=AUDIO").count(), 2, "{master}");
    assert!(
        master.contains("TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"简体\",LANGUAGE=\"zh-Hans\""),
        "{master}"
    );
    assert!(dir.join("out/3/0.vtt").is_file());

    let progress = progress.borrow();
    let kinds: Vec<TrackKind> = progress.tracks.iter().map(|t| t.kind).collect();
    assert_eq!(
        kinds,
        [
            TrackKind::Video,
            TrackKind::Audio,
            TrackKind::Audio,
            TrackKind::Subtitles,
            TrackKind::Subtitles
        ]
    );
    assert_eq!(progress.segments.done, 2 + 3 + 3);
    assert_eq!(progress.tracks[3].segments.done, 2);
}

/// 字幕播放列表的不连续段与画面的不同、或字幕播放列表为空，都不影响下载：字幕按不连续段序号落进同号的组，没有的组就
/// 没有字幕；另存 WebVTT 照样写出。变体里混着的音频加字幕时，本地 HLS 的主播放列表不写 AUDIO，字幕为第 1 条轨。
#[tokio::test(flavor = "multi_thread")]
async fn subtitle_playlists_do_not_decide_the_groups() {
    let dir = test_dir("subtitles_groups");
    let server = Server::start().await;
    for i in 0..2 {
        server.put(&format!("v/seg{i}.ts"), fixture(&format!("ts_a/seg{i}.ts")));
    }
    server.put(
        "v.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:1,\nv/seg0.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:1,\nv/seg1.ts\n\
         #EXT-X-ENDLIST\n",
    );
    server.put(
        "s/0.vtt",
        // ts_a 最早的 PTS 为 131280（90 kHz）
        "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:131280,LOCAL:00:00:00.000\n\n00:00.100 --> 00:00.500\n只有第 0 组有字幕\n",
    );
    server.put(
        "subs.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\ns/0.vtt\n#EXT-X-ENDLIST\n",
    );
    server.put(
        "empty.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-ENDLIST\n",
    );
    server.put(
        "master.m3u8",
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"s\",NAME=\"一\",LANGUAGE=\"zh\",URI=\"subs.m3u8\"\n\
         #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"s\",NAME=\"空\",LANGUAGE=\"en\",URI=\"empty.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=200000,SUBTITLES=\"s\"\nv.m3u8\n",
    );
    let mut req = request(server.url("master.m3u8"), &dir);
    req.source.preference.subtitles = Renditions::All;
    req.output.target = Target::Both {
        file: MergedFile {
            path: dir.join("out.mp4"),
            container: Container::Mp4,
        },
        hls: dir.join("out"),
    };
    req.output.subtitles.files = Some(SubtitleFileFormat::WebVtt);

    let output = run(req).await.unwrap();

    assert_eq!(output.missed, []);
    let counts: Vec<usize> = output.subtitle_files.iter().map(|f| f.cues).collect();
    assert_eq!(counts, [1, 0]);
    let vtt = std::fs::read_to_string(&output.subtitle_files[0].path).unwrap();
    assert!(
        vtt.starts_with("WEBVTT\n\n") && vtt.contains("只有第 0 组有字幕"),
        "{vtt}"
    );
    let master = std::fs::read_to_string(dir.join("out/index.m3u8")).unwrap();
    assert!(!master.contains("AUDIO="), "{master}");
    assert!(master.contains("URI=\"1/index.m3u8\""), "{master}");
    // 第 1 组没有字幕分片：本地 HLS 里补一个空的，各 rendition 的不连续段对得上
    let subs = std::fs::read_to_string(dir.join("out/1/index.m3u8")).unwrap();
    assert_eq!(subs.matches("#EXT-X-DISCONTINUITY").count(), 1, "{subs}");
    assert_eq!(subs.matches(".vtt").count(), 2, "{subs}");
}
