//! 一次运行内的录制：刷新、窗口滑动、缺失、结束条件、停滞的分类与服务器前后矛盾。

use std::num::NonZeroUsize;
use std::time::Duration;

use axum::http::StatusCode;
use hs_m3u8_core::{
    Engine, Error, HttpError, LiveEnd, LiveOptions, MissReason, Missed, Resume, Stage, StallCause,
    StallError,
};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use super::{
    STALL, expected_split, live_request, missed, playlist, playlist_with_target, put_long,
    put_split_master, report, split_source,
};
use crate::server::Server;
use crate::{assert_output, engine, expected, expected_long, fixture, run, test_dir, track};

/// 刷新一次多一个分片、窗口前移，直到出现 EXT-X-ENDLIST。stall_timeout 大到无法表示时视为不限。
#[tokio::test(flavor = "multi_thread")]
async fn records_until_endlist() {
    let dir = test_dir("live_endlist");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put_sequence(
        "live.m3u8",
        vec![
            playlist(&[0, 1], false),
            playlist(&[0, 1, 2], false),
            playlist(&[1, 2, 3], false),
            playlist(&[1, 2, 3], true),
        ],
    );

    let req = live_request(server.url("live.m3u8"), &dir, Duration::MAX);
    let job = engine().start(req).unwrap();
    let progress = job.progress();
    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
    let last = *progress.borrow();
    assert_eq!(
        (last.stage, last.segments_done, last.segments_total),
        (Stage::Done, 4, 4)
    );
}

/// 两次刷新之间窗口滑过了分片 1：记为缺失，其余照常合并，时间线在该处留空。
#[tokio::test(flavor = "multi_thread")]
async fn window_slide_is_reported_as_missed() {
    let dir = test_dir("live_slide");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put_sequence(
        "live.m3u8",
        vec![playlist(&[0], false), playlist(&[2, 3], true)],
    );

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 2, 3], &[3]));
    let missed = vec![missed(1, 1, MissReason::Expired)];
    assert_eq!(output.live, report(LiveEnd::EndList, 1, missed));
}

/// 列出的分片 404：记为缺失，录制继续；相邻且原因相同的缺失合成一个区间。
#[tokio::test(flavor = "multi_thread")]
async fn unavailable_segments_are_reported_as_missed() {
    let dir = test_dir("live_404");
    let server = Server::start().await;
    put_long(&server, "", &[0, 3]);
    server.put_sequence(
        "live.m3u8",
        vec![
            playlist(&[0, 1, 2, 3], false),
            playlist(&[0, 1, 2, 3], true),
        ],
    );

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 3], &[2]));
    let not_found = MissReason::Failed(HttpError::Status(404));
    assert_eq!(
        output.live,
        report(LiveEnd::EndList, 1, vec![missed(1, 2, not_found)])
    );
}

/// 调用 stop：停止刷新，合并已录到的部分。
#[tokio::test(flavor = "multi_thread")]
async fn stop_merges_what_was_recorded() {
    let dir = test_dir("live_stop");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));

    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.progress();
    progress.wait_for(|p| p.segments_done == 2).await.unwrap();
    job.stop();
    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    assert_eq!(output.live, report(LiveEnd::Stopped, 1, vec![]));
}

/// 播放列表不再出现新分片，随后被删除（404）：超过 stall_timeout 即按直播已结束收尾。
#[tokio::test(flavor = "multi_thread")]
async fn removed_playlist_ends_recording() {
    let dir = test_dir("live_gone");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));

    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.progress();
    progress.wait_for(|p| p.segments_total == 2).await.unwrap();
    server.remove("live.m3u8");
    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    let end = LiveEnd::Stalled {
        track: 0,
        cause: StallCause::PlaylistGone(404),
    };
    assert_eq!(output.live, report(end, 1, vec![]));
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

/// 录到的时长达到 max_duration 即停止。
#[tokio::test(flavor = "multi_thread")]
async fn max_duration_limits_recording() {
    let dir = test_dir("live_max");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1, 2, 3], false));
    let mut req = live_request(server.url("live.m3u8"), &dir, STALL);
    req.live = Some(LiveOptions {
        max_duration: Some(Duration::from_secs(2)),
        stall_timeout: STALL,
        resume: Resume::Continue,
    });

    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    assert_eq!(output.live, report(LiveEnd::DurationReached, 1, vec![]));
    assert_eq!(server.hits("seg2.ts"), 0);
}

