//! 运行下载的 tokio 运行时、对象所属的进程，以及 fork 前后与解释器退出时的处理。异步方法在运行时上执行，结果经
//! asyncio 事件循环送回协程（见 [`require_event_loop`]）。
//!
//! 每个进程一个运行时，第一次用到时建立，解释器退出前关闭（见 [`shutdown`]）。经 `os.fork`（含 multiprocessing 的
//! fork 方式）fork 出的子进程里没有父进程运行时的工作线程，也没有父进程建立的任务与在途请求：子进程第一次用到时
//! 另建运行时（macOS 上不建，见 [`after_fork_in_child`]）；父进程建立的引擎、任务与协程在子进程里拒绝使用，回收时
//! 不销毁（见 [`Owned`]）；继承来的任务目录锁在子进程里关掉（见 `core::fork`）。

use std::cell::RefCell;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use hs_m3u8_core as core;
use pyo3::exceptions::{PyOSError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::sync::{MutexExt, PyOnceLock};
use tokio::runtime::{Builder, Handle, Runtime};
use tokio_util::task::AbortOnDropHandle;

/// 本进程的运行时；fork 出的子进程里，从父进程继承来的那个不销毁也不用，见 [`after_fork_in_child`]。只在持有 GIL、
/// 期间不放开 GIL 时访问：fork 也在持有 GIL 时发生，所以 fork 时没有别的线程拿着它，fork 前后不必另外锁住。
static RUNTIME: Mutex<Slot> = Mutex::new(Slot::Empty);

enum Slot {
    /// 还没用到
    Empty,
    /// 建立它的进程（见 [`Process`]）与运行时
    Running(Process, Runtime),
    /// 不再建立，用到时抛 `RuntimeError`，值为原因：解释器正在退出（[`EXITING`]），或在 macOS 上 fork 出的子进程里
    /// （[`FORKED_ON_MACOS`]）
    Refused(&'static str),
}

const EXITING: &str = "解释器正在退出，不再开始新的操作";

/// 证书校验与读取系统代理要用 Apple 的系统框架，它们在多线程进程 fork 出的子进程里不可用，一用就使进程崩溃。
const FORKED_ON_MACOS: &str = "macOS 上 fork 出的子进程不能使用本库：证书校验与读取系统代理要用的系统框架在 fork 之后不可用；改用 multiprocessing 的 spawn 方式";

/// fork 计数：子进程里加一（见 [`after_fork_in_child`]）。
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// 一个进程：进程号与 fork 计数，两者都对得上才是同一个。只看计数不够：子进程里本库的 fork 处理运行之前（先导入的
/// 模块的处理排在前面），计数还没加一；只看进程号也不够：父进程退出后，它的进程号可能分给继承了它对象的孙进程。
#[derive(Clone, Copy, PartialEq, Eq)]
struct Process {
    id: u32,
    generation: u64,
}

impl Process {
    fn current() -> Self {
        Process {
            id: std::process::id(),
            generation: GENERATION.load(Ordering::Relaxed),
        }
    }
}

fn runtime() -> MutexGuard<'static, Slot> {
    RUNTIME.lock().expect("持锁期间不会 panic")
}

/// 本进程的运行时，第一次用到时建立；不再建立时（见 [`Slot::Refused`]）抛 `RuntimeError`。
pub(crate) fn handle() -> PyResult<Handle> {
    let mut slot = runtime();
    if let Slot::Running(process, _) = &*slot
        && *process != Process::current()
    {
        // fork 出的子进程里，本库的 fork 处理还没运行
        leave_inherited(&mut slot);
    }
    match &*slot {
        Slot::Running(_, runtime) => Ok(runtime.handle().clone()),
        Slot::Refused(reason) => Err(PyRuntimeError::new_err(*reason)),
        Slot::Empty => {
            let built = Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|cause| PyOSError::new_err(format!("建立 tokio 运行时失败：{cause}")))?;
            let handle = built.handle().clone();
            *slot = Slot::Running(Process::current(), built);
            Ok(handle)
        }
    }
}

