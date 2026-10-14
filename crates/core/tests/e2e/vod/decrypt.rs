//! 解密：显式的与按媒体序号推出的 IV、加密的 init 段、key 不对、自定义 key。

use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;

use axum::http::StatusCode;
use hs_m3u8_core::{Error, IntegrityFailure, KeyOverride, Stage, Unsupported};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use super::{TS_A, expected_ts_a};
use crate::server::Server;
use crate::{
    assert_output, encrypt, engine, expected, expected_long, fixture, hex, request, run, test_dir,
    track,
};

/// ts_long 的四个分片：第一个用显式 IV，其余三个换了 key 并用媒体序号推出的 IV。
/// 并发 4 时三个分片同时需要第二个 key，它只被拉取一次。
#[tokio::test(flavor = "multi_thread")]
async fn aes128_explicit_and_sequence_iv() {
    let dir = test_dir("aes128");
    let server = Server::start().await;
    let (key0, key1) = ([0x11; 16], [0x22; 16]);
    let iv0 = [0x33; 16];
    server.put("k0", key0);
    server.put("k1", key1);
    // MEDIA-SEQUENCE 为 7：分片 i 的序号为 7 + i
    let mut playlist = format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:7\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"k0\",IV=0x{}\n",
        hex(&iv0)
    );
    let mut served = 0;
    for i in 0..4u64 {
        let plain = fixture(&format!("ts_long/seg{i}.ts"));
        let cipher = match i {
            0 => encrypt(&plain, &key0, &iv0),
            _ => encrypt(&plain, &key1, &u128::from(7 + i).to_be_bytes()),
        };
        served += cipher.len();
        server.put(&format!("s{i}.ts"), cipher);
        if i == 1 {
            playlist += "#EXT-X-KEY:METHOD=AES-128,URI=\"k1\"\n";
        }
        playlist += &format!("#EXTINF:1,\ns{i}.ts\n");
    }
    let playlist = playlist + "#EXT-X-ENDLIST\n";
    served += playlist.len() + key0.len() + key1.len();
    server.put("index.m3u8", playlist);

    let mut req = request(server.url("index.m3u8"), &dir);
    req.concurrency = NonZeroUsize::new(4).unwrap();
    let job = engine().start(req).unwrap();
    let progress = job.control().progress();
    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.segments, 4);
    let last = progress.borrow().clone();
    assert_eq!(
        (last.stage, last.segments.done, last.segments.total),
        (Stage::Done, 4, 4)
    );
    // 读到的字节：播放列表、两个 key 与四个分片（密文），各一次
    assert_eq!(last.received, served as u64);
    assert_eq!(last.duration, Duration::from_secs(4));
    assert_eq!((server.hits("k0"), server.hits("k1")), (1, 1));
}

/// 加密的 init 段（RFC 8216 4.3.2.5）：EXT-X-MAP 之前的 EXT-X-KEY 写明 IV，init 段与分片都用它解密；没写 IV 时
/// init 段按全零 IV 解（同 hls.js），分片照常按媒体序号。init 段是 TS 的节目表时明确报不支持，不当作数据损坏。
#[tokio::test(flavor = "multi_thread")]
async fn encrypted_init_sections_are_decrypted_and_ts_ones_are_unsupported() {
    let dir = test_dir("encrypted_init");
    let server = Server::start().await;
    let (key, iv) = ([0x5A; 16], [0x11; 16]);
    for name in ["init.mp4", "seg0.m4s", "seg1.m4s"] {
        let plain = fixture(&format!("fmp4_a/video/{name}"));
        server.put(name, encrypt(&plain, &key, &iv));
    }
    server.put("key", key.to_vec());
    let playlist = |init: &str| {
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"key\",IV=0x11111111111111111111111111111111\n\
             #EXT-X-MAP:URI=\"{init}\"\n#EXTINF:1,\nseg0.m4s\n#EXTINF:1,\nseg1.m4s\n#EXT-X-ENDLIST\n"
        )
    };
    server.put("index.m3u8", playlist("init.mp4"));
    let want = |dir: &Path| {
        let segments = ["seg0.m4s", "seg1.m4s"];
        let tracks = vec![track("fmp4_a/video", Some("init.mp4"), &segments)];
        expected(dir, &[Streams::All], &[DiscontinuityGroup { tracks }])
    };

    let output = run(request(server.url("index.m3u8"), &dir)).await.unwrap();
    assert_output(&output, &want(&dir));

    let no_iv = test_dir("encrypted_init_no_iv");
    for (sequence, name) in [
        (None, "init.mp4"),
        (Some(0u128), "seg0.m4s"),
        (Some(1), "seg1.m4s"),
    ] {
        let plain = fixture(&format!("fmp4_a/video/{name}"));
        let iv = sequence.map_or([0; 16], u128::to_be_bytes);
        server.put(&format!("no_iv/{name}"), encrypt(&plain, &key, &iv));
    }
    server.put(
        "no_iv/index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"../key\"\n\
         #EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:1,\nseg0.m4s\n#EXTINF:1,\nseg1.m4s\n#EXT-X-ENDLIST\n",
    );
    let output = run(request(server.url("no_iv/index.m3u8"), &no_iv))
        .await
        .unwrap();
    assert_output(&output, &want(&no_iv));

    let ts = test_dir("ts_init");
    server.put("pat.ts", encrypt(&fixture("ts_a/seg0.ts"), &key, &iv));
    server.put("index.m3u8", playlist("pat.ts"));
    let err = run(request(server.url("index.m3u8"), &ts))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Unsupported(Unsupported::TsInit(url)) if **url == server.url("pat.ts")),
        "{err}"
    );
}

