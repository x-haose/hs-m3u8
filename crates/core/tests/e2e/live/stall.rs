//! 停滞的判定与分类：直播已结束照常收尾，故障使任务失败并保留目录。

use std::time::Duration;

use axum::http::StatusCode;
use hs_m3u8_core::{
    Error, FetchErrorKind, FetchFailure, HttpErrorKind, HttpFailure, LiveEnd, MissReason,
    RefreshReason, StallCause, StallReason,
};

use super::{
    JUDGED_TARGET, STALL, expected_split, judged_in, live_request, missed, outcome, playlist,
    playlist_with_target, put_long, put_split_master, report, split_source_with_target,
};
use crate::server::Server;
use crate::{assert_output, engine, expected_long, fixture, run, test_dir};

/// 播放列表不再出现新分片，随后被删除（404）：到了停滞时长即按直播已结束收尾；停滞期间进度里报刷新失败，收尾后清空。
#[tokio::test(flavor = "multi_thread")]
async fn removed_playlist_ends_recording() {
    let dir = test_dir("live_gone");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", judged_in("", &[0, 1], false));

    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments.total == 2).await.unwrap();
    server.remove("live.m3u8");
    let gone = Some(RefreshReason::Http(HttpFailure {
        kind: HttpErrorKind::Status(404),
        retry_after: None,
    }));
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
    assert_eq!(outcome(&output), report(end, 1, vec![]));
    assert_eq!(progress.borrow().refresh_errors, [None]);
}

/// 刷新照常成功、只是不再出现新分片：按直播已结束收尾。
#[tokio::test(flavor = "multi_thread")]
async fn no_new_segments_ends_recording() {
    let dir = test_dir("live_idle");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", judged_in("", &[0, 1], false));

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    let end = LiveEnd::Stalled {
        track: 0,
        cause: StallCause::NoNewSegments,
    };
    assert_eq!(outcome(&output), report(end, 1, vec![]));
}

/// 进程整个停住（如 Windows 上合盖睡眠，单调时钟照走）超过停滞时长，其间直播出了新分片：醒来时停滞已到，先刷新一次
/// 再下结论，接着录到新出的分片，之后真的不再出新分片才按直播已结束收尾。
#[tokio::test(flavor = "current_thread")]
async fn a_stall_after_the_process_was_suspended_is_judged_on_a_fresh_refresh() {
    let dir = test_dir("live_suspended");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", judged_in("", &[0, 1], false));
    // stall_timeout 取 1 毫秒：实际等三个目标时长，即 3 秒
    let job = engine()
        .start(live_request(
            server.url("live.m3u8"),
            &dir,
            Duration::from_millis(1),
        ))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments.done == 2).await.unwrap();
    server.put("live.m3u8", judged_in("", &[0, 1, 2, 3], false));
    // 停住唯一的运行时线程，模拟进程被挂起：其间录制与测试服务都不运行，时钟照走，超过停滞时长
    std::thread::sleep(Duration::from_secs(4));

    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    let end = LiveEnd::Stalled {
        track: 0,
        cause: StallCause::NoNewSegments,
    };
    assert_eq!(outcome(&output), report(end, 1, vec![]));
}

/// 来源断流期间一直写 EXT-X-GAP（超过停滞时长）：直播还在，不算停滞，录制照常进行，直到出现 ENDLIST；GAP 记为缺失。
#[tokio::test(flavor = "multi_thread")]
async fn continuous_gaps_are_not_a_stall() {
    let dir = test_dir("live_gap_stall");
    let server = Server::start().await;
    put_long(&server, "", &[0]);
    let window = |gaps: u64| {
        let mut text =
            "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:1,\nseg0.ts\n"
                .to_owned();
        for i in 1..=gaps {
            text += &format!("#EXT-X-GAP\n#EXTINF:1,\nseg{i}.ts\n");
        }
        text
    };
    // 每次刷新多一个 GAP，合计远长于 0.5 秒的停滞时长
    let mut windows: Vec<String> = (0..=15).map(window).collect();
    windows.push(window(15) + "#EXT-X-ENDLIST\n");
    server.put_sequence("live.m3u8", windows);

    let output = run(live_request(
        server.url("live.m3u8"),
        &dir,
        Duration::from_millis(500),
    ))
    .await
    .unwrap();

    assert_output(&output, &expected_long(&dir, &[0], &[1]));
    let gaps = missed(1, 15, MissReason::Gap);
    assert_eq!(outcome(&output), report(LiveEnd::EndList, 1, vec![gaps]));
}

