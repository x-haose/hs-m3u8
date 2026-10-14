//! HTTP 请求：证书校验失败与重定向失败不重试，原因写在错误里；重定向后保留调用方给的 Referer；HTTP 库用不上的代理
//! 报参数错误；Cookie 与 Authorization 的发送范围，调用方的 Cookie 存储；响应体的上限。

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::{Arc, Mutex};

use hs_m3u8_core::{CookieHeader, CookieStore, Error, HttpError, Proxy, Purpose, Source, Url};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

use crate::server::Server;
use crate::{engine, fixture, request, run, test_dir};

/// 用 tests/fixtures/tls 的自签名证书握手的本地 TLS 服务，返回其上的播放列表地址。握手后不回 HTTP 响应：证书不受
/// 信任时客户端在握手时就拒绝，握手失败在这里是预期的。
async fn self_signed_server() -> Url {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/tls");
    let cert = CertificateDer::from(std::fs::read(dir.join("cert.der")).unwrap());
    let key = PrivatePkcs8KeyDer::from(std::fs::read(dir.join("key.der")).unwrap());
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key.into())
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(acceptor.accept(stream));
        }
    });
    Url::parse(&format!("https://{addr}/index.m3u8")).unwrap()
}

/// 证书不受信任：报证书校验失败、不可重试，原因里有 TLS 库给的说明；允许不受信任的证书后不再是这个错误。
#[tokio::test(flavor = "multi_thread")]
async fn untrusted_certificates_are_reported_without_retrying() {
    let mut source = Source::new(self_signed_server().await);

    let err = engine().probe(&source).await.unwrap_err();

    match &err {
        Error::Http {
            kind: HttpError::Certificate(reason),
            ..
        } => assert!(!reason.is_empty()),
        other => panic!("应报证书校验失败：{other}"),
    }
    assert!(!err.retryable());

    source.http.insecure = true;
    source.http.retry.attempts = NonZeroU32::MIN;
    let err = engine().probe(&source).await.unwrap_err();
    assert!(
        !matches!(
            err,
            Error::Http {
                kind: HttpError::Certificate(_),
                ..
            }
        ),
        "{err}"
    );
}

/// 两个地址互相重定向：报重定向失败、不可重试。
#[tokio::test(flavor = "multi_thread")]
async fn redirect_loops_are_reported_without_retrying() {
    let server = Server::start().await;
    server.redirect("a.m3u8", "b.m3u8");
    server.redirect("b.m3u8", "a.m3u8");

    let err = engine()
        .probe(&Source::new(server.url("a.m3u8")))
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            Error::Http {
                kind: HttpError::Redirect(_),
                ..
            }
        ),
        "{err}"
    );
    assert!(!err.retryable());
}

/// 跟随重定向后的请求带的仍是调用方给的 Referer，不被换成上一跳的地址。
#[tokio::test(flavor = "multi_thread")]
async fn redirects_keep_the_callers_referer() {
    let server = Server::start().await;
    server.require_header("referer", "https://example.com/");
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    server.redirect("watch", "index.m3u8");
    let mut source = Source::new(server.url("watch"));
    source.http.headers = vec![("Referer".into(), "https://example.com/".into())];

    let probe = engine().probe(&source).await.unwrap();

    assert_eq!(probe.tracks[0].segments, 1);
}

/// HTTP 库用不上的代理（协议写错，或 socks 地址的主机名它解析不了）：报参数错误，不发出请求。不报的话代理不起作用，
/// 请求直连。
#[tokio::test(flavor = "multi_thread")]
async fn proxies_the_http_library_cannot_use_are_rejected() {
    let server = Server::start().await;
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    for proxy in [
        "sock5://127.0.0.1:9",
        "socks5://例子:9",
        "socks5://{host}:9",
    ] {
        let mut source = Source::new(server.url("index.m3u8"));
        source.http.proxy = Proxy::Url(Url::parse(proxy).unwrap());
        match engine().probe(&source).await {
            Err(Error::InvalidInput(_)) => {}
            other => panic!("{proxy} 应报参数错误：{other:?}"),
        }
    }
    assert_eq!(server.hits("index.m3u8"), 0);
}

/// 来源 `server` 上的播放列表：seg0 在来源的主机（127.0.0.1）上，seg1 用另一个主机名（localhost）指向同一个服务。
/// 播放列表的响应下发 `sid=fresh`。
fn two_host_source(server: &Server) -> Url {
    let mut other = server.url("seg1.ts");
    other.set_host(Some("localhost")).unwrap();
    server.put(
        "index.m3u8",
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\n{other}\n#EXT-X-ENDLIST\n"
        ),
    );
    server.set_cookie("index.m3u8", "sid=fresh; Path=/");
    server.put("seg0.ts", fixture("ts_long/seg0.ts"));
    server.put("seg1.ts", fixture("ts_long/seg1.ts"));
    server.url("index.m3u8")
}

