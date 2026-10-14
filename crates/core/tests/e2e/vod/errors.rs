//! 错误：说明里不出现凭据与令牌；下载前就能判定的失败。

use hs_m3u8_core::hls::Encryption;
use hs_m3u8_core::{Error, HttpErrorKind, RequestParts, Unsupported, Url};

use crate::server::Server;
use crate::{engine, fixture, request, run, test_dir};

/// 地址带用户名、密码与令牌时，错误信息与调试输出里都不出现它们（`unwrap` 与日志会打印后者）。
#[tokio::test(flavor = "multi_thread")]
async fn errors_do_not_reveal_credentials_or_tokens() {
    let dir = test_dir("redaction");
    let server = Server::start().await;
    let mut url = server.url("gone.m3u8?token=SECRET");
    url.set_username("user").unwrap();
    url.set_password(Some("PASSWD")).unwrap();
    let err = run(request(url, &dir)).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Http {
                kind: HttpErrorKind::Status(404),
                ..
            }
        ),
        "{err}"
    );

    let unsupported = Url::parse("ftp://user:PASSWD@h.example/x?token=SECRET").unwrap();
    let Err(invalid) = engine().start(request(unsupported, &dir)) else {
        panic!("ftp 地址应被拒绝");
    };

    // 探测结果、进度与可修改的请求的调试输出同样不带：界面与日志常直接打印它们
    server.put(
        "master.m3u8",
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8?token=SECRET\n",
    );
    server.put(
        "v.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts?token=SECRET\n#EXT-X-ENDLIST\n",
    );
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    let req = request(server.url("master.m3u8?token=SECRET"), &dir);
    let probe = engine().probe(&req.source).await.unwrap();
    let job = engine().start(req).unwrap();
    let progress = job.control().progress();
    job.wait().await.unwrap();
    let parts = RequestParts {
        url: Url::parse("https://user:PASSWD@h.example/x?token=SECRET").unwrap(),
        headers: vec![("cookie".into(), "SECRET".into())],
    };

    for text in [
        err.to_string(),
        format!("{err:?}"),
        invalid.to_string(),
        format!("{invalid:?}"),
        format!("{probe:?}"),
        format!("{:?}", *progress.borrow()),
        format!("{parts:?}"),
    ] {
        assert!(
            !text.contains("PASSWD") && !text.contains("SECRET"),
            "{text}"
        );
    }
}

/// 下载前就能判定的失败：输出已存在、不录制直播时遇到直播、独立音频用了 DRM（报出是哪个播放列表的哪一行）、
/// 各轨不连续段不一致。
#[tokio::test(flavor = "multi_thread")]
async fn rejected_before_download() {
    let dir = test_dir("rejected");
    let server = Server::start().await;

    let req = request(server.url("index.m3u8"), &dir);
    std::fs::write(req.output.target.mp4().unwrap(), b"").unwrap();
    assert!(matches!(
        run(req.clone()).await,
        Err(Error::OutputExists(_))
    ));
    std::fs::remove_file(req.output.target.mp4().unwrap()).unwrap();

    server.put(
        "live.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n",
    );
    let mut req = request(server.url("live.m3u8"), &dir);
    req.live = None;
    let err = run(req).await.unwrap_err();
    assert!(matches!(err, Error::LiveNotEnabled), "{err}");

    server.put(
        "video.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nv0.ts\n#EXT-X-DISCONTINUITY\n\
         #EXTINF:1,\nv1.ts\n#EXT-X-ENDLIST\n",
    );
    server.put(
        "audio.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n\
         #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
         #EXTINF:1,\na0.aac\n#EXT-X-ENDLIST\n",
    );
    server.put(
        "master.m3u8",
        "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"x\",URI=\"audio.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"a\"\nvideo.m3u8\n",
    );
    let err = run(request(server.url("master.m3u8"), &dir))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            Error::Unsupported(Unsupported::Encryption { url, line: 3, what: Encryption::Drm { .. } })
                if **url == server.url("audio.m3u8")
        ),
        "{err}"
    );

    server.put(
        "audio.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\na0.aac\n#EXTINF:1,\na1.aac\n#EXT-X-ENDLIST\n",
    );
    server.put(
        "master.m3u8",
        "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"x\",URI=\"audio.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"a\"\nvideo.m3u8\n",
    );
    let err = run(request(server.url("master.m3u8"), &dir))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            Error::Unsupported(Unsupported::DiscontinuityMismatch { track: 1, first, found })
                if *first == [0, 1] && *found == [0]
        ),
        "{err}"
    );
    // 下载前失败不留下任务目录与输出
    assert!(!dir.join("out.hsdl").exists());
    assert_eq!(server.hits("v0.ts") + server.hits("a0.aac"), 0);
}
