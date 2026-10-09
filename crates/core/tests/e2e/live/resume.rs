//! 中断后续录：接着原会话或另起会话、补录、缺失的原因、跨会话的时长上限、选轨与来源的核对。

use std::time::Duration;

use hs_m3u8_core::{Error, JobType, LiveEnd, LiveOptions, MissReason, Resume, Url, WorkDirProblem};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use super::{
    STALL, expected_split_long, interrupt, live_request, missed, playlist, playlist_in, put_long,
    put_split_master, report,
};
use crate::server::Server;
use crate::{assert_output, engine, expected, expected_long, fixture, run, test_dir, track};

/// 中断期间窗口滑过了已录的部分（与之前没有重叠）：另起一个会话，与之前的首尾相接。
/// 中断期间直播已结束（播放列表出现 ENDLIST），仍按直播收尾。
#[tokio::test(flavor = "multi_thread")]
async fn disjoint_window_starts_a_new_session() {
    let dir = test_dir("resume_new_session");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 2).await;

    server.put("live.m3u8", playlist(&[2, 3], true));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[2, 2]));
    assert_eq!(output.live, report(LiveEnd::EndList, 2, vec![]));
}

/// 窗口仍含已录的分片：接着原会话录，重叠部分不重录，时间线连续成一组。
#[tokio::test(flavor = "multi_thread")]
async fn overlapping_window_continues_the_session() {
    let dir = test_dir("resume_overlap");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 2).await;

    server.put("live.m3u8", playlist(&[1, 2, 3], true));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
    // 重叠中最新的 seg1 再下载一次，与已存的比对内容
    assert_eq!((server.hits("seg0.ts"), server.hits("seg1.ts")), (1, 2));
}

/// EVENT 型（从头列出全部分片）：续录不把之前录过的再录一遍。
#[tokio::test(flavor = "multi_thread")]
async fn event_playlist_records_each_segment_once() {
    let dir = test_dir("resume_event");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 2).await;

    server.put("live.m3u8", playlist(&[0, 1, 2, 3], true));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 上次中断时没录到的分片还在窗口里：续录补上，成片里没有缺口。
#[tokio::test(flavor = "multi_thread")]
async fn segments_missing_from_the_session_are_refilled() {
    let dir = test_dir("resume_refill");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 3]);
    server.put("live.m3u8", playlist(&[0, 1, 2, 3], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 3 && p.segments_failed == 1).await;

    put_long(&server, "", &[2]);
    server.put("live.m3u8", playlist(&[0, 1, 2, 3], true));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
    assert_eq!(server.hits("seg0.ts"), 1);
}

/// 之前的运行里缺的分片已不在窗口中：报告为原因不明（原因随那次运行一起丢了）。
#[tokio::test(flavor = "multi_thread")]
async fn holes_from_an_earlier_run_are_unknown() {
    let dir = test_dir("resume_holes");
    let server = Server::start().await;
    put_long(&server, "", &[0, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1, 2, 3], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 3 && p.segments_failed == 1).await;

    server.put("live.m3u8", playlist(&[3], true));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 2, 3], &[3]));
    let missed = vec![missed(1, 1, MissReason::Unknown)];
    assert_eq!(output.live, report(LiveEnd::EndList, 1, missed));
}

/// max_duration 计入之前各会话录到的时长：中断前录了 1 秒，续录只再录 1 秒。
#[tokio::test(flavor = "multi_thread")]
async fn max_duration_counts_earlier_sessions() {
    let dir = test_dir("resume_max");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0], false));
    let mut req = live_request(server.url("live.m3u8"), &dir, STALL);
    req.live = Some(LiveOptions {
        max_duration: Some(Duration::from_secs(2)),
        stall_timeout: STALL,
        resume: Resume::Continue,
    });
    interrupt(&req, |p| p.segments_done == 1).await;

    server.put("live.m3u8", playlist(&[1, 2, 3], false));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1], &[1, 1]));
    assert_eq!(output.live, report(LiveEnd::DurationReached, 2, vec![]));
    assert_eq!(server.hits("seg2.ts"), 0);
}

