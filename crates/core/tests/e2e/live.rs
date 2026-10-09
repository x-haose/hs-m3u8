//! 直播录制：播放列表按请求次数逐步变化，模拟刷新、窗口滑动、结束与中断。
//! 样本 ts_long 为 4 个时间戳连续的 1 秒分片；TARGETDURATION 设为 0.1 秒，刷新间隔随之很短。

use std::time::Duration;

use hs_m3u8_core::{
    Error, JobRequest, LiveEnd, LiveOptions, LiveReport, MissReason, Missed, Stage, Url,
};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use crate::server::Server;
use crate::{assert_output, engine, expected, fixture, request, run, test_dir, track};

/// ts_long 中的分片 `indices` 直接合并（漏掉的分片在时间线上留空）的期望输出。
fn expected_long(dir: &std::path::Path, indices: &[u64]) -> Vec<u8> {
    let names: Vec<String> = indices.iter().map(|i| format!("seg{i}.ts")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let tracks = vec![track("ts_long", None, &names)];
    expected(dir, &[Streams::All], &[DiscontinuityGroup { tracks }])
}

/// 序号即 ts_long 分片编号的直播播放列表；`indices` 须连续。
fn playlist(indices: &[u64], end: bool) -> String {
    let mut text = format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MEDIA-SEQUENCE:{}\n",
        indices.first().copied().unwrap_or(0)
    );
    for i in indices {
        text += &format!("#EXTINF:1,\nseg{i}.ts\n");
    }
    if end {
        text += "#EXT-X-ENDLIST\n";
    }
    text
}

fn put_long(server: &Server, indices: &[u64]) {
    for i in indices {
        server.put(
            &format!("seg{i}.ts"),
            fixture(&format!("ts_long/seg{i}.ts")),
        );
    }
}

fn live_request(url: Url, dir: &std::path::Path) -> JobRequest {
    let mut req = request(url, dir);
    req.live = Some(LiveOptions {
        max_duration: None,
        stall_timeout: Duration::from_secs(2),
    });
    req
}

fn report(end: LiveEnd, missed: Vec<Missed>) -> Option<LiveReport> {
    Some(LiveReport { end, missed })
}

/// 刷新一次多一个分片、窗口前移，直到出现 EXT-X-ENDLIST。
#[tokio::test(flavor = "multi_thread")]
async fn records_until_endlist() {
    let dir = test_dir("live_endlist");
    let server = Server::start().await;
    put_long(&server, &[0, 1, 2, 3]);
    server.put_sequence(
        "live.m3u8",
        vec![
            playlist(&[0, 1], false),
            playlist(&[0, 1, 2], false),
            playlist(&[1, 2, 3], false),
            playlist(&[1, 2, 3], true),
        ],
    );

    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir))
        .unwrap();
    let progress = job.progress();
    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3]));
    assert_eq!(output.live, report(LiveEnd::EndList, vec![]));
    let last = *progress.borrow();
    assert_eq!(
        (last.stage, last.segments_done, last.segments_total),
        (Stage::Done, 4, 4)
    );
}

/// 两次刷新之间窗口滑过了分片 1：记为漏段，其余照常合并，时间线在该处留空。
#[tokio::test(flavor = "multi_thread")]
async fn window_slide_is_reported_as_missed() {
    let dir = test_dir("live_slide");
    let server = Server::start().await;
    put_long(&server, &[0, 1, 2, 3]);
    server.put_sequence(
        "live.m3u8",
        vec![playlist(&[0], false), playlist(&[2, 3], true)],
    );

    let output = run(live_request(server.url("live.m3u8"), &dir))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 2, 3]));
    let missed = vec![Missed {
        track: 0,
        first: 1,
        last: 1,
        reason: MissReason::Expired,
    }];
    assert_eq!(output.live, report(LiveEnd::EndList, missed));
}

/// 列出的分片 404：记为漏段，录制继续。
#[tokio::test(flavor = "multi_thread")]
async fn unavailable_segment_is_reported_as_missed() {
    let dir = test_dir("live_404");
    let server = Server::start().await;
    put_long(&server, &[0, 2, 3]);
    server.put_sequence(
        "live.m3u8",
        vec![
            playlist(&[0, 1, 2, 3], false),
            playlist(&[0, 1, 2, 3], true),
        ],
    );

    let output = run(live_request(server.url("live.m3u8"), &dir))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 2, 3]));
    let live = output.live.unwrap();
    assert_eq!(live.end, LiveEnd::EndList);
    assert!(
        matches!(
            live.missed.as_slice(),
            [Missed { track: 0, first: 1, last: 1, reason: MissReason::Failed(e) }] if e.contains("404")
        ),
        "{:?}",
        live.missed
    );
}

/// 调用 stop：停止刷新，合并已录到的部分。
#[tokio::test(flavor = "multi_thread")]
async fn stop_merges_what_was_recorded() {
    let dir = test_dir("live_stop");
    let server = Server::start().await;
    put_long(&server, &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));

    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir))
        .unwrap();
    let mut progress = job.progress();
    progress.wait_for(|p| p.segments_done == 2).await.unwrap();
    job.stop();
    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1]));
    assert_eq!(output.live, report(LiveEnd::Stopped, vec![]));
}

