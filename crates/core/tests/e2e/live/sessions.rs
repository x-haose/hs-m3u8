//! 续录时会话的接续：会话起点决定补录的范围、各轨最近的会话不同、换了令牌、已录满的轨。

use std::num::NonZeroUsize;
use std::time::Duration;

use axum::http::StatusCode;
use hs_m3u8_core::{Error, LiveEnd, LiveOptions, Url, WorkDirProblem};

use super::{
    STALL, expected_split, expected_split_long, interrupt, live_request, playlist, playlist_in,
    put_long, put_split_master, report, run_until_full, split_source,
};
use crate::server::Server;
use crate::{assert_output, engine, expected_long, fixture, run, test_dir};

/// 运行 2 音频窗口前移、另起会话 1，视频跳过录过的序号、还没刷到新分片就中断；运行 3 换了令牌，两轨各自接得上
/// 自己最近的会话（视频 0、音频 1）：接着会话 1 录，视频跳过录过的并入，不重录；改记新地址；两轨录到的都进成片。
#[tokio::test(flavor = "multi_thread")]
async fn tracks_whose_latest_sessions_differ_continue_the_latest_one() {
    let dir = test_dir("sessions_latest_differ");
    let server = Server::start().await;
    put_split_master(&server);
    put_long(&server, "v/", &[0, 1, 2, 3]);
    put_long(&server, "a/", &[0, 1, 2, 3]);
    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    let with_token = |token: u32| {
        let url = Url::parse(&format!("{}?token={token}", server.url("master.m3u8"))).unwrap();
        live_request(url, &dir, STALL)
    };
    interrupt(&with_token(1), |p| p.segments_done == 4).await;

    server.put("audio.m3u8", playlist_in("a/", &[2, 3], false));
    interrupt(&with_token(1), |p| p.segments_done == 6).await;
    let video_hits = server.hits("v/seg0.ts");

    server.put("video.m3u8", playlist_in("v/", &[0, 1, 2, 3], true));
    server.put("audio.m3u8", playlist_in("a/", &[2, 3], true));
    let output = run(with_token(2)).await.unwrap();

    assert_eq!(server.hits("v/seg0.ts"), video_hits);
    let want = expected_split_long(&dir, &[(&[0, 1], &[0, 1]), (&[2, 3], &[2, 3])]);
    assert_output(&output, &want);
    assert_eq!(output.live, report(LiveEnd::EndList, 2, vec![]));
}

/// 会话 0 录在编码器重启前（序号 100、101），会话 1 录重启后的 0、1；运行 3 视频接得上会话 1、音频窗口前移，
/// 另起会话 2，视频跳过 0、1 只录 2；运行 4 两轨都接得上会话 2：视频跳过的 0、1 不补进会话 2。
#[tokio::test(flavor = "multi_thread")]
async fn skipped_sequences_stay_skipped_after_an_encoder_restart() {
    let dir = test_dir("sessions_skip_after_restart");
    let server = Server::start().await;
    put_split_master(&server);
    for kind in ["v", "a"] {
        server.put(&format!("{kind}/seg100.ts"), fixture("ts_long/seg0.ts"));
        server.put(&format!("{kind}/seg101.ts"), fixture("ts_long/seg1.ts"));
        put_long(&server, &format!("{kind}/"), &[0, 1, 2, 3]);
    }
    server.put("video.m3u8", playlist_in("v/", &[100, 101], false));
    server.put("audio.m3u8", playlist_in("a/", &[100, 101], false));
    let req = live_request(server.url("master.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 4).await;

    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    interrupt(&req, |p| p.segments_done == 8).await;

    server.put("video.m3u8", playlist_in("v/", &[0, 1, 2], false));
    server.put("audio.m3u8", playlist_in("a/", &[2, 3], false));
    interrupt(&req, |p| p.segments_done == 11).await;
    let video_hits = server.hits("v/seg0.ts");

    server.put("video.m3u8", playlist_in("v/", &[0, 1, 2, 3], true));
    server.put("audio.m3u8", playlist_in("a/", &[2, 3], true));
    let output = run(req).await.unwrap();

    assert_eq!(server.hits("v/seg0.ts"), video_hits);
    let want = expected_split_long(
        &dir,
        &[(&[0, 1], &[0, 1]), (&[0, 1], &[0, 1]), (&[2, 3], &[2, 3])],
    );
    assert_output(&output, &want);
}

/// 序号 10 处换了分片（编码器重启后序号与旧的重叠），另起会话 1；并发下载时会话开头的 10 还在下载、后面的
/// 已完成就被中断。续录接着会话 1：10 不比会话 0 录过的最大序号大，仍补进会话 1。
#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_sessions_head_is_refilled_below_an_earlier_maximum() {
    let dir = test_dir("sessions_fresh_head");
    let server = Server::start().await;
    server.put("old-seg10.ts", fixture("ts_long/seg3.ts"));
    server.put("live.m3u8", playlist_in("old-", &[10], false));
    let mut req = live_request(server.url("live.m3u8"), &dir, STALL);
    req.concurrency = NonZeroUsize::new(2).unwrap();
    interrupt(&req, |p| p.segments_done == 1).await;

    for i in 0..4 {
        server.put(
            &format!("seg{}.ts", 10 + i),
            fixture(&format!("ts_long/seg{i}.ts")),
        );
    }
    server.put("live.m3u8", playlist(&[10, 11, 12], false));
    let gate = server.gate("seg10.ts");
    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.control().progress();
    gate.arrived.notified().await;
    progress.wait_for(|p| p.segments_done == 3).await.unwrap();
    job.control().cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    server.ungate("seg10.ts");

    server.put("live.m3u8", playlist(&[10, 11, 12, 13], true));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[3, 0, 1, 2, 3], &[1, 4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 2, vec![]));
}

