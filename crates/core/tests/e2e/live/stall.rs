//! 停滞的判定与分类：直播已结束照常收尾，故障使任务失败并保留目录。

use std::time::Duration;

use axum::http::StatusCode;
use hs_m3u8_core::{Error, HttpError, LiveEnd, MissReason, RefreshCause, StallCause, StallError};

use super::{
    STALL, expected_split, live_request, missed, playlist, playlist_with_target, put_long,
    put_split_master, report, split_source,
};
use crate::server::Server;
use crate::{assert_output, engine, expected_long, fixture, run, test_dir};

/// 播放列表不再出现新分片，随后被删除（404）：超过 stall_timeout 即按直播已结束收尾；停滞期间进度里报
/// 刷新失败，收尾后清空。
#[tokio::test(flavor = "multi_thread")]
async fn removed_playlist_ends_recording() {
    let dir = test_dir("live_gone");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));

    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments_total == 2).await.unwrap();
    server.remove("live.m3u8");
    let gone = Some(RefreshCause::Http {
        kind: HttpError::Status(404),
        retry_after: None,
    });
    progress
        .wait_for(|p| p.refresh_errors == [gone.clone()])
        .await
        .unwrap();
    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    let end = LiveEnd::Stalled {
        track: 0,
        cause: StallCause::PlaylistGone(404),
    };
    assert_eq!(output.live, report(end, 1, vec![]));
    assert_eq!(progress.borrow().refresh_errors, [None]);
}

/// 刷新照常成功、只是不再出现新分片：按直播已结束收尾。
#[tokio::test(flavor = "multi_thread")]
async fn no_new_segments_ends_recording() {
    let dir = test_dir("live_idle");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));

    let output = run(live_request(
        server.url("live.m3u8"),
        &dir,
        Duration::from_millis(500),
    ))
    .await
    .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    let end = LiveEnd::Stalled {
        track: 0,
        cause: StallCause::NoNewSegments,
    };
    assert_eq!(output.live, report(end, 1, vec![]));
}

