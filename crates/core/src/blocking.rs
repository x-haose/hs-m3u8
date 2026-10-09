//! 在阻塞线程池中执行同步代码（文件 IO、FFmpeg、回调）。

use crate::Error;

/// 在阻塞线程池中执行 `f`。`f` panic 时原样传播；运行时关闭导致它被取消时返回 [`Error::Cancelled`]。
pub(crate) async fn blocking<T, F>(f: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => Ok(value),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => Err(Error::Cancelled),
    }
}