/// 中断期间主播放列表多了一个更高的变体：续录按记录找回原来那个变体，不改录新变体。
#[tokio::test(flavor = "multi_thread")]
async fn continue_finds_the_recorded_variant() {
    let dir = test_dir("resume_variant");
    let server = Server::start().await;
    for i in 0..2 {
        server.put(
            &format!("lo/seg{i}.ts"),
            fixture(&format!("ts_long/seg{i}.ts")),
        );
        server.put(
            &format!("hi/seg{i}.ts"),
            fixture(&format!("ts_a/seg{i}.ts")),
        );
    }
    let media = |dir: &str, end: bool| {
        let mut text = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n".to_owned();
        for i in 0..2 {
            text += &format!("#EXTINF:1,\n{dir}/seg{i}.ts\n");
        }
        if end {
            text += "#EXT-X-ENDLIST\n";
        }
        text
    };
    server.put("lo.m3u8", media("lo", false));
    server.put("hi.m3u8", media("hi", true));
    let lo = "#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=160x90\nlo.m3u8\n";
    server.put("master.m3u8", format!("#EXTM3U\n{lo}"));
    let req = live_request(server.url("master.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 2).await;

    server.put(
        "master.m3u8",
        format!("#EXTM3U\n{lo}#EXT-X-STREAM-INF:BANDWIDTH=2,RESOLUTION=320x180\nhi.m3u8\n"),
    );
    server.put("lo.m3u8", media("lo", true));
    let output = run(req.clone()).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1], &[2]));
    assert_eq!(server.hits("hi.m3u8"), 0);

    // 记录的变体不在了：明确失败，不改录别的变体
    std::fs::remove_file(&req.output).unwrap();
    server.put("master.m3u8", format!("#EXTM3U\n{lo}"));
    server.put("lo.m3u8", media("lo", false));
    interrupt(&req, |p| p.segments_done == 2).await;
    server.put(
        "master.m3u8",
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2,RESOLUTION=320x180\nhi.m3u8\n",
    );
    let err = run(req).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::WorkDir {
                problem: WorkDirProblem::SelectionGone,
                ..
            }
        ),
        "{err}"
    );
}

/// 来源地址只有查询串（令牌）变了：窗口与已录的接得上才续录；接不上时无法确认是同一个直播，明确失败。
#[tokio::test(flavor = "multi_thread")]
async fn new_token_continues_only_when_the_window_overlaps() {
    let dir = test_dir("resume_token");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let with_token = |token: u32| {
        live_request(
            Url::parse(&format!("{}?token={token}", server.url("live.m3u8"))).unwrap(),
            &dir,
            STALL,
        )
    };
    interrupt(&with_token(1), |p| p.segments_done == 2).await;

    server.put("live.m3u8", playlist(&[3], false));
    let err = run(with_token(2)).await.unwrap_err();
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
    assert_eq!(server.hits("seg3.ts"), 0);

    server.put("live.m3u8", playlist(&[1, 2, 3], true));
    let output = run(with_token(3)).await.unwrap();
    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
}

/// 目录里是直播录制、中断期间直播结束了，续传的请求却没开启直播：按点播运行，报类型不符，目录保留。
#[tokio::test(flavor = "multi_thread")]
async fn a_live_directory_without_live_options_is_a_kind_mismatch() {
    let dir = test_dir("resume_without_live_options");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 2).await;

    server.put("live.m3u8", playlist(&[0, 1], true));
    let mut vod = req;
    vod.live = None;
    let err = run(vod).await.unwrap_err();

    assert!(
        matches!(
            err,
            Error::WorkDir {
                problem: WorkDirProblem::KindMismatch {
                    recorded: JobType::Live,
                    current: JobType::Vod
                },
                ..
            }
        ),
        "{err}"
    );
    assert!(dir.join("out.mp4.hsdl/job.json").exists());
}

/// 只合并时目录里是点播任务：明确报不是直播录制，不当作「没有录到」。
#[tokio::test(flavor = "multi_thread")]
async fn merge_only_rejects_a_vod_directory() {
    let dir = test_dir("resume_merge_vod");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_long/seg0.ts"));
    server.put(
        "vod.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n",
    );
    let req = live_request(server.url("vod.m3u8"), &dir, STALL);
    assert!(run(req.clone()).await.is_err());

    let mut merge = req;
    merge.live = merge.live.map(|live| LiveOptions {
        resume: Resume::MergeOnly,
        ..live
    });
    let err = run(merge).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::WorkDir {
                problem: WorkDirProblem::NotLiveRecording,
                ..
            }
        ),
        "{err}"
    );
}

/// 选轨偏好变了即是另一个任务：有已录内容时拒绝，不混进同一个目录。
#[tokio::test(flavor = "multi_thread")]
async fn changed_preference_is_rejected() {
    let dir = test_dir("resume_preference");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 2).await;

    let mut other = req;
    other.preference.audio_language = Some("en".into());
    let err = run(other).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::WorkDir {
                problem: WorkDirProblem::SourceMismatch,
                ..
            }
        ),
        "{err}"
    );
}

