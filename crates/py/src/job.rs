//! 运行中的任务：等结果、取消、停止、读进度。

use std::any::Any;
use std::sync::{Arc, Mutex, OnceLock};

use hs_m3u8_core as core;
use pyo3::coroutine::CancelHandle;
use pyo3::exceptions::PyBaseException;
use pyo3::panic::PanicException;
use pyo3::prelude::*;
use pyo3::sync::MutexExt;
use pyo3::{PyTraverseError, PyVisit};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::error::to_pyerr;
use crate::hooks::Hooks;
use crate::report::{Output, Progress};
use crate::runtime::{self, Owned};

/// 运行中的任务（`Engine.start`、`Engine.merge_recorded` 返回）。不再被引用、也没有在等它时（被回收）取消任务。异步方法
/// 返回协程，在 asyncio 事件循环里 await（在别处驱动时抛 `RuntimeError`）。只能在建立它的进程里使用。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen)]
pub struct Job {
    running: Owned<Running>,
    /// 任务所用的回调对象：core 只持它的弱引用（见 `hooks` 模块），由这里持有到 `Job` 被回收
    hooks: Option<Py<Hooks>>,
}

struct Running {
    control: core::JobControl,
    /// 任务结束前为 None；运行时关闭（解释器正在退出）时发送端关闭而没有值
    finished: watch::Receiver<Option<Arc<Finished>>>,
}

/// 任务的结果。
enum Finished {
    Done(Output),
    Failed(Failure),
    /// 本库内部出错：panic 的说明
    Panicked(String),
}

/// 任务的错误。翻译为异常要取得 GIL，放到第一次等待时做：后台任务里不创建 Python 对象。
/// 各次等待抛同一个异常对象。
struct Failure {
    /// 翻译前的错误；翻译时取走，翻译期间拿着锁
    error: Mutex<Option<core::Error>>,
    /// 翻译后的异常；`OnceLock` 不要 GIL 凭证就能读，`__traverse__` 里要读它
    exception: OnceLock<Py<PyBaseException>>,
}

impl Failure {
    fn new(error: core::Error) -> Self {
        Failure {
            error: Mutex::new(Some(error)),
            exception: OnceLock::new(),
        }
    }

    /// 要抛出的异常。清掉上次抛出时附上的回溯，以免多次等待时回溯越接越长。
    fn raise(&self, py: Python<'_>) -> PyErr {
        let exception = match self.exception.get() {
            Some(exception) => exception,
            None => {
                // 翻译要调用解释器，期间可能放开 GIL：别的等待放开 GIL 等这把锁，拿到时异常已设好
                let mut error = self.error.lock_py_attached(py).expect("持锁期间不会 panic");
                if let Some(error) = error.take() {
                    let exception = to_pyerr(py, error).into_value(py);
                    self.exception
                        .set(exception)
                        .expect("只有拿着锁取走错误的一方设置");
                }
                self.exception.get().expect("取走错误的一方已设好")
            }
        };
        let exception = exception.bind(py);
        match exception.setattr("__traceback__", py.None()) {
            Ok(()) => PyErr::from_value(exception.clone().into_any()),
            Err(failure) => failure,
        }
    }

    /// 向垃圾回收器报出持有的异常：翻译后的，或翻译前回调抛出的。
    fn traverse(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        if let Some(exception) = self.exception.get() {
            visit.call(exception)?;
        }
        // 正在翻译时不等：这次少报的引用只会使相关对象这一轮不被回收
        if let Ok(error) = self.error.try_lock()
            && let Some(error) = &*error
        {
            crate::error::traverse(error, visit)?;
        }
        Ok(())
    }
}

impl Job {
    /// 在 `runtime` 上等 `job` 结束，把结果交给各次 `wait`。等待的后台任务随 `job` 结束而结束：`Job` 被回收时取消
    /// `job`，core 保证取消后任务会结束。`hooks` 为任务所用的回调对象。
    pub(crate) fn spawn(runtime: &Handle, job: core::Job, hooks: Option<Py<Hooks>>) -> Job {
        let control = job.control();
        let (sender, finished) = watch::channel(None);
        // 等结果放在单独的任务里：它 panic 时，从它的句柄取得原因
        let waiting = runtime.spawn(job.wait());
        runtime.spawn(async move {
            let finished = match waiting.await {
                Ok(Ok(output)) => Finished::Done(output.into()),
                Ok(Err(error)) => Finished::Failed(Failure::new(error)),
                Err(failure) => match failure.try_into_panic() {
                    Ok(payload) => Finished::Panicked(panic_message(payload)),
                    // 被取消只会是运行时正在关闭：不送结果，各次等待得到运行时已关闭的错误
                    Err(_) => return,
                },
            };
            sender.send_replace(Some(Arc::new(finished)));
        });
        Job {
            running: Owned::new(Running { control, finished }),
            hooks,
        }
    }
}

/// panic 的说明：`panic!` 给的字符串。
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => payload.downcast_ref::<&str>().map_or_else(
            || "（panic 的值不是字符串）".to_owned(),
            |message| (*message).to_owned(),
        ),
    }
}

/// 在 fork 出的子进程里回收时不取消：任务在父进程的运行时上（见 [`Owned`]）。
impl Drop for Job {
    fn drop(&mut self) {
        if let Some(running) = self.running.here() {
            running.control.cancel();
        }
    }
}

