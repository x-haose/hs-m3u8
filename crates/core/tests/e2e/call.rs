//! 调用接口：任何状态都返回整个响应，只有 GET 跟随重定向；POST、PATCH 只在确定没发出时重试，GET、DELETE 照常重试；
//! 响应体的上限；请求体的 Content-Type。

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use hs_m3u8_core::{
    Body, Call, Error, HookError, HookKind, Hooks, HttpErrorKind, Method, Proxy, Purpose,
    RequestParts, RetryPolicy, Source, Url,
};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

use crate::engine;

/// 接口服务各路径收到的请求数。
#[derive(Default)]
struct Hits(Mutex<HashMap<String, usize>>);

impl Hits {
    fn of(&self, path: &str) -> usize {
        self.0.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

/// 本地接口服务，返回它的根地址与各路径收到的请求数。`echo` 回 200，响应体为方法、Content-Type 与请求体；`denied` 回
/// 401 与 JSON 的错误码；`unavailable` 回 503；`moved` 重定向（302）到 `echo`；`big` 回 2000 字节。
async fn api_server() -> (Url, Arc<Hits>) {
    let hits = Arc::new(Hits::default());
    let app = axum::Router::new().fallback(api).with_state(hits.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, hits)
}

async fn api(
    State(hits): State<Arc<Hits>>,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().trim_start_matches('/').to_owned();
    *hits.0.lock().unwrap().entry(path.clone()).or_default() += 1;
    let json = [(header::CONTENT_TYPE, "application/json")];
    match path.as_str() {
        "echo" => {
            let content_type = headers
                .get(header::CONTENT_TYPE)
                .map_or("-", |v| v.to_str().unwrap());
            let mut echoed = format!("{method} {content_type}\n").into_bytes();
            echoed.extend_from_slice(&body);
            echoed.into_response()
        }
        "denied" => (
            StatusCode::UNAUTHORIZED,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::RETRY_AFTER, "30"),
            ],
            r#"{"error":"unauthenticated","error_code":16}"#,
        )
            .into_response(),
        "unavailable" => (StatusCode::SERVICE_UNAVAILABLE, json, "{}").into_response(),
        "moved" => (StatusCode::FOUND, [(header::LOCATION, "/echo")]).into_response(),
        "big" => vec![b'x'; 2000].into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 直连、快速重试三次的来源。
fn api_source(url: Url) -> Source {
    let mut source = Source::new(url);
    source.http.proxy = Proxy::Direct;
    source.http.retry = RetryPolicy {
        attempts: NonZeroU32::new(3).unwrap(),
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    source
}

fn call(method: Method, body: Option<Body>) -> Call {
    Call {
        method,
        body,
        limit: 1024,
    }
}

fn json(data: &str) -> Option<Body> {
    Some(Body {
        content_type: "application/json".into(),
        data: data.into(),
    })
}

fn header<'a>(headers: &'a [(String, Vec<u8>)], name: &str) -> Option<&'a [u8]> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, value)| value.as_slice())
}

/// 非 2xx 照样作为结果返回：状态、响应头与 JSON 的错误码都在，不重试；请求体与它的 Content-Type 原样发出。
#[tokio::test(flavor = "multi_thread")]
async fn every_status_comes_back_with_its_body() {
    let (base, hits) = api_server().await;
    let echo = api_source(base.join("echo").unwrap());

    let posted = engine()
        .call(&echo, &call(Method::Post, json(r#"{"kind":"offline"}"#)))
        .await
        .unwrap();
    let denied = engine()
        .call(
            &api_source(base.join("denied").unwrap()),
            &call(Method::Get, None),
        )
        .await
        .unwrap();
    let unavailable = engine()
        .call(
            &api_source(base.join("unavailable").unwrap()),
            &call(Method::Delete, None),
        )
        .await
        .unwrap();

    assert_eq!(posted.status, 200);
    assert_eq!(
        posted.body,
        b"POST application/json\n{\"kind\":\"offline\"}"
    );
    assert_eq!(denied.status, 401);
    assert_eq!(
        denied.body,
        br#"{"error":"unauthenticated","error_code":16}"#
    );
    assert_eq!(
        header(&denied.headers, "content-type"),
        Some(&b"application/json"[..])
    );
    assert_eq!(header(&denied.headers, "retry-after"), Some(&b"30"[..]));
    assert_eq!(unavailable.status, 503);
    assert_eq!((hits.of("denied"), hits.of("unavailable")), (1, 1));
}

/// GET 跟随重定向；POST、PATCH、DELETE 拿回 302 本身，不发往新地址。
#[tokio::test(flavor = "multi_thread")]
async fn only_get_follows_redirects() {
    let (base, hits) = api_server().await;
    let moved = api_source(base.join("moved").unwrap());

    let got = engine()
        .call(&moved, &call(Method::Get, None))
        .await
        .unwrap();
    assert_eq!((got.status, got.url.path()), (200, "/echo"));
    assert_eq!(got.body, b"GET -\n");
    for (method, body) in [
        (Method::Post, json("{}")),
        (Method::Patch, json("{}")),
        (Method::Delete, None),
    ] {
        let reply = engine().call(&moved, &call(method, body)).await.unwrap();
        assert_eq!(reply.status, 302, "{method:?}");
        assert_eq!(reply.url.path(), "/moved", "{method:?}");
        assert_eq!(header(&reply.headers, "location"), Some(&b"/echo"[..]));
    }
    assert_eq!(hits.of("echo"), 1);
}

/// 数调用接口时 `on_request` 被调用的次数，即发出的尝试数。
#[derive(Default)]
struct Attempts(AtomicUsize);

impl Hooks for Attempts {
    fn on_request(&self, purpose: Purpose, _: &mut RequestParts) -> Result<(), HookError> {
        assert_eq!(purpose, Purpose::Call);
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// 收下请求头后不回响应就断开的服务，返回它的地址与收到的连接数。
async fn hanging_up() -> (Url, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/api", listener.local_addr().unwrap())).unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = connections.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            counted.fetch_add(1, Ordering::Relaxed);
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            // 请求体可能与请求头一同读到
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
        }
    });
    (url, connections)
}

/// 连不上（请求一定没发出）时各种方法都按重试策略重试，报连接失败；发出后没等到响应时 GET、DELETE 重试，POST、PATCH
/// 不重试（服务器可能已处理），报传输错误。
#[tokio::test(flavor = "multi_thread")]
async fn only_unsent_requests_are_retried_unless_idempotent() {
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        Url::parse(&format!("http://{}/api", listener.local_addr().unwrap())).unwrap()
    };
    for (method, body) in [(Method::Post, json("{}")), (Method::Patch, json("{}"))] {
        let attempts = Arc::new(Attempts::default());
        let mut source = api_source(closed.clone());
        source.hooks = attempts.clone();

        let err = engine()
            .call(&source, &call(method, body))
            .await
            .unwrap_err();

        assert!(
            matches!(
                err,
                Error::Http {
                    kind: HttpErrorKind::Connect(_),
                    ..
                }
            ),
            "{method:?}：{err:?}"
        );
        assert_eq!(attempts.0.load(Ordering::Relaxed), 3, "{method:?}");
    }

    for (method, body, tries) in [
        (Method::Get, None, 3),
        (Method::Delete, None, 3),
        (Method::Post, json("{}"), 1),
        (Method::Patch, json("{}"), 1),
    ] {
        let (url, connections) = hanging_up().await;

        let err = engine()
            .call(&api_source(url), &call(method, body))
            .await
            .unwrap_err();

        assert!(
            matches!(
                err,
                Error::Http {
                    kind: HttpErrorKind::Transport(_),
                    ..
                }
            ),
            "{method:?}：{err:?}"
        );
        assert!(err.retryable(), "{method:?}");
        assert_eq!(connections.load(Ordering::Relaxed), tries, "{method:?}");
    }
}

/// 响应体超过上限时读到即停、报响应体过大，不重试。
#[tokio::test(flavor = "multi_thread")]
async fn oversized_replies_are_rejected_without_retrying() {
    let (base, hits) = api_server().await;
    let big = api_source(base.join("big").unwrap());
    let mut limited = call(Method::Get, None);
    limited.limit = 1000;

    let err = engine().call(&big, &limited).await.unwrap_err();
    limited.limit = 2000;
    let whole = engine().call(&big, &limited).await.unwrap();

    assert!(
        matches!(
            err,
            Error::ResponseTooLarge {
                purpose: Purpose::Call,
                limit: 1000,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(whole.body.len(), 2000);
    assert_eq!(hits.of("big"), 2);
}

/// 在请求头里加上 Content-Type 的回调。
struct TypedByHook;

impl Hooks for TypedByHook {
    fn on_request(&self, _: Purpose, request: &mut RequestParts) -> Result<(), HookError> {
        request
            .headers
            .push(("Content-Type".into(), "text/plain".into()));
        Ok(())
    }
}

/// 带了请求体时 Content-Type 只随请求体给出：请求头里另有的报参数错误，回调另加的报回调错误，都不发出请求；GET 不能带
/// 请求体。没有请求体时请求头里的 Content-Type 照常发出。
#[tokio::test(flavor = "multi_thread")]
async fn request_bodies_carry_their_own_content_type() {
    let (base, hits) = api_server().await;
    let mut typed = api_source(base.join("echo").unwrap());
    typed.http.headers = vec![("Content-Type".into(), "text/plain".into())];
    let mut hooked = api_source(base.join("echo").unwrap());
    hooked.hooks = Arc::new(TypedByHook);

    let in_headers = engine()
        .call(&typed, &call(Method::Post, json("{}")))
        .await
        .unwrap_err();
    let from_hook = engine()
        .call(&hooked, &call(Method::Post, json("{}")))
        .await
        .unwrap_err();
    let get_body = engine()
        .call(&hooked, &call(Method::Get, json("{}")))
        .await
        .unwrap_err();
    let without_body = engine()
        .call(&typed, &call(Method::Delete, None))
        .await
        .unwrap();

    assert!(
        matches!(in_headers, Error::InvalidInput(_)),
        "{in_headers:?}"
    );
    assert!(
        matches!(
            from_hook,
            Error::Hook {
                hook: HookKind::Request,
                ..
            }
        ),
        "{from_hook:?}"
    );
    assert!(matches!(get_body, Error::InvalidInput(_)), "{get_body:?}");
    assert_eq!(without_body.body, b"DELETE text/plain\n");
    assert_eq!(hits.of("echo"), 1);
}
