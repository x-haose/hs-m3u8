//! 重试：服务器错误按重试策略重试、退避中的进度、超过等待上限的 Retry-After。

use std::time::Duration;

use axum::http::StatusCode;
use hs_m3u8_core::{Error, FetchFailure, HttpErrorKind, HttpFailure, Purpose};

use super::{expected_ts_a, put_ts_a};
use crate::server::Server;
use crate::{assert_output, engine, request, run, test_dir};

/// 500 按重试策略重试后成功。
#[tokio::test(flavor = "multi_thread")]
async fn retries_server_errors() {
    let dir = test_dir("retries");
    let server = Server::start().await;
    put_ts_a(&server, "index.m3u8", 1.0);
    server.fail("seg0.ts", 2);

    let output = run(request(server.url("index.m3u8"), &dir)).await.unwrap();

    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
    assert_eq!(server.hits("seg0.ts"), 3);
}

/// 退避等着重试时进度里列出这个请求：用途、失败原因、第几次重试、退避多久；退避结束（这里是取消）即去掉。
#[tokio::test(flavor = "multi_thread")]
async fn retry_waits_show_in_progress() {
    let dir = test_dir("retry_progress");
    let server = Server::start().await;
    put_ts_a(&server, "index.m3u8", 1.0);
    server.fail("seg1.ts", 1);
    let mut req = request(server.url("index.m3u8"), &dir);
    req.source.http.retry.base_delay = Duration::from_secs(3600);
    req.source.http.retry.max_delay = Duration::from_secs(3600);
    let job = engine().start(req).unwrap();
    let mut progress = job.control().progress();

    let retrying = progress
        .wait_for(|p| !p.retrying.is_empty())
        .await
        .unwrap()
        .retrying
        .clone();

    let [waiting] = &retrying[..] else {
        panic!("应只有一个在等：{retrying:?}");
    };
    let failure = FetchFailure::Http(HttpFailure {
        kind: HttpErrorKind::Status(500),
        retry_after: None,
    });
    assert_eq!(
        (waiting.purpose, &waiting.failure, waiting.retry),
        (Purpose::Segment, &failure, 1)
    );
    // 一小时乘以 [0.75, 1.25) 的抖动
    let jittered = Duration::from_secs(2700)..Duration::from_secs(4500);
    assert!(jittered.contains(&waiting.delay), "{:?}", waiting.delay);
    job.control().cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    assert!(progress.borrow().retrying.is_empty());
}

/// 服务器要求的等待超过重试的等待上限：不再重试，任务失败；错误可重试，服务器要求的等待包在分片的失败里（key
/// 的失败又包在分片的失败里），从最外层就能取到。
#[tokio::test(flavor = "multi_thread")]
async fn a_long_retry_after_is_reported_with_the_failure() {
    let dir = test_dir("retry_after");
    let server = Server::start().await;
    put_ts_a(&server, "index.m3u8", 1.0);
    server.status_retry_after("seg1.ts", StatusCode::TOO_MANY_REQUESTS, "3600");

    let err = run(request(server.url("index.m3u8"), &dir))
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Segment { sequence: 1, .. }), "{err}");
    assert!(err.retryable());
    assert_eq!(err.retry_after(), Some(Duration::from_secs(3600)));
    assert_eq!(server.hits("seg1.ts"), 1);

    server.status_retry_after("k", StatusCode::TOO_MANY_REQUESTS, "3600");
    server.put(
        "encrypted.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n\
         #EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let err = run(request(server.url("encrypted.m3u8"), &dir.join("key")))
        .await
        .unwrap_err();

    assert!(
        matches!(&err, Error::Segment { cause, .. } if matches!(**cause, Error::Key { .. })),
        "{err}"
    );
    assert!(err.retryable());
    assert_eq!(err.retry_after(), Some(Duration::from_secs(3600)));
}
