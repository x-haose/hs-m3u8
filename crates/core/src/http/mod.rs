//! 带重试、超时、取消、全局并发上限与请求回调的 HTTP GET。

mod client;

use std::hash::BuildHasher;
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hs_m3u8_hls::ByteRange;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, RANGE, RETRY_AFTER};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use url::Url;

use self::client::{Clients, Cookies};
use crate::cookies::Provider;
use crate::hooks::{HookKind, Hooks, Purpose, RequestParts, run_hook};
use crate::ident::bare_url;
use crate::request::{RetryPolicy, Source, check_header, cookie_pairs};
use crate::{Error, HttpError, Integrity};

/// 请求占哪一份引擎名额（见 [`Permits`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Permit {
    /// 不占名额
    Exempt,
    /// 占点播的那份
    Vod,
    /// 占直播的那份
    Live,
}

/// 引擎的请求名额：点播与直播各一份同样大小的，所有任务共享（用途见 [`crate::Engine::new`]）。
pub(crate) struct Permits {
    vod: Semaphore,
    live: Semaphore,
}

impl Permits {
    /// 每份 `limit` 个；超过 [`Semaphore::MAX_PERMITS`] 的按它算：它远超能同时发出的请求数，同样相当于不限。
    pub(crate) fn new(limit: usize) -> Arc<Permits> {
        let limit = limit.min(Semaphore::MAX_PERMITS);
        Arc::new(Permits {
            vod: Semaphore::new(limit),
            live: Semaphore::new(limit),
        })
    }
}

pub(crate) struct Http {
    clients: Clients,
    /// 附加到所有请求的请求头（调用方给的，Cookie 与 Authorization 除外）
    headers: Vec<(String, String)>,
    /// 调用方给的 Authorization，只附加到与来源地址同源（协议、主机、端口都相同）的请求
    credentials: Vec<(String, String)>,
    /// 来源地址的源
    source: url::Origin,
    retry: RetryPolicy,
    hooks: Arc<dyn Hooks>,
    /// 引擎内所有任务共享；占名额的每次尝试从调用 `on_request` 回调到读完响应体占用一个
    requests: Arc<Permits>,
    /// 每读到一块响应体，以它的字节数调用；不需要计数（探测）时为 None
    on_received: Option<OnReceived>,
    jitter: std::hash::RandomState,
}

/// 读到响应体时的回调，参数为这一块的字节数。
pub(crate) type OnReceived = Box<dyn Fn(u64) + Send + Sync>;

/// 一次成功的请求。
pub(crate) struct Response {
    /// 跟随重定向之后的最终地址
    pub url: Url,
    pub body: Vec<u8>,
}

impl Http {
    /// 访问 `source` 的客户端（`source.http` 已校验过，见 [`crate::HttpOptions::validate`]）。
    pub(crate) fn new(
        source: &Source,
        requests: Arc<Permits>,
        on_received: Option<OnReceived>,
    ) -> Result<Self, Error> {
        let options = &source.http;
        let is = |name: &str, wanted: &str| name.eq_ignore_ascii_case(wanted);
        let cookies: Box<dyn reqwest::cookie::CookieStore> = match &options.cookies {
            Some(store) => Box::new(Provider(store.clone())),
            None => {
                let jar = reqwest::cookie::Jar::default();
                for (_, value) in options.headers.iter().filter(|(n, _)| is(n, "cookie")) {
                    for pair in cookie_pairs(value).flatten() {
                        jar.add_cookie_str(&format!("{pair}; Path=/"), &source.url);
                    }
                }
                Box::new(jar)
            }
        };
        let clients = Clients::new(options, Cookies(cookies))?;
        let (credentials, headers) = options
            .headers
            .iter()
            .filter(|(name, _)| !is(name, "cookie"))
            .cloned()
            .partition(|(name, _)| is(name, "authorization"));
        Ok(Http {
            clients,
            headers,
            credentials,
            source: source.url.origin(),
            retry: options.retry,
            hooks: source.hooks.clone(),
            requests,
            on_received,
            jitter: std::hash::RandomState::new(),
        })
    }

    /// GET `url`；`range` 不为空时只取这段字节。失败按 [`Http::retrying`] 重试。
    pub(crate) async fn get(
        &self,
        purpose: Purpose,
        url: &Url,
        range: Option<ByteRange>,
        permit: Permit,
        cancel: &CancellationToken,
    ) -> Result<Response, Error> {
        self.retrying(url, cancel, || {
            self.get_once(purpose, url, range, permit, cancel)
        })
        .await
    }

