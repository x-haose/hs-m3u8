//! 下载引擎：持有跨任务共享的在途请求上限。

use std::num::NonZeroUsize;

use hs_m3u8_core as core;
use pyo3::prelude::*;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};

use crate::convert::{Positive, PyPath, Seq};
use crate::error::to_pyerr;
use crate::info::Probe;
use crate::job::Job;
use crate::report::Leftover;
use crate::request::{JobRequest, OutputOptions, Source, Unsettled};
use crate::runtime::{self, Owned};

/// 下载引擎：所有任务共用一个在途请求上限。异步方法返回协程，在 asyncio 事件循环里 await（在别处驱动时抛
/// `RuntimeError`，不开始操作）；协程被取消时，所等的操作随之取消。只能在建立它的进程里使用，fork 出的子进程要
/// 另建；fork 时正在运行的任务所用的任务目录，子进程不占着。macOS 上 fork 出的子进程不能使用本库（证书校验与读取
/// 系统代理要用的系统框架在 fork 之后不可用），开始操作时抛 `RuntimeError`，要用 multiprocessing 的 spawn 方式。
/// 在 atexit 处理里第一次导入本库时，退出前不会关闭运行时（CPython 不调用 atexit 运行期间新登记的处理）：那样开始的
/// 任务要在处理返回前等它结束。
///
/// `max_requests`：所有任务合计同时在途的 key、分片与点播 init 段请求数，至少 1，默认为每个任务默认并发的四倍。
/// 播放列表与直播刷新时新出现的 init 段不占名额，以免直播刷新排在其他任务的大批下载之后。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen)]
pub struct Engine {
    inner: Owned<core::Engine>,
}

// 签名手写，默认值写 `...`，见 crate 文档
pyo3_stub_gen::inventory::submit! {
    pyo3_stub_gen::derive::gen_methods_from_python! {
        r#"
        import builtins

        class Engine:
            def __new__(cls, *, max_requests: builtins.int = ...) -> Engine:
                """`max_requests` 见类的说明；小于 1 时抛 `InvalidInputError`。"""
        "#
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Engine {
    #[new]
    #[gen_stub(skip)]
    #[pyo3(signature = (*, max_requests = Positive(core::Engine::DEFAULT_MAX_REQUESTS)))]
    fn new(max_requests: Positive<NonZeroUsize>) -> Self {
        Engine {
            inner: Owned::new(core::Engine::new(max_requests.0)),
        }
    }

    /// 拉取来源并按偏好选轨，不下载分片、不碰任务目录；请求与回调同下载。用于开始下载前列出可选的清晰度与音轨、
    /// 查看时长与是否直播。失败时抛 `HsM3u8Error` 的子类。
    async fn probe(&self, source: Py<Source>) -> PyResult<Probe> {
        let engine = self.inner.get("Engine")?.clone();
        runtime::require_event_loop()?;
        let source = Python::attach(|py| source.get().to_core(py))?;
        let probed = runtime::spawn(async move { engine.probe(&source).await })?.await?;
        match probed {
            Ok(probe) => Ok(probe.into()),
            Err(error) => Err(Python::attach(|py| to_pyerr(py, error))),
        }
    }

    /// 校验请求并开始任务；参数错误立即抛出。输出已存在等要读文件系统的检查、建出输出所在的目录在任务开头、任何
    /// 下载之前进行，结果经 `Job.wait` 抛出。
    fn start(&self, py: Python<'_>, request: &Bound<'_, JobRequest>) -> PyResult<Job> {
        let engine = self.inner.get("Engine")?;
        let request = request.get().to_core(py)?;
        let runtime = runtime::handle()?;
        let job = {
            let _entered = runtime.enter();
            engine.start(request)
        };
        let job = job.map_err(|error| to_pyerr(py, error))?;
        Ok(Job::spawn(&runtime, job))
    }

    /// 不联网，只把任务目录中已录到的直播分片写成输出；用于中断后不再续录、或来源已取不到的录制。进度给出已录的
    /// 分片数、字节数与时长，阶段先为 `Stage.Preparing`、写出时为 `Stage.Writing`；开始写出前可取消，停止不起作用。
    /// 参数错误立即抛出。目录里是点播的下载时抛 `WorkDirError`（`WorkDirProblem.NotLiveRecording`；点播用同样的请求
    /// 再次运行即可续传）；目录不存在或没有已完成的分片时抛 `NothingRecordedError`。结果的 `LiveReport.end` 为 None。
    fn merge_recorded(&self, py: Python<'_>, output: OutputOptions) -> PyResult<Job> {
        let engine = self.inner.get("Engine")?;
        let runtime = runtime::handle()?;
        let job = {
            let _entered = runtime.enter();
            engine.merge_recorded(output.to_core())
        };
        let job = job.map_err(|error| to_pyerr(py, error))?;
        Ok(Job::spawn(&runtime, job))
    }

    /// 放弃任务：删除任务目录 `work_dir`（相对路径按当前目录补全），只删本库写的文件。先按其中的记录收拾写到一半的
    /// 输出：删掉临时输出，挪开的旧输出按开始任务时的规则放回或删掉。返回留下的东西，记录已随任务目录删掉，要用户
    /// 处理：原处是别的东西而没放回的旧输出各一项；收拾不了的（盘读不到或收拾出错），`unsettled` 为默认的
    /// `Unsettled.Fail()` 时报错，为 `Unsettled.Report()` 时记下；任务目录没删干净（里面有不是本库写的文件，或删除
    /// 出错）时一项 `LeftoverKind.WorkDir`。目录不存在时什么也不做。协程被取消时，已开始的收拾与删除做完当前一步才停，
    /// 再次放弃即可接着做。
    ///
    /// 路径为空或不合要求时抛 `InvalidInputError`。任务目录正被任务使用时抛 `WorkDirError`
    /// （`WorkDirProblem.Locked`），不是本库的任务目录时为 `WorkDirProblem.NotEmpty`。记录是另一个格式版本写的
    /// （`UnsupportedVersion`）或无法解析（`Corrupt`）时什么也不动：不知道它记着什么就不收拾，可用写下它的版本放弃，
    /// 或由用户删掉记录（`outputs.json`；写了一半的临时输出与没能放回的旧输出以 `hsdl-` 开头，留在输出旁）后再放弃。
    #[pyo3(signature = (work_dir, *, unsettled = Unsettled::Fail {}))]
    async fn discard(&self, work_dir: PyPath, unsettled: Unsettled) -> PyResult<Seq<Leftover>> {
        let engine = self.inner.get("Engine")?.clone();
        runtime::require_event_loop()?;
        let unsettled = core::Unsettled::from(&unsettled);
        let discarded =
            runtime::spawn(async move { engine.discard(&work_dir.0, unsettled).await })?.await?;
        match discarded {
            Ok(leftovers) => Ok(leftovers.into_iter().map(Leftover::from).collect()),
            Err(error) => Err(Python::attach(|py| to_pyerr(py, error))),
        }
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Engine>()
}
