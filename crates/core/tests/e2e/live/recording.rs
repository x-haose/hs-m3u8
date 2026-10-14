//! 一次运行内的录制：刷新、窗口滑动、缺失、结束条件、多轨与 init 段。

use std::num::NonZeroUsize;
use std::time::Duration;

use axum::http::StatusCode;
use hs_m3u8_core::hls::SyntaxError;
use hs_m3u8_core::{
    Engine, Error, FetchError, HttpError, HttpFailure, LiveEnd, LiveOptions, MissReason, Missed,
    Mp4Unsupported, RefreshCause, Stage, Unsupported,
};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use super::{
    STALL, expected_split, live_request, missed, playlist, playlist_in, put_long, report,
    signed_fmp4_playlist, split_source,
};
use crate::server::Server;
use crate::{
    assert_output, engine, expected, expected_long, fixture, request, run, test_dir, track,
};

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
    let progress = job.control().progress();
    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
    let last = progress.borrow().clone();
    assert_eq!(
        (last.stage, last.segments_done, last.segments_total),
        (Stage::Done, 4, 4)
    );
}

/// 编码放不进 MP4（TS 里的 Opus 音频）：录到第一个分片就在进度里报出原因，录制照常进行（之后的分片照样录）；直播结束后
/// 合并 MP4 时以同一个原因失败。
#[tokio::test(flavor = "multi_thread")]
async fn unsupported_codecs_show_in_progress_while_recording_goes_on() {
    let dir = test_dir("live_unsupported_codec");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_opus/seg0.ts"));
    server.put("seg1.ts", fixture("ts_opus/seg0.ts"));
    server.put("live.m3u8", playlist(&[0], false));

    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir, Duration::MAX))
        .unwrap();
    let mut progress = job.control().progress();
    let reason = tokio::time::timeout(
        Duration::from_secs(10),
        progress.wait_for(|p| p.mp4_unsupported.is_some()),
    )
    .await
    .expect("录到第一个分片就应在进度里报出")
    .unwrap()
    .mp4_unsupported
    .clone();
    assert!(
        matches!(reason, Some(Mp4Unsupported::Codec { codec: "opus", .. })),
        "{reason:?}"
    );

    server.put("live.m3u8", playlist(&[0, 1], true));
    let failed = job.wait().await;
    assert!(
        matches!(
            failed,
            Err(Error::Unsupported(Unsupported::Mp4(
                Mp4Unsupported::Codec { codec: "opus", .. }
            )))
        ),
        "{failed:?}"
    );
    assert_eq!(server.hits("seg1.ts"), 1, "报出原因之后照常录");
}

/// 进入录制阶段时进度里各轨就有一项（还没有刷新失败）：这里 init 段还没取到，会话还没开始录。
#[tokio::test(flavor = "multi_thread")]
async fn refresh_errors_have_an_item_per_track_once_recording() {
    let dir = test_dir("live_refresh_errors_initial");
    let server = Server::start().await;
    server.put("v/init.mp4", fixture("fmp4_a/video/init.mp4"));
    server.put("v/seg0.m4s", fixture("fmp4_a/video/seg0.m4s"));
    server.put("live.m3u8", signed_fmp4_playlist("v", "1", 1, 1, false));
    let gate = server.gate("v/init.mp4");
    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir, STALL))
        .unwrap();
    let progress = job.control().progress();
    gate.arrived.notified().await;

    let snapshot = progress.borrow().clone();
    job.control().cancel();
    server.ungate("v/init.mp4");
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    assert_eq!(
        (snapshot.stage, snapshot.refresh_errors),
        (Stage::Recording, vec![None])
    );
}

/// 刷新失败（503）时进度里给出哪条轨、什么原因；恢复后清空，录制照常结束。
#[tokio::test(flavor = "multi_thread")]
async fn refresh_errors_show_in_progress_until_recovered() {
    let dir = test_dir("live_refresh_error");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0], false));
    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments_done == 1).await.unwrap();

    server.status("live.m3u8", StatusCode::SERVICE_UNAVAILABLE);
    let failed = tokio::time::timeout(
        Duration::from_secs(10),
        progress.wait_for(|p| p.refresh_errors.iter().any(Option::is_some)),
    )
    .await
    .expect("刷新失败应出现在进度里")
    .unwrap()
    .clone();
    let unavailable = RefreshCause::Http(HttpFailure {
        kind: HttpError::Status(503),
        retry_after: None,
    });
    assert_eq!(failed.refresh_errors, [Some(unavailable)]);
    // 已录 1 秒
    assert_eq!(failed.duration_us, 1_000_000);

    server.put("live.m3u8", playlist(&[0, 1], true));
    let output = job.wait().await.unwrap();
    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    assert_eq!(progress.borrow().refresh_errors, [None]);
}

