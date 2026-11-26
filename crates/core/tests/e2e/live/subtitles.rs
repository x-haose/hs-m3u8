//! 直播录字幕：字幕轨与音视频一样按会话录、停止后一起合并，跨片重复的 cue 只留一条。

use std::time::Duration;

use axum::http::StatusCode;
use hs_m3u8_core::hls::Renditions;
use hs_m3u8_core::remux::{Container, Shape};
use hs_m3u8_core::{
    Error, FetchErrorKind, HttpErrorKind, HttpFailure, JobRequest, LiveEnd, MergedFile, MissReason,
    Missed, NewSession, Progress, RefreshReason, SourceBreak, StallCause, StallReason,
    SubtitleFileFormat, SubtitleOutput, TailCause, Target, TrackKind, UnrecordedTail,
};

use super::{STALL, interrupt, live_request, playlist_in, put_long};
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

/// 画面还在更新时字幕播放列表一直不变、也没有 ENDLIST：字幕轨不算停滞，录制等画面停滞才结束，结束原因落在画面上。
#[tokio::test(flavor = "multi_thread")]
async fn a_subtitle_playlist_that_stops_updating_does_not_stall_the_recording() {
    let dir = test_dir("live_subtitles_stall");
    let server = Server::start().await;
    let video = |count: usize| {
        let mut playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n".to_owned();
        for i in 0..count {
            playlist += &format!("#EXTINF:1,\nv/seg{i}.ts\n");
        }
        playlist
    };
    for i in 0..2 {
        server.put(&format!("v/seg{i}.ts"), fixture(&format!("ts_a/seg{i}.ts")));
    }
    server.put("v.m3u8", video(1));
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
    let job = engine().start(req).unwrap();
    let mut progress = job.control().progress();
    progress
        .wait_for(|p| p.tracks.iter().map(|t| t.segments.done).eq([1, 1]))
        .await
        .unwrap();
    // 两条轨都录到第一片之后画面再出一片，然后不再更新；字幕播放列表从头到尾不变，比画面早满停滞时限
    server.put("v.m3u8", video(2));
    let output = job.wait().await.unwrap();

    let live = output.live.unwrap();
    assert!(
        matches!(live.end, Some(LiveEnd::Stalled { track: 0, .. })),
        "{:?}",
        live.end
    );
    assert_eq!(output.segments, 2);
    // 字幕在第 0 片之后没再录到：它不再出新分片，画面也结束了
    assert_eq!(
        live.unrecorded_tails,
        [UnrecordedTail {
            session: 0,
            track: 1,
            after: Some(0),
            at: Some(Duration::from_secs(1)),
            cause: Some(TailCause::Ended(StallCause::NoNewSegments)),
        }]
    );
}

/// 字幕播放列表一直 403：录制照常由画面结束，字幕轨记为尾部没录到、带上原因；之后只合并时仍报出这一项，原因不落盘。
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_subtitle_playlist_is_reported_as_an_unrecorded_tail() {
    let dir = test_dir("live_subtitles_forbidden");
    let server = Server::start().await;
    let video = |count: usize| {
        let mut playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n".to_owned();
        for i in 0..count {
            playlist += &format!("#EXTINF:1,\nv/seg{i}.ts\n");
        }
        playlist
    };
    for i in 0..2 {
        server.put(&format!("v/seg{i}.ts"), fixture(&format!("ts_a/seg{i}.ts")));
    }
    server.put("v.m3u8", video(1));
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
    server.status("subs.m3u8", StatusCode::FORBIDDEN);
    server.put("v.m3u8", video(2));
    let output = job.wait().await.unwrap();

    let live = output.live.unwrap();
    assert!(
        matches!(live.end, Some(LiveEnd::Stalled { track: 0, .. })),
        "{:?}",
        live.end
    );
    let [tail] = &live.unrecorded_tails[..] else {
        panic!("{:?}", live.unrecorded_tails);
    };
    assert_eq!(
        (tail.session, tail.track, tail.after, tail.at),
        (0, 1, Some(0), Some(Duration::from_secs(1)))
    );
    assert!(
        matches!(
            &tail.cause,
            Some(TailCause::Failed(StallReason::RefreshFailed(
                RefreshReason::Http(HttpFailure {
                    kind: HttpErrorKind::Status(403),
                    ..
                })
            )))
        ),
        "{:?}",
        tail.cause
    );
    server.stop().await;

    req.output.target = Target::File(MergedFile {
        path: dir.join("again.mkv"),
        container: Container::Matroska,
    });
    req.output.work_dir = Some(dir.join("out.hsdl"));
    let merged = engine()
        .merge_recorded(req.output)
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(
        merged.live.unwrap().unrecorded_tails,
        [UnrecordedTail {
            session: 0,
            track: 1,
            after: Some(0),
            at: Some(Duration::from_secs(1)),
            cause: None,
        }]
    );
}