/// 音频轨的播放列表在首次之后被删除、视频轨照常：按音频轨的直播已结束收尾。
#[tokio::test(flavor = "multi_thread")]
async fn one_track_whose_playlist_is_removed_ends_the_recording() {
    let dir = test_dir("live_track_gone");
    let server = Server::start().await;
    // 视频前几次刷新没有新分片，之后出 seg1：它最近一次录到新分片明显晚于音频轨
    let mut video_refreshes = vec![(1, 0, false); 5];
    video_refreshes.push((2, 0, false));
    let (video, audio) = split_source(&server, &video_refreshes, &[(1, 0, false)]);
    let job = engine()
        .start(live_request(server.url("master.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments_total >= 2).await.unwrap();
    server.remove("audio.m3u8");

    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_split(&dir, &video, &audio[..1]));
    let end = LiveEnd::Stalled {
        track: 1,
        cause: StallCause::PlaylistGone(404),
    };
    assert_eq!(output.live.unwrap().end, Some(end));
}

/// 音频轨的刷新一直 500、视频轨一直有新分片：音频轨的故障使任务失败（目录保留，可续录），
/// 不当作直播结束，也不因视频轨正常而被掩盖。
#[tokio::test(flavor = "multi_thread")]
async fn one_failing_track_fails_the_recording() {
    let dir = test_dir("live_track_fails");
    let server = Server::start().await;
    for name in ["init.mp4", "seg0.m4s"] {
        server.put(
            &format!("audio/{name}"),
            fixture(&format!("fmp4_a/audio/{name}")),
        );
    }
    server.put("video/init.mp4", fixture("fmp4_a/video/init.mp4"));
    // 视频每次刷新窗口前移一个分片；内容重复无妨，任务以失败结束、不合并
    let windows: Vec<String> = (0..200u64)
        .map(|n| {
            format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MEDIA-SEQUENCE:{n}\n\
                 #EXT-X-MAP:URI=\"video/init.mp4\"\n#EXTINF:1,\nvideo/s{n}.m4s\n\
                 #EXTINF:1,\nvideo/s{}.m4s\n",
                n + 1
            )
        })
        .collect();
    for n in 0..=200u64 {
        server.put(
            &format!("video/s{n}.m4s"),
            fixture(&format!("fmp4_a/video/seg{}.m4s", n % 2)),
        );
    }
    server.put_sequence("video.m3u8", windows);
    server.put(
        "audio.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MAP:URI=\"audio/init.mp4\"\n\
         #EXTINF:1,\naudio/seg0.m4s\n",
    );
    put_split_master(&server);
    let job = engine()
        .start(live_request(server.url("master.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments_done >= 3).await.unwrap();
    server.status("audio.m3u8", StatusCode::INTERNAL_SERVER_ERROR);

    let err = job.wait().await.unwrap_err();

    match &err {
        Error::LiveStalled {
            track: 1,
            cause: StallError::RefreshFailed(cause),
        } => assert!(
            matches!(
                cause,
                RefreshCause::Http {
                    kind: HttpError::Status(500),
                    ..
                }
            ),
            "{cause}"
        ),
        other => panic!("{other}"),
    }
    assert!(err.retryable());
    assert!(dir.join("out.hsdl/job.json").exists());
}

/// 音频轨每次都有新分片、但它们的 init 段一直 404：一个都录不到，超过 stall_timeout 即任务失败，
/// 不会无限录下去。
#[tokio::test(flavor = "multi_thread")]
async fn a_track_that_records_nothing_fails_the_recording() {
    let dir = test_dir("live_unrecordable");
    let server = Server::start().await;
    for name in ["init.mp4", "seg0.m4s", "seg1.m4s"] {
        server.put(
            &format!("video/{name}"),
            fixture(&format!("fmp4_a/video/{name}")),
        );
    }
    server.put(
        "video.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MAP:URI=\"video/init.mp4\"\n\
         #EXTINF:1,\nvideo/seg0.m4s\n#EXTINF:1,\nvideo/seg1.m4s\n",
    );
    let audio: Vec<String> = (0..100u64)
        .map(|n| {
            format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MEDIA-SEQUENCE:{n}\n\
                 #EXT-X-MAP:URI=\"audio/missing.mp4\"\n#EXTINF:1,\naudio/seg{n}.m4s\n"
            )
        })
        .collect();
    server.put_sequence("audio.m3u8", audio);
    put_split_master(&server);

    let err = run(live_request(server.url("master.m3u8"), &dir, STALL))
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            Error::LiveStalled {
                track: 1,
                cause: StallError::Unrecordable(HttpError::Status(404))
            }
        ),
        "{err}"
    );
    assert!(!err.retryable());
}

/// 分片一直 503（CDN 临时故障）：一个都录不到，任务失败，但可以稍后重试（续录）。
#[tokio::test(flavor = "multi_thread")]
async fn segments_that_keep_failing_temporarily_are_retryable() {
    let dir = test_dir("live_segments_503");
    let server = Server::start().await;
    let windows: Vec<String> = (0..100u64).map(|n| playlist(&[n, n + 1], false)).collect();
    for n in 0..=100u64 {
        server.status(&format!("seg{n}.ts"), StatusCode::SERVICE_UNAVAILABLE);
    }
    server.put_sequence("live.m3u8", windows);

    let err = run(live_request(
        server.url("live.m3u8"),
        &dir,
        Duration::from_millis(500),
    ))
    .await
    .unwrap_err();

    assert!(
        matches!(
            err,
            Error::LiveStalled {
                track: 0,
                cause: StallError::Unrecordable(HttpError::Status(503))
            }
        ),
        "{err}"
    );
    assert!(err.retryable());
}

/// 直播收尾时最后列出的一个分片 404，之后播放列表不再更新：按直播已结束收尾，那个分片记为缺失。
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_last_segment_does_not_turn_the_end_into_a_failure() {
    let dir = test_dir("live_last_404");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put_sequence(
        "live.m3u8",
        vec![playlist(&[0, 1], false), playlist(&[0, 1, 2], false)],
    );

    let output = run(live_request(
        server.url("live.m3u8"),
        &dir,
        Duration::from_millis(500),
    ))
    .await
    .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    let end = LiveEnd::Stalled {
        track: 0,
        cause: StallCause::NoNewSegments,
    };
    let not_found = MissReason::Failed(HttpError::Status(404));
    assert_eq!(output.live, report(end, 1, vec![missed(2, 2, not_found)]));
}

/// 音频轨的分片排在视频的一个慢分片后面、迟迟轮不到下载：在等本任务的下载队列，不算停滞。
#[tokio::test(flavor = "multi_thread")]
async fn queued_segments_do_not_stall_a_track() {
    let dir = test_dir("live_queued");
    let server = Server::start().await;
    // 音频第一份没有分片，视频的 seg0 先占住唯一的下载名额；音频随后列出的分片都排在它后面。
    // 音频刷新十次后多出 seg2 并结束：等到它被排入下载时，已经过了好几个停滞时长
    let mut audio = vec![(0, 0, false)];
    audio.extend([(2, 0, false); 10]);
    audio.push((3, 0, true));
    let (video, audio) = split_source(&server, &[(2, 0, false), (2, 0, true)], &audio);
    let gate = server.gate("video/seg0.m4s");
    let job = engine()
        .start(live_request(
            server.url("master.m3u8"),
            &dir,
            Duration::from_millis(1),
        ))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments_total == 5).await.unwrap();
    server.ungate("video/seg0.m4s");
    drop(gate);

    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_split(&dir, &video, &audio));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 视频轨一直有新分片，音频轨的播放列表卡住不再更新（打包器故障）：不当作直播结束，任务失败、目录保留，