/// 每次刷新都列出一个 EXT-X-GAP 与一个取不到的分片：有要取的新分片一个也录不到，照样判为停滞（GAP 只在一次列出的
/// 新分片全是 GAP 时推迟停滞）。
#[tokio::test(flavor = "multi_thread")]
async fn gaps_listed_with_unrecordable_segments_still_stall() {
    let dir = test_dir("live_gap_404");
    let server = Server::start().await;
    put_long(&server, "", &[0]);
    let window = |count: u64| {
        let mut text = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:{JUDGED_TARGET}\n#EXT-X-MEDIA-SEQUENCE:0\n\
             #EXTINF:1,\nseg0.ts\n"
        );
        for i in 1..=count {
            text += &format!("#EXT-X-GAP\n#EXTINF:1,\ngap{i}.ts\n#EXTINF:1,\nmissing{i}.ts\n");
        }
        text
    };
    server.put_sequence("live.m3u8", (0..200).map(window).collect());

    let err = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            Error::LiveStalled {
                track: 0,
                cause: StallReason::Unrecordable(FetchFailure::Http(HttpFailure {
                    kind: HttpErrorKind::Status(404),
                    ..
                }))
            }
        ),
        "{err}"
    );
}

/// 音频轨的播放列表在首次之后被删除、视频轨照常：按音频轨的直播已结束收尾。
#[tokio::test(flavor = "multi_thread")]
async fn one_track_whose_playlist_is_removed_ends_the_recording() {
    let dir = test_dir("live_track_gone");
    let server = Server::start().await;
    // 视频前两次刷新没有新分片，之后出 seg1：它最近一次录到新分片明显晚于音频轨
    let mut video_refreshes = vec![(1, 0, false); 2];
    video_refreshes.push((2, 0, false));
    let (video, audio) =
        split_source_with_target(&server, JUDGED_TARGET, &video_refreshes, &[(1, 0, false)]);
    let job = engine()
        .start(live_request(server.url("master.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments.total >= 2).await.unwrap();
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
                "#EXTM3U\n#EXT-X-TARGETDURATION:{JUDGED_TARGET}\n#EXT-X-MEDIA-SEQUENCE:{n}\n\
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
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:{JUDGED_TARGET}\n#EXT-X-MAP:URI=\"audio/init.mp4\"\n\
             #EXTINF:1,\naudio/seg0.m4s\n"
        ),
    );
    put_split_master(&server);
    let job = engine()
        .start(live_request(server.url("master.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments.done >= 3).await.unwrap();
    server.status("audio.m3u8", StatusCode::INTERNAL_SERVER_ERROR);

    let err = job.wait().await.unwrap_err();

    match &err {
        Error::LiveStalled {
            track: 1,
            cause: StallReason::RefreshFailed(cause),
        } => assert!(
            matches!(
                cause,
                RefreshReason::Http(HttpFailure {
                    kind: HttpErrorKind::Status(500),
                    ..
                })
            ),
            "{cause}"
        ),
        other => panic!("{other}"),
    }
    assert!(err.retryable());
    assert!(dir.join("out.hsdl/job.json").exists());
}

/// 音频轨每次都有新分片、但它们的 init 段一直 404：一个都录不到，到了停滞时长即任务失败，不会无限录下去；停滞那一刻
/// 即使有刷新在途，原因也是取不到分片。
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
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:{JUDGED_TARGET}\n#EXT-X-MAP:URI=\"video/init.mp4\"\n\
             #EXTINF:1,\nvideo/seg0.m4s\n#EXTINF:1,\nvideo/seg1.m4s\n"
        ),
    );
    let audio: Vec<String> = (0..100u64)
        .map(|n| {
            format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:{JUDGED_TARGET}\n#EXT-X-MEDIA-SEQUENCE:{n}\n\
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
                cause: StallReason::Unrecordable(FetchFailure::Http(HttpFailure {
                    kind: HttpErrorKind::Status(404),
                    retry_after: None
                }))
            }
        ),
        "{err}"
    );
    assert!(!err.retryable());
}

