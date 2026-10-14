//! 源站给出不完整的分片：没写完的重新取，最后一个 box 的长度写 0 的改写成实际长度。

use hs_m3u8_core::{Error, IntegrityFailure};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use super::{TS_A, expected_ts_a};
use crate::server::Server;
use crate::{assert_output, encrypt, expected, fixture, request, run, test_dir, track};

/// 加密的 TS 分片没写完，解出的是截在包中间的 TS：密文截在整块之内或整块处（解密失败），或截断的 TS 被完整地加密
/// （解密成功，如填充恰好像合法的）。都重新取，取到完整的照常合并。源站对分片返回错误页面、或完整的分片后面多了字节
/// 时，长度同样不是 16 的整数倍，而解出的不是截断的 TS，报密文长度不对，不重试。
#[tokio::test(flavor = "multi_thread")]
async fn a_truncated_encrypted_segment_is_fetched_again() {
    let dir = test_dir("truncated_encrypted");
    let server = Server::start().await;
    let (key, iv) = ([0x01; 16], [0u8; 16]);
    server.put("key", key);
    let plain = TS_A.map(|name| fixture(&format!("ts_a/{name}")));
    let [seg0, seg1] = plain.each_ref().map(|data| encrypt(data, &key, &iv));
    // 密文是 16 的整数倍，截到一半再多 5 字节必不是；截在整块处时避开 TS 包的边界
    let unaligned = seg0[..seg0.len() / 2 + 5].to_vec();
    let mut at_block = seg0.len() / 2 / 16 * 16;
    if at_block % 188 == 0 {
        at_block += 16;
    }
    let aligned = seg0[..at_block].to_vec();
    server.put_sequence("seg0.ts", vec![unaligned, aligned, seg0.clone()]);
    let cut_plain = encrypt(&plain[1][..plain[1].len() - 100], &key, &iv);
    server.put_sequence("seg1.ts", vec![cut_plain, seg1]);
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"key\",IV=0x00000000000000000000000000000000\n\
         #EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n",
    );

    let output = run(request(server.url("index.m3u8"), &dir)).await.unwrap();
    assert_output(&output, &expected_ts_a(&dir));
    assert_eq!(server.hits("seg0.ts"), 3);
    assert_eq!(server.hits("seg1.ts"), 2);

    std::fs::remove_file(dir.join("out.mp4")).unwrap();
    let mut hits = 3;
    for body in [
        b"<html>not found</html>".to_vec(),
        [seg0.as_slice(), b"\r\n"].concat(),
    ] {
        server.put("seg0.ts", body.clone());
        let err = run(request(server.url("index.m3u8"), &dir))
            .await
            .unwrap_err();
        assert!(!err.retryable(), "{err}");
        match err {
            Error::Segment { cause, .. } => assert!(
                matches!(
                    *cause,
                    Error::Integrity {
                        kind: IntegrityFailure::CipherLength(len),
                        ..
                    } if len == body.len()
                ),
                "{cause}"
            ),
            other => panic!("{other}"),
        }
        hits += 1;
        assert_eq!(server.hits("seg0.ts"), hits);
    }
}

