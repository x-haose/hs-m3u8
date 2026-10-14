//! 下载引擎：持有跨任务共享的在途请求上限。

use std::num::NonZeroUsize;

use hs_m3u8_core as core;
use pyo3::coroutine::CancelHandle;
use pyo3::prelude::*;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};

use crate::convert::{Positive, PyPath, Seq};
use crate::error::to_pyerr;
use crate::hooks::Hooks;
use crate::info::Probe;
use crate::job::Job;
use crate::report::Leftover;
use crate::request::{JobRequest, OutputOptions, Source, Unsettled};
use crate::runtime::{self, Owned};

/// 下载引擎：所有任务共用一个在途请求上限。异步方法返回协程，在 asyncio 事件循环里 await（在别处驱动时抛
/// `RuntimeError`，不开始操作）；协程被取消时的处理见各方法。只能在建立它的进程里使用：fork 出的子进程要另建，
/// 不占着 fork 时正在运行的任务所用的任务目录；macOS 上导入本库之后 fork 出的子进程不能使用本库（系统框架在 fork
/// 之后不可用），开始操作时抛 `RuntimeError`，要用 multiprocessing 的 spawn 方式。在 atexit 处理里第一次导入本库时，
/// 退出前不会关闭运行时，那样开始的任务要在处理返回前等它结束。
///
/// `max_requests`：所有点播任务合计同时在途的 key、分片与 init 段请求数，至少 1，默认为每个任务默认并发的四倍；所有直播
/// 任务的 key 与分片另有同样大小的一份，不与点播共用（直播排在大批下载之后时分片会滑出窗口）。播放列表与刷新时拉的
/// init 段（连同它的 key）不占名额（每条轨同时最多一个；那个 key 正由分片请求时等那次请求）。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen)]
pub struct Engine {
    inner: Owned<core::Engine>,
}