/// fork 出的子进程里：换下从父进程继承来的运行时，不销毁（销毁要等它的工作线程退出，而它们不在子进程里）；macOS
/// 上此后不再建立（见 [`FORKED_ON_MACOS`]）。
fn leave_inherited(slot: &mut Slot) {
    let next = if cfg!(target_os = "macos") {
        Slot::Refused(FORKED_ON_MACOS)
    } else {
        Slot::Empty
    };
    if let Slot::Running(_, inherited) = std::mem::replace(slot, next) {
        std::mem::forget(inherited);
    }
}

/// 等关闭运行时的线程时，多久查一次 Ctrl+C。
const SIGNAL_CHECK: Duration = Duration::from_millis(100);

/// 解释器退出前（atexit）关闭本进程的运行时：丢掉其上的任务（任务目录可以续传），放开 GIL 等阻塞线程池里正在执行的
/// 操作（回调、写文件、合并 MP4）做完，之后再没有线程进入解释器（终结开始后进入的线程会被终止或永远挂起）。Ctrl+C
/// 放弃等待，这时仍在执行的操作与唤醒协程的后台线程可能使进程异常退出。在 atexit 处理里第一次导入本库时登记不上
/// （CPython 不调用 atexit 运行期间新登记的处理）。
#[pyfunction]
fn shutdown(py: Python<'_>) -> PyResult<()> {
    let Slot::Running(_, runtime) = std::mem::replace(&mut *runtime(), Slot::Refused(EXITING))
    else {
        return Ok(());
    };
    let (closed, done) = mpsc::channel::<Infallible>();
    // 在另一个线程里关闭，这里才能一边等一边响应 Ctrl+C
    let close = move || {
        drop(runtime);
        drop(closed);
    };
    if py.detach(|| thread::Builder::new().spawn(close)).is_err() {
        // 建不了线程：没执行的 close 随 spawn 的失败被丢弃，运行时已在释放 GIL 期间关闭
        return Ok(());
    }
    let mut done = done;
    loop {
        // 接收端不能在线程间共用，交给释放 GIL 的闭包再拿回来
        let (received, receiver) = py.detach(move || (done.recv_timeout(SIGNAL_CHECK), done));
        done = receiver;
        match received {
            Err(RecvTimeoutError::Timeout) => py.check_signals()?,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Ok(never) => match never {},
        }
    }
}

/// 有正在运行的 asyncio 事件循环，否则抛 `RuntimeError`。异步方法先查它再开始操作：协程的结果经事件循环送回
/// （PyO3 的协程只支持 asyncio），在别处驱动（如 trio、手动 `send`）时，不查的话操作已经开始，结果却送不回来。
pub(crate) fn require_event_loop() -> PyResult<()> {
    static GET_RUNNING_LOOP: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    Python::attach(|py| {
        GET_RUNNING_LOOP
            .import(py, "asyncio", "get_running_loop")?
            .call0()?;
        Ok(())
    })
}

/// 在运行时上执行 `future`；返回的句柄被丢弃（等它的协程被取消）时中止它。
pub(crate) fn spawn<F>(future: F) -> PyResult<Spawned<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    Ok(Spawned(Owned::new(AbortOnDropHandle::new(
        handle()?.spawn(future),
    ))))
}

/// [`spawn`] 的句柄：await 得到结果，在建立它的进程里被丢弃时中止任务。
pub(crate) struct Spawned<T>(Owned<AbortOnDropHandle<T>>);

/// 结果：任务内部 panic 原样继续（由 PyO3 转为 `PanicException`）；运行时已关闭时为 [`closed_error`]；在 fork
/// 出的子进程里（如 fork 时正在等的协程，在子进程里继续等）为 `RuntimeError`。
impl<T> Future for Spawned<T> {
    type Output = PyResult<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let handle = match self.0.get_mut("协程") {
            Ok(handle) => handle,
            Err(foreign) => return Poll::Ready(Err(foreign)),
        };
        Pin::new(handle).poll(cx).map(|joined| match joined {
            Ok(value) => Ok(value),
            Err(failure) => match failure.try_into_panic() {
                Ok(payload) => std::panic::resume_unwind(payload),
                // 句柄被丢弃时才中止任务，那时已没有人等它；能等到的只有运行时关闭
                Err(_) => Err(closed_error()),
            },
        })
    }
}