/// key 不对：解密后的填充或内容校验失败，不写盘。
#[tokio::test(flavor = "multi_thread")]
async fn wrong_key_fails_integrity() {
    let dir = test_dir("wrong_key");
    let server = Server::start().await;
    let iv = [0u8; 16];
    server.put("key", [0x01; 16]);
    server.put(
        "seg0.ts",
        encrypt(&fixture("ts_a/seg0.ts"), &[0x02; 16], &iv),
    );
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n\
         #EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );

    let err = run(request(server.url("index.m3u8"), &dir))
        .await
        .unwrap_err();
    match err {
        Error::Segment { cause, .. } => assert!(
            matches!(
                *cause,
                Error::Integrity {
                    kind: IntegrityFailure::Padding | IntegrityFailure::UnrecognizedSegment(_),
                    ..
                }
            ),
            "{cause}"
        ),
        other => panic!("{other}"),
    }
    let written = std::fs::read_dir(dir.join("out.hsdl/tracks/0"))
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(written, 0, "校验失败的分片不应写盘");
}

/// 自定义 key：分片用它与给定的 IV 解密；播放列表里的 key 地址（这里 404）不请求，分片自己的 IV
/// （由媒体序号推出）不用。
#[tokio::test(flavor = "multi_thread")]
async fn key_override_replaces_the_playlist_key_and_iv() {
    let dir = test_dir("key_override");
    let server = Server::start().await;
    let (key, iv) = ([0x3C; 16], [0x7E; 16]);
    for name in TS_A {
        server.put(name, encrypt(&fixture(&format!("ts_a/{name}")), &key, &iv));
    }
    server.status("key", StatusCode::NOT_FOUND);
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n\
         #EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n",
    );

    let mut req = request(server.url("index.m3u8"), &dir);
    req.key = Some(KeyOverride { key, iv: Some(iv) });
    let output = run(req).await.unwrap();

    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
    assert_eq!(server.hits("key"), 0);
}

/// EXT-X-KEY 没写 URI：探测标出要自定义 key；没有自定义 key 时下载前即明确报不支持、带播放列表地址（不是校验
/// 失败，不建任务目录），有了照常解密。
#[tokio::test(flavor = "multi_thread")]
async fn a_key_without_uri_needs_a_key_override() {
    let dir = test_dir("key_without_uri");
    let server = Server::start().await;
    let (key, iv) = ([0x3C; 16], [0x7E; 16]);
    for name in TS_A {
        server.put(name, encrypt(&fixture(&format!("ts_a/{name}")), &key, &iv));
    }
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=AES-128,IV=0x7E7E7E7E7E7E7E7E7E7E7E7E7E7E7E7E\n\
         #EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n",
    );
    let mut req = request(server.url("index.m3u8"), &dir);
    let probe = engine().probe(&req.source).await.unwrap();
    assert!(probe.tracks[0].encrypted && probe.tracks[0].needs_custom_key);

    let err = run(req.clone()).await.unwrap_err();
    assert!(
        matches!(&err, Error::Unsupported(Unsupported::KeyWithoutUri(url))
            if **url == server.url("index.m3u8")),
        "{err}"
    );
    assert!(!err.retryable());
    assert!(!dir.join("out.hsdl").exists());
    assert_eq!(server.hits("seg0.ts"), 0);

    req.key = Some(KeyOverride { key, iv: None });
    let output = run(req).await.unwrap();
    assert_output(&output, &expected_ts_a(&dir));
}