/// 分片一直 503（CDN 临时故障，要求一小时后再试）：一个都录不到，任务失败，但可以稍后重试（续录），带上服务器
/// 要求的等待。
#[tokio::test(flavor = "multi_thread")]
async fn segments_that_keep_failing_temporarily_are_retryable() {
    let dir = test_dir("live_segments_503");
    let server = Server::start().await;
    let windows: Vec<String> = (0..100u64)
        .map(|n| judged_in("", &[n, n + 1], false))
        .collect();
    for n in 0..=100u64 {
        let path = format!("seg{n}.ts");
        server.status_retry_after(&path, StatusCode::SERVICE_UNAVAILABLE, "3600");
    }
    server.put_sequence("live.m3u8", windows);

    let err = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            Error::LiveStalled {
                track: 0,
                cause: StallReason::Unrecordable(FetchFailure::Http(HttpFailure {
                    kind: HttpErrorKind::Status(503),
                    ..
                }))
            }
        ),
        "{err}"
    );
    assert!(err.retryable());
    assert_eq!(err.retry_after(), Some(Duration::from_secs(3600)));
}

/// 刷新一直拿到空内容（如源站重启中）：等到停滞，任务失败；录制时已当作暂时故障，可以稍后续录。
#[tokio::test(flavor = "multi_thread")]
async fn playlists_that_stay_empty_stall_retryably() {
    let dir = test_dir("live_stays_empty");
    let server = Server::start().await;
    put_long(&server, "", &[0]);
    server.put("live.m3u8", judged_in("", &[0], false));
    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments.done == 1).await.unwrap();
    server.put("live.m3u8", "");

    let err = job.wait().await.unwrap_err();

    assert!(
        matches!(
            err,
            Error::LiveStalled {
                track: 0,
                cause: StallReason::RefreshFailed(RefreshReason::Empty),
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
        vec![
            judged_in("", &[0, 1], false),
            judged_in("", &[0, 1, 2], false),
        ],
    );

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    let end = LiveEnd::Stalled {
        track: 0,
        cause: StallCause::NoNewSegments,
    };
    let not_found = MissReason::Failed(FetchErrorKind::Http(HttpErrorKind::Status(404)));
    assert_eq!(
        outcome(&output),
        report(end, 1, vec![missed(2, 2, not_found)])
    );
}

/// 音频轨的分片排在视频的一个慢分片后面、迟迟轮不到下载：在等本任务的下载队列，不算停滞。
#[tokio::test(flavor = "multi_thread")]
async fn queued_segments_do_not_stall_a_track() {
    let dir = test_dir("live_queued");
    let server = Server::start().await;
    // 音频第一份没有分片，视频的 seg0 先占住唯一的下载名额；音频随后列出的分片都排在它后面。
    // 音频刷新五次后多出 seg2 并结束：等到它被排入下载时，已经过了不止一个停滞时长
    let mut audio = vec![(0, 0, false)];
    audio.extend([(2, 0, false); 5]);
    audio.push((3, 0, true));
    let video = [(2, 0, false), (2, 0, true)];
    let (video, audio) = split_source_with_target(&server, JUDGED_TARGET, &video, &audio);
    let gate = server.gate("video/seg0.m4s");
    let job = engine()
        .start(live_request(
            server.url("master.m3u8"),
            &dir,
            Duration::from_millis(1),
        ))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments.total == 5).await.unwrap();
    server.ungate("video/seg0.m4s");
    drop(gate);

    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_split(&dir, &video, &audio));
    assert_eq!(outcome(&output), report(LiveEnd::EndList, 1, vec![]));
}

