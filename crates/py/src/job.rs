//! 运行中的任务：等结果、取消、停止、读进度。

use std::sync::{Arc, Mutex};

use hs_m3u8_core as core;
use pyo3::exceptions::PyBaseException;
use pyo3::panic::PanicException;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use tokio::runtime::Runtime;
use tokio::sync::watch;

use crate::error::to_pyerr;
use crate::report::{Output, Progress};
use crate::runtime::Owner;

/// 运行中的任务（`Engine.start`、`Engine.merge_recorded` 返回）。不再被引用、也没有在等它时（被回收）取消任务；已开始
/// 写出（`Stage.Writing`）的仍在后台写完。异步方法返回协程，在 asyncio 事件循环里 await。只能在建立它的进程里使用。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen)]
pub struct Job {
    control: core::JobControl,
    /// 任务结束前为 None；任务内部 panic 时发送端关闭而没有值
    finished: watch::Receiver<Option<Arc<Finished>>>,
    owner: Owner,
}

/// 任务的结果。
enum Finished {
    Done(Output),
    Failed(Failure),
}

/// 任务的错误。翻译为异常要附着到解释器，放到第一次等待时做：后台任务不碰解释器，解释器退出时它还在跑也无妨。
/// 各次等待抛同一个异常对象。
struct Failure {
    /// 翻译前的错误；翻译时取走
    error: Mutex<Option<core::Error>>,
    exception: PyOnceLock<Py<PyBaseException>>,
}

impl Failure {
    /// 要抛出的异常。清掉上次抛出时附上的回溯，以免多次等待时回溯越接越长。
    fn raise(&self, py: Python<'_>) -> PyErr {
        let exception = self.exception.get_or_init(py, || {
            let error = self.error.lock().expect("持锁期间不会 panic").take();
            to_pyerr(py, error.expect("错误只在初始化异常时取走一次")).into_value(py)
        });
        let exception = exception.bind(py);
        match exception.setattr("__traceback__", py.None()) {
            Ok(()) => PyErr::from_value(exception.clone().into_any()),
            Err(failure) => failure,
        }
    }
}

impl Job {
    /// 在 `runtime` 上等 `job` 结束，把结果交给各次 `wait`。等待的后台任务随 `job` 结束而结束：`Job` 被回收时取消
    /// `job`，core 保证取消后任务会结束。
    pub(crate) fn spawn(runtime: &Runtime, job: core::Job) -> Job {
        let control = job.control();
        let (sender, finished) = watch::channel(None);
        runtime.spawn(async move {
            let finished = match job.wait().await {
                Ok(output) => Finished::Done(output.into()),
                Err(error) => Finished::Failed(Failure {
                    error: Mutex::new(Some(error)),
                    exception: PyOnceLock::new(),
                }),
            };
            sender.send_replace(Some(Arc::new(finished)));
        });
        Job {
            control,
            finished,
            owner: Owner::current(),
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

/// 等待被取消（协程被丢弃）时取消任务；正常拿到结果后解除。
struct CancelOnDrop(Option<core::JobControl>);

impl CancelOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(control) = self.0.take() {
            control.cancel();
        }
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Job {
    /// 当前的进度快照。每读到一块数据更新一次，要显示进度就定时取；任务结束后仍取得到最后一份。
    fn progress(&self) -> PyResult<Progress> {
        self.owner.check("Job")?;
        Ok(self.control.progress().borrow().clone().into())
    }

    /// 请求取消；`wait` 随后抛 `JobCancelledError`，已完成的分片保留在任务目录中。开始写出后不响应取消：已开始写出的
    /// 任务照常完成。
    fn cancel(&self) -> PyResult<()> {
        self.owner.check("Job")?;
        self.control.cancel();
        Ok(())
    }

    /// 直播：停止刷新，把已拉到的播放列表处理完、已列出的分片下完后写出，`wait` 返回录到的部分；续录时会话还没定下就
    /// 立即结束，不录新的分片。点播与只合并不受影响。
    fn stop(&self) -> PyResult<()> {
        self.owner.check("Job")?;
        self.control.stop();
        Ok(())
    }

    /// 等任务结束，返回输出；失败时抛 `HsM3u8Error` 的子类，本库内部出错时抛 `pyo3_runtime.PanicException`。可多次
    /// 等待，得到同一结果或同一异常；等待的协程被取消时任务也取消。
    async fn wait(&self) -> PyResult<Output> {
        self.owner.check("Job")?;
        let mut finished = self.finished.clone();
        let mut cancel_on_drop = CancelOnDrop(Some(self.control.clone()));
        let finished = finished
            .wait_for(Option::is_some)
            .await
            .map(|value| value.clone().expect("wait_for 返回时值已满足条件"));
        cancel_on_drop.disarm();
        let Ok(finished) = finished else {
            return Err(PanicException::new_err("任务内部出错，没有结果"));
        };
        match &*finished {
            Finished::Done(output) => Ok(output.clone()),
            Finished::Failed(failure) => Err(Python::attach(|py| failure.raise(py))),
        }
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Job>()
}