/// 刷新拿到写了一半的播放列表（为空、末尾的 EXTINF 还没有 URI 行，或截在多字节字符中间）：等下次刷新，进度里
/// 给出原因；写完后照常录。
#[tokio::test(flavor = "multi_thread")]
async fn half_written_playlists_are_waited_out() {
    let dir = test_dir("live_half_written");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0], false));
    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments_done == 1).await.unwrap();

    let dangling = RefreshCause::Syntax {
        line: 6,
        kind: SyntaxError::InfoWithoutUri { tag: "EXTINF" },
    };
    let mut cut = (playlist(&[0], false) + "#EXTINF:1,\n第1段.ts\n").into_bytes();
    cut.truncate(cut.len() - "段.ts\n".len() + 1);
    let half_written = [
        (Vec::new(), RefreshCause::Empty),
        (
            (playlist(&[0], false) + "#EXTINF:1,\n").into_bytes(),
            dangling,
        ),
        (cut, RefreshCause::Truncated),
    ];
    for (text, cause) in half_written {
        server.put("live.m3u8", text);
        let shown = progress.wait_for(|p| p.refresh_errors == [Some(cause.clone())]);
        tokio::time::timeout(Duration::from_secs(10), shown)
            .await
            .expect("写了一半的播放列表应作为刷新失败出现在进度里")
            .unwrap();
    }

    server.put("live.m3u8", playlist(&[0, 1], true));
    let output = job.wait().await.unwrap();
    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
}

/// 两次刷新之间窗口滑过了分片 1、2：记为缺失（进度按分片计），其余照常合并，时间线在该处留空。
#[tokio::test(flavor = "multi_thread")]
async fn window_slide_is_reported_as_missed() {
    let dir = test_dir("live_slide");
    let server = Server::start().await;
    put_long(&server, "", &[0, 3]);
    server.put_sequence(
        "live.m3u8",
        vec![playlist(&[0], false), playlist(&[3], true)],
    );

    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir, STALL))
        .unwrap();
    let progress = job.control().progress();
    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 3], &[2]));
    let missed = vec![missed(1, 2, MissReason::Expired)];
    assert_eq!(output.live, report(LiveEnd::EndList, 1, missed));
    let last = progress.borrow().clone();
    assert_eq!(
        (
            last.segments_done,
            last.segments_total,
            last.segments_failed,
            last.segments_expired
        ),
        (2, 2, 0, 2)
    );
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
    let not_found = MissReason::Failed(FetchError::Http(HttpError::Status(404)));
    assert_eq!(
        output.live,
        report(LiveEnd::EndList, 1, vec![missed(1, 2, not_found)])
    );
}