/// 视频轨一直有新分片，音频轨的播放列表卡住不再更新（打包器故障）：不当作直播结束，任务失败、目录保留，
/// 可以续录。
#[tokio::test(flavor = "multi_thread")]
async fn a_frozen_track_fails_while_the_other_keeps_going() {
    let dir = test_dir("live_frozen_track");
    let server = Server::start().await;
    put_long(&server, "a/", &[0]);
    // 视频每次刷新窗口前移一个分片，持续远长于音频的停滞时长；内容重复无妨，任务以失败结束、不合并
    for n in 0..=12u64 {
        server.put(
            &format!("v/seg{n}.ts"),
            fixture(&format!("ts_long/seg{}.ts", n % 4)),
        );
    }
    let windows = (0..12u64).map(|n| judged_in("v/", &[n, n + 1], false));
    server.put_sequence("video.m3u8", windows.collect());
    server.put("audio.m3u8", judged_in("a/", &[0], false));
    put_split_master(&server);

    let err = run(live_request(server.url("master.m3u8"), &dir, STALL))
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            Error::LiveStalled {
                track: 1,
                cause: StallReason::TrackStopped(StallCause::NoNewSegments)
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
    progress.wait_for(|p| p.segments.done == 1).await.unwrap();
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
                cause: StallReason::RefreshFailed(_),
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
    assert_eq!(outcome(&output), report(LiveEnd::EndList, 1, vec![]));
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
    progress.wait_for(|p| p.segments.total == 2).await.unwrap();
    server.status("live.m3u8", StatusCode::FORBIDDEN);

    let err = job.wait().await.unwrap_err();

    assert!(
        matches!(
            err,
            Error::Http {
                kind: HttpErrorKind::Status(403),
                ..
            }
        ),
        "{err}"
    );
}

/// 音频轨不再出新分片、先到停滞，这时视频轨的刷新一直 500：视频轨的情况不明（故障），不当作直播结束；以视频轨的
/// 刷新失败使任务失败，目录保留，可以续录。
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_other_track_is_not_taken_as_the_end() {
    let dir = test_dir("live_other_fails");
    let server = Server::start().await;
    put_long(&server, "v/", &[0, 1, 2]);
    put_long(&server, "a/", &[0]);
    server.put_sequence(
        "video.m3u8",
        vec![
            judged_in("v/", &[0], false),
            judged_in("v/", &[0, 1], false),
            judged_in("v/", &[0, 1, 2], false),
        ],
    );
    server.put("audio.m3u8", judged_in("a/", &[0], false));
    put_split_master(&server);
    // 音频的停滞时长 6 秒，远长于视频列出 seg2 所需的三次刷新：视频开始失败之后音频才停滞
    let job = engine()
        .start(live_request(
            server.url("master.m3u8"),
            &dir,
            Duration::from_secs(6),
        ))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments.done == 4).await.unwrap();
    server.status("video.m3u8", StatusCode::INTERNAL_SERVER_ERROR);

    let err = job.wait().await.unwrap_err();

    assert!(
        matches!(
            &err,
            Error::LiveStalled {
                track: 0,
                cause: StallReason::RefreshFailed(RefreshReason::Http(HttpFailure {
                    kind: HttpErrorKind::Status(500),
                    ..
                })),
            }
        ),
        "{err}"
    );
    assert!(dir.join("out.hsdl/job.json").exists());
}

/// 停滞开始之前发起的刷新挂住不返回：察觉停滞后再过一个目标时长即以刷新没有返回使任务失败，不等请求本身超时。
#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_hanging_since_before_the_stall_is_concluded_promptly() {
    let dir = test_dir("live_hung_refresh");
    let server = Server::start().await;
    put_long(&server, "", &[0]);
    server.put("live.m3u8", playlist(&[0], false));
    let job = engine()
        .start(live_request(
            server.url("live.m3u8"),
            &dir,
            Duration::from_millis(300),
        ))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments.done == 1).await.unwrap();
    let _hung = server.gate("live.m3u8");

    let err = tokio::time::timeout(Duration::from_secs(5), job.wait())
        .await
        .expect("应在察觉停滞后一个目标时长内下结论，不等请求超时")
        .unwrap_err();

    assert!(
        matches!(
            err,
            Error::LiveStalled {
                track: 0,
                cause: StallReason::RefreshPending
            }
        ),
        "{err}"
    );
}

/// 刷新返回 503、要求 5 秒后再来：停滞时不提前再刷新，照服务器的意思等；这次失败本身即是结论，带上要求的等待。
#[tokio::test(flavor = "multi_thread")]
async fn a_stall_respects_the_servers_retry_after() {
    let dir = test_dir("live_stall_retry_after");
    let server = Server::start().await;
    put_long(&server, "", &[0]);
    server.put("live.m3u8", playlist(&[0], false));
    let job = engine()
        .start(live_request(
            server.url("live.m3u8"),
            &dir,
            Duration::from_millis(300),
        ))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments.done == 1).await.unwrap();
    server.status_retry_after("live.m3u8", StatusCode::SERVICE_UNAVAILABLE, "5");
    let before = server.hits("live.m3u8");

    let err = job.wait().await.unwrap_err();

    assert!(
        matches!(
            err,
            Error::LiveStalled {
                track: 0,
                cause: StallReason::RefreshFailed(RefreshReason::Http(HttpFailure {
                    kind: HttpErrorKind::Status(503),
                    ..
                })),
            }
        ),
        "{err}"
    );
    assert_eq!(err.retry_after(), Some(Duration::from_secs(5)));
    assert_eq!(server.hits("live.m3u8") - before, 1);
}
