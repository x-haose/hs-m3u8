//! 判定期间发起、会话定下之后才返回的刷新：暂存的播放列表照常准备、处理，不被它挡住或越过；结束时不等它。
//!
//! 场景都是视频与音频（fMP4）各录了 seg0 后中断。续录时视频首次拉到 `video/first.m3u8`（它的候选），之后的刷新
//! 依次被重定向到 `video/later.m3u8` 与 `video/final.m3u8`；later 被挡住，直到会话定下之后才放行。

use std::time::Duration;

use axum::http::StatusCode;
use hs_m3u8_core::{JobRequest, LiveEnd, StallCause};

use super::{expected_split, interrupt, live_request, report, split_source};
use crate::server::Server;
use crate::{assert_output, engine, run, test_dir};

/// 视频的直播窗口，序号 `first..=last`（`last` 为 None 时没有分片），init 段签名为 `sig`；地址相对于 `video/`。
fn video_window(first: u64, last: Option<u64>, sig: u32, end: bool) -> String {
    let mut text = format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MEDIA-SEQUENCE:{first}\n\
         #EXT-X-MAP:URI=\"init.mp4?sig={sig}\"\n"
    );
    for i in last.map_or(first..first, |last| first..last + 1) {
        text += &format!("#EXTINF:1,\nseg{i}.m4s\n");
    }
    if end {
        text += "#EXT-X-ENDLIST\n";
    }
    text
}

/// 录好第一次运行，放好续录时视频的三份播放列表（`later` 为 None 时它返回 404）与音频的播放列表。
async fn resume_with_late_refresh(
    server: &Server,
    name: &str,
    stall: Duration,
    first: String,
    later: Option<String>,
    last: String,
    audio: String,
) -> JobRequest {
    split_source(server, &[(1, 1, false)], &[(1, 1, false)]);
    let req = live_request(server.url("master.m3u8"), &test_dir(name), stall);
    interrupt(&req, |p| p.segments_done == 2).await;

    server.put("video/first.m3u8", first);
    match later {
        Some(later) => server.put("video/later.m3u8", later),
        None => server.status("video/later.m3u8", StatusCode::NOT_FOUND),
    }
    server.put("video/final.m3u8", last);
    server.redirect_sequence(
        "video.m3u8",
        vec![
            "video/first.m3u8".into(),
            "video/later.m3u8".into(),
            "video/final.m3u8".into(),
        ],
    );
    server.put("audio.m3u8", audio);
    req
}

fn audio_window(count: usize, end: bool) -> String {
    super::signed_fmp4_playlist("audio", "0.1", count, 1, end)
}