/// 视频与独立音频 rendition 各自刷新；init 段地址每次刷新都带不同的签名，内容相同，共用一个文件。
#[tokio::test(flavor = "multi_thread")]
async fn split_tracks_with_signed_init_urls() {
    let dir = test_dir("live_split");
    let server = Server::start().await;
    let (video, audio) = split_source(
        &server,
        &[(1, 0, false), (2, 1, true)],
        &[(2, 0, false), (3, 1, true)],
    );

    let output = run(live_request(server.url("master.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_split(&dir, &video, &audio));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
    assert_eq!(
        (server.hits("video/init.mp4"), server.hits("audio/init.mp4")),
        (2, 2)
    );
}

/// max_duration 对每条轨分别生效：音频轨不会录满整个窗口。
#[tokio::test(flavor = "multi_thread")]
async fn max_duration_applies_to_every_track() {
    let dir = test_dir("live_max_split");
    let server = Server::start().await;
    let (video, audio) = split_source(&server, &[(2, 0, false)], &[(3, 0, false)]);
    let mut req = live_request(server.url("master.m3u8"), &dir, STALL);
    req.live = Some(LiveOptions {
        max_duration: Some(Duration::from_secs(1)),
        stall_timeout: STALL,
        resume: Resume::Continue,
    });

    let output = run(req).await.unwrap();

    assert_output(&output, &expected_split(&dir, &video[..1], &audio[..1]));
    assert_eq!(output.live.unwrap().end, LiveEnd::DurationReached);
    assert_eq!(
        (server.hits("video/seg1.m4s"), server.hits("audio/seg1.m4s")),
        (0, 0)
    );
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
    let mut progress = job.progress();
    progress.wait_for(|p| p.segments_total >= 2).await.unwrap();
    server.remove("audio.m3u8");

    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_split(&dir, &video, &audio[..1]));
    let end = LiveEnd::Stalled {
        track: 1,
        cause: StallCause::PlaylistGone(404),
    };
    assert_eq!(output.live.unwrap().end, end);
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
    let mut progress = job.progress();
    progress.wait_for(|p| p.segments_done >= 3).await.unwrap();
    server.status("audio.m3u8", StatusCode::INTERNAL_SERVER_ERROR);

    let err = job.wait().await.unwrap_err();

    match &err {
        Error::LiveStalled {
            track: 1,
            cause: StallError::RefreshFailed(cause),
        } => assert!(
            matches!(
                **cause,
                Error::Http {
                    kind: HttpError::Status(500),
                    ..
                }
            ),
            "{cause}"
        ),
        other => panic!("{other}"),
    }
    assert!(err.retryable());
    assert!(dir.join("out.mp4.hsdl/job.json").exists());
}

/// 每次刷新都被 302 到不同的路径（CDN 调度），分片用相对地址：同一分片的地址随之变化，但文件名相同，不算服务器错误。
#[tokio::test(flavor = "multi_thread")]
async fn redirect_to_a_new_path_each_refresh_is_not_a_change() {
    let dir = test_dir("live_redirect");
    let server = Server::start().await;
    let windows = [
        playlist(&[0, 1], false),
        playlist(&[0, 1, 2], false),
        playlist(&[1, 2, 3], true),
    ];
    for (n, window) in windows.iter().enumerate() {
        put_long(&server, &format!("s{n}/"), &[0, 1, 2, 3]);
        server.put(&format!("s{n}/live.m3u8"), window.clone());
    }
    server.redirect_sequence(
        "live.m3u8",
        (0..windows.len())
            .map(|n| format!("s{n}/live.m3u8"))
            .collect(),
    );

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 服务器不写 EXT-X-DISCONTINUITY-SEQUENCE：带 DISCONTINUITY 的分片滑出窗口后编号整体变小，
/// 靠与上次重叠的分片校正，分组不变（0 | 1 2 3），不会把 3 归到 0 那一组。
#[tokio::test(flavor = "multi_thread")]
async fn renumbered_discontinuities_keep_their_groups() {
    let dir = test_dir("live_renumbered");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    let first = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXTINF:1,\nseg0.ts\n#EXT-X-DISCONTINUITY\n\
                 #EXTINF:1,\nseg1.ts\n#EXTINF:1,\nseg2.ts\n";
    let second = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MEDIA-SEQUENCE:2\n\
                  #EXTINF:1,\nseg2.ts\n#EXTINF:1,\nseg3.ts\n#EXT-X-ENDLIST\n";
    server.put_sequence("live.m3u8", vec![first.into(), second.into()]);

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[1, 3]));
}

/// 服务器不写 EXT-X-DISCONTINUITY-SEQUENCE、带 DISCONTINUITY 的分片滑出后编号变小，且窗口整体前移、
/// 与上次没有重叠：编号比已用过的小，新分片另起一组排在最后，不会编进更早的组而打乱顺序。
#[tokio::test(flavor = "multi_thread")]
async fn window_jump_with_renumbered_discontinuities_starts_a_new_group() {
    let dir = test_dir("live_jump");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 3]);
    let first = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXTINF:1,\nseg0.ts\n#EXT-X-DISCONTINUITY\n\
                 #EXTINF:1,\nseg1.ts\n";
    let second = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MEDIA-SEQUENCE:3\n\
                  #EXTINF:1,\nseg3.ts\n#EXT-X-ENDLIST\n";
    server.put_sequence("live.m3u8", vec![first.into(), second.into()]);

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 3], &[1, 1, 1]));
    let missed = vec![missed(2, 2, MissReason::Expired)];
    assert_eq!(output.live, report(LiveEnd::EndList, 1, missed));
}

