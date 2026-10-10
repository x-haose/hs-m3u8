//! 续录时判定会话期间与会话定下时的失败与边界：在途分片的补录、等候选的轨的停滞、已录满或已结束而没有分片的轨、
//! 核对失败与换更旧的重叠分片、慢的核对与 init 段不挡住其他轨。

use std::num::NonZeroUsize;
use std::time::Duration;

use axum::http::StatusCode;
use hs_m3u8_core::{Error, HttpError, LiveEnd, LiveOptions, StallCause, StallError, Url};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use super::{
    STALL, expected_split, expected_split_long, interrupt, live_request, playlist, playlist_in,
    put_long, put_split_master, report, run_until_full, signed_fmp4_playlist, slow_retry,
    split_source,
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

/// 判定期间视频已有候选、照常刷新且一直列出新分片，音频的播放列表一直没有分片：视频仍在出，音频不出新分片
/// 是它的故障，任务失败、目录保留，而不是当作直播已结束。
#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_track_stops_while_another_keeps_going() {
    let dir = test_dir("deciding_waiting_stops");
    let server = Server::start().await;
    put_split_master(&server);
    put_long(&server, "v/", &[0, 1]);
    put_long(&server, "a/", &[0, 1]);
    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    let req = live_request(server.url("master.m3u8"), &dir, Duration::from_millis(500));
    interrupt(&req, |p| p.segments_done == 4).await;

    // 视频每次刷新多一个分片（会话定下之前不下载，不必有内容），远长于音频停滞所需的时间
    let growing = (2..100)
        .map(|last| playlist_in("v/", &(0..=last).collect::<Vec<u64>>(), false))
        .collect();
    server.put_sequence("video.m3u8", growing);
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

/// 中断期间直播结束了：视频停在 [0,1]（没有 ENDLIST、不再更新），音频的播放列表变空。音频停滞时视频也不再出
/// 新分片，按直播已结束收尾、合并已录的，与录制期间遇到同样的状态一致。
#[tokio::test(flavor = "multi_thread")]
async fn an_ended_live_with_an_emptied_track_ends_normally() {
    let dir = test_dir("deciding_emptied_track");
    let server = Server::start().await;
    put_split_master(&server);
    put_long(&server, "v/", &[0, 1]);
    put_long(&server, "a/", &[0, 1]);
    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    let req = live_request(server.url("master.m3u8"), &dir, Duration::from_millis(500));
    interrupt(&req, |p| p.segments_done == 4).await;

    server.put("audio.m3u8", playlist_in("a/", &[], false));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_split_long(&dir, &[(&[0, 1], &[0, 1])]));
    let end = LiveEnd::Stalled {
        track: 1,
        cause: StallCause::NoNewSegments,
    };
    assert_eq!(output.live, report(end, 1, vec![]));
}