    /// 反复做 `attempt` 直到成功：可重试的失败（[`Error::retryable`]）按 [`RetryPolicy`] 退避后再做，`url` 决定退避
    /// 的抖动；服务器要求的等待超过 `max_delay` 时不再重试，错误里带上它要求的等待。
    pub(crate) async fn retrying<T, F>(
        &self,
        url: &Url,
        cancel: &CancellationToken,
        mut attempt: impl FnMut() -> F,
    ) -> Result<T, Error>
    where
        F: Future<Output = Result<T, Error>>,
    {
        let mut tried = 1;
        loop {
            let error = match attempt().await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            let asked = error.retry_after().unwrap_or_default();
            if !error.retryable()
                || tried >= self.retry.attempts.get()
                || asked > self.retry.max_delay
            {
                return Err(error);
            }
            let delay = self.backoff(url, tried).max(asked);
            tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                _ = tokio::time::sleep(delay) => {}
            }
            tried += 1;
        }
    }

    /// GET `url` 一次，不重试（要重试的连同对内容的处理一起交给 [`Http::retrying`]）。先拿到名额，再经 `on_request`
    /// 回调、立即发出：回调做的签名不在排队中过期。
    pub(crate) async fn get_once(
        &self,
        purpose: Purpose,
        url: &Url,
        range: Option<ByteRange>,
        permit: Permit,
        cancel: &CancellationToken,
    ) -> Result<Response, Error> {
        let semaphore = match permit {
            Permit::Exempt => None,
            Permit::Vod => Some(&self.requests.vod),
            Permit::Live => Some(&self.requests.live),
        };
        let _held = match semaphore {
            None => None,
            Some(semaphore) => Some(tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                held = semaphore.acquire() => held.expect("引擎的信号量从不关闭"),
            }),
        };
        let parts = self.prepare(purpose, url).await?;
        let response = self.send(&parts, range, cancel).await?;
        self.read(response, &parts.url, purpose, range, cancel)
            .await
    }

    /// 本次尝试的地址与请求头：经 `on_request` 回调修改，改后不合法时报回调错误（不重试）。
    async fn prepare(&self, purpose: Purpose, url: &Url) -> Result<RequestParts, Error> {
        let mut headers = self.headers.clone();
        if url.origin() == self.source {
            headers.extend(self.credentials.iter().cloned());
        }
        let parts = RequestParts {
            url: url.clone(),
            headers,
        };
        let parts = run_hook(&self.hooks, HookKind::Request, move |hooks| {
            let mut parts = parts;
            hooks.on_request(purpose, &mut parts).map(|()| parts)
        })
        .await?;
        let invalid = |message: String| Error::Hook {
            hook: HookKind::Request,
            cause: message.into(),
        };
        if !matches!(parts.url.scheme(), "http" | "https") {
            return Err(invalid(format!(
                "只支持 http/https 地址：{}",
                bare_url(&parts.url)
            )));
        }
        for (name, value) in &parts.headers {
            check_header(name, value).map_err(invalid)?;
        }
        Ok(parts)
    }

    /// 发出请求并检查状态码：非 2xx 为失败（429/503 时带上 Retry-After），字节范围请求须返回 206。
    async fn send(
        &self,
        parts: &RequestParts,
        range: Option<ByteRange>,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, Error> {
        let mut builder = self.clients.get()?.get(parts.url.as_str());
        for (name, value) in &parts.headers {
            builder = builder.header(name, value);
        }
        if let Some(r) = range {
            // hls 保证 offset + length 不溢出且 length ≥ 1
            builder = builder.header(
                RANGE,
                format!("bytes={}-{}", r.offset, r.offset + r.length - 1),
            );
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            sent = builder.send() => sent.map_err(|e| http_error(&parts.url, classify(e)))?,
        };
        let status = response.status();
        if !status.is_success() {
            let retry_after = matches!(
                status,
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
            )
            .then(|| {
                let value = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
                requested_wait(value, SystemTime::now())
            })
            .flatten();
            return Err(Error::Http {
                url: Box::new(parts.url.clone()),
                kind: HttpError::Status(status.as_u16()),
                retry_after,
            });
        }
        if let Some(r) = range {
            if status != StatusCode::PARTIAL_CONTENT {
                return Err(http_error(
                    &parts.url,
                    HttpError::RangeIgnored(status.as_u16()),
                ));
            }
            let content_range = response
                .headers()
                .get(CONTENT_RANGE)
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
            if !content_range
                .as_deref()
                .is_some_and(|v| range_matches(v, r))
            {
                return Err(http_error(
                    &parts.url,
                    HttpError::RangeMismatch(content_range),
                ));
            }
        }
        Ok(response)
    }

    /// 第 `attempt` 次失败后的等待，抖动由地址与次数的哈希决定。
    fn backoff(&self, url: &Url, attempt: u32) -> Duration {
        let permille = 750 + (self.jitter.hash_one((url.as_str(), attempt)) % 500) as u32;
        backoff_delay(&self.retry, attempt, permille)
    }

    /// 读完响应体，读到的每块计入进度；`url` 为发出请求的地址，用于错误信息。用途为 `purpose` 的响应体超过它的
    /// 上限（见 [`body_limit`]）、字节范围请求的响应体超过请求的长度时读到即停；字节范围请求的响应体须恰好是请求
    /// 的长度。
    async fn read(
        &self,
        mut response: reqwest::Response,
        url: &Url,
        purpose: Purpose,
        range: Option<ByteRange>,
        cancel: &CancellationToken,
    ) -> Result<Response, Error> {
        let final_url = response.url().clone();
        let wrong_range = |expected: u64, found: usize| Error::Integrity {
            url: Box::new(final_url.clone()),
            kind: Integrity::RangeLength { expected, found },
        };
        let limit = body_limit(purpose);
        let mut body = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                chunk = response.chunk() => chunk.map_err(|e| http_error(url, classify(e)))?,
            };
            let Some(chunk) = chunk else { break };
            if let Some(on_received) = &self.on_received {
                on_received(chunk.len() as u64);
            }
            body.extend_from_slice(&chunk);
            let len = body.len() as u64;
            if let Some(r) = range
                && len > r.length
            {
                return Err(wrong_range(r.length, body.len()));
            }
            if let Some(limit) = limit
                && len > limit
            {
                return Err(Error::ResponseTooLarge {
                    url: Box::new(final_url),
                    purpose,
                    limit,
                });
            }
        }
        if let Some(r) = range
            && body.len() as u64 != r.length
        {
            return Err(wrong_range(r.length, body.len()));
        }
        Ok(Response {
            url: final_url,
            body,
        })
    }
}