/// 播放列表不再出现新分片：超过 stall_timeout 即结束；期间刷新失败时附上最后的错误。
#[tokio::test(flavor = "multi_thread")]
async fn stalled_stream_ends_recording() {
    let dir = test_dir("live_stall");
    let server = Server::start().await;
    put_long(&server, &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));

    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir))
        .unwrap();
    let mut progress = job.progress();
    progress.wait_for(|p| p.segments_total == 2).await.unwrap();
    server.remove("live.m3u8");
    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1]));
    match output.live.unwrap().end {
        LiveEnd::Stalled {
            last_error: Some(e),
        } => assert!(e.contains("404"), "{e}"),
        other => panic!("{other:?}"),
    }
}

/// 录制被取消后，用同样的请求再次运行：不联网，直接合并已录到的分片。
#[tokio::test(flavor = "multi_thread")]
async fn interrupted_recording_is_merged_on_rerun() {
    let dir = test_dir("live_interrupted");
    let server = Server::start().await;
    put_long(&server, &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let req = live_request(server.url("live.m3u8"), &dir);

    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.progress();
    progress.wait_for(|p| p.segments_done == 2).await.unwrap();
    job.cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    assert!(!req.output.exists());

    let hits = server.hits("live.m3u8");
    let output = run(req).await.unwrap();
    assert_eq!(server.hits("live.m3u8"), hits);
    assert_output(&output, &expected_long(&dir, &[0, 1]));
    assert_eq!(output.live, report(LiveEnd::Interrupted, vec![]));
}

/// 录到的时长达到 max_duration 即停止。
#[tokio::test(flavor = "multi_thread")]
async fn max_duration_limits_recording() {
    let dir = test_dir("live_max");
    let server = Server::start().await;
    put_long(&server, &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1, 2, 3], false));
    let mut req = live_request(server.url("live.m3u8"), &dir);
    req.live = Some(LiveOptions {
        max_duration: Some(Duration::from_secs(2)),
        stall_timeout: Duration::from_secs(2),
    });

    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1]));
    assert_eq!(output.live, report(LiveEnd::MaxDuration, vec![]));
    assert_eq!(server.hits("seg2.ts"), 0);
}

/// 视频与独立音频 rendition 各自刷新；init 段地址每次刷新都带不同的签名，按内容判定为同一个。
#[tokio::test(flavor = "multi_thread")]
async fn split_tracks_with_signed_init_urls() {
    let dir = test_dir("live_split");
    let server = Server::start().await;
    let video = ["seg0.m4s", "seg1.m4s"];
    let audio = ["seg0.m4s", "seg1.m4s", "seg2.m4s"];
    for (kind, names) in [("video", &video[..]), ("audio", &audio[..])] {
        for name in ["init.mp4"].iter().chain(names) {
            server.put(
                &format!("{kind}/{name}"),
                fixture(&format!("fmp4_a/{kind}/{name}")),
            );
        }
    }
    let media = |kind: &str, count: usize, refresh: usize, end: bool| {
        let mut text = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MAP:URI=\"{kind}/init.mp4?sig={refresh}\"\n"
        );
        for i in 0..count {
            text += &format!("#EXTINF:1,\n{kind}/seg{i}.m4s\n");
        }
        if end {
            text += "#EXT-X-ENDLIST\n";
        }
        text
    };
    server.put_sequence(
        "video.m3u8",
        vec![media("video", 1, 0, false), media("video", 2, 1, true)],
    );
    server.put_sequence(
        "audio.m3u8",
        vec![media("audio", 2, 0, false), media("audio", 3, 1, true)],
    );
    server.put(
        "master.m3u8",
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=320x180,AUDIO=\"aud\"\nvideo.m3u8\n",
    );

    let output = run(live_request(server.url("master.m3u8"), &dir))
        .await
        .unwrap();

    let tracks = vec![
        track("fmp4_a/video", Some("init.mp4"), &video),
        track("fmp4_a/audio", Some("init.mp4"), &audio),
    ];
    let want = expected(
        &dir,
        &[Streams::Video, Streams::Audio],
        &[DiscontinuityGroup { tracks }],
    );
    assert_output(&output, &want);
    assert_eq!(output.live, report(LiveEnd::EndList, vec![]));
    assert_eq!(
        (server.hits("video/init.mp4"), server.hits("audio/init.mp4")),
        (2, 2)
    );
}

/// 同一序号的分片在两次刷新之间换了地址：服务器错误，任务失败。
#[tokio::test(flavor = "multi_thread")]
async fn changed_segment_fails() {
    let dir = test_dir("live_changed");
    let server = Server::start().await;
    put_long(&server, &[0, 1, 2]);
    let changed = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg2.ts\n";
    server.put_sequence(
        "live.m3u8",
        vec![playlist(&[0, 1], false), changed.to_owned()],
    );

    let err = run(live_request(server.url("live.m3u8"), &dir))
        .await
        .unwrap_err();

    assert!(
        matches!(err, Error::LiveSegmentChanged { sequence: 1, .. }),
        "{err}"
    );
}