/// 可以续录。
#[tokio::test(flavor = "multi_thread")]
async fn a_frozen_track_fails_while_the_other_keeps_going() {
    let dir = test_dir("live_frozen_track");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put_sequence(
        "video.m3u8",
        vec![
            playlist_with_target("0.3", &[0], false),
            playlist_with_target("0.3", &[0, 1], false),
            playlist_with_target("0.3", &[0, 1, 2], false),
            playlist_with_target("0.3", &[0, 1, 2, 3], false),
        ],
    );
    server.put("audio.m3u8", playlist(&[0], false));
    put_split_master(&server);

    let err = run(live_request(
        server.url("master.m3u8"),
        &dir,
        Duration::from_millis(500),
    ))
    .await
    .unwrap_err();

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
    assert!(err.retryable());
    assert!(dir.join("out.hsdl/job.json").exists());
}

/// 刷新返回 503 并要求等一个无法表示的时长：不再刷新，按刷新失败的停滞结束，不会因时间溢出而崩溃。
#[tokio::test(flavor = "multi_thread")]
async fn endless_retry_after_ends_as_a_refresh_failure() {
    let dir = test_dir("live_retry_after");
    let server = Server::start().await;
    put_long(&server, "", &[0]);
    server.put("live.m3u8", playlist(&[0], false));
    let job = engine()
        .start(live_request(
            server.url("live.m3u8"),
            &dir,
            Duration::from_millis(500),
        ))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments_done == 1).await.unwrap();
    server.status_retry_after(
        "live.m3u8",
        StatusCode::SERVICE_UNAVAILABLE,
        "18446744073709551615",
    );

    let err = job.wait().await.unwrap_err();

    assert!(
        matches!(
            err,
            Error::LiveStalled {
                track: 0,
                cause: StallError::RefreshFailed(_),
            }
        ),
        "{err}"
    );
    assert!(err.retry_after().is_some());
}

/// TARGETDURATION 比 stall_timeout 长：停滞至少等三个目标时长，正常的刷新节奏不会被误判为停滞。
#[tokio::test(flavor = "multi_thread")]
async fn long_target_duration_extends_the_stall_deadline() {
    let dir = test_dir("live_long_target");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put_sequence(
        "live.m3u8",
        vec![
            playlist_with_target("0.5", &[0], false),
            playlist_with_target("0.5", &[0, 1], false),
            playlist_with_target("0.5", &[0, 1, 2, 3], true),
        ],
    );

    let output = run(live_request(
        server.url("live.m3u8"),
        &dir,
        Duration::from_millis(200),
    ))
    .await
    .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 刷新返回 403（如令牌过期）：任务失败，不当作直播自然结束。
#[tokio::test(flavor = "multi_thread")]
async fn forbidden_refresh_fails_the_task() {
    let dir = test_dir("live_403");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments_total == 2).await.unwrap();
    server.status("live.m3u8", StatusCode::FORBIDDEN);

    let err = job.wait().await.unwrap_err();

    assert!(
        matches!(
            err,
            Error::Http {
                kind: HttpError::Status(403),
                ..
            }
        ),
        "{err}"
    );
}
