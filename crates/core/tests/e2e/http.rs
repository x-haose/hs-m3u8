//! 请求失败的归类：重试也不会成功的证书校验失败、重定向失败不重试，原因写在错误里。

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;

use hs_m3u8_core::{Error, HttpError, Source, Url};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

use crate::engine;
use crate::server::Server;

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