/// 第一次运行音频一个分片都没录到（都 404），之后换了令牌续录：音频没有可核对的，不阻挡接续；视频核对一致，
/// 改记新地址，音频并入原会话从窗口起点录，两轨都进成片。
#[tokio::test(flavor = "multi_thread")]
async fn a_track_never_recorded_joins_the_session() {
    let dir = test_dir("sessions_never_recorded");
    let server = Server::start().await;
    put_split_master(&server);
    put_long(&server, "v/", &[0, 1, 2, 3]);
    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    let with_token = |token: u32| {
        let url = Url::parse(&format!("{}?token={token}", server.url("master.m3u8"))).unwrap();
        live_request(url, &dir, STALL)
    };
    interrupt(&with_token(1), |p| {
        p.segments_done == 2 && p.segments_failed == 2
    })
    .await;

    put_long(&server, "a/", &[0, 1, 2, 3]);
    server.put("video.m3u8", playlist_in("v/", &[0, 1, 2, 3], true));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1, 2, 3], true));
    let output = run(with_token(2)).await.unwrap();

    let want = expected_split_long(&dir, &[(&[0, 1, 2, 3], &[0, 1, 2, 3])]);
    assert_output(&output, &want);
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 视频已录满 max_duration、音频一个分片都没录到，换了令牌续录：这次只有音频要录，它没有可核对的，没有一条轨
/// 核对过内容，无法确认是同一个直播。
#[tokio::test(flavor = "multi_thread")]
async fn a_new_token_needs_a_verified_track() {
    let dir = test_dir("sessions_needs_verified");
    let server = Server::start().await;
    put_split_master(&server);
    put_long(&server, "v/", &[0, 1]);
    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    let with_token = |token: u32, max: Option<u64>| {
        let url = Url::parse(&format!("{}?token={token}", server.url("master.m3u8"))).unwrap();
        let mut req = live_request(url, &dir, STALL);
        req.live = Some(LiveOptions {
            max_duration: max.map(Duration::from_secs),
            ..req.live.unwrap()
        });
        req
    };
    interrupt(&with_token(1, None), |p| {
        p.segments_done == 2 && p.segments_failed == 2
    })
    .await;

    // 视频已录到 2 秒，满了；音频这次取得到
    put_long(&server, "a/", &[0, 1]);
    let err = run(with_token(2, Some(2))).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::WorkDir {
                problem: WorkDirProblem::SourceUnverified,
                ..
            }
        ),
        "{err}"
    );
}

/// 令牌 1 录满 max_duration；中断期间换成了另一个直播。令牌 2、同样的上限：各轨都已录满、这次不录，不改记地址，
/// 照常合并；之后令牌 2、不限时长：仍要核对，接不上报无法确认是同一个直播。
#[tokio::test(flavor = "multi_thread")]
async fn a_full_directory_keeps_the_recorded_url() {
    let dir = test_dir("sessions_full_keeps_url");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1, 2, 3], false));
    let with_token = |token: u32, max: Option<u64>| {
        let url = Url::parse(&format!("{}?token={token}", server.url("live.m3u8"))).unwrap();
        let mut req = live_request(url, &dir, STALL);
        req.output.keep_work_dir = true;
        req.output.overwrite = true;
        req.live = Some(LiveOptions {
            max_duration: max.map(Duration::from_secs),
            ..req.live.unwrap()
        });
        req
    };
    let first = run(with_token(1, Some(2))).await.unwrap();
    assert_eq!(first.live.unwrap().end, Some(LiveEnd::DurationReached));
    let job_json = dir.join("out.mp4.hsdl/job.json");
    let recorded = std::fs::read(&job_json).unwrap();

    server.put("seg0.ts", fixture("ts_long/seg2.ts"));
    server.put("seg1.ts", fixture("ts_long/seg3.ts"));
    server.put("live.m3u8", playlist(&[0, 1], true));
    let second = run(with_token(2, Some(2))).await.unwrap();
    assert_eq!(second.live.unwrap().end, Some(LiveEnd::DurationReached));
    assert_eq!(std::fs::read(&job_json).unwrap(), recorded);

    let err = run(with_token(2, None)).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::WorkDir {
                problem: WorkDirProblem::SourceUnverified,
                ..
            }
        ),
        "{err}"
    );
}

/// 视频此前已录满 max_duration，这次不录；它的 init 段现在取不到（403）。不拉它，音频接着原会话补满时长。
#[tokio::test(flavor = "multi_thread")]
async fn a_full_track_fetches_no_init() {
    let dir = test_dir("sessions_full_track_init");
    let server = Server::start().await;
    let (video, audio) = split_source(&server, &[(2, 0, false)], &[(3, 0, false)]);
    server.remove("audio/seg1.m4s");
    let mut req = live_request(server.url("master.m3u8"), &dir, STALL);
    req.live = Some(LiveOptions {
        max_duration: Some(Duration::from_secs(2)),
        ..req.live.unwrap()
    });
    // 第一次运行两轨都满 2 秒（音频 seg1 取不到也计入）即结束
    run_until_full(&req).await;

    server.put("audio/seg1.m4s", fixture("fmp4_a/audio/seg1.m4s"));
    server.status("video/init.mp4", StatusCode::FORBIDDEN);
    let init_hits = server.hits("video/init.mp4");
    let output = run(req).await.unwrap();

    assert_eq!(server.hits("video/init.mp4"), init_hits);
    assert_output(&output, &expected_split(&dir, &video[..2], &audio[..2]));
    assert_eq!(output.live, report(LiveEnd::DurationReached, 1, vec![]));
}