/// CDN 返回了更旧的缓存窗口（多出一个更早的不连续段）：与上次重叠部分一致，不算矛盾，录制照常。
#[tokio::test(flavor = "multi_thread")]
async fn stale_older_window_is_not_inconsistent() {
    let dir = test_dir("live_stale");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    let first = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MEDIA-SEQUENCE:1\n\
                 #EXTINF:1,\nseg1.ts\n#EXTINF:1,\nseg2.ts\n";
    let stale = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXTINF:1,\nseg0.ts\n#EXT-X-DISCONTINUITY\n\
                 #EXTINF:1,\nseg1.ts\n#EXTINF:1,\nseg2.ts\n";
    let third = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MEDIA-SEQUENCE:1\n\
                 #EXTINF:1,\nseg1.ts\n#EXTINF:1,\nseg2.ts\n#EXTINF:1,\nseg3.ts\n#EXT-X-ENDLIST\n";
    server.put_sequence("live.m3u8", vec![first.into(), stale.into(), third.into()]);

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[1, 2, 3], &[3]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
    assert_eq!(server.hits("seg0.ts"), 0);
}

/// 媒体序号回退且与上次窗口不重叠，连续两次即认定编码器重启：结束录制，合并重启前的部分。
#[tokio::test(flavor = "multi_thread")]
async fn sequence_regression_ends_as_restarted() {
    let dir = test_dir("live_restart");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put_sequence(
        "live.m3u8",
        vec![playlist(&[2, 3], false), playlist(&[0], false)],
    );

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[2, 3], &[2]));
    assert_eq!(output.live.unwrap().end, LiveEnd::Restarted { track: 0 });
    assert_eq!(server.hits("seg0.ts"), 0);
}

/// 只回退一次（多为 CDN 的旧缓存）：不算重启，之后的正常窗口照常录。
#[tokio::test(flavor = "multi_thread")]
async fn a_single_regression_is_ignored() {
    let dir = test_dir("live_regress_once");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put_sequence(
        "live.m3u8",
        vec![
            playlist(&[1, 2], false),
            playlist(&[0], false),
            playlist(&[1, 2, 3], true),
        ],
    );

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[1, 2, 3], &[3]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 刷新偶尔拿到没有分片的播放列表、或空的响应体（服务器正在重写文件）：等下次刷新，不算回退或失败。
#[tokio::test(flavor = "multi_thread")]
async fn empty_refreshes_are_waited_out() {
    let dir = test_dir("live_empty");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put_sequence(
        "live.m3u8",
        vec![
            playlist(&[0, 1], false),
            playlist(&[], false),
            String::new(),
            playlist(&[0, 1], false),
            playlist(&[0, 1], false),
            playlist(&[0, 1, 2, 3], true),
        ],
    );

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 同一序号的分片在两次刷新之间换了文件：服务器错误，结束录制并合并之前的部分。
#[tokio::test(flavor = "multi_thread")]
async fn changed_segment_ends_recording() {
    let dir = test_dir("live_changed");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2]);
    let changed = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg2.ts\n";
    server.put_sequence(
        "live.m3u8",
        vec![playlist(&[0, 1], false), changed.to_owned()],
    );

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    assert_eq!(
        output.live.unwrap().end,
        LiveEnd::Inconsistent {
            track: 0,
            sequence: 1
        }
    );
}