/// 用途为 `purpose` 的响应体的上限，字节（见 [`Error::ResponseTooLarge`]）。分片不设：合法的分片随码率与时长相差
/// 很大，由请求的超时限住。
fn body_limit(purpose: Purpose) -> Option<u64> {
    match purpose {
        // 十万个分片、每个带几百字节签名地址的点播播放列表也在这之内
        Purpose::Playlist => Some(64 << 20),
        // init 段只有各轨的编码参数，没有样本
        Purpose::Init => Some(16 << 20),
        // key 本身 16 字节；交给 on_key 回调换算的原始内容（如 JSON、加了密的）也不会大
        Purpose::Key => Some(1 << 20),
        Purpose::Segment => None,
    }
}

/// 206 响应的 Content-Range `value` 是不是请求的那一段 `r`（RFC 9110 14.4：`bytes 首-尾/总长`，总长可为 `*`）。
fn range_matches(value: &str, r: ByteRange) -> bool {
    let span = value
        .trim()
        .strip_prefix("bytes ")
        .and_then(|rest| rest.split_once('/'))
        .and_then(|(span, _total)| span.split_once('-'));
    // hls 保证 offset + length 不溢出且 length ≥ 1
    span.is_some_and(|(first, last)| {
        first.trim().parse() == Ok(r.offset) && last.trim().parse() == Ok(r.offset + r.length - 1)
    })
}

