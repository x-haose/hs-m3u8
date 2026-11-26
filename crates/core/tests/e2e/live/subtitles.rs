//! 直播录字幕：字幕轨与音视频一样按会话录、停止后一起合并，跨片重复的 cue 只留一条。

use hs_m3u8_core::hls::Renditions;
use hs_m3u8_core::remux::{Container, Shape};
use hs_m3u8_core::{LiveEnd, MergedFile, Target, TrackKind};

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
