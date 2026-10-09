//! 站点适配回调。

use std::fmt;
use std::sync::Arc;

use url::Url;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestParts {
    pub url: Url,
    /// 本次请求的请求头（已含 [`crate::JobRequest::headers`]）
    pub headers: Vec<(String, String)>,
}

/// 出错的是哪个回调。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    Playlist,
    Request,
    Key,
    Segment,
}

impl fmt::Display for HookKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HookKind::Playlist => "播放列表",
            HookKind::Request => "请求",
            HookKind::Key => "key",
            HookKind::Segment => "分片",
        })
    }
}

/// 站点适配回调。在阻塞线程池中执行；返回 `Err` 时任务失败，不重试。
/// 取消不会打断正在执行的回调，任务等它返回后才结束。
pub trait Hooks: Send + Sync {
    /// 改写拉到的播放列表（如解密、转码）；结果须为 UTF-8 文本。`url` 为重定向之后的地址。
    fn on_playlist(&self, url: &Url, body: Vec<u8>) -> Result<Vec<u8>, HookError> {
        let _ = url;
        Ok(body)
    }

    /// 发出请求前修改地址与请求头（如签名）。每次尝试（含重试）调用一次；改后的请求不合法时任务失败。
    fn on_request(&self, purpose: Purpose, request: &mut RequestParts) -> Result<(), HookError> {
        let _ = (purpose, request);
        Ok(())
    }

    /// 变换拉到的 key（如站点自定义的 key 加密）；结果须为 16 字节。`url` 为播放列表中的 key 地址。
    fn on_key(&self, url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError> {
        let _ = url;
        Ok(data)
    }

    /// 在解密之前变换分片（如去掉伪装成图片的前缀字节）。`url` 为播放列表中的分片地址；init 段不经过此回调。
    fn on_segment(&self, url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError> {
        let _ = url;
        Ok(data)
    }
}

/// 不做任何改动的回调。
pub struct NoHooks;

impl Hooks for NoHooks {}

/// 在阻塞线程池中执行回调。
pub(crate) async fn run_hook<T, F>(hooks: &Arc<dyn Hooks>, hook: HookKind, f: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce(&dyn Hooks) -> Result<T, HookError> + Send + 'static,
{
    let hooks = hooks.clone();
    blocking(move || f(hooks.as_ref()))
        .await?
        .map_err(|cause| Error::Hook { hook, cause })
}