/// Retry-After 要求的等待：秒数，或 HTTP 日期离 `now` 还有多久（已经过去的为 0）；无法解析的值视为没有要求。
fn requested_wait(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse() {
        return Some(Duration::from_secs(seconds));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

fn http_error(url: &Url, kind: HttpError) -> Error {
    Error::Http {
        url: Box::new(url.clone()),
        kind,
        retry_after: None,
    }
}

/// 第 `attempt`（从 1 起）次失败后的等待：base × 2^(attempt-1)，不超过 max，再乘以 `permille`‰ 的抖动
/// （750..1250）。逐次翻倍、到 max 即止，Duration 至多约 2^94 纳秒，翻倍不超过 94 次；各步饱和运算，
/// 调用方用极大的时长表示「不封顶」时不溢出。
fn backoff_delay(retry: &RetryPolicy, attempt: u32, permille: u32) -> Duration {
    let mut delay = retry.base_delay;
    for _ in 1..attempt {
        if delay >= retry.max_delay || delay.is_zero() {
            break;
        }
        delay = delay.saturating_mul(2);
    }
    (delay.min(retry.max_delay) / 1000).saturating_mul(permille)
}

/// 归类传输错误；说明文字含各层原因，不含地址（外层的 [`Error::Http`] 已带只到路径的地址）。
fn classify(error: reqwest::Error) -> HttpError {
    let error = error.without_url();
    if error.is_timeout() {
        HttpError::Timeout
    } else if error.is_redirect() {
        HttpError::Redirect(chain_text(&error))
    } else if error.is_builder() {
        HttpError::InvalidRequest(chain_text(&error))
    } else if let Some(tls) = invalid_certificate(&error) {
        HttpError::Certificate(tls.to_string())
    } else if let Some(refusal) = proxy_refusal(&error) {
        HttpError::Proxy(refusal)
    } else if error.is_connect() {
        HttpError::Connect(chain_text(&error))
    } else {
        HttpError::Transport(chain_text(&error))
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
fn chain_text(error: &(dyn std::error::Error + 'static)) -> String {
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    #[test]
    fn content_range_must_be_the_requested_span() {
        let r = ByteRange {
            offset: 100,
            length: 50,
        };
        for value in [
            "bytes 100-149/1000",
            "bytes 100-149/*",
            " bytes 100-149/1000 ",
        ] {
            assert!(range_matches(value, r), "{value}");
        }
        for value in [
            "bytes 0-49/1000",
            "bytes 100-150/1000",
            "bytes 100-149",
            "100-149/1000",
            "items 100-149/1000",
        ] {
            assert!(!range_matches(value, r), "{value}");
        }
    }

    #[test]
    fn retry_after_takes_seconds_or_an_http_date() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let later = httpdate::fmt_http_date(now + Duration::from_secs(90));
        let earlier = httpdate::fmt_http_date(now - Duration::from_secs(90));
        assert_eq!(requested_wait(" 120 ", now), Some(Duration::from_secs(120)));
        assert_eq!(requested_wait(&later, now), Some(Duration::from_secs(90)));
        assert_eq!(requested_wait(&earlier, now), Some(Duration::ZERO));
        // RFC 9110 认可的旧写法（RFC 850 与 asctime）同样认得：都是 2027-01-15 08:00:00 GMT，即 `before` 之后 1000 秒
        let before = now - Duration::from_secs(1000);
        for legacy in ["Friday, 15-Jan-27 08:00:00 GMT", "Fri Jan 15 08:00:00 2027"] {
            assert_eq!(
                requested_wait(legacy, before),
                Some(Duration::from_secs(1000)),
                "{legacy}"
            );
        }
        assert_eq!(requested_wait("soon", now), None);
    }

    #[test]
    fn backoff_grows_exponentially_up_to_the_cap_and_never_overflows() {
        let retry = RetryPolicy {
            attempts: NonZeroU32::new(8).unwrap(),
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(3),
        };
        assert_eq!(backoff_delay(&retry, 1, 1000), Duration::from_millis(500));
        assert_eq!(backoff_delay(&retry, 3, 1000), Duration::from_secs(2));
        assert_eq!(backoff_delay(&retry, 4, 1000), Duration::from_secs(3));
        assert_eq!(backoff_delay(&retry, 4, 750), Duration::from_millis(2250));
        let unbounded = RetryPolicy {
            attempts: NonZeroU32::new(8).unwrap(),
            base_delay: Duration::MAX,
            max_delay: Duration::MAX,
        };
        assert_eq!(backoff_delay(&unbounded, 8, 1249), Duration::MAX);
        // 上限是基础间隔的 2^20 倍以上时照样翻倍到上限
        let patient = RetryPolicy {
            attempts: NonZeroU32::MAX,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_secs(3600),
        };
        assert_eq!(
            backoff_delay(&patient, 22, 1000),
            Duration::from_millis(1 << 21)
        );
        assert_eq!(
            backoff_delay(&patient, u32::MAX, 1000),
            Duration::from_secs(3600)
        );
        let immediate = RetryPolicy {
            base_delay: Duration::ZERO,
            ..patient
        };
        assert_eq!(backoff_delay(&immediate, u32::MAX, 1000), Duration::ZERO);
    }
}