/// 协程收到的、要抛出的异常（取消时 `CancelHandle` 交来的那个）。
pub(crate) fn thrown(exception: Py<PyAny>) -> PyErr {
    Python::attach(|py| PyErr::from_value(exception.into_bound(py)))
}

/// 运行时已关闭（解释器正在退出），等的结果不会来了。
pub(crate) fn closed_error() -> PyErr {
    PyRuntimeError::new_err("解释器正在退出，运行时已关闭，等不到结果")
}

/// 只属于建立它的进程（见 [`Process`]）的值：引擎、任务及等它们的协程所持有的。引擎持有跨任务共享的请求名额，
/// 任务与协程等的是父进程运行时上的后台任务，都不随 fork 进入子进程：在子进程里用，名额可能永远被占着、结果永远等
/// 不到，所以拒绝。在子进程里回收时不销毁里面的值：销毁 tokio 的同步原语要拿它们的锁，fork 时若父进程的线程正拿着，
/// 子进程就会永远等下去。
pub(crate) struct Owned<T> {
    /// drop 时才取走
    value: Option<T>,
    owner: Process,
}

impl<T> Owned<T> {
    pub(crate) fn new(value: T) -> Self {
        Owned {
            value: Some(value),
            owner: Process::current(),
        }
    }

    fn is_here(&self) -> bool {
        self.owner == Process::current()
    }

    /// 在建立它的进程里时为里面的值，在 fork 出的子进程里为 None。
    pub(crate) fn here(&self) -> Option<&T> {
        self.is_here()
            .then(|| self.value.as_ref().expect("值只在 drop 时取走"))
    }

    /// 同 [`Owned::here`]，在 fork 出的子进程里抛 `RuntimeError`；`what` 为对象的名字。
    pub(crate) fn get(&self, what: &str) -> PyResult<&T> {
        self.here().ok_or_else(|| foreign(what))
    }

    /// 同 [`Owned::get`]。
    pub(crate) fn get_mut(&mut self, what: &str) -> PyResult<&mut T> {
        if !self.is_here() {
            return Err(foreign(what));
        }
        Ok(self.value.as_mut().expect("值只在 drop 时取走"))
    }
}

impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        let value = self.value.take();
        if !self.is_here() {
            std::mem::forget(value);
        }
    }
}

fn foreign(what: &str) -> PyErr {
    let instead = if cfg!(target_os = "macos") {
        "macOS 上 fork 出的子进程不能使用本库，要改用 multiprocessing 的 spawn 方式"
    } else {
        "fork 出的子进程要另建 Engine"
    };
    PyRuntimeError::new_err(format!("{what}：只能在建立它的进程里使用；{instead}"))
}

thread_local! {
    /// fork 前拿到、fork 后在同一线程里放开的任务目录锁登记表：fork 时别的线程若正在改登记，子进程里的副本就是改了
    /// 一半的
    static FORKING: RefCell<Option<core::fork::ForkGuard>> = const { RefCell::new(None) };
}

/// 锁住任务目录锁的登记表。等的时候放开 GIL：拿着它的可能是另一个正在 fork 的线程，它要等 GIL 执行 fork 前后的
/// 其他处理（Python 写的处理让出 GIL 时，别的线程就会进来）。
#[pyfunction]
fn before_fork(py: Python<'_>) {
    let locks = core::fork::prepare(|registry| registry.lock_py_attached(py));
    FORKING.with_borrow_mut(|forking| *forking = Some(locks));
}

#[pyfunction]
fn after_fork_in_parent() {
    FORKING.with_borrow_mut(Option::take);
}

/// 关掉继承来的任务目录锁，fork 计数加一，换下继承来的运行时（见 [`leave_inherited`]）。
#[pyfunction]
fn after_fork_in_child() {
    match FORKING.with_borrow_mut(Option::take) {
        Some(locks) => locks.child(),
        // 本库在这次 fork 开始之后才登记（另一个线程正在导入它），fork 前的处理没有运行
        None => core::fork::close_inherited_locks(),
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
    leave_inherited(&mut runtime());
}

/// 登记解释器退出时与 fork 前后的处理（没有 fork 的平台上只有前者）。
pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module
        .py()
        .import("atexit")?
        .call_method1("register", (wrap_pyfunction!(shutdown, module)?,))?;
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
