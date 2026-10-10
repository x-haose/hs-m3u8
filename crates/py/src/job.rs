//! 运行中的任务：等结果、取消、停止、读进度。

use std::sync::Arc;

use hs_m3u8_core as core;
use pyo3::panic::PanicException;
use pyo3::prelude::*;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};
use tokio::sync::watch;

use crate::error::to_pyerr;
use crate::report::{Output, Progress};

/// 任务的结果：输出，或任务结束时翻译好的异常（各次等待抛同一个异常对象）。
type Finished = Arc<Result<Output, PyErr>>;

/// 运行中的任务（`Engine.start`、`Engine.merge_recorded` 返回）。不再被引用、也没有在等它时（被回收）取消任务；已开始
/// 写出（`Stage.Writing`）的仍在后台写完。异步方法在 asyncio 事件循环中调用。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen)]
pub struct Job {
    control: core::JobControl,
    /// 任务结束前为 None
    finished: watch::Receiver<Option<Finished>>,
}

impl Job {
    /// 在 tokio 运行时上等 `job` 结束，把结果交给各次 `wait`。等待的任务随 `job` 结束而结束：`Job` 被回收时取消
    /// `job`，core 保证取消后任务会结束。内部 panic 时结果没有送出，各次 `wait` 抛 `PanicException`。
    pub(crate) fn spawn(job: core::Job) -> Job {
        let control = job.control();
        let (sender, finished) = watch::channel(None);
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            let result = match job.wait().await {
                Ok(output) => Ok(Output::from(output)),
                Err(error) => Err(Python::attach(|py| to_pyerr(py, error))),
            };
            sender.send_replace(Some(Arc::new(result)));
        });
        Job { control, finished }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

/// 等待被取消（future 被丢弃）时取消任务；正常拿到结果后解除。
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
    fn progress(&self) -> Progress {
        self.control.progress().borrow().clone().into()
    }

    /// 请求取消；`wait` 随后抛 `JobCancelledError`，已完成的分片保留在任务目录中。开始写出后不响应取消：已开始写出的
    /// 任务照常完成。
    fn cancel(&self) {
        self.control.cancel();
    }

    /// 直播：停止刷新，把已拉到的播放列表处理完、已列出的分片下完后写出，`wait` 返回录到的部分；续录时会话还没定下就
    /// 立即结束，不录新的分片。点播与只合并不受影响。
    fn stop(&self) {
        self.control.stop();
    }

    /// 等任务结束，返回输出；失败时抛 `HsM3u8Error` 的子类。可多次等待，得到同一结果或同一异常；等待被取消时任务也
    /// 取消。
    #[gen_stub(override_return_type(
        type_repr = "collections.abc.Awaitable[Output]",
        imports = ("collections.abc",)
    ))]
    fn wait<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let this = slf.get();
        let mut finished = this.finished.clone();
        let mut cancel_on_drop = CancelOnDrop(Some(this.control.clone()));
        // 等着的时候任务对象不能被回收（回收即取消）：`await engine.start(request).wait()` 里它没有别的引用
        let job = slf.clone().unbind();
        pyo3_async_runtimes::tokio::future_into_py(slf.py(), async move {
            let _job = job;
            let finished = finished
                .wait_for(Option::is_some)
                .await
                .map(|value| value.clone().expect("wait_for 返回时值已满足条件"));
            cancel_on_drop.disarm();
            match finished {
                Ok(result) => Python::attach(|py| match &*result {
                    Ok(output) => Ok(output.clone()),
                    Err(error) => Err(error.clone_ref(py)),
                }),
                Err(_) => Err(PanicException::new_err("任务内部出错，没有结果")),
            }
        })
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Job>()
}
