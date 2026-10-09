//! 带重试、超时、取消、全局并发上限与请求回调的 HTTP GET。

use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::Duration;

use hs_m3u8_hls::ByteRange;
use reqwest::StatusCode;
use reqwest::header::{RANGE, RETRY_AFTER};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    Error, Hooks, HttpError, Integrity, JobRequest, Purpose, RequestParts, RetryPolicy, run_hook,
};

pub(crate) struct Http {
    client: reqwest::Client,
    headers: Vec<(String, String)>,
    retry: RetryPolicy,
    hooks: Arc<dyn Hooks>,
    /// 引擎内所有任务共享；每次尝试从发出请求到读完响应体占用一个名额
    requests: Arc<Semaphore>,
    jitter: std::hash::RandomState,
}

/// 一次成功的请求。
pub(crate) struct Fetched {
    /// 跟随重定向之后的最终地址
    pub url: Url,
    pub body: Vec<u8>,
}

/// 一次失败的尝试；`retry_after` 为服务器在 429/503 时要求的最短等待。错误装箱，失败路径上才分配。
struct Failure {
    error: Box<Error>,
    retry_after: Option<Duration>,
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Failure {
            error: Box::new(error),
            retry_after: None,
        }
    }
}

impl Http {
    pub(crate) fn new(request: &JobRequest, requests: Arc<Semaphore>) -> Result<Self, Error> {
        let t = request.timeouts;
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("hs-m3u8/", env!("CARGO_PKG_VERSION")))
            .cookie_store(true)
            .connect_timeout(t.connect)
            .read_timeout(t.read_idle)
            .timeout(t.request)
            .tls_danger_accept_invalid_certs(request.insecure);
        if let Some(proxy) = &request.proxy {
            let proxy = reqwest::Proxy::all(proxy.as_str())
                .map_err(|e| Error::InvalidInput(format!("代理地址不可用 {proxy}：{e}")))?;
            builder = builder.proxy(proxy);
        }
        let client = builder
            .build()
            .map_err(|e| Error::InvalidInput(format!("无法创建 HTTP 客户端：{e}")))?;
        Ok(Http {
            client,
            headers: request.headers.clone(),
            retry: request.retry,
            hooks: request.hooks.clone(),
            requests,
            jitter: std::hash::RandomState::new(),
        })
    }

    /// GET `url`；`range` 不为空时只取这段字节。可重试的失败按 [`RetryPolicy`] 重试。
    pub(crate) async fn get(
        &self,
        purpose: Purpose,
        url: &Url,
        range: Option<ByteRange>,
        cancel: &CancellationToken,
    ) -> Result<Fetched, Error> {
        let mut attempt = 1;
        loop {
            match self.attempt(purpose, url, range, cancel).await {
                Ok(fetched) => return Ok(fetched),
                Err(failure) if failure.error.retryable() && attempt < self.retry.attempts => {
                    let delay = self
                        .backoff(url, attempt)
                        .max(failure.retry_after.unwrap_or_default());
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(Error::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    attempt += 1;
                }
                Err(failure) => return Err(*failure.error),
            }
        }
    }

    async fn attempt(
        &self,
        purpose: Purpose,
        url: &Url,
        range: Option<ByteRange>,
        cancel: &CancellationToken,
    ) -> Result<Fetched, Failure> {
        let parts = RequestParts {
            purpose,
            url: url.clone(),
            headers: self.headers.clone(),
        };
        let parts = run_hook(&self.hooks, purpose, move |hooks| {
            let mut parts = parts;
            hooks.on_request(&mut parts).map(|()| parts)
        })
        .await?;
        let failed = |kind| Error::Http {
            url: Box::new(parts.url.clone()),
            kind,
        };

        let _permit = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled.into()),
            permit = self.requests.acquire() => permit.expect("引擎的信号量从不关闭"),
        };
        let mut builder = self.client.get(parts.url.as_str());
        for (name, value) in &parts.headers {
            builder = builder.header(name, value);
        }
        if let Some(r) = range {
            builder = builder.header(
                RANGE,
                format!("bytes={}-{}", r.offset, r.offset + r.length - 1),
            );
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled.into()),
            sent = builder.send() => sent.map_err(|e| failed(classify(&e)))?,
        };

        let status = response.status();
        if !status.is_success() {
            let retry_after = matches!(
                status,
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
            )
            .then(|| {
                response
                    .headers()
                    .get(RETRY_AFTER)?
                    .to_str()
                    .ok()?
                    .trim()
                    .parse()
                    .ok()
            })
            .flatten()
            .map(Duration::from_secs);
            return Err(Failure {
                error: Box::new(failed(HttpError::Status(status.as_u16()))),
                retry_after,
            });
        }
        if range.is_some() && status != StatusCode::PARTIAL_CONTENT {
            return Err(failed(HttpError::RangeIgnored(status.as_u16())).into());
        }
        let final_url = Url::parse(response.url().as_str()).expect("reqwest 的响应地址是合法 URL");
        let body = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Cancelled.into()),
            body = response.bytes() => body.map_err(|e| failed(classify(&e)))?,
        };
        if let Some(r) = range
            && body.len() as u64 != r.length
        {
            return Err(Error::Integrity {
                url: Box::new(final_url),
                kind: Integrity::RangeLength {
                    expected: r.length,
                    found: body.len(),
                },
            }
            .into());
        }
        Ok(Fetched {
            url: final_url,
            body: body.to_vec(),
        })
    }

    /// 第 `attempt` 次失败后的等待：base × 2^(attempt-1)，不超过 max，再乘以 [0.75, 1.25) 的抖动。
    fn backoff(&self, url: &Url, attempt: u32) -> Duration {
        let exponential = self
            .retry
            .base_delay
            .saturating_mul(1u32 << (attempt - 1).min(20));
        let capped = exponential.min(self.retry.max_delay);
        let permille = 750 + (self.jitter.hash_one((url.as_str(), attempt)) % 500) as u32;
        capped * permille / 1000
    }
}

fn classify(error: &reqwest::Error) -> HttpError {
    if error.is_timeout() {
        HttpError::Timeout
    } else if error.is_connect() {
        HttpError::Connect(error.to_string())
    } else {
        HttpError::Other(error.to_string())
    }
}
