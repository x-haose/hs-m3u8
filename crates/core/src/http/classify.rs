//! reqwest 的传输错误归类为 [`HttpErrorKind`]：超时、重定向、请求不合法、证书、代理拒绝、连接失败与其余传输错误；
//! 另有错误连同各层原因的说明（[`chain_text`]）。

use std::io;

use crate::HttpErrorKind;

/// 归类传输错误；说明文字含各层原因，不含地址（外层的 [`crate::Error::Http`] 已带只到路径的地址）。
pub(super) fn classify(error: reqwest::Error) -> HttpErrorKind {
    let error = error.without_url();
    if error.is_timeout() {
        HttpErrorKind::Timeout
    } else if error.is_redirect() {
        HttpErrorKind::Redirect(chain_text(&error))
    } else if error.is_builder() {
        HttpErrorKind::InvalidRequest(chain_text(&error))
    } else if let Some(tls) = invalid_certificate(&error) {
        HttpErrorKind::Certificate(tls.to_string())
    } else if let Some(refusal) = proxy_refusal(&error) {
        HttpErrorKind::Proxy(refusal)
    } else if error.is_connect() {
        HttpErrorKind::Connect(chain_text(&error))
    } else {
        HttpErrorKind::Transport(chain_text(&error))
    }
}

/// 原因链上代理拒绝的那一层的说明：确定出在代理的设置或策略上，重试无用。HTTP 代理要求认证（407）；SOCKS 代理认证
/// 失败、不允许或不支持这次连接、回的不是 SOCKS 协议（多为把别的代理当成了 SOCKS 代理）。CONNECT 回了别的非成功状态
/// （403 与 502/503/504 分不开）、SOCKS 代理报目标连不上等多为上游的临时故障，与连不上代理、与代理之间读写出错一样
/// 不算，按连接失败重试。
///
/// 简化：hyper-util 0.1.21 不导出 TunnelError 与 SocksError，按它们的说明文字认（`tunnel error: `、`SOCKS error: `
/// 起头，见 hyper-util 的 client/legacy/connect/proxy）；它导出这些类型后改按类型认。
fn proxy_refusal(error: &(dyn std::error::Error + 'static)) -> Option<String> {
    const TUNNEL: [&str; 1] = ["proxy authorization required"];
    const SOCKS: [&str; 11] = [
        "credentials not accepted",
        "server does not support user/pass authentication",
        "server implements authentication incorrectly",
        "connection not allowed",
        "command not supported",
        "address type not supported",
        "host address is more than 255 characters",
        "IPV6 is not supported",
        "server ident service failed",
        "server ident service did not recognise client identifier",
        "failed parsing server response",
    ];
    let mut next = error.source();
    while let Some(layer) = next {
        let text = layer.to_string();
        match (
            text.strip_prefix("tunnel error: "),
            text.strip_prefix("SOCKS error: "),
        ) {
            (Some(detail), _) if TUNNEL.contains(&detail) => return Some(detail.to_owned()),
            (_, Some(detail)) if SOCKS.contains(&detail) => return Some(detail.to_owned()),
            _ => {}
        }
        next = layer.source();
    }
    None
}

/// 错误本身与各层原因的说明，以「：」相连。
pub(super) fn chain_text(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut next = error.source();
    while let Some(cause) = next {
        text.push('：');
        text.push_str(&cause.to_string());
        next = cause.source();
    }
    text
}

/// 原因链上 TLS 证书校验失败的那一层。rustls 的错误被包在 `io::Error` 里，而 `io::Error::source` 跳过它包着的
/// 错误，所以每层还要看它包着的。
fn invalid_certificate<'a>(
    error: &'a (dyn std::error::Error + 'static),
) -> Option<&'a rustls::Error> {
    let mut next = error.source();
    while let Some(layer) = next {
        let mut inner = layer;
        loop {
            if let Some(tls) = inner.downcast_ref::<rustls::Error>()
                && matches!(tls, rustls::Error::InvalidCertificate(_))
            {
                return Some(tls);
            }
            match inner
                .downcast_ref::<io::Error>()
                .and_then(io::Error::get_ref)
            {
                Some(wrapped) => inner = wrapped,
                None => break,
            }
        }
        next = layer.source();
    }
    None
}
