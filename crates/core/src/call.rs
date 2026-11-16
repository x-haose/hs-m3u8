//! 调用接口（如网盘的 JSON 接口）：同下载一样的请求设置与回调，任何状态都把整个响应交给调用方。

use std::fmt;

use tokio_util::sync::CancellationToken;
use url::Url;

use crate::hooks::Purpose;
use crate::http::Http;
use crate::ident::bare_url;
use crate::request::{Call, Source};
use crate::{Error, Sent};

/// 接口的响应（[`crate::Engine::call`]），不论状态。
pub struct Reply {
    /// GET 为跟随重定向之后的地址，其余方法为发出请求的地址（[`crate::Hooks::on_request`] 改写之后）
    pub url: Url,
    pub status: u16,
    /// 响应头：名为小写，同名的多条各占一项，顺序不一定与收到的相同；值为原样的字节
    pub headers: Vec<(String, Vec<u8>)>,
    /// 响应体，原样（不解压、不解码）
    pub body: Vec<u8>,
}

/// 只显示地址到路径、状态、响应头的名字与响应体的长度：地址的查询串、响应头与响应体常带令牌。
impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("Reply")
            .field("url", &bare_url(&self.url))
            .field("status", &self.status)
            .field("headers", &names)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// 见 [`crate::Engine::call`]。随返回的 future 被丢弃而取消。
pub(crate) async fn call(http: &Http, source: &Source, request: &Call) -> Result<Reply, Error> {
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let url = &source.url;
    let method = request.method;
    let mut tried = 1;
    loop {
        let (error, sent) = match attempt(http, url, request, &cancel).await {
            Ok(reply) => return Ok(reply),
            Err(failure) => failure,
        };
        let failed = |cause| Error::Call {
            method,
            sent,
            cause: Box::new(cause),
        };
        // 可否重发由 Error::retryable 按走到的那一步判断；退避中取消报的 Cancelled 同样包上
        http.wait_to_retry(Purpose::Call, url, failed(error), &mut tried, &cancel)
            .await
            .map_err(|error| match error {
                Error::Call { .. } => error,
                other => failed(other),
            })?;
    }
}

/// 发一次 `request`、读完响应体；失败时带上走到了哪一步。
async fn attempt(
    http: &Http,
    url: &Url,
    request: &Call,
    cancel: &CancellationToken,
) -> Result<Reply, (Error, Sent)> {
    let stream = http
        .exchange(
            Purpose::Call,
            url,
            request.method,
            request.body.as_ref(),
            cancel,
        )
        .await
        .map_err(|failure| {
            let sent = if failure.unsent {
                Sent::No
            } else {
                Sent::Maybe
            };
            (failure.error, sent)
        })?;
    let status = stream.status().as_u16();
    let headers = stream
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
        .collect();
    let response = stream
        .read_to_end(Vec::new(), Some(request.limit), Purpose::Call, None, cancel)
        .await
        .map_err(|error| (error, Sent::Answered { status }))?;
    Ok(Reply {
        url: response.url,
        status,
        headers,
        body: response.body,
    })
}