/// 续录时字幕播放列表还没有分片：会话判定不等它（字幕轨不参与判定），画面照常续录；字幕之后出了分片照样接着录进这个
/// 会话。
#[tokio::test(flavor = "multi_thread")]
async fn resuming_without_subtitle_segments_does_not_wait_for_them() {
    let dir = test_dir("live_subtitles_resume_empty");
    let server = Server::start().await;
    let video = |count: usize, ended: bool| {
        let mut playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n".to_owned();
        for i in 0..count {
            playlist += &format!("#EXTINF:1,\nv/seg{i}.ts\n");
        }
        if ended {
            playlist += "#EXT-X-ENDLIST\n";
        }
        playlist
    };
    for i in 0..2 {
        server.put(&format!("v/seg{i}.ts"), fixture(&format!("ts_a/seg{i}.ts")));
    }
    server.put("v.m3u8", video(1, false));
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
    let mut req = live_request(server.url("master.m3u8"), &dir, LONG_STALL);
    req.source.preference.subtitles = Renditions::All;
    req.output.target = Target::File(MergedFile {
        path: dir.join("out.mkv"),
        container: Container::Matroska,
    });
    interrupt(&req, |p| {
        p.tracks.iter().map(|t| t.segments.done).eq([1, 1])
    })
    .await;

    server.put("subs.m3u8", "#EXTM3U\n#EXT-X-TARGETDURATION:1\n");
    server.put("v.m3u8", video(2, false));
    let job = engine().start(req).unwrap();
    let mut progress = job.control().progress();
    // 会话定下、画面接着录到第 1 片：判定没有等字幕（等它要等满远长于此的停滞时限）
    let decided = progress.wait_for(|p| p.tracks.first().is_some_and(|t| t.segments.done == 2));
    tokio::time::timeout(LONG_STALL / 3, decided)
        .await
        .expect("会话判定不等字幕轨")
        .unwrap();
    server.put(
        "subs/1.vtt",
        "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:221280,LOCAL:00:00:00.000\n\n00:00.300 --> 00:00.800\n又有字幕\n",
    );
    server.put(
        "subs.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:1,\nsubs/1.vtt\n",
    );
    let joined = progress.wait_for(|p| p.tracks.get(1).is_some_and(|t| t.segments.done == 2));
    tokio::time::timeout(LONG_STALL / 3, joined)
        .await
        .expect("字幕出了分片照样并入")
        .unwrap();
    server.put("v.m3u8", video(2, true));
    let output = job.wait().await.unwrap();

    let live = output.live.unwrap();
    assert_eq!(live.end, Some(LiveEnd::EndList));
    assert_eq!(live.session_count, 1);
    assert!(
        live.unrecorded_tails.is_empty(),
        "{:?}",
        live.unrecorded_tails
    );
}

/// 远长于测试本身的停滞时限：测试里不会有轨停滞。
const LONG_STALL: Duration = Duration::from_secs(30);