/// 判定期间视频的核对因重试变慢（长于 stall_timeout），音频第一份播放列表恰好没有分片、下一份才有：核对不挡住
/// 音频刷新，音频不因此被判停滞，两轨都录到之后的分片。
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_check_does_not_stall_a_waiting_track() {
    let dir = test_dir("deciding_slow_check_waiting");
    let server = Server::start().await;
    put_split_master(&server);
    put_long(&server, "v/", &[0, 1, 2, 3]);
    put_long(&server, "a/", &[0, 1, 2, 3]);
    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    let mut req = live_request(server.url("master.m3u8"), &dir, Duration::from_millis(300));
    interrupt(&req, |p| p.segments_done == 4).await;

    req.retry = slow_retry(Duration::from_millis(300));
    server.fail("v/seg1.ts", 2);
    server.put("video.m3u8", playlist_in("v/", &[0, 1, 2, 3], true));
    server.put_sequence(
        "audio.m3u8",
        vec![
            playlist_in("a/", &[], false),
            playlist_in("a/", &[0, 1, 2, 3], true),
        ],
    );
    let output = run(req).await.unwrap();

    let want = expected_split_long(&dir, &[(&[0, 1, 2, 3], &[0, 1, 2, 3])]);
    assert_output(&output, &want);
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 判定期间音频一直没有分片，停滞时视频也不再出新分片（它的候选已结束）：按直播已结束收尾，视频候选里新列出的
/// 分片照常录进成片。
#[tokio::test(flavor = "multi_thread")]
async fn a_stall_while_deciding_still_records_listed_segments() {
    let dir = test_dir("deciding_stall_records_listed");
    let server = Server::start().await;
    put_split_master(&server);
    put_long(&server, "v/", &[0, 1, 2, 3]);
    put_long(&server, "a/", &[0, 1]);
    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    let req = live_request(server.url("master.m3u8"), &dir, Duration::from_millis(500));
    interrupt(&req, |p| p.segments_done == 4).await;

    server.put("video.m3u8", playlist_in("v/", &[0, 1, 2, 3], true));
    server.put("audio.m3u8", playlist_in("a/", &[], false));
    let output = run(req).await.unwrap();

    let want = expected_split_long(&dir, &[(&[0, 1, 2, 3], &[0, 1])]);
    assert_output(&output, &want);
    let end = LiveEnd::Stalled {
        track: 1,
        cause: StallCause::NoNewSegments,
    };
    assert_eq!(output.live, report(end, 1, vec![]));
}

/// 会话定下后视频新签名的 init 段一时拉不到（重试退避长于音频的停滞时长），音频不受影响：照常刷新，录到之后
/// 出现的分片。视频的目标时长较长，它自己的停滞时限长于拉 init 段的时间。
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_init_fetch_does_not_stall_another_track() {
    let dir = test_dir("deciding_slow_init");
    let server = Server::start().await;
    let (video, audio) = split_source(&server, &[(1, 1, false)], &[(1, 1, false)]);
    server.put(
        "video.m3u8",
        signed_fmp4_playlist("video", "2", 1, 1, false),
    );
    let mut req = live_request(server.url("master.m3u8"), &dir, Duration::from_millis(300));
    interrupt(&req, |p| p.segments_done == 2).await;

    req.retry = slow_retry(Duration::from_millis(300));
    server.put("video.m3u8", signed_fmp4_playlist("video", "2", 2, 2, true));
    server.fail("video/init.mp4", 2);
    server.put_sequence(
        "audio.m3u8",
        vec![
            signed_fmp4_playlist("audio", "0.1", 1, 1, false),
            signed_fmp4_playlist("audio", "0.1", 3, 1, true),
        ],
    );
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_split(&dir, &video, &audio));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 中断期间直播结束，音频的播放列表只剩 ENDLIST、没有分片，地址还换了令牌：音频这次没有可录的、不影响判定，
/// 视频接着原会话录完，改记新地址；不另起会话，也不报无法确认是同一个直播。
#[tokio::test(flavor = "multi_thread")]
async fn an_ended_track_without_segments_does_not_affect_the_decision() {
    let dir = test_dir("deciding_ended_empty");
    let server = Server::start().await;
    put_split_master(&server);
    put_long(&server, "v/", &[0, 1, 2, 3]);
    put_long(&server, "a/", &[0, 1]);
    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    let with_token = |token: u32| {
        let url = Url::parse(&format!("{}?token={token}", server.url("master.m3u8"))).unwrap();
        live_request(url, &dir, STALL)
    };
    interrupt(&with_token(1), |p| p.segments_done == 4).await;

    server.put("video.m3u8", playlist_in("v/", &[0, 1, 2, 3], true));
    server.put("audio.m3u8", playlist_in("a/", &[], true));
    let output = run(with_token(2)).await.unwrap();

    let want = expected_split_long(&dir, &[(&[0, 1, 2, 3], &[0, 1])]);
    assert_output(&output, &want);
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 判定期间两条轨的播放列表一直没有分片：直播看起来已结束，两条轨这次都不录，按停滞收尾、合并已录的。
#[tokio::test(flavor = "multi_thread")]
async fn tracks_without_candidates_all_end_together() {
    let dir = test_dir("deciding_no_candidates");
    let server = Server::start().await;
    put_split_master(&server);
    put_long(&server, "v/", &[0, 1]);
    put_long(&server, "a/", &[0, 1]);
    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    let req = live_request(server.url("master.m3u8"), &dir, Duration::from_millis(300));
    interrupt(&req, |p| p.segments_done == 4).await;

    server.put("video.m3u8", playlist_in("v/", &[], false));
    server.put("audio.m3u8", playlist_in("a/", &[], false));
    let output = tokio::time::timeout(Duration::from_secs(10), run(req))
        .await
        .expect("还没拿到候选的轨应一起结束")
        .unwrap();

    assert_output(&output, &expected_split_long(&dir, &[(&[0, 1], &[0, 1])]));
    let live = output.live.unwrap();
    assert!(
        matches!(
            live.end,
            LiveEnd::Stalled {
                cause: StallCause::NoNewSegments,
                ..
            }
        ),
        "{:?}",
        live.end
    );
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
    // 第一次运行两轨处理完第一份播放列表就都满了 2 秒（音频 seg1 取不到也计入）
    run_until_full(&req).await;

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

/// 重叠中最新的分片一直取不到（重试后仍 500），更早的取得到：用更早的核对，接得上就接着原会话录。
#[tokio::test(flavor = "multi_thread")]
async fn an_older_overlap_is_checked_when_the_newest_keeps_failing() {
    let dir = test_dir("deciding_older_overlap");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 2).await;

    server.put("live.m3u8", playlist(&[0, 1, 2, 3], true));
    server.status("seg1.ts", StatusCode::SERVICE_UNAVAILABLE);
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

/// 核对花的时间（重试退避使它长于 stall_timeout）不计入停滞，其间播放列表没有新分片也不算：会话定下后照常刷新，
/// 录到之后出现的分片。
#[tokio::test(flavor = "multi_thread")]
async fn slow_check_does_not_count_as_a_stall() {
    let dir = test_dir("deciding_slow_check");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let mut req = live_request(server.url("live.m3u8"), &dir, Duration::from_millis(300));
    interrupt(&req, |p| p.segments_done == 2).await;

    // 核对 seg1 先失败两次，退避合计至少 600 毫秒；其间播放列表一直没有新分片、也没结束（约 100 毫秒刷新一次，
    // 前六次都是 [0,1]），之后才出现 2、3
    req.retry = slow_retry(Duration::from_millis(400));
    server.fail("seg1.ts", 2);
    let mut windows = vec![playlist(&[0, 1], false); 6];
    windows.push(playlist(&[0, 1, 2], false));
    windows.push(playlist(&[0, 1, 2, 3], true));
    server.put_sequence("live.m3u8", windows);
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}
