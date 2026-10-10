//! 运行下载的 tokio 运行时，与对象所属的进程。
//!
//! 每个进程一个运行时，第一次用到时建立。fork 出的子进程里没有父进程运行时的工作线程，也没有父进程建立的任务与
//! 在途请求：子进程第一次用到时另建运行时，父进程建立的引擎与任务在子进程里拒绝使用（见 [`Owner`]）。

use std::future::Future;
use std::sync::Mutex;

use pyo3::exceptions::{PyOSError, PyRuntimeError};
use pyo3::prelude::*;
use tokio::runtime::{Builder, Runtime};
use tokio_util::task::AbortOnDropHandle;

/// 当前进程的运行时。从父进程继承来的那个留着不用、也不销毁：销毁要等它那些在子进程里不存在的工作线程。
pub(crate) fn get() -> PyResult<&'static Runtime> {
    static RUNTIME: Mutex<Option<(u32, &'static Runtime)>> = Mutex::new(None);
    let pid = std::process::id();
    let mut current = RUNTIME.lock().expect("持锁期间不会 panic");
    if let Some((owner, runtime)) = *current
        && owner == pid
    {
        return Ok(runtime);
    }
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|cause| PyOSError::new_err(format!("建立 tokio 运行时失败：{cause}")))?;
    let runtime: &'static Runtime = Box::leak(Box::new(runtime));
    *current = Some((pid, runtime));
    Ok(runtime)
}

/// 在运行时上执行 `future`；返回的句柄被丢弃（等它的协程被取消）时中止它。
pub(crate) fn spawn<F>(future: F) -> PyResult<AbortOnDropHandle<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    Ok(AbortOnDropHandle::new(get()?.spawn(future)))
}

/// 建立对象的进程。引擎持有跨任务共享的请求名额，任务的结果由父进程运行时上的后台任务送出，都不随 fork 进入子进程：
/// 在子进程里用，名额可能永远被占着、结果永远等不到，所以拒绝。
#[derive(Clone, Copy)]
pub(crate) struct Owner(u32);

impl Owner {
    pub(crate) fn current() -> Self {
        Owner(std::process::id())
    }

    /// 不是当前进程时抛 `RuntimeError`；`what` 为对象的类名。
    pub(crate) fn check(self, what: &str) -> PyResult<()> {
        if self.0 == std::process::id() {
            return Ok(());
        }
        Err(PyRuntimeError::new_err(format!(
            "{what} 只能在建立它的进程里使用；fork 出的子进程要另建 Engine"
        )))
    }
}