/// 等待的协程被丢弃（没有收到取消就被回收）时取消任务，在 fork 出的子进程里除外（同 `Job`）；拿到结果或按取消
/// 收尾后解除。
struct CancelOnDrop {
    control: Owned<core::JobControl>,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed
            && let Some(control) = self.control.here()
        {
            control.cancel();
        }
    }
}

// 签名手写：存根生成认不出 `cancel_handle` 参数（它不是 Python 参数）；说明与方法的文档注释逐字相同
pyo3_stub_gen::inventory::submit! {
    pyo3_stub_gen::derive::gen_methods_from_python! {
        r#"
        class Job:
            async def wait(self) -> Output:
                """等任务结束，返回输出；失败时抛 `HsM3u8Error` 的子类，本库内部出错时抛 `pyo3_runtime.PanicException`，
                解释器正在退出（运行时已关闭）时抛 `RuntimeError`。可多次等待，得到同一结果或同一异常。等待的协程被取消时
                任务也取消，等任务真正结束（任务目录放开、在执行的回调返回）才抛出 `CancelledError`，随后用同一任务目录不会
                遇到它还被占着；写出时换上输出的几步不中断，取消晚于它们时任务照常成功。"""
        "#
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Job {
    /// 当前的进度快照。每读到一块数据更新一次，要显示进度就定时取；任务结束后仍取得到最后一份。
    fn progress(&self) -> PyResult<Progress> {
        let running = self.running.get("Job")?;
        Ok(running.control.progress().borrow().clone().into())
    }

    /// 请求取消；`wait` 随后抛 `JobCancelledError`，已完成的分片保留在任务目录中。写出（`Stage.Writing`）中取消时删掉
    /// 写了一半的输出，已有的输出不受影响；换上输出的几步改名不中断，取消晚于它们时任务照常成功。
    fn cancel(&self) -> PyResult<()> {
        self.running.get("Job")?.control.cancel();
        Ok(())
    }

    /// 直播：停止刷新，把已拉到的播放列表处理完、已列出的分片下完后写出，`wait` 返回录到的部分。续录刚开始、还没确定
    /// 能否接上之前录到的内容时停止，不录新的分片，只写出之前录到的。写出完成之前进程退出时，任务目录里不记停止：用
    /// 同样的请求再次开始会接着录，只写出已录到的用 `Engine.merge_recorded`。点播与只合并不受影响。
    fn stop(&self) -> PyResult<()> {
        self.running.get("Job")?.control.stop();
        Ok(())
    }

    /// 等任务结束，返回输出；失败时抛 `HsM3u8Error` 的子类，本库内部出错时抛 `pyo3_runtime.PanicException`，
    /// 解释器正在退出（运行时已关闭）时抛 `RuntimeError`。可多次等待，得到同一结果或同一异常。等待的协程被取消时
    /// 任务也取消，等任务真正结束（任务目录放开、在执行的回调返回）才抛出 `CancelledError`，随后用同一任务目录不会
    /// 遇到它还被占着；写出时换上输出的几步不中断，取消晚于它们时任务照常成功。
    #[gen_stub(skip)]
    async fn wait(&self, #[pyo3(cancel_handle)] mut cancelled: CancelHandle) -> PyResult<Output> {
        let running = self.running.get("Job")?;
        runtime::require_event_loop()?;
        let control = running.control.clone();
        let mut finished = running.finished.clone();
        let mut cancel_on_drop = CancelOnDrop {
            control: Owned::new(control.clone()),
            armed: true,
        };
        // 在运行时上等，协程只持有句柄：它在 fork 出的子进程里被回收时，不碰父进程的同步原语
        let mut waiting = runtime::spawn(async move {
            let finished = finished.wait_for(Option::is_some).await;
            finished.map(|value| value.clone().expect("wait_for 返回时值已满足条件"))
        })?;
        let finished = tokio::select! {
            biased;
            finished = &mut waiting => finished?,
            exception = cancelled.cancelled() => {
                control.cancel();
                // 任务的结果不在这里用：要抛的是取消，再次等待仍拿得到它
                let _ended = waiting.await;
                cancel_on_drop.armed = false;
                return Err(Python::attach(|py| PyErr::from_value(exception.into_bound(py))));
            }
        };
        cancel_on_drop.armed = false;
        let Ok(finished) = finished else {
            return Err(runtime::closed_error());
        };
        match &*finished {
            Finished::Done(output) => Ok(output.clone()),
            Finished::Failed(failure) => Err(Python::attach(|py| failure.raise(py))),
            Finished::Panicked(message) => Err(PanicException::new_err(message.clone())),
        }
    }

    /// 回调对象常引用这个任务（如在回调里调用 `stop`）；失败的任务持有异常，异常的回溯里常有引用这个任务的帧
    /// （`job = engine.start(...); await job.wait()`），回调抛出的异常还经回调的帧引用回调对象：引用环要靠垃圾回收器
    /// 经这里找到。
    #[gen_stub(skip)]
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.hooks)?;
        let Some(running) = self.running.here() else {
            return Ok(());
        };
        let finished = running.finished.borrow();
        if let Some(Finished::Failed(failure)) = finished.as_deref() {
            failure.traverse(&visit)?;
        }
        Ok(())
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Job>()
}
