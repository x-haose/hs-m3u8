//! 调用方持有的 Cookie 存储。

use reqwest::header::HeaderValue;
use url::Url;

use crate::Error;

/// 跨任务保存 Cookie 的存储，由调用方持有（如与浏览器同步、存进数据库），经 [`crate::HttpOptions::cookies`] 交给
/// 任务。发往哪个地址带哪些 Cookie 由存储按 Cookie 的规则（RFC 6265 的域、路径、Secure、过期）决定。两个方法在发出
/// 请求与收到响应时同步调用（跟随重定向的每一跳都调），要快、不阻塞、不 panic。
pub trait CookieStore: Send + Sync {
    /// 发往 `url` 的请求要带的 Cookie；没有时为 None。
    fn cookies(&self, url: &Url) -> Option<CookieHeader>;
    /// `url` 的响应下发的 Set-Cookie，每项一个；不是 UTF-8 的不传。
    fn set_cookies(&self, url: &Url, set_cookie: &[&str]);
}

/// Cookie 请求头的值（`名=值; 名=值`）。
#[derive(Clone, PartialEq, Eq)]
pub struct CookieHeader(HeaderValue);

impl CookieHeader {
    /// 值放不进请求头（含换行等控制字符）时报 [`Error::InvalidInput`]。
    pub fn new(value: &str) -> Result<Self, Error> {
        HeaderValue::from_str(value)
            .map(CookieHeader)
            .map_err(|_| Error::InvalidInput("Cookie 的值放不进请求头".to_owned()))
    }
}

/// 调用方的存储接到 reqwest 上。
pub(crate) struct Provider(pub(crate) std::sync::Arc<dyn CookieStore>);

impl reqwest::cookie::CookieStore for Provider {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        let values: Vec<&str> = headers
            .filter_map(|v| std::str::from_utf8(v.as_bytes()).ok())
            .collect();
        if !values.is_empty() {
            self.0.set_cookies(url, &values);
        }
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        self.0.cookies(url).map(|header| header.0)
    }
}
