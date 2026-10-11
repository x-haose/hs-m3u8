//! 运行下载的 tokio 运行时、对象所属的进程，以及 fork 前后的处理。
//!
//! 每个进程一个运行时，第一次用到时建立。经 `os.fork`（含 multiprocessing 的 fork 方式）fork 出的子进程里没有父进程
//! 运行时的工作线程，也没有父进程建立的任务与在途请求：子进程第一次用到时另建运行时；父进程建立的引擎、任务与协程在
//! 子进程里拒绝使用，回收时什么也不做（见 [`Owner`]）；继承来的任务目录锁在子进程里关掉（见 `core::prepare_fork`）。

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll};

use hs_m3u8_core as core;
use pyo3::exceptions::{PyOSError, PyRuntimeError};
use pyo3::prelude::*;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::JoinError;
use tokio_util::task::AbortOnDropHandle;

/// 本进程的运行时；fork 出的子进程里，从父进程继承来的那个不销毁也不用，见 [`after_fork_in_child`]。
static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);

/// fork 的代次：子进程里加一。对象记下建立时的代次，不同即是从父进程继承来的。
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn runtime() -> MutexGuard<'static, Option<Runtime>> {
    RUNTIME.lock().expect("持锁期间不会 panic")
}

/// 本进程的运行时，第一次用到时建立。
pub(crate) fn handle() -> PyResult<Handle> {
    let mut runtime = runtime();
    if let Some(runtime) = &*runtime {
        return Ok(runtime.handle().clone());
    }
    let built = Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|cause| PyOSError::new_err(format!("建立 tokio 运行时失败：{cause}")))?;
    let handle = built.handle().clone();
    *runtime = Some(built);
    Ok(handle)
}

/// 在运行时上执行 `future`；返回的句柄被丢弃（等它的协程被取消）时中止它。
pub(crate) fn spawn<F>(future: F) -> PyResult<Spawned<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    Ok(Spawned {
        handle: Some(AbortOnDropHandle::new(handle()?.spawn(future))),
        owner: Owner::current(),
    })
}

/// [`spawn`] 的句柄：await 得到结果；在建立它的进程里被丢弃时中止任务，在 fork 出的子进程里什么也不做（任务在
/// 父进程的运行时上，子进程里碰它会去唤醒不存在的工作线程）。
pub(crate) struct Spawned<T> {
    /// drop 时才取走
    handle: Option<AbortOnDropHandle<T>>,
    owner: Owner,
}

impl<T> Future for Spawned<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let handle = self.handle.as_mut().expect("句柄只在 drop 时取走");
        Pin::new(handle).poll(cx)
    }
}

impl<T> Drop for Spawned<T> {
    fn drop(&mut self) {
        if !self.owner.is_current() {
            std::mem::forget(self.handle.take());
        }
    }
}

/// 建立对象的进程（按 fork 的代次）。引擎持有跨任务共享的请求名额，任务的结果由父进程运行时上的后台任务送出，都不随
/// fork 进入子进程：在子进程里用，名额可能永远被占着、结果永远等不到，所以拒绝。
#[derive(Clone, Copy)]
pub(crate) struct Owner(u64);

impl Owner {
    pub(crate) fn current() -> Self {
        Owner(GENERATION.load(Ordering::Relaxed))
    }

    /// 在建立它的进程里。
    pub(crate) fn is_current(self) -> bool {
        self.0 == GENERATION.load(Ordering::Relaxed)
    }

    /// 不是当前进程时抛 `RuntimeError`；`what` 为对象的类名。
    pub(crate) fn check(self, what: &str) -> PyResult<()> {
        if self.is_current() {
            return Ok(());
        }
        Err(PyRuntimeError::new_err(format!(
            "{what} 只能在建立它的进程里使用；fork 出的子进程要另建 Engine"
        )))
    }
}

/// fork 期间拿着的锁：fork 时别的线程若正在改运行时或任务目录锁的登记，子进程里的副本就是改了一半的。
struct Forking {
    runtime: MutexGuard<'static, Option<Runtime>>,
    locks: core::ForkGuard,
}

thread_local! {
    /// fork 前拿到、fork 后在同一线程里放开的锁
    static FORKING: RefCell<Option<Forking>> = const { RefCell::new(None) };
}

#[pyfunction]
fn before_fork() {
    let locks = core::prepare_fork();
    let runtime = runtime();
    FORKING.with_borrow_mut(|forking| *forking = Some(Forking { runtime, locks }));
}

#[pyfunction]
fn after_fork_in_parent() {
    FORKING.with_borrow_mut(Option::take);
}

#[pyfunction]
fn after_fork_in_child() {
    let Forking { mut runtime, locks } = FORKING
        .with_borrow_mut(Option::take)
        .expect("fork 前已拿到");
    GENERATION.fetch_add(1, Ordering::Relaxed);
    // 销毁要等它的工作线程退出，而它们不在子进程里
    std::mem::forget(runtime.take());
    locks.child();
}

/// 登记 fork 前后的处理；没有 fork 的平台上什么也不做。
pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    if cfg!(unix) {
        let handlers = pyo3::types::PyDict::new(module.py());
        handlers.set_item("before", wrap_pyfunction!(before_fork, module)?)?;
        handlers.set_item(
            "after_in_parent",
            wrap_pyfunction!(after_fork_in_parent, module)?,
        )?;
        handlers.set_item(
            "after_in_child",
            wrap_pyfunction!(after_fork_in_child, module)?,
        )?;
        module
            .py()
            .import("os")?
            .call_method("register_at_fork", (), Some(&handlers))?;
    }
    Ok(())
}