/// 源站给出没写完的 TS 分片（截在包中间，Content-Length 与截断后的长度一致）：重新取，取到完整的照常合并。
#[tokio::test(flavor = "multi_thread")]
async fn a_truncated_ts_segment_is_fetched_again() {
    let dir = test_dir("truncated_ts");
    let server = Server::start().await;
    let seg0 = fixture("ts_a/seg0.ts");
    server.put_sequence("seg0.ts", vec![seg0[..seg0.len() - 100].to_vec(), seg0]);
    server.put("seg1.ts", fixture("ts_a/seg1.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n",
    );

    let output = run(request(server.url("index.m3u8"), &dir)).await.unwrap();
    assert_output(&output, &expected_ts_a(&dir));
    assert_eq!(server.hits("seg0.ts"), 2);
}

/// 源站给出没写完的 fMP4 分片（Content-Length 与截断后的长度一致）：截在 mdat 里、截在 mdat 之前（顶层 box 都完整，
/// 靠 trun 声明的样本数据看出）或是空的，都重新取，取到完整的照常合并；一直没写完时按重试策略取完次数后失败（可重试），
/// 不写盘，不合并出把下一个分片的字节读成这一片样本的 MP4。
#[tokio::test(flavor = "multi_thread")]
async fn a_truncated_fmp4_segment_is_fetched_again() {
    let dir = test_dir("truncated_fmp4");
    let server = Server::start().await;
    let seg0 = fixture("fmp4_a/video/seg0.m4s");
    let truncated = seg0[..seg0.len() * 9 / 10].to_vec();
    let before_mdat = seg0[..last_box(&seg0)].to_vec();
    server.put("init.mp4", fixture("fmp4_a/video/init.mp4"));
    server.put_sequence("seg0.m4s", vec![truncated.clone(), before_mdat, seg0]);
    server.put_sequence(
        "seg1.m4s",
        vec![Vec::new(), fixture("fmp4_a/video/seg1.m4s")],
    );
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"init.mp4\"\n\
         #EXTINF:1,\nseg0.m4s\n#EXTINF:1,\nseg1.m4s\n#EXT-X-ENDLIST\n",
    );

    let output = run(request(server.url("index.m3u8"), &dir)).await.unwrap();
    let tracks = vec![track(
        "fmp4_a/video",
        Some("init.mp4"),
        &["seg0.m4s", "seg1.m4s"],
    )];
    assert_output(
        &output,
        &expected(&dir, &[Streams::All], &[DiscontinuityGroup { tracks }]),
    );
    assert_eq!(server.hits("seg0.m4s"), 3);
    assert_eq!(server.hits("seg1.m4s"), 2);

    std::fs::remove_file(dir.join("out.mp4")).unwrap();
    server.put("seg0.m4s", truncated);
    let err = run(request(server.url("index.m3u8"), &dir))
        .await
        .unwrap_err();
    assert!(err.retryable(), "{err}");
    match err {
        Error::Segment { cause, .. } => {
            assert!(matches!(*cause, Error::Incomplete { .. }), "{cause}")
        }
        other => panic!("{other}"),
    }
    assert_eq!(server.hits("seg0.m4s"), 3 + 3);
}

/// fMP4 数据里最后一个顶层 box 的起点（各 box 的长度都写在 32 位字段里）。
fn last_box(data: &[u8]) -> usize {
    let mut at = 0;
    loop {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        if at + size == data.len() {
            return at;
        }
        at += size;
    }
}

/// 源站把 fMP4 分片与 init 段最后一个 box（mdat、moov）的长度写为 0（到文件末尾）：下载时改写成实际长度，合并出的 MP4
/// 与原样的样本合并的一致。不改写时合并把后面的分片读成这个 box 的一部分。分片第一次取到时还没写完：靠长度看不出，按
/// moof 里 trun 声明的样本数据看出，重新取。
#[tokio::test(flavor = "multi_thread")]
async fn a_last_box_sized_to_the_end_is_rewritten() {
    let dir = test_dir("fmp4_box_to_end");
    let server = Server::start().await;
    let to_end = |path: &str| {
        let mut data = fixture(path);
        let last = last_box(&data);
        data[last..last + 4].fill(0);
        data
    };
    let seg0 = to_end("fmp4_a/video/seg0.m4s");
    let unfinished = seg0[..seg0.len() - 10].to_vec();
    server.put("init.mp4", to_end("fmp4_a/video/init.mp4"));
    server.put_sequence("seg0.m4s", vec![unfinished, seg0]);
    server.put("seg1.m4s", fixture("fmp4_a/video/seg1.m4s"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"init.mp4\"\n\
         #EXTINF:1,\nseg0.m4s\n#EXTINF:1,\nseg1.m4s\n#EXT-X-ENDLIST\n",
    );

    let output = run(request(server.url("index.m3u8"), &dir)).await.unwrap();

    let tracks = vec![track(
        "fmp4_a/video",
        Some("init.mp4"),
        &["seg0.m4s", "seg1.m4s"],
    )];
    let want = expected(&dir, &[Streams::All], &[DiscontinuityGroup { tracks }]);
    assert_output(&output, &want);
    assert_eq!(server.hits("seg0.m4s"), 2);
}
