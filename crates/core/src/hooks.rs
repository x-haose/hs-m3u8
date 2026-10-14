//! 站点适配回调。

use std::fmt;
use std::sync::Arc;

use url::Url;

use crate::ident::bare_url;
use crate::{Error, blocking};

/// 回调返回的错误；原样保存在 [`Error::Hook`] 中，绑定层可以取回原始错误（如 Python 异常）。
pub type HookError = Box<dyn std::error::Error + Send + Sync>;

/// 请求的用途。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Playlist,
    Key,
    Init,
    Segment,
}

/// [`Hooks::on_request`] 可修改的请求。
#[derive(Clone, PartialEq, Eq)]
pub struct RequestParts {
    pub url: Url,
    /// 本次请求的请求头（已含 [`crate::HttpOptions::headers`]）
    pub headers: Vec<(String, String)>,
}

/// 只显示地址到路径与请求头的名字：请求头常带 Cookie 与令牌，地址的查询串常带签名。
impl fmt::Debug for RequestParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("RequestParts")
            .field("url", &bare_url(&self.url))
            .field("headers", &names)
            .finish()
    }
}

/// 出错的是哪个回调。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    Playlist,
    Request,
    Key,
    Init,
    Segment,
}

impl fmt::Display for HookKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HookKind::Playlist => "播放列表",
            HookKind::Request => "请求",
            HookKind::Key => "key",
            HookKind::Init => "init 段",
            HookKind::Segment => "分片",
        })
    }
}

/// 站点适配回调。在阻塞线程池中执行（[`Hooks::implements`] 为 false 的除外）；返回 `Err` 时任务失败，不重试。
/// 取消不会打断正在执行的回调，任务等它返回后才结束。
///
/// `on_playlist`、`on_key` 返回 `None` 表示不改：拉到的内容不合要求时是来源的问题（如
/// [`crate::Error::KeyLength`]）；返回的值由类型保证合要求。
pub trait Hooks: Send + Sync {
    /// 实现了回调 `hook`。为 false 时本库就地调用它、不进阻塞线程池：那时的调用须立即返回、结果同下面的默认实现。
    fn implements(&self, hook: HookKind) -> bool {
        let _ = hook;
        true
    }

    /// 改写拉到的播放列表（如解密、转码），返回改写后的文本。`url` 为重定向之后的地址。
    fn on_playlist(&self, url: &Url, body: &[u8]) -> Result<Option<String>, HookError> {
        let _ = (url, body);
        Ok(None)
    }

    /// 发出请求前修改地址与请求头（如签名）。每次尝试（含重试）调用一次，在拿到引擎的请求名额之后、调完立即发出，
    /// 签名不在排队中过期；改后的请求不合法（不是 http/https，请求头放不进请求或是本库控制的头，见
    /// [`crate::HttpOptions::validate`]）时任务失败。
    fn on_request(&self, purpose: Purpose, request: &mut RequestParts) -> Result<(), HookError> {
        let _ = (purpose, request);
        Ok(())
    }

    /// 变换拉到的 key（如站点自定义的 key 加密），返回 key。`url` 为播放列表中的 key 地址。
    fn on_key(&self, url: &Url, data: &[u8]) -> Result<Option<[u8; 16]>, HookError> {
        let _ = (url, data);
        Ok(None)
    }

    /// 在解密之前变换 init 段（EXT-X-MAP），同 [`Hooks::on_segment`]；`url` 为播放列表中的 init 段地址。
    fn on_init(&self, url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError> {
        let _ = url;
        Ok(data)
    }

    /// 在解密之前变换分片（如去掉伪装成图片的前缀字节）。`url` 为播放列表中的分片地址；init 段经
    /// [`Hooks::on_init`]，空的响应不经过此回调。重试时同一个分片会再经过它，`data` 可能没写完。
    fn on_segment(&self, url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError> {
        let _ = url;
        Ok(data)
    }
}

/// 不做任何改动的回调。
pub struct NoHooks;

impl Hooks for NoHooks {
    fn implements(&self, _hook: HookKind) -> bool {
        false
    }
}

/// 执行调用回调 `hook` 的 `f`：实现了的（见 [`Hooks::implements`]）在阻塞线程池中执行，没实现的就地调用。
pub(crate) async fn run_hook<T, F>(hooks: &Arc<dyn Hooks>, hook: HookKind, f: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce(&dyn Hooks) -> Result<T, HookError> + Send + 'static,
{
    let result = if hooks.implements(hook) {
        let hooks = hooks.clone();
        blocking(move || f(hooks.as_ref())).await?
    } else {
        f(hooks.as_ref())
    };
    result.map_err(|cause| Error::Hook { hook, cause })
}