/// 一个画面加一条字幕的直播源：画面播放列表 v.m3u8 先列 ts_a 的第 0 片，字幕播放列表 subs.m3u8 列一片与之对齐的字幕；
/// 都没有 ENDLIST。返回输出为 MKV、停滞时限为 `stall_timeout` 的录制请求。
fn subtitled_live(server: &Server, dir: &std::path::Path, stall_timeout: Duration) -> JobRequest {
    for i in 0..2 {
        server.put(&format!("v/seg{i}.ts"), fixture(&format!("ts_a/seg{i}.ts")));
    }
    server.put("v.m3u8", ts_video(1, false));
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
    let mut req = live_request(server.url("master.m3u8"), dir, stall_timeout);
    req.source.preference.subtitles = Renditions::All;
    req.output.target = Target::File(MergedFile {
        path: dir.join("out.mkv"),
        container: Container::Matroska,
    });
    req
}

/// 画面播放列表：列 ts_a 的前 `count` 片，`ended` 时带 ENDLIST。
fn ts_video(count: usize, ended: bool) -> String {
    let mut playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n".to_owned();
    for i in 0..count {
        playlist += &format!("#EXTINF:1,\nv/seg{i}.ts\n");
    }
    if ended {
        playlist += "#EXT-X-ENDLIST\n";
    }
    playlist
}

/// 等两条轨都录到 `done` 片。
async fn both_recorded(progress: &mut tokio::sync::watch::Receiver<Progress>, done: usize) {
    progress
        .wait_for(|p| p.tracks.iter().map(|t| t.segments.done).eq([done, done]))
        .await
        .unwrap();
}

/// 字幕轨记为尾部没录到时即写进任务目录：之后任务被取消，只合并照样报出这一项（原因不落盘）。
#[tokio::test(flavor = "multi_thread")]
async fn a_tail_noticed_before_cancelling_survives_into_merging() {
    let dir = test_dir("live_subtitles_tail_cancel");
    let server = Server::start().await;
    let mut req = subtitled_live(&server, &dir, STALL);
    req.output.keep_work_dir = true;
    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.control().progress();
    both_recorded(&mut progress, 1).await;
    server.status("subs.m3u8", StatusCode::FORBIDDEN);
    // 画面再出一片，比字幕晚满停滞时限
    server.put("v.m3u8", ts_video(2, false));
    progress
        .wait_for(|p| p.tracks.get(1).is_some_and(|t| t.unrecorded_tail))
        .await
        .unwrap();
    job.control().cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    server.stop().await;

    let merged = engine()
        .merge_recorded(req.output)
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(
        merged.live.unwrap().unrecorded_tails,
        [UnrecordedTail {
            session: 0,
            track: 1,
            after: Some(0),
            at: Some(Duration::from_secs(1)),
            cause: None,
        }]
    );
}

/// 字幕分片 403 只记为这一片缺失，录制照常；字幕播放列表的序号处矛盾（同一序号换了分片）只停字幕轨，尾部原因为接不上。
#[tokio::test(flavor = "multi_thread")]
async fn subtitle_failures_only_affect_the_subtitle_track() {
    let dir = test_dir("live_subtitles_failures");
    let server = Server::start().await;
    // 停滞时限远长于这条测试：尾部只会由接不上记下
    let req = subtitled_live(&server, &dir, LONG_STALL);
    let job = engine().start(req).unwrap();
    let mut progress = job.control().progress();
    both_recorded(&mut progress, 1).await;
    server.status("subs/1.vtt", StatusCode::FORBIDDEN);
    server.put(
        "subs.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nsubs/0.vtt\n#EXTINF:1,\nsubs/1.vtt\n",
    );
    progress
        .wait_for(|p| p.tracks.get(1).is_some_and(|t| t.segments.failed == 1))
        .await
        .unwrap();
    // 第 0 片换成了别的分片：与之前刷新得到的矛盾
    server.put(
        "subs.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nsubs/other.vtt\n#EXTINF:1,\nsubs/1.vtt\n",
    );
    progress
        .wait_for(|p| p.tracks.get(1).is_some_and(|t| t.unrecorded_tail))
        .await
        .unwrap();
    server.put("v.m3u8", ts_video(2, true));
    let output = job.wait().await.unwrap();

    let live = output.live.unwrap();
    assert_eq!(live.end, Some(LiveEnd::EndList));
    assert_eq!(
        output.missed,
        [Missed {
            session: 0,
            track: 1,
            first: 1,
            last: 1,
            reason: MissReason::Failed(FetchErrorKind::Http(HttpErrorKind::Status(403))),
        }]
    );
    let [tail] = &live.unrecorded_tails[..] else {
        panic!("{:?}", live.unrecorded_tails);
    };
    assert_eq!((tail.track, tail.after), (1, Some(0)));
    assert!(
        matches!(
            tail.cause,
            Some(TailCause::Broken(SourceBreak::Inconsistent { sequence: 0 }))
        ),
        "{:?}",
        tail.cause
    );
}