/// 刷新中新出现的 init 段 404：引用它的分片记为缺失，不挡住其余内容与 ENDLIST。
#[tokio::test(flavor = "multi_thread")]
async fn unavailable_new_init_is_missed() {
    let dir = test_dir("live_init_404");
    let server = Server::start().await;
    for name in ["init.mp4", "seg0.m4s"] {
        server.put(
            &format!("a/{name}"),
            fixture(&format!("fmp4_a/video/{name}")),
        );
    }
    server.put("b/seg0.m4s", fixture("fmp4_b/video/seg0.m4s"));
    let first = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MAP:URI=\"a/init.mp4\"\n\
                 #EXTINF:1,\na/seg0.m4s\n";
    let second = format!(
        "{first}#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"b/init.mp4\"\n#EXTINF:1,\nb/seg0.m4s\n\
         #EXT-X-ENDLIST\n"
    );
    server.put_sequence("live.m3u8", vec![first.into(), second]);

    let output = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap();

    let tracks = vec![track("fmp4_a/video", Some("init.mp4"), &["seg0.m4s"])];
    let want = expected(&dir, &[Streams::All], &[DiscontinuityGroup { tracks }]);
    assert_output(&output, &want);
    let not_found = MissReason::InitFailed(HttpError::Status(404));
    assert_eq!(
        output.live,
        report(LiveEnd::EndList, 1, vec![missed(1, 1, not_found)])
    );
    assert_eq!(server.hits("b/seg0.m4s"), 0);
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
                cause: StallError::Unrecordable
            }
        ),
        "{err}"
    );
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

/// 引擎的在途名额被本任务一个慢分片占满时，刷新与刷新中新出现的 init 段（签名每次不同）仍照常进行。
#[tokio::test(flavor = "multi_thread")]
async fn refresh_does_not_wait_for_engine_permits() {
    let dir = test_dir("live_permits");
    let server = Server::start().await;
    for name in ["init.mp4", "seg0.m4s", "seg1.m4s"] {
        server.put(
            &format!("video/{name}"),
            fixture(&format!("fmp4_a/video/{name}")),
        );
    }
    let media = |count: usize, sig: u32, end: bool| {
        let mut text = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MAP:URI=\"video/init.mp4?sig={sig}\"\n"
        );
        for i in 0..count {
            text += &format!("#EXTINF:1,\nvideo/seg{i}.m4s\n");
        }
        if end {
            text += "#EXT-X-ENDLIST\n";
        }
        text
    };
    server.put("v.m3u8", media(1, 0, false));
    let gate = server.gate("video/seg0.m4s");
    let engine = Engine::new(NonZeroUsize::new(1).unwrap());
    let job = engine
        .start(live_request(server.url("v.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.progress();
    // seg0 占着唯一的名额；此后的刷新须拉新签名的 init 段
    gate.arrived.notified().await;
    server.put("v.m3u8", media(2, 1, true));
    progress.wait_for(|p| p.segments_total == 2).await.unwrap();
    server.ungate("video/seg0.m4s");

    let output = job.wait().await.unwrap();

    let tracks = vec![track(
        "fmp4_a/video",
        Some("init.mp4"),
        &["seg0.m4s", "seg1.m4s"],
    )];
    let want = expected(&dir, &[Streams::All], &[DiscontinuityGroup { tracks }]);
    assert_output(&output, &want);
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 视频轨有一个不连续段、音频轨没有：只合并各轨都有的组，视频多出的那组记为无法合并。
#[tokio::test(flavor = "multi_thread")]
async fn groups_missing_on_a_track_are_unmergeable() {
    let dir = test_dir("live_unmergeable");
    let server = Server::start().await;
    let (video, audio) = split_source(&server, &[(1, 0, false)], &[(3, 0, false), (3, 0, true)]);
    let video_text = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MAP:URI=\"video/init.mp4\"\n\
                      #EXTINF:1,\nvideo/seg0.m4s\n#EXT-X-DISCONTINUITY\n#EXTINF:1,\nvideo/seg1.m4s\n";
    server.put_sequence(
        "video.m3u8",
        vec![
            video_text.to_owned(),
            format!("{video_text}#EXT-X-ENDLIST\n"),
        ],
    );

    let output = run(live_request(server.url("master.m3u8"), &dir, STALL))
        .await
        .unwrap();

    assert_output(&output, &expected_split(&dir, &video[..1], &audio));
    assert_eq!(
        output.live,
        report(
            LiveEnd::EndList,
            1,
            vec![Missed {
                session: 0,
                track: 0,
                first: 1,
                last: 1,
                reason: MissReason::Unmergeable
            }]
        )
    );
}

/// key 取不到是系统性问题：任务失败，不当作缺失。
#[tokio::test(flavor = "multi_thread")]
async fn key_failure_fails_the_task() {
    let dir = test_dir("live_key");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put(
        "live.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXTINF:1,\nseg0.ts\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:1,\nseg1.ts\n",
    );

    let err = run(live_request(server.url("live.m3u8"), &dir, STALL))
        .await
        .unwrap_err();

    match err {
        Error::Segment {
            sequence: 1, cause, ..
        } => {
            assert!(matches!(*cause, Error::Key { .. }), "{cause}")
        }
        other => panic!("{other}"),
    }
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
    let mut progress = job.progress();
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