/// 晚到的那份是唯一列出 seg1 的，且 init 段换了签名（要先拉到才能处理）：暂存后准备好、录进成片。
#[tokio::test(flavor = "multi_thread")]
async fn a_late_refresh_with_a_new_init_is_prepared_and_recorded() {
    let server = Server::start().await;
    let req = resume_with_late_refresh(
        &server,
        "late_refresh_new_init",
        super::STALL,
        video_window(0, Some(0), 1, false),
        Some(video_window(1, Some(1), 2, false)),
        video_window(2, None, 2, true),
        audio_window(2, true),
    )
    .await;
    let check = server.gate("video/seg0.m4s");
    let later = server.gate("video/later.m3u8");
    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.control().progress();
    check.arrived.notified().await;
    later.arrived.notified().await;
    server.ungate("video/seg0.m4s");
    // 音频的 seg1 排入下载，说明会话已定下
    progress.wait_for(|p| p.segments_total >= 3).await.unwrap();
    server.ungate("video/later.m3u8");
    let output = tokio::time::timeout(Duration::from_secs(10), job.wait())
        .await
        .expect("晚到的刷新应照常处理")
        .unwrap();

    let dir = req.output.path.parent().unwrap();
    let want = expected_split(dir, &["seg0.m4s", "seg1.m4s"], &["seg0.m4s", "seg1.m4s"]);
    assert_output(&output, &want);
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 会话定下时视频的刷新还挂着：暂存的候选照常处理（录到 seg1）；随后 stop 不等那次刷新，按停止收尾。
#[tokio::test(flavor = "multi_thread")]
async fn stop_after_the_decision_does_not_wait_for_a_late_refresh() {
    let server = Server::start().await;
    let req = resume_with_late_refresh(
        &server,
        "late_refresh_stop",
        super::STALL,
        video_window(0, Some(1), 1, false),
        Some(video_window(1, Some(1), 1, false)),
        video_window(1, Some(1), 1, false),
        audio_window(2, false),
    )
    .await;
    let check = server.gate("video/seg0.m4s");
    let later = server.gate("video/later.m3u8");
    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.control().progress();
    check.arrived.notified().await;
    later.arrived.notified().await;
    server.ungate("video/seg0.m4s");
    // 两轨的 seg1 都排入下载：视频暂存的候选没有被挂着的刷新挡住
    tokio::time::timeout(
        Duration::from_secs(10),
        progress.wait_for(|p| p.segments_total == 4),
    )
    .await
    .expect("暂存的候选应在会话定下时处理")
    .unwrap();
    job.control().stop();
    let output = tokio::time::timeout(Duration::from_secs(10), job.wait())
        .await
        .expect("stop 不应等挂着的刷新")
        .unwrap();
    server.ungate("video/later.m3u8");

    let dir = req.output.path.parent().unwrap();
    let want = expected_split(dir, &["seg0.m4s", "seg1.m4s"], &["seg0.m4s", "seg1.m4s"]);
    assert_output(&output, &want);
    assert_eq!(output.live, report(LiveEnd::Stopped, 1, vec![]));
}

/// 晚到的刷新失败（404），之后的刷新窗口已滑过 seg1：暂存的候选仍先处理，seg1 照常录到，不被越过。
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_late_refresh_does_not_skip_held_playlists() {
    let server = Server::start().await;
    let req = resume_with_late_refresh(
        &server,
        "late_refresh_failed",
        super::STALL,
        video_window(0, Some(1), 1, false),
        None,
        video_window(2, None, 1, true),
        audio_window(2, true),
    )
    .await;
    let check = server.gate("video/seg0.m4s");
    let later = server.gate("video/later.m3u8");
    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.control().progress();
    check.arrived.notified().await;
    later.arrived.notified().await;
    server.ungate("video/seg0.m4s");
    progress.wait_for(|p| p.segments_total >= 3).await.unwrap();
    server.ungate("video/later.m3u8");
    let output = tokio::time::timeout(Duration::from_secs(10), job.wait())
        .await
        .expect("刷新失败后应照常结束")
        .unwrap();

    let dir = req.output.path.parent().unwrap();
    let want = expected_split(dir, &["seg0.m4s", "seg1.m4s"], &["seg0.m4s", "seg1.m4s"]);
    assert_output(&output, &want);
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 判定期间音频一直没有分片而停滞，这时视频的刷新挂着：按直播已结束收尾，视频暂存的候选照常录到，
/// 不等那次刷新。
#[tokio::test(flavor = "multi_thread")]
async fn a_stall_while_deciding_does_not_wait_for_a_late_refresh() {
    let server = Server::start().await;
    let req = resume_with_late_refresh(
        &server,
        "late_refresh_stall",
        Duration::from_millis(300),
        video_window(0, Some(1), 1, false),
        Some(video_window(1, Some(1), 1, false)),
        video_window(1, Some(1), 1, false),
        audio_window(0, false),
    )
    .await;
    let _later = server.gate("video/later.m3u8");
    let output = tokio::time::timeout(Duration::from_secs(10), run(req.clone()))
        .await
        .expect("停滞收尾不应等挂着的刷新")
        .unwrap();

    let dir = req.output.path.parent().unwrap();
    let want = expected_split(dir, &["seg0.m4s", "seg1.m4s"], &["seg0.m4s"]);
    assert_output(&output, &want);
    let end = LiveEnd::Stalled {
        track: 1,
        cause: StallCause::NoNewSegments,
    };
    assert_eq!(output.live, report(end, 1, vec![]));
}
