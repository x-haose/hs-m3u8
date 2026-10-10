//! 带重试、超时、取消、全局并发上限与请求回调的 HTTP GET。

use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::Duration;

use hs_m3u8_hls::ByteRange;
use reqwest::StatusCode;
use reqwest::header::{RANGE, RETRY_AFTER};
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::hooks::{HookKind, Hooks, Purpose, RequestParts, run_hook};
use crate::ident::bare_url;
use crate::request::{HttpOptions, RetryPolicy, check_header};
use crate::{Error, HttpError, Integrity, Progress};

/// 请求是否计入引擎的在途上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Permit {
    /// 计入：key、分片与点播的 init 段，量大
    Required,
    /// 不计入：播放列表与直播刷新时新出现的 init 段，量小；排在其他任务的大批下载之后会让直播停滞
    Exempt,
}

pub(crate) struct Http {
    client: reqwest::Client,
    headers: Vec<(String, String)>,
    retry: RetryPolicy,
    hooks: Arc<dyn Hooks>,
    /// 引擎内所有任务共享；[`Permit::Required`] 的每次尝试从发出请求到读完响应体占用一个名额
    requests: Arc<Semaphore>,
    /// 读到的响应体字节计入 [`Progress::received`]；探测时没有进度，为 None
    progress: Option<watch::Sender<Progress>>,
    jitter: std::hash::RandomState,
}

/// 一次成功的请求。
pub(crate) struct Response {
    /// 跟随重定向之后的最终地址
    pub url: Url,
    pub body: Vec<u8>,
}

impl Http {
    pub(crate) fn new(
        options: &HttpOptions,
        hooks: Arc<dyn Hooks>,
        requests: Arc<Semaphore>,
        progress: Option<watch::Sender<Progress>>,
    ) -> Result<Self, Error> {
        let t = options.timeouts;
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("hs-m3u8/", env!("CARGO_PKG_VERSION")))
            .cookie_store(true)
            .connect_timeout(t.connect)
            .read_timeout(t.read_idle)
            .timeout(t.request)
            .tls_danger_accept_invalid_certs(options.insecure);
        if let Some(proxy) = &options.proxy {
            let proxy = reqwest::Proxy::all(proxy.as_str()).map_err(|e| {
                Error::InvalidInput(format!(
                    "代理地址不可用 {}：{}",
                    bare_url(proxy),
                    e.without_url()
                ))
            })?;
            builder = builder.proxy(proxy);
        }
        let client = builder
            .build()
            .map_err(|e| Error::InvalidInput(format!("无法创建 HTTP 客户端：{e}")))?;
        Ok(Http {
            client,
            headers: options.headers.clone(),
            retry: options.retry,
            hooks,
            requests,
            progress,
            jitter: std::hash::RandomState::new(),
        })
    }

    /// GET `url`；`range` 不为空时只取这段字节。可重试的失败按 [`RetryPolicy`] 重试；
    /// 服务器要求的等待超过 `max_delay` 时不再重试，错误里带上它要求的等待。
    pub(crate) async fn get(
        &self,
        purpose: Purpose,
        url: &Url,
        range: Option<ByteRange>,
        permit: Permit,
        cancel: &CancellationToken,
    ) -> Result<Response, Error> {
        let mut attempt = 1;
        loop {
            let error = match self.attempt(purpose, url, range, permit, cancel).await {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            let asked = error.retry_after().unwrap_or_default();
            if !error.retryable()
                || attempt >= self.retry.attempts.get()
                || asked > self.retry.max_delay
            {
                return Err(error);
            }
            let delay = self.backoff(url, attempt).max(asked);
            tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                _ = tokio::time::sleep(delay) => {}
            }
            attempt += 1;
        }
    }

    async fn attempt(
        &self,
        purpose: Purpose,
        url: &Url,
        range: Option<ByteRange>,
        permit: Permit,
        cancel: &CancellationToken,
    ) -> Result<Response, Error> {
        let parts = self.prepare(purpose, url).await?;
        let _permit = match permit {
            Permit::Exempt => None,
            Permit::Required => Some(tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                permit = self.requests.acquire() => permit.expect("引擎的信号量从不关闭"),
            }),
        };
        let response = self.send(&parts, range, cancel).await?;
        self.read(response, &parts.url, range, cancel).await
    }

    /// 本次尝试的地址与请求头：经 `on_request` 回调修改，改后不合法时报回调错误（不重试）。
    async fn prepare(&self, purpose: Purpose, url: &Url) -> Result<RequestParts, Error> {
        let parts = RequestParts {
            url: url.clone(),
            headers: self.headers.clone(),
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
        let mut builder = self.client.get(parts.url.as_str());
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
            .then(|| requested_wait(&response))
            .flatten();
            return Err(Error::Http {
                url: Box::new(parts.url.clone()),
                kind: HttpError::Status(status.as_u16()),
                retry_after,
            });
        }
        if range.is_some() && status != StatusCode::PARTIAL_CONTENT {
            return Err(http_error(
                &parts.url,
                HttpError::RangeIgnored(status.as_u16()),
            ));
        }
        Ok(response)
    }

    /// 第 `attempt` 次失败后的等待，抖动由地址与次数的哈希决定。
    fn backoff(&self, url: &Url, attempt: u32) -> Duration {
        let permille = 750 + (self.jitter.hash_one((url.as_str(), attempt)) % 500) as u32;
        backoff_delay(&self.retry, attempt, permille)
    }

    /// 读完响应体，读到的每块计入进度；`url` 为发出请求的地址，用于错误信息。字节范围请求的响应体须恰好是
    /// 请求的长度。
    async fn read(
        &self,
        mut response: reqwest::Response,
        url: &Url,
        range: Option<ByteRange>,
        cancel: &CancellationToken,
    ) -> Result<Response, Error> {
        let final_url = response.url().clone();
        let mut body = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                chunk = response.chunk() => chunk.map_err(|e| http_error(url, classify(e)))?,
            };
            let Some(chunk) = chunk else { break };
            if let Some(progress) = &self.progress {
                progress.send_modify(|p| p.received += chunk.len() as u64);
            }
            body.extend_from_slice(&chunk);
        }
        if let Some(r) = range
            && body.len() as u64 != r.length
        {
            return Err(Error::Integrity {
                url: Box::new(final_url),
                kind: Integrity::RangeLength {
                    expected: r.length,
                    found: body.len(),
                },
            });
        }
        Ok(Response {
            url: final_url,
            body,
        })
    }
}

/// Retry-After 的秒数写法；HTTP 日期写法与无法解析的值视为没有要求。
fn requested_wait(response: &reqwest::Response) -> Option<Duration> {
    let seconds = response
        .headers()
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds))
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

/// 归类传输错误；说明文字不含地址（外层的 [`Error::Http`] 已带只到路径的地址）。
fn classify(error: reqwest::Error) -> HttpError {
    if error.is_timeout() {
        HttpError::Timeout
    } else if error.is_builder() {
        HttpError::InvalidRequest(error.without_url().to_string())
    } else if error.is_connect() {
        HttpError::Connect(error.without_url().to_string())
    } else {
        HttpError::Transport(error.without_url().to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

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
