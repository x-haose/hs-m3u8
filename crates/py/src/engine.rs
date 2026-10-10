//! 下载引擎：持有跨任务共享的在途请求上限。

use std::num::NonZeroUsize;

use hs_m3u8_core as core;
use pyo3::prelude::*;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};

use crate::error::{invalid_input, to_pyerr};
use crate::info::Probe;
use crate::job::Job;
use crate::path::PyPath;
use crate::report::Leftover;
use crate::request::{JobRequest, OutputOptions, Source, Unsettled};

/// 下载引擎：所有任务共用一个在途请求上限。异步方法在 asyncio 事件循环中调用；等待被取消时，所等的操作随之取消。
///
/// `max_requests`：所有任务合计同时在途的 key、分片与点播 init 段请求数，至少 1，默认为每个任务默认并发的四倍。
/// 播放列表与直播刷新时新出现的 init 段不占名额，以免直播刷新排在其他任务的大批下载之后。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen)]
pub struct Engine {
    inner: core::Engine,
}

// 默认值取自 core 的常量，存根生成写不出它的值，签名在这里写明
pyo3_stub_gen::inventory::submit! {
    pyo3_stub_gen::derive::gen_methods_from_python! {
        r#"
        class Engine:
            def __new__(cls, *, max_requests: int = ...) -> Engine:
                """`max_requests` 见类的说明；为 0 时抛 `InvalidInputError`。"""
        "#
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Engine {
    #[new]
    #[gen_stub(skip)]
    #[pyo3(signature = (*, max_requests = core::Engine::DEFAULT_MAX_REQUESTS.get()))]
    fn new(py: Python<'_>, max_requests: usize) -> PyResult<Self> {
        let max_requests = NonZeroUsize::new(max_requests)
            .ok_or_else(|| invalid_input(py, "max_requests 至少为 1".to_owned()))?;
        Ok(Engine {
            inner: core::Engine::new(max_requests),
        })
    }

    /// 拉取来源并按偏好选轨，不下载分片、不碰任务目录；请求与回调同下载。用于开始下载前列出可选的清晰度与音轨、
    /// 查看时长与是否直播。失败时抛 `HsM3u8Error` 的子类。
    #[gen_stub(override_return_type(
        type_repr = "collections.abc.Awaitable[Probe]",
        imports = ("collections.abc",)
    ))]
    fn probe<'py>(
        &self,
        py: Python<'py>,
        source: &Bound<'py, Source>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let source = source.get().to_core(py)?;
        let engine = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            match engine.probe(&source).await {
                Ok(probe) => Ok(Probe::from(probe)),
                Err(error) => Err(Python::attach(|py| to_pyerr(py, error))),
            }
        })
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Engine {
    /// 校验请求并开始任务；参数错误立即抛出。输出已存在等要读文件系统的检查、建出输出所在的目录在任务开头、任何
    /// 下载之前进行，结果经 `Job.wait` 抛出。
    fn start(&self, py: Python<'_>, request: &Bound<'_, JobRequest>) -> PyResult<Job> {
        let request = request.get().to_core(py)?;
        let _runtime = pyo3_async_runtimes::tokio::get_runtime().enter();
        let job = self
            .inner
            .start(request)
            .map_err(|error| to_pyerr(py, error))?;
        Ok(Job::spawn(job))
    }

    /// 不联网，只把任务目录中已录到的直播分片写成输出；用于中断后不再续录、或来源已取不到的录制。进度给出已录的
    /// 分片数、字节数与时长，阶段先为 `Stage.Preparing`、写出时为 `Stage.Writing`；开始写出前可取消，停止不起作用。
    /// 参数错误立即抛出。目录里是点播的下载时抛 `WorkDirError`（`WorkDirProblem.NotLiveRecording`；点播用同样的请求
    /// 再次运行即可续传）；目录不存在或没有已完成的分片时抛 `NothingRecordedError`。结果的 `LiveReport.end` 为 None。
    fn merge_recorded(&self, py: Python<'_>, output: OutputOptions) -> PyResult<Job> {
        let _runtime = pyo3_async_runtimes::tokio::get_runtime().enter();
        let job = self
            .inner
            .merge_recorded(output.to_core())
            .map_err(|error| to_pyerr(py, error))?;
        Ok(Job::spawn(job))
    }

    /// 放弃任务：删除任务目录 `work_dir`（相对路径按当前目录补全），只删本库写的文件。先按其中的记录收拾写到一半的
    /// 输出：删掉临时输出，挪开的旧输出按开始任务时的规则放回或删掉。返回留下的东西，记录已随任务目录删掉，要用户
    /// 处理：原处是别的东西而没放回的旧输出各一项；收拾不了而按 `unsettled` 记下的；任务目录没删干净（里面有不是本库
    /// 写的文件，或删除出错）时一项 `LeftoverKind.WorkDir`。目录不存在时什么也不做。
    ///
    /// 路径为空或不合要求时抛 `InvalidInputError`。任务目录正被任务使用时抛 `WorkDirError`
    /// （`WorkDirProblem.Locked`），不是本库的任务目录时为 `WorkDirProblem.NotEmpty`。记录是另一个格式版本写的
    /// （`UnsupportedVersion`）或无法解析（`Corrupt`）时什么也不动：不知道它记着什么就不收拾，可用写下它的版本放弃，
    /// 或由用户删掉记录（`outputs.json`；写了一半的临时输出与没能放回的旧输出以 `hsdl-` 开头，留在输出旁）后再放弃。
    #[gen_stub(override_return_type(
        type_repr = "collections.abc.Awaitable[list[Leftover]]",
        imports = ("collections.abc",)
    ))]
    #[pyo3(signature = (work_dir, *, unsettled = Unsettled::Fail {}))]
    fn discard<'py>(
        &self,
        py: Python<'py>,
        work_dir: PyPath,
        unsettled: Unsettled,
    ) -> PyResult<Bound<'py, PyAny>> {
        let engine = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            match engine.discard(&work_dir.0, (&unsettled).into()).await {
                Ok(leftovers) => Ok(leftovers
                    .into_iter()
                    .map(Leftover::from)
                    .collect::<Vec<_>>()),
                Err(error) => Err(Python::attach(|py| to_pyerr(py, error))),
            }
        })
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Engine>()
}