/// 中断期间编码器重启，序号与文件名都从 0 重来、内容换了：与已录的「同序号、同文件名」，但重新下载核对内容不同，
/// 另起会话录新内容，不跳过、也不混进旧的组。
#[tokio::test(flavor = "multi_thread")]
async fn encoder_restart_with_reused_names_starts_a_new_session() {
    let dir = test_dir("resume_restart");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1, 2, 3], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 4).await;

    // 重启后的新节目：seg0、seg1 是另两段内容
    server.put("seg0.ts", fixture("ts_long/seg2.ts"));
    server.put("seg1.ts", fixture("ts_long/seg3.ts"));
    server.put("live.m3u8", playlist(&[0, 1], true));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3, 2, 3], &[4, 2]));
    assert_eq!(output.live, report(LiveEnd::EndList, 2, vec![]));
}

/// 视频与独立音频：第二次运行视频接得上、音频接不上，另起会话、视频跳过已录的序号；第三次运行两轨都接得上会话 1，
/// 补录只补会话 1 起点之后的缺口，不把第二次跳过的序号当成没录完再录一遍。
#[tokio::test(flavor = "multi_thread")]
async fn skipped_segments_are_not_refilled_later() {
    let dir = test_dir("resume_after_then_continue");
    let server = Server::start().await;
    put_split_master(&server);
    put_long(&server, "v/", &[0, 1, 2, 3]);
    put_long(&server, "a/", &[0]);
    // 运行 1：窗口 [0,1]，音频 seg1 取不到
    server.put("video.m3u8", playlist_in("v/", &[0, 1], false));
    server.put("audio.m3u8", playlist_in("a/", &[0, 1], false));
    let req = live_request(server.url("master.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 3 && p.segments_failed == 1).await;

    // 运行 2：窗口 [1,2]；视频与会话 0 重叠，音频没有 → 会话 1，视频只录 2，音频录 1、2
    put_long(&server, "a/", &[1, 2, 3]);
    server.put("video.m3u8", playlist_in("v/", &[1, 2], false));
    server.put("audio.m3u8", playlist_in("a/", &[1, 2], false));
    interrupt(&req, |p| p.segments_done == 6).await;

    // 运行 3：窗口 [1,2,3] 并结束；两轨都接得上会话 1
    server.put("video.m3u8", playlist_in("v/", &[1, 2, 3], true));
    server.put("audio.m3u8", playlist_in("a/", &[1, 2, 3], true));
    let output = run(req).await.unwrap();

    let want = expected_split_long(&dir, &[(&[0, 1], &[0]), (&[2, 3], &[1, 2, 3])]);
    assert_output(&output, &want);
    assert_eq!(output.segments, 8);
}

/// 续录时第一份播放列表恰好没有分片（服务器正在重写）：等到有分片的再判定，不另起会话重录。
#[tokio::test(flavor = "multi_thread")]
async fn empty_first_playlist_does_not_decide_the_session() {
    let dir = test_dir("resume_empty_first");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 2).await;

    server.put_sequence(
        "live.m3u8",
        vec![playlist(&[], false), playlist(&[1, 2, 3], true)],
    );
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.live, report(LiveEnd::EndList, 1, vec![]));
}

/// 上次只落盘了 init 段、一个分片都没录到：换了令牌照常录，不要求「接得上」。
#[tokio::test(flavor = "multi_thread")]
async fn init_only_directory_accepts_a_new_token() {
    let dir = test_dir("resume_init_only");
    let server = Server::start().await;
    for name in ["init.mp4", "seg0.m4s", "seg1.m4s"] {
        server.put(
            &format!("video/{name}"),
            fixture(&format!("fmp4_a/video/{name}")),
        );
    }
    let media = |end: bool| {
        let mut text = String::from(
            "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MAP:URI=\"video/init.mp4\"\n\
             #EXTINF:1,\nvideo/seg0.m4s\n#EXTINF:1,\nvideo/seg1.m4s\n",
        );
        if end {
            text += "#EXT-X-ENDLIST\n";
        }
        text
    };
    server.put("live.m3u8", media(false));
    let with_token = |token: u32| {
        live_request(
            Url::parse(&format!("{}?token={token}", server.url("live.m3u8"))).unwrap(),
            &dir,
            STALL,
        )
    };
    let gate = server.gate("video/seg0.m4s");
    let job = engine().start(with_token(1)).unwrap();
    // init 段先于分片落盘；分片的请求到达时 init 段已在目录里
    gate.arrived.notified().await;
    job.cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    server.ungate("video/seg0.m4s");

    server.put("live.m3u8", media(true));
    let output = run(with_token(2)).await.unwrap();

    let tracks = vec![track(
        "fmp4_a/video",
        Some("init.mp4"),
        &["seg0.m4s", "seg1.m4s"],
    )];
    let want = expected(&dir, &[Streams::All], &[DiscontinuityGroup { tracks }]);
    assert_output(&output, &want);
}
