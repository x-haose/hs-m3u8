//! 直播录制：播放列表按请求次数逐步变化，模拟刷新、窗口滑动、结束、中断与服务器前后矛盾。
//! 样本 ts_long 为 4 个时间戳连续的 1 秒分片；TARGETDURATION 设为 0.1 秒，刷新间隔随之很短。

use std::path::Path;
use std::time::Duration;

use axum::http::StatusCode;
use hs_m3u8_core::{
    Error, HttpError, JobRequest, LiveEnd, LiveOptions, LiveReport, MissReason, Missed, Resume,
    Stage, StallCause, Url,
};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use crate::server::Server;
use crate::{
    assert_output, engine, expected, expected_long, fixture, request, run, test_dir, track,
};

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

/// 在 `dir` 下放 ts_long 的分片 `indices`（路径 `<dir>seg<i>.ts`）。
fn put_long(server: &Server, dir: &str, indices: &[u64]) {
    for i in indices {
        server.put(
            &format!("{dir}seg{i}.ts"),
            fixture(&format!("ts_long/seg{i}.ts")),
        );
    }
}

fn live_request(url: Url, dir: &Path, stall_timeout: Duration) -> JobRequest {
    let mut req = request(url, dir);
    req.live = Some(LiveOptions {
        max_duration: None,
        stall_timeout,
        resume: Resume::Continue,
    });
    req
}

const STALL: Duration = Duration::from_secs(2);

fn report(end: LiveEnd, sessions: u32, missed: Vec<Missed>) -> Option<LiveReport> {
    Some(LiveReport {
        end,
        sessions,
        missed,
    })
}

fn missed(first: u64, last: u64, reason: MissReason) -> Missed {
    Missed {
        session: 0,
        track: 0,
        first,
        last,
        reason,
    }
}

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

/// 两次刷新之间窗口滑过了分片 1：记为漏段，其余照常合并，时间线在该处留空。
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

/// 列出的分片 404：记为漏段，录制继续；相邻且原因相同的漏段合成一个区间。
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

/// 播放列表不再出现新分片且刷新开始 404：超过 stall_timeout 即结束，附上最后的刷新错误。
#[tokio::test(flavor = "multi_thread")]
async fn stalled_stream_ends_recording() {
    let dir = test_dir("live_stall");
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
    match output.live.unwrap().end {
        LiveEnd::Stalled {
            track: 0,
            cause: StallCause::RefreshFailed(e),
        } => assert!(e.contains("404"), "{e}"),
        other => panic!("{other:?}"),
    }
}

/// 录制被取消后，用同样的请求再次运行：继续录制，两次录制首尾相接合并。
/// 中断期间直播已结束（播放列表出现 ENDLIST），仍按直播收尾。
#[tokio::test(flavor = "multi_thread")]
async fn continue_after_interruption_appends_a_session() {
    let dir = test_dir("live_continue");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);

    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.progress();
    progress.wait_for(|p| p.segments_done == 2).await.unwrap();
    job.cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    assert!(!req.output.exists());

    server.put("live.m3u8", playlist(&[2, 3], true));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[2, 2]));
    assert_eq!(output.live, report(LiveEnd::EndList, 2, vec![]));
}

/// 只合并：不联网，直接合并已录到的分片。
#[tokio::test(flavor = "multi_thread")]
async fn merge_only_does_not_touch_the_network() {
    let dir = test_dir("live_merge_only");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);

    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.progress();
    progress.wait_for(|p| p.segments_done == 2).await.unwrap();
    job.cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));

    let hits = server.hits("live.m3u8");
    let mut merge = req;
    merge.live = merge.live.map(|live| LiveOptions {
        resume: Resume::MergeOnly,
        ..live
    });
    let output = run(merge).await.unwrap();
    assert_eq!(server.hits("live.m3u8"), hits);
    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    assert_eq!(output.live, report(LiveEnd::MergeOnly, 1, vec![]));
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
    assert_eq!(output.live, report(LiveEnd::MaxDuration, 1, vec![]));
    assert_eq!(server.hits("seg2.ts"), 0);
}

/// 视频加独立音频 rendition 的直播源：放 `video_segments`、`audio_segments` 个 fmp4_a 分片，
/// 两条媒体播放列表按 `video` / `audio` 依次返回（每项为 (分片数, init 签名, 是否结束)）。
fn split_source(
    server: &Server,
    video: &[(usize, u32, bool)],
    audio: &[(usize, u32, bool)],
) -> (Vec<&'static str>, Vec<&'static str>) {
    let video_segments = vec!["seg0.m4s", "seg1.m4s"];
    let audio_segments = vec!["seg0.m4s", "seg1.m4s", "seg2.m4s"];
    for (kind, names) in [("video", &video_segments), ("audio", &audio_segments)] {
        for name in ["init.mp4"].iter().chain(names.iter()) {
            server.put(
                &format!("{kind}/{name}"),
                fixture(&format!("fmp4_a/{kind}/{name}")),
            );
        }
    }
    let media = |kind: &str, &(count, sig, end): &(usize, u32, bool)| {
        let mut text = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MAP:URI=\"{kind}/init.mp4?sig={sig}\"\n"
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
        video.iter().map(|v| media("video", v)).collect(),
    );
    server.put_sequence(
        "audio.m3u8",
        audio.iter().map(|a| media("audio", a)).collect(),
    );
    server.put(
        "master.m3u8",
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=320x180,AUDIO=\"aud\"\nvideo.m3u8\n",
    );
    (video_segments, audio_segments)
}

/// fmp4_a 视频与音频各取前若干个分片、合成一组的期望输出。
fn expected_split(dir: &Path, video: &[&str], audio: &[&str]) -> Vec<u8> {
    let tracks = vec![
        track("fmp4_a/video", Some("init.mp4"), video),
        track("fmp4_a/audio", Some("init.mp4"), audio),
    ];
    expected(
        dir,
        &[Streams::Video, Streams::Audio],
        &[DiscontinuityGroup { tracks }],
    )
}

/// 视频与独立音频 rendition 各自刷新；init 段地址每次刷新都带不同的签名，按内容判定为同一个。
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
    assert_eq!(output.live.unwrap().end, LiveEnd::MaxDuration);
    assert_eq!(
        (server.hits("video/seg1.m4s"), server.hits("audio/seg1.m4s")),
        (0, 0)
    );
}

/// 音频轨的刷新在首次之后一直失败、视频轨照常出新分片：按音频轨停滞结束，原因里带刷新错误。
#[tokio::test(flavor = "multi_thread")]
async fn a_track_whose_refresh_keeps_failing_ends_the_recording() {
    let dir = test_dir("live_track_fails");
    let server = Server::start().await;
    // 视频前几次刷新没有新分片，之后出 seg1：它的「最近一次新分片」明显晚于音频轨
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
    match output.live.unwrap().end {
        LiveEnd::Stalled {
            track: 1,
            cause: StallCause::RefreshFailed(e),
        } => assert!(e.contains("404"), "{e}"),
        other => panic!("{other:?}"),
    }
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
        LiveEnd::SegmentChanged {
            track: 0,
            sequence: 1
        }
    );
}

/// 刷新中新出现的 init 段 404：引用它的分片记为漏段，不挡住其余内容与 ENDLIST。
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
    let not_found = MissReason::Failed(HttpError::Status(404));
    assert_eq!(
        output.live,
        report(LiveEnd::EndList, 1, vec![missed(1, 1, not_found)])
    );
    assert_eq!(server.hits("b/seg0.m4s"), 0);
}

/// key 取不到是系统性问题：任务失败，不当作漏段。
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
