//! 续录时判定会话期间的失败与边界：在途分片的补录、暂停的轨、已录满的轨、核对失败、停止。

use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use axum::http::StatusCode;
use hs_m3u8_core::{
    Error, HttpError, LiveEnd, LiveOptions, RetryPolicy, StallCause, StallError, Url,
};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use super::{
    STALL, expected_split_long, interrupt, live_request, playlist, playlist_in, put_long,
    put_split_master, report,
};
use crate::server::Server;
use crate::{assert_output, engine, expected, expected_long, fixture, run, test_dir, track};

/// 并发下载时，会话开头的分片还在下载、后面的已完成就被中断：续录时它仍在窗口里，补录进原会话。
#[tokio::test(flavor = "multi_thread")]
async fn in_flight_first_segment_is_refilled() {
    let dir = test_dir("deciding_first_in_flight");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1, 2, 3], false));
    let mut req = live_request(server.url("live.m3u8"), &dir, STALL);
    req.concurrency = NonZeroUsize::new(2).unwrap();
    let gate = server.gate("seg0.ts");
    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.progress();
    gate.arrived.notified().await;
    progress.wait_for(|p| p.segments_done == 3).await.unwrap();
    job.cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    server.ungate("seg0.ts");

    server.put("live.m3u8", playlist(&[0, 1, 2, 3], true));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 判定期间视频已有仍在直播的候选、暂停刷新，音频的播放列表一直没有分片：暂停的视频既不算停滞，
/// 也算仍在出新分片，于是音频不出新分片是它的故障，任务失败、目录保留，而不是当作直播已结束。
#[tokio::test(flavor = "multi_thread")]
async fn a_held_track_counts_as_live() {
    let dir = test_dir("deciding_held");
    let server = Server::start().await;
    put_split_master(&server);
    put_long(&server, "v/", &[0, 1, 2]);
    put_long(&server, "a/", &[0, 1]);
    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    let req = live_request(server.url("master.m3u8"), &dir, Duration::from_millis(500));
    interrupt(&req, |p| p.segments_done == 4).await;

    server.put("video.m3u8", playlist_in("v/", &[0, 1, 2], false));
    server.put("audio.m3u8", playlist_in("a/", &[], false));
    let err = run(req).await.unwrap_err();

    assert!(
        matches!(
            err,
            Error::LiveStalled {
                track: 1,
                cause: StallError::TrackStopped(StallCause::NoNewSegments)
            }
        ),
        "{err}"
    );
    assert!(dir.join("out.mp4.hsdl/job.json").exists());
}

/// 视频此前已录满 max_duration、这次第一份播放列表恰好没有分片：它不参与判定，音频接着原会话补满时长。
#[tokio::test(flavor = "multi_thread")]
async fn a_full_track_does_not_block_the_decision() {
    let dir = test_dir("deciding_full_track");
    let server = Server::start().await;
    put_split_master(&server);
    put_long(&server, "v/", &[0, 1]);
    put_long(&server, "a/", &[0]);
    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    let mut req = live_request(server.url("master.m3u8"), &dir, Duration::from_millis(500));
    req.live = Some(LiveOptions {
        max_duration: Some(Duration::from_secs(2)),
        ..req.live.unwrap()
    });
    // 第一次运行两轨处理完第一份播放列表就都满了 2 秒（音频 seg1 取不到也计入），自行结束；
    // 保留任务目录、删掉输出，再续录
    let mut first = req.clone();
    first.keep_work_dir = true;
    let output = run(first).await.unwrap();
    assert_eq!(output.live.unwrap().end, LiveEnd::DurationReached);
    std::fs::remove_file(&req.output).unwrap();

    put_long(&server, "a/", &[1, 2, 3]);
    server.put("video.m3u8", playlist_in("v/", &[], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1, 2, 3], false));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_split_long(&dir, &[(&[0, 1], &[0, 1])]));
    assert_eq!(output.live, report(LiveEnd::DurationReached, 1, vec![]));
    assert_eq!(server.hits("a/seg2.ts"), 0);
}

/// 核对用的分片暂时取不到（重试后仍 500）：不当作内容不同而另起会话重录，如实报错且可重试；
/// 故障过去后再续录，照常接着原会话。
#[tokio::test(flavor = "multi_thread")]
async fn transient_failure_while_checking_is_reported() {
    let dir = test_dir("deciding_check_500");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 2).await;

    server.put("live.m3u8", playlist(&[1, 2, 3], true));
    server.fail("seg1.ts", 3);
    let err = run(req.clone()).await.unwrap_err();
    assert!(
        matches!(err, Error::Segment { sequence: 1, .. }) && err.retryable(),
        "{err}"
    );

    let output = run(req).await.unwrap();
    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 同上，来源地址还换了令牌：报的是取分片的故障（可重试），不是「无法确认是同一个直播」。
