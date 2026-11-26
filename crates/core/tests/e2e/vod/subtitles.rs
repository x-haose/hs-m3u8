//! 多条音轨与字幕：探测时字幕能不能下，自动选到的下不了时跳过、点名的报错；合并为 MKV、另存字幕文件、本地 HLS 带
//! rendition。

use axum::http::StatusCode;
use hs_m3u8_core::hls::{RenditionChoice, Renditions};
use hs_m3u8_core::remux::{Container, Shape, StreamKind};
use hs_m3u8_core::{
    Error, MergedFile, Output, SubtitleFileFormat, SubtitleFormat, SubtitleProblem, Target,
    TrackKind, Unsupported,
};

use crate::server::Server;
use crate::{engine, fixture, request, run, test_dir};

/// fmp4_a 的视频与音频（音频另放一份在 `a2/`），三条字幕：`zh` 为 WebVTT 文本（两片，第二片重写了跨片的 cue），`en` 的
/// 分片是 fMP4，`ja` 的媒体播放列表取不到（404）。
fn put_tracks(server: &Server) {
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
    server.status("ja.m3u8", StatusCode::NOT_FOUND);
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
    put_tracks(&server);
    let mut req = request(server.url("master.m3u8"), &dir);
    req.source.preference.subtitles = Renditions::Default;

    let probe = engine().probe(&req.source).await.unwrap();

    let master = probe.master.unwrap();
    let support: Vec<_> = master.subtitles.iter().map(|s| s.support.clone()).collect();
    assert_eq!(support[0], Some(Ok(())));
    assert_eq!(
        support[1],
        Some(Err(SubtitleProblem::Format(SubtitleFormat::Fmp4 {
            codecs: vec!["wvtt".into()]
        })))
    );
    assert!(
        matches!(&support[2], Some(Err(SubtitleProblem::Fetch(failure))) if failure.kind == hs_m3u8_core::HttpErrorKind::Status(404)),
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

/// 点名选的字幕下不了时开始即报错：格式不支持报 SubtitleFormat，取不到报那次请求的错误。
#[tokio::test(flavor = "multi_thread")]
async fn named_subtitles_that_cannot_be_downloaded_are_errors() {
    let dir = test_dir("subtitles_named");
    let server = Server::start().await;
    put_tracks(&server);
    let named = |language: &str| {
        let mut req = request(server.url("master.m3u8"), &dir.join(language));
        req.source.preference.subtitles =
            Renditions::These(vec![RenditionChoice::Language(language.into())]);
        req
    };

    let fmp4 = run(named("en")).await.unwrap_err();
    assert!(
        matches!(fmp4, Error::Unsupported(Unsupported::SubtitleFormat { .. })),
        "{fmp4:?}"
    );
    let missing = run(named("ja")).await.unwrap_err();
    assert!(matches!(missing, Error::Http { .. }), "{missing:?}");
}

/// 选全部音频与字幕，输出 MKV 与本地 HLS、另存 SRT：下不了的两条字幕跳过，合并文件里两路音频、一路字幕（跨片重复的 cue
/// 只留一条），轨道写上语言；SRT 与封进去的对齐；本地 HLS 的主播放列表列出两条音频、一条字幕；进度按轨分开，字幕不计入
/// 合计。
#[tokio::test(flavor = "multi_thread")]
async fn all_tracks_are_merged_into_matroska_with_subtitle_files() {
    let dir = test_dir("subtitles_mkv");
    let server = Server::start().await;
    put_tracks(&server);
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
    assert_eq!(output.subtitle_files, [dir.join("out.zh-Hans.srt")]);
    let srt = std::fs::read_to_string(&output.subtitle_files[0]).unwrap();
    assert!(srt.contains("<i>你好</i>") && srt.contains("再见"), "{srt}");
    assert_eq!(srt.matches("你好").count(), 1, "{srt}");

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
            TrackKind::Subtitles
        ]
    );
    assert_eq!(progress.segments.done, 2 + 3 + 3);
    assert_eq!(progress.tracks[3].segments.done, 2);
}
