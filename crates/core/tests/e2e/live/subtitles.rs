//! 直播录字幕：字幕轨与音视频一样按会话录、停止后一起合并，跨片重复的 cue 只留一条。

use hs_m3u8_core::hls::Renditions;
use hs_m3u8_core::remux::{Container, Shape};
use hs_m3u8_core::{
    Error, LiveEnd, MergedFile, SubtitleFileFormat, SubtitleOutput, Target, TrackKind,
};

use super::{STALL, live_request};
use crate::server::Server;
use crate::{engine, fixture, test_dir};

#[tokio::test(flavor = "multi_thread")]
async fn subtitles_are_recorded_with_the_other_tracks() {
    let dir = test_dir("live_subtitles");
    let server = Server::start().await;
    for (kind, count) in [("video", 2), ("audio", 3)] {
        server.put(
            &format!("{kind}/init.mp4"),
            fixture(&format!("fmp4_a/{kind}/init.mp4")),
        );
        let mut playlist =
            format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"{kind}/init.mp4\"\n");
        for i in 0..count {
            server.put(
                &format!("{kind}/seg{i}.m4s"),
                fixture(&format!("fmp4_a/{kind}/seg{i}.m4s")),
            );
            playlist += &format!("#EXTINF:1,\n{kind}/seg{i}.m4s\n");
        }
        server.put(&format!("{kind}.m3u8"), playlist);
    }
    let cue = "WEBVTT\n\n00:00.300 --> 00:01.200\n跨片\n";
    server.put("subs/0.vtt", cue);
    server.put("subs/1.vtt", cue);
    server.put(
        "subs.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nsubs/0.vtt\n#EXTINF:1,\nsubs/1.vtt\n",
    );
    server.put(
        "master.m3u8",
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
         #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"zh\",LANGUAGE=\"zh\",URI=\"subs.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=200000,AUDIO=\"aud\",SUBTITLES=\"subs\"\nvideo.m3u8\n",
    );
    let mut req = live_request(server.url("master.m3u8"), &dir, STALL);
    req.source.preference.subtitles = Renditions::All;
    req.output.target = Target::File(MergedFile {
        path: dir.join("out.mkv"),
        container: Container::Matroska,
    });

    let job = engine().start(req).unwrap();
    let mut progress = job.control().progress();
    progress
        .wait_for(|p| {
            p.tracks.len() == 3
                && p.tracks[2].kind == TrackKind::Subtitles
                && p.tracks.iter().map(|t| t.segments.done).eq([2, 3, 2])
        })
        .await
        .unwrap();
    job.control().stop();
    let output = job.wait().await.unwrap();

    assert_eq!(output.live.unwrap().end, Some(LiveEnd::Stopped));
    assert_eq!(output.missed, []);
    let streams = output.merged.unwrap().report.streams;
    assert_eq!(streams.len(), 3);
    assert_eq!(
        (streams[2].shape, streams[2].packets),
        (Shape::Subtitle { codec: "subrip" }, 1)
    );
}

/// 画面与音频出现 ENDLIST 即结束录制，字幕播放列表一直没有 ENDLIST 也不等它。
#[tokio::test(flavor = "multi_thread")]
async fn the_recording_ends_without_waiting_for_the_subtitle_playlist() {
    let dir = test_dir("live_subtitles_end");
    let server = Server::start().await;
    server.put("v/seg0.ts", fixture("ts_a/seg0.ts"));
    let video = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nv/seg0.ts\n";
    server.put("v.m3u8", video);
    server.put("subs/0.vtt", "WEBVTT\n\n00:00.300 --> 00:01.200\n字幕\n");
    server.put(
        "subs.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nsubs/0.vtt\n",
    );
    server.put(
        "master.m3u8",
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"zh\",LANGUAGE=\"zh\",URI=\"subs.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=200000,SUBTITLES=\"subs\"\nv.m3u8\n",
    );
    let mut req = live_request(server.url("master.m3u8"), &dir, STALL);
    req.source.preference.subtitles = Renditions::All;
    req.output.target = Target::File(MergedFile {
        path: dir.join("out.mkv"),
        container: Container::Matroska,
    });

    let job = engine().start(req).unwrap();
    let mut progress = job.control().progress();
    progress
        .wait_for(|p| p.tracks.iter().map(|t| t.segments.done).eq([1, 1]))
        .await
        .unwrap();
    // 画面结束，字幕播放列表照旧没有 ENDLIST
    server.put("v.m3u8", format!("{video}#EXT-X-ENDLIST\n"));
    let output = job.wait().await.unwrap();

    assert_eq!(output.live.unwrap().end, Some(LiveEnd::EndList));
}

/// 只合并记有字幕轨的录制：输出用不上字幕（不封、不另存、没有本地 HLS）时报参数错误，不把字幕悄悄丢掉；另存为 SRT
/// 时照常合并。
#[tokio::test(flavor = "multi_thread")]
async fn merging_a_recording_with_subtitles_requires_a_use_for_them() {
    let dir = test_dir("live_subtitles_merge_use");
    let server = Server::start().await;
    server.put("v/seg0.ts", fixture("ts_a/seg0.ts"));
    server.put(
        "v.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nv/seg0.ts\n",
    );
    server.put(
        "subs/0.vtt",
        "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:131280,LOCAL:00:00:00.000\n\n00:00.300 --> 00:00.800\n字幕\n",
    );
    server.put(
        "subs.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nsubs/0.vtt\n",
    );
    server.put(
        "master.m3u8",
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"zh\",LANGUAGE=\"zh\",URI=\"subs.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=200000,SUBTITLES=\"subs\"\nv.m3u8\n",
    );
    let mut req = live_request(server.url("master.m3u8"), &dir, STALL);
    req.source.preference.subtitles = Renditions::All;
    req.output.target = Target::File(MergedFile {
        path: dir.join("out.mkv"),
        container: Container::Matroska,
    });
    req.output.keep_work_dir = true;
    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.control().progress();
    progress
        .wait_for(|p| p.tracks.iter().map(|t| t.segments.done).eq([1, 1]))
        .await
        .unwrap();
    job.control().stop();
    job.wait().await.unwrap();
    server.stop().await;

    // 停止时已合并出 out.mkv，只合并写到另一个文件，任务目录照旧
    req.output.work_dir = Some(dir.join("out.hsdl"));
    req.output.target = Target::File(MergedFile {
        path: dir.join("again.mkv"),
        container: Container::Matroska,
    });
    let mut unused = req.output.clone();
    unused.subtitles = SubtitleOutput {
        embed: false,
        files: None,
    };
    let failed = engine().merge_recorded(unused).unwrap().wait().await;
    assert!(matches!(failed, Err(Error::InvalidInput(_))), "{failed:?}");

    let mut files = req.output;
    files.subtitles = SubtitleOutput {
        embed: false,
        files: Some(SubtitleFileFormat::Srt),
    };
    let output = engine()
        .merge_recorded(files)
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(output.subtitle_files.len(), 1);
    assert_eq!(output.subtitle_files[0].cues, 1);
}