#[tokio::test(flavor = "multi_thread")]
async fn transient_failure_with_a_new_token_is_not_unverified() {
    let dir = test_dir("deciding_token_500");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let with_token = |token: u32| {
        let url = Url::parse(&format!("{}?token={token}", server.url("live.m3u8"))).unwrap();
        live_request(url, &dir, STALL)
    };
    interrupt(&with_token(1), |p| p.segments_done == 2).await;

    server.put("live.m3u8", playlist(&[1, 2], true));
    server.fail("seg1.ts", 3);
    let err = run(with_token(2)).await.unwrap_err();

    assert!(
        matches!(
            &err,
            Error::Segment { cause, .. }
                if matches!(**cause, Error::Http { kind: HttpError::Status(500), .. })
        ) && err.retryable(),
        "{err}"
    );
}

/// 只被已录分片引用的旧 init 段现在取不到（403），新分片换了 init 段：只拉新分片要用的，续录照常。
#[tokio::test(flavor = "multi_thread")]
async fn inits_of_recorded_segments_are_not_fetched_again() {
    let dir = test_dir("deciding_old_init");
    let server = Server::start().await;
    for name in ["init.mp4", "seg0.m4s", "seg1.m4s"] {
        server.put(
            &format!("a/{name}"),
            fixture(&format!("fmp4_a/video/{name}")),
        );
    }
    for name in ["init.mp4", "seg0.m4s"] {
        server.put(
            &format!("b/{name}"),
            fixture(&format!("fmp4_b/video/{name}")),
        );
    }
    let head = |sequence: u64| {
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MEDIA-SEQUENCE:{sequence}\n\
             #EXT-X-MAP:URI=\"a/init.mp4\"\n"
        )
    };
    server.put(
        "live.m3u8",
        format!(
            "{}#EXTINF:1,\na/seg0.m4s\n#EXTINF:1,\na/seg1.m4s\n",
            head(0)
        ),
    );
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 2).await;

    server.status("a/init.mp4", StatusCode::FORBIDDEN);
    server.put(
        "live.m3u8",
        format!(
            "{}#EXTINF:1,\na/seg1.m4s\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"b/init.mp4\"\n\
             #EXTINF:1,\nb/seg0.m4s\n#EXT-X-ENDLIST\n",
            head(1)
        ),
    );
    let output = run(req).await.unwrap();

    let group = |name: &str, segments: &[&str]| DiscontinuityGroup {
        tracks: vec![track(name, Some("init.mp4"), segments)],
    };
    let want = expected(
        &dir,
        &[Streams::All],
        &[
            group("fmp4_a/video", &["seg0.m4s", "seg1.m4s"]),
            group("fmp4_b/video", &["seg0.m4s"]),
        ],
    );
    assert_output(&output, &want);
}

/// 核对用的分片请求挂住时调用 stop：不等它返回，按停止收尾，合并已录的部分。
#[tokio::test(flavor = "multi_thread")]
async fn stop_is_observed_while_checking() {
    let dir = test_dir("deciding_stop");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 2).await;

    server.put("live.m3u8", playlist(&[1, 2, 3], false));
    let gate = server.gate("seg1.ts");
    let job = engine().start(req).unwrap();
    gate.arrived.notified().await;
    job.stop();
    // 不响应 stop 时任务会一直挂着：给一个远大于正常用时的上限，超过即失败
    let output = tokio::time::timeout(Duration::from_secs(10), job.wait())
        .await
        .expect("stop 之后应及时结束")
        .unwrap();
    server.ungate("seg1.ts");

    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    assert_eq!(output.live, report(LiveEnd::Stopped, 1, vec![]));
}

/// 核对花的时间（重试退避使它长于 stall_timeout）不计入停滞：会话定下后照常刷新，录到之后出现的分片。
#[tokio::test(flavor = "multi_thread")]
async fn slow_check_does_not_count_as_a_stall() {
    let dir = test_dir("deciding_slow_check");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let mut req = live_request(server.url("live.m3u8"), &dir, Duration::from_millis(300));
    interrupt(&req, |p| p.segments_done == 2).await;

    req.retry = RetryPolicy {
        attempts: NonZeroU32::new(3).unwrap(),
        base_delay: Duration::from_millis(300),
        max_delay: Duration::from_millis(300),
    };
    server.fail("seg1.ts", 2);
    server.put_sequence(
        "live.m3u8",
        vec![playlist(&[0, 1], false), playlist(&[0, 1, 2, 3], true)],
    );
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}