// 签名手写：默认值写 `...`（见 crate 文档）；`discard` 的 `cancel_handle` 参数不是 Python 参数，存根生成认不出它，
// 说明与方法的文档注释逐字相同
pyo3_stub_gen::inventory::submit! {
    pyo3_stub_gen::derive::gen_methods_from_python! {
        r#"
        import builtins
        import os

        class Engine:
            def __new__(cls, *, max_requests: builtins.int = ...) -> Engine:
                """`max_requests` 见类的说明；小于 1 时抛 `InvalidInputError`。"""

            async def discard(
                self, work_dir: builtins.str | os.PathLike[str], *, unsettled: Unsettled = ...
            ) -> builtins.tuple[Leftover, ...]:
                """放弃任务：删除任务目录 `work_dir`（相对路径按当前目录补全），只删本库写的文件。先按其中的记录收拾写到
                一半的输出：删掉临时输出，挪开的旧输出按开始任务时的规则放回或删掉。返回留下的东西，记录已随任务目录删掉，
                要用户处理：原处是别的东西而没放回的旧输出各一项；收拾不了的（盘读不到或收拾出错），`unsettled` 为默认的
                `Unsettled.Fail()` 时报错，为 `Unsettled.Report()` 时记下；任务目录没删干净（里面有不是本库写的文件，或
                删除出错）时一项 `LeftoverKind.WorkDir`。目录不存在时什么也不做。

                分几步做：收拾输出、删各轨的分片、删记录与目录本身。协程被取消时做完进行中的一步就停下、放开任务目录的锁，再抛出
                `CancelledError`：随后用这个目录不会遇到它还被占着，记录还在，再放弃即可接着做、照样返回留下的东西；取消晚于最后
                一步开始时放弃照常做完，这次不返回留下的东西（写了一半的临时输出与没能放回的旧输出以 `hsdl-` 开头，留在输出
                旁）。开始之前就被取消的直接抛出，什么也不做。

                路径为空或不合要求时抛 `InvalidInputError`。任务目录正被任务使用时抛 `WorkDirError`
                （`WorkDirProblem.Locked`），不是本库的任务目录时为 `WorkDirProblem.NotEmpty`。记录是另一个格式版本写的
                （`UnsupportedVersion`）或无法解析（`Corrupt`）时什么也不动：不知道它记着什么就不收拾，可用写下它的版本
                放弃，或由用户删掉记录（`outputs.json`；写了一半的临时输出与没能放回的旧输出以 `hsdl-` 开头，留在输出旁）
                后再放弃。"""
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
    /// 查看时长与是否直播。失败时抛 `HsM3u8Error` 的子类。协程被取消时探测随之取消、立即抛出 `CancelledError`，正在
    /// 执行的回调在后台跑完。
    async fn probe(&self, source: Py<Source>) -> PyResult<Probe> {
        // 简化：取消时不等正在执行的回调（探测不碰任务目录，不影响随后的操作）；要等它返回再抛时，给 core 的探测加
        // 可协作的取消
        let engine = self.inner.get("Engine")?.clone();
        runtime::require_event_loop()?;
        let core_source = Python::attach(|py| source.get().to_core(py))?;
        let probed = runtime::spawn(async move { engine.probe(&core_source).await })?.await?;
        // 持着来源直到探测结束：core 只持回调对象的弱引用
        drop(source);
        match probed {
            Ok(probe) => Ok(probe.into()),
            Err(error) => Err(Python::attach(|py| to_pyerr(py, error))),
        }
    }

    /// 校验请求并开始任务；参数错误立即抛出。输出已存在等要读文件系统的检查、建出输出所在的目录在任务开头、任何
    /// 下载之前进行，结果经 `Job.wait` 抛出。
    fn start(&self, py: Python<'_>, request: &Bound<'_, JobRequest>) -> PyResult<Job> {
        let engine = self.inner.get("Engine")?;
        let hooks = request
            .get()
            .source
            .get()
            .hooks
            .as_ref()
            .map(|h| h.clone_ref(py));
        let request = request.get().to_core(py)?;
        start_job(py, || engine.start(request), hooks)
    }

    /// 不联网，只把任务目录中已录到的直播分片写成输出；用于中断后不再续录、或来源已取不到的录制。进度给出已录的
    /// 分片数、字节数与时长，阶段先为 `Stage.Preparing`、写出时为 `Stage.Writing`；开始写出前可取消，停止不起作用。
    /// 参数错误立即抛出。目录里是点播的下载时抛 `WorkDirError`（`WorkDirProblem.NotLiveRecording`；点播用同样的请求
    /// 再次运行即可续传）；目录不存在或没有已完成的分片时抛 `NothingRecordedError`。结果的 `LiveReport.end` 为 None。
    fn merge_recorded(&self, py: Python<'_>, output: OutputOptions) -> PyResult<Job> {
        let engine = self.inner.get("Engine")?;
        start_job(py, || engine.merge_recorded(output.to_core()), None)
    }

    /// 放弃任务：删除任务目录 `work_dir`（相对路径按当前目录补全），只删本库写的文件。先按其中的记录收拾写到
    /// 一半的输出：删掉临时输出，挪开的旧输出按开始任务时的规则放回或删掉。返回留下的东西，记录已随任务目录删掉，
    /// 要用户处理：原处是别的东西而没放回的旧输出各一项；收拾不了的（盘读不到或收拾出错），`unsettled` 为默认的
    /// `Unsettled.Fail()` 时报错，为 `Unsettled.Report()` 时记下；任务目录没删干净（里面有不是本库写的文件，或
    /// 删除出错）时一项 `LeftoverKind.WorkDir`。目录不存在时什么也不做。
    ///
    /// 分几步做：收拾输出、删各轨的分片、删记录与目录本身。协程被取消时做完进行中的一步就停下、放开任务目录的锁，再抛出
    /// `CancelledError`：随后用这个目录不会遇到它还被占着，记录还在，再放弃即可接着做、照样返回留下的东西；取消晚于最后
    /// 一步开始时放弃照常做完，这次不返回留下的东西（写了一半的临时输出与没能放回的旧输出以 `hsdl-` 开头，留在输出
    /// 旁）。开始之前就被取消的直接抛出，什么也不做。
    ///
    /// 路径为空或不合要求时抛 `InvalidInputError`。任务目录正被任务使用时抛 `WorkDirError`
    /// （`WorkDirProblem.Locked`），不是本库的任务目录时为 `WorkDirProblem.NotEmpty`。记录是另一个格式版本写的
    /// （`UnsupportedVersion`）或无法解析（`Corrupt`）时什么也不动：不知道它记着什么就不收拾，可用写下它的版本
    /// 放弃，或由用户删掉记录（`outputs.json`；写了一半的临时输出与没能放回的旧输出以 `hsdl-` 开头，留在输出旁）
    /// 后再放弃。
    #[gen_stub(skip)]
    #[pyo3(signature = (work_dir, *, unsettled = Unsettled::Fail {}))]
    async fn discard(
        &self,
        work_dir: PyPath,
        unsettled: Unsettled,
        #[pyo3(cancel_handle)] mut cancelled: CancelHandle,
    ) -> PyResult<Seq<Leftover>> {
        let engine = self.inner.get("Engine")?.clone();
        runtime::require_event_loop()?;
        if cancelled.is_cancelled() {
            return Err(runtime::thrown(cancelled.cancelled().await));
        }
        let unsettled = core::Unsettled::from(&unsettled);
        let cancel = Owned::new(core::CancellationToken::new());
        let token = cancel.get("Engine")?.clone();
        let mut discarding =
            runtime::spawn(async move { engine.discard(&work_dir.0, unsettled, &token).await })?;
        let discarded = tokio::select! {
            biased;
            discarded = &mut discarding => discarded?,
            exception = cancelled.cancelled() => {
                if let Some(cancel) = cancel.here() {
                    cancel.cancel();
                }
                // 放弃持着任务目录的锁：停下才抛；结果不在这里用，要抛的是取消
                let _stopped = discarding.await;
                return Err(runtime::thrown(exception));
            }
        };
        match discarded {
            Ok(leftovers) => Ok(leftovers.into_iter().map(Leftover::from).collect()),
            Err(error) => Err(Python::attach(|py| to_pyerr(py, error))),
        }
    }
}

/// 在本进程的运行时里调用 `start`（core 在当前运行时上启动任务），返回任务句柄；`hooks` 为任务所用的回调对象。
fn start_job(
    py: Python<'_>,
    start: impl FnOnce() -> Result<core::Job, core::Error>,
    hooks: Option<Py<Hooks>>,
) -> PyResult<Job> {
    let runtime = runtime::handle()?;
    let job = {
        let _entered = runtime.enter();
        start()
    };
    let job = job.map_err(|error| to_pyerr(py, error))?;
    Ok(Job::spawn(&runtime, job, hooks))
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Engine>()
}