/// 一条视频加一条字幕时视频编码器重启（序号回退）：字幕轨不算进轨数，同不选字幕时一样另起会话接着录，字幕轨跟着进新
/// 会话。
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_of_the_only_media_track_starts_a_new_session_with_subtitles() {
    let dir = test_dir("live_subtitles_restart");
    let server = Server::start().await;
    put_long(&server, "v/", &[0, 1, 2, 3]);
    server.put_sequence(
        "v.m3u8",
        vec![
            playlist_in("v/", &[2, 3], false),
            playlist_in("v/", &[0], false),
            playlist_in("v/", &[0], false),
            playlist_in("v/", &[0, 1], true),
        ],
    );
    server.put("subs/0.vtt", "WEBVTT\n\n00:00.300 --> 00:00.800\n字幕\n");
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
    let mut req = live_request(server.url("master.m3u8"), &dir, LONG_STALL);
    req.source.preference.subtitles = Renditions::All;
    req.output.target = Target::File(MergedFile {
        path: dir.join("out.mkv"),
        container: Container::Matroska,
    });

    let output = tokio::time::timeout(LONG_STALL / 3, engine().start(req).unwrap().wait())
        .await
        .expect("录制不等字幕")
        .unwrap();

    let live = output.live.unwrap();
    assert_eq!(live.end, Some(LiveEnd::EndList));
    assert_eq!(
        live.new_sessions,
        [NewSession {
            session: 1,
            cause: SourceBreak::Restarted,
        }]
    );
    assert_eq!(live.session_count, 2);
    // 字幕轨接着原窗口录，录过的不重录
    assert_eq!(server.hits("subs/0.vtt"), 1);
}

/// 续录时画面播放列表已结束而没有分片（这次没有要录的音视频轨）、字幕播放列表仍有分片：字幕这次也不录，直接合并之前
/// 录到的。
#[tokio::test(flavor = "multi_thread")]
async fn subtitles_are_not_recorded_when_no_media_track_is() {
    let dir = test_dir("live_subtitles_no_media");
    let server = Server::start().await;
    let req = subtitled_live(&server, &dir, LONG_STALL);
    interrupt(&req, |p| {
        p.tracks.iter().map(|t| t.segments.done).eq([1, 1])
    })
    .await;

    server.put(
        "v.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-ENDLIST\n",
    );
    server.put(
        "subs.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nsubs/0.vtt\n#EXTINF:1,\nsubs/1.vtt\n",
    );
    let output = tokio::time::timeout(LONG_STALL / 3, engine().start(req).unwrap().wait())
        .await
        .expect("没有要录的轨时直接合并")
        .unwrap();

    assert_eq!(output.segments, 1);
    assert_eq!(server.hits("subs/1.vtt"), 0);
    assert!(output.live.unwrap().unrecorded_tails.is_empty());
}
