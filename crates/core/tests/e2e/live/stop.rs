//! 停止录制（[`hs_m3u8_core::Job::stop`]）：会话定下之前立即结束；定下之后把已拉到的播放列表处理完、
//! 已列出的分片下完，再合并。

use std::time::Duration;

use hs_m3u8_core::LiveEnd;

use super::{
    STALL, expected_split, interrupt, live_request, playlist, put_long, report, split_source,
};
use crate::server::Server;
use crate::{assert_output, engine, expected_long, test_dir};

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

/// 视频与音频都要先拉 init 段，启动后立即 stop：两轨首次拉到的播放列表都照常处理，每次都两轨都进成片。
#[tokio::test(flavor = "multi_thread")]
async fn stop_right_after_start_records_every_track() {
    let server = Server::start().await;
    let (video, audio) = split_source(&server, &[(2, 0, false)], &[(3, 0, false)]);
    for i in 0..10 {
        let dir = test_dir(&format!("stop_at_start_split_{i}"));
        let job = engine()
            .start(live_request(server.url("master.m3u8"), &dir, STALL))
            .unwrap();
        job.stop();
        let output = job.wait().await.unwrap();
        assert_output(&output, &expected_split(&dir, &video, &audio));
        assert_eq!(output.live, report(LiveEnd::Stopped, 1, vec![]));
    }
}

/// 启动后立即 stop：首次拉到的播放列表照常处理，每次都录到同样的分片。
#[tokio::test(flavor = "multi_thread")]
async fn stop_right_after_start_records_the_first_playlist() {
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    for i in 0..10 {
        let dir = test_dir(&format!("stop_at_start_{i}"));
        let job = engine()
            .start(live_request(server.url("live.m3u8"), &dir, STALL))
            .unwrap();
        job.stop();
        let output = job.wait().await.unwrap();
        assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
        assert_eq!(output.live, report(LiveEnd::Stopped, 1, vec![]));
    }
}
