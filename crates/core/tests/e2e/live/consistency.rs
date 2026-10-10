//! 服务器前后不一致：重定向、不连续段重新编号、旧缓存、序号回退、分片被替换、空的刷新。

use std::time::Duration;

use hs_m3u8_core::{LiveEnd, MissReason};

use super::{STALL, interrupt, live_request, missed, playlist, put_long, report, slow_retry};
use crate::server::Server;
use crate::{assert_output, expected_long, fixture, run, test_dir};

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

/// 判定期间暂存的第二份播放列表与第一份矛盾（同一序号换了分片）：会话定下后处理到它即结束录制，
/// 合并之前的部分，不卡住。
#[tokio::test(flavor = "multi_thread")]
async fn a_held_playlist_that_contradicts_ends_recording() {
    let dir = test_dir("live_held_inconsistent");
    let server = Server::start().await;
    put_long(&server, "", &[0, 1, 2, 3]);
    server.put("live.m3u8", playlist(&[0, 1], false));
    let mut req = live_request(server.url("live.m3u8"), &dir, STALL);
    interrupt(&req, |p| p.segments_done == 2).await;

    // 核对 seg1 先失败两次（退避 300 毫秒），其间刷新拿到序号 2 换了分片的一份
    req.source.http.retry = slow_retry(Duration::from_millis(300));
    server.fail("seg1.ts", 2);
    server.put("other2.ts", fixture("ts_long/seg2.ts"));
    let contradicting = "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MEDIA-SEQUENCE:0\n\
                         #EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXTINF:1,\nother2.ts\n";
    server.put_sequence(
        "live.m3u8",
        vec![playlist(&[0, 1, 2], false), contradicting.into()],
    );
    let output = tokio::time::timeout(Duration::from_secs(10), run(req))
        .await
        .expect("处理到矛盾的播放列表应结束录制")
        .unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2], &[3]));
    let end = LiveEnd::Inconsistent {
        track: 0,
        sequence: 2,
    };
    assert_eq!(output.live, report(end, 1, vec![]));
}
