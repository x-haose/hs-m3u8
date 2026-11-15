//! 取回一个小资源（如 BitTorrent 的公共 tracker 列表）：同下载一样的请求设置、回调与重试，读完整个响应体。

use tokio_util::sync::CancellationToken;
use url::Url;

use crate::Error;
use crate::hooks::Purpose;
use crate::http::{Http, Permit, RangeRequest};
use crate::request::Source;

/// 取回的资源（[`crate::Engine::fetch`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// 跟随重定向之后的最终地址
    pub url: Url,
    /// 响应体，原样（不解压、不解码）
    pub body: Vec<u8>,
}

/// 见 [`crate::Engine::fetch`]。随返回的 future 被丢弃而取消。
pub(crate) async fn fetch(http: &Http, source: &Source, limit: u64) -> Result<Fetched, Error> {
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let url = &source.url;
    http.retrying(Purpose::File, url, &cancel, || async {
        let request = RangeRequest::whole();
        let stream = http
            .open(Purpose::File, url, &request, Permit::Exempt, &cancel)
            .await?;
        let response = stream
            .read_to_end(Vec::new(), Some(limit), Purpose::File, None, &cancel)
            .await?;
        Ok(Fetched {
            url: response.url,
            body: response.body,
        })
    })
    .await
}