/// fMP4 分片一直没写完（mdat 截断）：按重试策略取完次数后仍不完整，记为缺失，录制继续。
#[tokio::test(flavor = "multi_thread")]
async fn segments_still_incomplete_after_retries_are_missed() {
    let dir = test_dir("live_incomplete");
    let server = Server::start().await;
    for name in ["init.mp4", "seg0.m4s"] {
        server.put(
            &format!("video/{name}"),
            fixture(&format!("fmp4_a/video/{name}")),
        );
    }
    let seg1 = fixture("fmp4_a/video/seg1.m4s");
    server.put("video/seg1.m4s", seg1[..seg1.len() / 2].to_vec());
    server.put_sequence(
        "video.m3u8",
        vec![
            signed_fmp4_playlist("video", "0.1", 2, 0, false),
            signed_fmp4_playlist("video", "0.1", 2, 0, true),
        ],
    );

    let output = run(live_request(server.url("video.m3u8"), &dir, STALL))
        .await
        .unwrap();

    let tracks = vec![track("fmp4_a/video", Some("init.mp4"), &["seg0.m4s"])];
    assert_output(
        &output,
        &expected(&dir, &[Streams::All], &[DiscontinuityGroup { tracks }]),
    );
    let incomplete = MissReason::Failed(FetchError::Incomplete);
    assert_eq!(
        output.live,
        report(LiveEnd::EndList, 1, vec![missed(1, 1, incomplete)])
    );
    assert_eq!(server.hits("video/seg1.m4s"), 3);
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
    });

    let output = run(req).await.unwrap();

    assert_output(&output, &expected_split(&dir, &video[..1], &audio[..1]));
    assert_eq!(output.live.unwrap().end, Some(LiveEnd::DurationReached));
    assert_eq!(
        (server.hits("video/seg1.m4s"), server.hits("audio/seg1.m4s")),
        (0, 0)
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

    let job = engine()
        .start(live_request(server.url("live.m3u8"), &dir, STALL))
        .unwrap();
    let progress = job.control().progress();
    let output = job.wait().await.unwrap();

    // 取不到 init 段的分片也算在要下载的分片里
    let last = progress.borrow().clone();
    assert_eq!(
        (
            last.segments_done,
            last.segments_total,
            last.segments_failed,
            last.segments_expired
        ),
        (1, 2, 1, 0)
    );
    let tracks = vec![track("fmp4_a/video", Some("init.mp4"), &["seg0.m4s"])];
    let want = expected(&dir, &[Streams::All], &[DiscontinuityGroup { tracks }]);
    assert_output(&output, &want);
    let not_found = MissReason::InitFailed(FetchError::Http(HttpError::Status(404)));
    assert_eq!(
        output.live,
        report(LiveEnd::EndList, 1, vec![missed(1, 1, not_found)])
    );
    assert_eq!(server.hits("b/seg0.m4s"), 0);
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
    server.put("v.m3u8", signed_fmp4_playlist("video", "0.1", 1, 0, false));
    let gate = server.gate("video/seg0.m4s");
    let engine = Engine::new(NonZeroUsize::new(1).unwrap());
    let job = engine
        .start(live_request(server.url("v.m3u8"), &dir, STALL))
        .unwrap();
    let mut progress = job.control().progress();
    // seg0 占着唯一的名额；此后的刷新须拉新签名的 init 段
    gate.arrived.notified().await;
    server.put("v.m3u8", signed_fmp4_playlist("video", "0.1", 2, 1, true));
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

/// 直播的分片同样有总上限（直播那份名额）：唯一的名额被一个挂着的分片占着时，下一个分片不发出，挂着的返回后照常
/// 录完。
#[tokio::test(flavor = "multi_thread")]
async fn live_segments_share_the_live_permits() {
    let dir = test_dir("permits_live_bounded");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let engine = Engine::new(NonZeroUsize::new(1).unwrap());
    let gate = server.gate("seg0.ts");
    let live = engine
        .start(live_request(server.url("live.m3u8"), &dir, STALL))
        .unwrap();
    gate.arrived.notified().await;
    // 刷新照常进行（不占名额）：等下一次刷新的请求到达，下一个分片仍没有发出
    let refresh = server.gate("live.m3u8");
    refresh.arrived.notified().await;
    assert_eq!(server.hits("seg1.ts"), 0);

    server.put("live.m3u8", playlist(&[0, 1], true));
    server.ungate("live.m3u8");
    server.ungate("seg0.ts");
    let output = live.wait().await.unwrap();
    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
}

/// 引擎点播的名额被占满（这里唯一的名额被一个挂着的分片请求占着）时，直播的分片照常下载：直播另占一份名额，不会
/// 排在点播的大批下载之后、等到分片滑出窗口。
#[tokio::test(flavor = "multi_thread")]
async fn live_segments_do_not_wait_for_permits_held_by_vod_downloads() {
    let server = Server::start().await;
    put_long(&server, "vod/", &[0, 1]);
    server.put("vod.m3u8", playlist_in("vod/", &[0, 1], true));
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let engine = Engine::new(NonZeroUsize::new(1).unwrap());
    let gate = server.gate("vod/seg0.ts");
    let vod = engine
        .start(request(server.url("vod.m3u8"), &test_dir("permits_vod")))
        .unwrap();
    gate.arrived.notified().await;

    let live_dir = test_dir("permits_live");
    let live = engine
        .start(live_request(server.url("live.m3u8"), &live_dir, STALL))
        .unwrap();
    let mut progress = live.control().progress();
    tokio::time::timeout(
        Duration::from_secs(10),
        progress.wait_for(|p| p.segments_done == 2),
    )
    .await
    .expect("直播的分片不应等点播占着的名额")
    .unwrap();
    server.put("live.m3u8", playlist(&[0, 1], true));
    let output = live.wait().await.unwrap();
    assert_output(&output, &expected_long(&live_dir, &[0, 1], &[2]));

    server.ungate("vod/seg0.ts");
    vod.wait().await.unwrap();
}