/// 一条 Cookie 请求头里的各项，排好序。
fn pairs(cookie: Option<String>) -> Vec<String> {
    let mut pairs: Vec<String> = cookie
        .iter()
        .flat_map(|c| c.split("; ").map(str::to_owned))
        .collect();
    pairs.sort();
    pairs
}

/// 调用方给的 Cookie 只发往来源地址的主机，来源下发的同名 Cookie 替换它；Authorization 只发往与来源同源的地址。
/// 播放列表里列出的别的主机上的分片两者都收不到。
#[tokio::test(flavor = "multi_thread")]
async fn cookies_and_authorization_go_only_to_the_source_host() {
    let dir = test_dir("http_credentials");
    let server = Server::start().await;
    let mut req = request(two_host_source(&server), &dir);
    req.source.http.headers = vec![
        ("Cookie".into(), "sid=stale; lang=zh".into()),
        ("Authorization".into(), "Bearer token".into()),
    ];

    run(req).await.unwrap();

    assert_eq!(
        pairs(server.received("seg0.ts", "cookie")),
        ["lang=zh", "sid=fresh"]
    );
    assert_eq!(
        server.received("seg0.ts", "authorization").as_deref(),
        Some("Bearer token")
    );
    assert_eq!(server.received("seg1.ts", "cookie"), None);
    assert_eq!(server.received("seg1.ts", "authorization"), None);
}

/// 调用方的 Cookie 存储：只给来源主机 Cookie，记下收到的 Set-Cookie。
#[derive(Default)]
struct Store {
    received: Mutex<Vec<(String, Vec<String>)>>,
}

impl CookieStore for Store {
    fn cookies(&self, url: &Url) -> Option<CookieHeader> {
        (url.host_str() == Some("127.0.0.1")).then(|| CookieHeader::new("sid=app").unwrap())
    }

    fn set_cookies(&self, url: &Url, set_cookie: &[&str]) {
        let values = set_cookie.iter().map(|&v| v.to_owned()).collect();
        let path = url.path().to_owned();
        self.received.lock().unwrap().push((path, values));
    }
}

/// 给了 Cookie 存储：请求带它给的 Cookie，响应下发的 Set-Cookie 交给它；请求头里不能再带 Cookie。
#[tokio::test(flavor = "multi_thread")]
async fn the_callers_cookie_store_is_read_and_written() {
    let dir = test_dir("http_cookie_store");
    let server = Server::start().await;
    let store = Arc::new(Store::default());
    let mut req = request(two_host_source(&server), &dir);
    req.source.http.cookies = Some(store.clone());

    run(req.clone()).await.unwrap();

    assert_eq!(
        server.received("seg0.ts", "cookie").as_deref(),
        Some("sid=app")
    );
    assert_eq!(server.received("seg1.ts", "cookie"), None);
    let received = store.received.lock().unwrap().clone();
    assert_eq!(
        received,
        [(
            "/index.m3u8".to_owned(),
            vec!["sid=fresh; Path=/".to_owned()]
        )]
    );

    req.source.http.headers = vec![("Cookie".into(), "sid=stale".into())];
    assert!(matches!(engine().start(req), Err(Error::InvalidInput(_))));
}

/// key 地址返回的内容超过 key 的上限（地址指向了别的东西）：读到上限即停，报响应体过大，不可重试。
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_key_response_is_rejected_without_retrying() {
    let dir = test_dir("http_oversized_key");
    let server = Server::start().await;
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n\
         #EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    server.put("seg0.ts", fixture("ts_long/seg0.ts"));
    server.put("key.bin", vec![0u8; (1 << 20) + 1]);

    let err = run(request(server.url("index.m3u8"), &dir))
        .await
        .unwrap_err();

    let Error::Segment { cause, .. } = &err else {
        panic!("应报分片失败：{err:?}");
    };
    let Error::Key { cause, .. } = &**cause else {
        panic!("应报取 key 失败：{err:?}");
    };
    assert!(
        matches!(
            &**cause,
            Error::ResponseTooLarge {
                purpose: Purpose::Key,
                limit,
                ..
            } if *limit == 1 << 20
        ),
        "{err:?}"
    );
    assert!(!err.retryable());
    assert_eq!(server.hits("key.bin"), 1);
}
