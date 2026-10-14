//! 任务：自定义 key、直播录制方式与一个下载任务的配置。

use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::time::Duration;

use hs_m3u8_core as core;
use pyo3::PyTraverseError;
use pyo3::gc::PyVisit;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyTuple};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};

use super::{OutputOptions, Source};
use crate::convert::{Positive, Span};
use crate::error::{invalid_input, to_pyerr};
use crate::record::{field_names, record, reduce};

/// 自定义 key：加密的分片与 init 段一律用它解密，不请求播放列表里的 key 地址，也不经过 `Hooks.on_key`。用于 key 地址取
/// 不到、没写（EXT-X-KEY 没有 URI）或返回的不是真正的 key，而调用方已知 key 的站点；不加密的分片不受影响。`repr` 不显示
/// key 与 IV。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen, eq, hash, from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct KeyOverride {
    key: [u8; 16],
    iv: Option<[u8; 16]>,
}

#[gen_stub_pymethods]
#[pymethods]
impl KeyOverride {
    /// `key`、`iv` 须为 16 字节，否则抛 `InvalidInputError`。
    #[new]
    #[pyo3(signature = (key, *, iv = None))]
    fn new(
        py: Python<'_>,
        key: &Bound<'_, PyBytes>,
        iv: Option<&Bound<'_, PyBytes>>,
    ) -> PyResult<Self> {
        let block = |what: &str, bytes: &Bound<'_, PyBytes>| {
            let bytes = bytes.as_bytes();
            <[u8; 16]>::try_from(bytes).map_err(|_| {
                invalid_input(
                    py,
                    format!("{what} 应为 16 字节，实际 {} 字节", bytes.len()),
                )
            })
        };
        Ok(KeyOverride {
            key: block("key", key)?,
            iv: iv.map(|iv| block("iv", iv)).transpose()?,
        })
    }

    /// 16 字节的 key
    #[getter]
    fn key<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.key)
    }

    /// 所有加密的分片与 init 段都用的 IV；None 时用各自的：分片为播放列表写明的或由媒体序号推出的，init 段为它的
    /// EXT-X-KEY 写明的、没写时为全零（同 hls.js）。IV 只影响解密后的头 16 字节，给错了校验多半发现不了、错的内容照样
    /// 写进输出，只在确知时给
    #[getter]
    fn iv<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyBytes>> {
        self.iv.map(|iv| PyBytes::new(py, &iv))
    }

    fn __repr__(&self) -> String {
        let iv = if self.iv.is_some() { "..." } else { "None" };
        format!("KeyOverride(key=..., iv={iv})")
    }

    #[gen_stub(skip)]
    fn __reduce__<'py>(
        slf: &Bound<'py, Self>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyTuple>)> {
        reduce(slf.as_any(), field_names!(KeyOverride { key, iv }))
    }
}

record! {
    @custom_new
    /// 直播的录制方式。
    ///
    /// 录制从当前播放列表里的全部分片开始，按 RFC 8216 6.3.4 的节奏刷新，直到所有轨出现 EXT-X-ENDLIST、调用
    /// `Job.stop`、各轨都录满 `max_duration`、任一轨停滞，或录的不止一条轨时某条轨的播放列表前后接不上（序号回退、
    /// 同一序号换了分片；只录一条轨时另起会话接着录，见 `NewSession`）。结束后把已拉到的播放列表处理完、已列出的分片
    /// 下完再合并，结束原因见 `LiveEnd`。
    ///
    /// 停滞：任一轨持续 `stall_timeout` 没有录到新分片（有分片还在排队或下载时不算；一份播放列表列出的新分片全是来源标
    /// 了 EXT-X-GAP 的也不算：直播还在，只是这段没有内容）。结论依据那之后各轨的刷新结果（请求在途的，察觉停滞后再等一
    /// 个目标时长；服务器要求等待的照它的意思等，要等过察觉停滞后一个目标时长的以那次失败为据），进程被挂起醒来时不凭挂
    /// 起前的情况下结论：这条轨的播放列表不再出新分片或已被删除（404/410），且其他轨也不再出，视为直播已结束，照常合
    /// 并；有轨刷新失败或一直不返回，这条轨仍在出新分片却一个都下载不成功，或只有这条轨不再出新分片，任务失败
    /// （`LiveStalledError`），任务目录保留，可稍后续录。
    ///
    /// 窗口已滑过、来源标了 EXT-X-GAP 或取不到（404/410、重试后仍失败的临时故障）的分片记为缺失（见 `Output.missed`），
    /// 录制继续。其他失败（如 403、key 失败、校验失败）使任务失败。
    ///
    /// 续录：任务中断后用同样的请求再次运行。至少一条轨当前的窗口与之前录到的接得上（重叠的分片重新下载后内容相同；已录
    /// 满、这次不录的轨同样核对），且没有一条轨内容不同时，接着录：补上窗口内之前没录完的分片，时间线连续，之前没录到的
    /// 轨从窗口起点并入；否则另起一段，与之前的首尾相接（中断期间的内容不在输出中），之前录过的分片不重录，另起的一段里
    /// 有轨这次不录（已录满或没有内容）时报 `WorkDirProblem.Unaligned`。最后一并合并。地址与记录的不同时，接着录才改记
    /// 新地址，另起一段报 `WorkDirProblem.SourceUnverified`；这次没有要录的轨时不改记，直接合并。不联网、只合并已录到的
    /// 部分用 `Engine.merge_recorded`。
    pub struct LiveOptions {
        /// 每条轨的分片声明时长之和达到此值后不再录，各轨都达到即结束；计入之前各会话已录到的、本次排入下载的、本次
        /// 列出了但取不到的，不计窗口已滑过的与之前运行里缺失的。None 不限，不能为 0
        pub max_duration: Option<Duration>,
        /// 停滞判定的时长，默认 60 秒，不能为 0；实际至少等三个目标时长
        pub stall_timeout: Duration,
    }
}

// 签名手写，默认值写 `...`，见 crate 文档
pyo3_stub_gen::inventory::submit! {
    pyo3_stub_gen::derive::gen_methods_from_python! {
        r#"
        import datetime
        import typing

        class LiveOptions:
            def __new__(
                cls,
                *,
                max_duration: typing.Optional[datetime.timedelta] = None,
                stall_timeout: datetime.timedelta = ...,
            ) -> LiveOptions:
                """各项的默认值见同名属性；为负或为 0 时抛 `InvalidInputError`。"""
        "#
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl LiveOptions {
    #[new]
    #[gen_stub(skip)]
    #[pyo3(signature = (*, max_duration = None, stall_timeout = Span(core::LiveOptions::default().stall_timeout)))]
    fn new(py: Python<'_>, max_duration: Option<Span>, stall_timeout: Span) -> PyResult<Self> {
        let live = core::LiveOptions {
            max_duration: max_duration.map(|span| span.0),
            stall_timeout: stall_timeout.0,
        };
        live.validate().map_err(|error| to_pyerr(py, error))?;
        Ok(live.into())
    }
}

/// 一个下载任务的配置。按值比较（见 `Source`）。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen, eq, hash, get_all)]
pub struct JobRequest {
    /// 来源与访问它的方式
    pub source: Py<Source>,
    /// 输出到哪里、任务目录在哪里
    pub output: OutputOptions,
    /// 自定义 key；None 时按播放列表取 key
    pub key: Option<KeyOverride>,
    /// 本任务同时下载的分片数，至少 1，默认 16；站点限流时调低
    pub concurrency: Positive<NonZeroUsize>,
    /// 直播的录制方式；None 时遇到直播抛 `LiveNotEnabledError`
    pub live: Option<LiveOptions>,
}

impl PartialEq for JobRequest {
    fn eq(&self, other: &Self) -> bool {
        self.source.get() == other.source.get()
            && self.output == other.output
            && self.key == other.key
            && self.concurrency == other.concurrency
            && self.live == other.live
    }
}

impl Eq for JobRequest {}

impl Hash for JobRequest {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.source.get().hash(state);
        self.output.hash(state);
        self.key.hash(state);
        self.concurrency.hash(state);
        self.live.hash(state);
    }
}

// 签名手写，默认值写 `...`，见 crate 文档
pyo3_stub_gen::inventory::submit! {
    pyo3_stub_gen::derive::gen_methods_from_python! {
        r#"
        import builtins
        import typing

        class JobRequest:
            def __new__(
                cls,
                source: Source,
                output: OutputOptions,
                *,
                key: typing.Optional[KeyOverride] = None,
                concurrency: builtins.int = ...,
                live: typing.Optional[LiveOptions] = ...,
            ) -> JobRequest:
                """`concurrency` 默认 16，小于 1 时抛 `InvalidInputError`；`live` 默认 `LiveOptions()`。"""
        "#
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl JobRequest {
    #[new]
    #[gen_stub(skip)]
    #[pyo3(signature = (
        source,
        output,
        *,
        key = None,
        concurrency = Positive(core::JobRequest::DEFAULT_CONCURRENCY),
        live = Some(LiveOptions::from(core::LiveOptions::default())),
    ))]
    fn new(
        source: Py<Source>,
        output: OutputOptions,
        key: Option<KeyOverride>,
        concurrency: Positive<NonZeroUsize>,
        live: Option<LiveOptions>,
    ) -> Self {
        JobRequest {
            source,
            output,
            key,
            concurrency,
            live,
        }
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let source = self.source.bind(py).repr()?;
        let output = Bound::new(py, self.output.clone())?.repr()?;
        let key = self.key.clone().into_pyobject(py)?.repr()?;
        let live = self.live.clone().into_pyobject(py)?.repr()?;
        Ok(format!(
            "JobRequest(source={source}, output={output}, key={key}, concurrency={}, live={live})",
            self.concurrency.0
        ))
    }

    #[gen_stub(skip)]
    fn __reduce__<'py>(
        slf: &Bound<'py, Self>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyTuple>)> {
        reduce(
            slf.as_any(),
            field_names!(JobRequest {
                source,
                output,
                key,
                concurrency,
                live,
            }),
        )
    }

    /// 见 `Source.__traverse__`。
    #[gen_stub(skip)]
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.source)
    }
}

impl JobRequest {
    /// 换成 core 的请求；接上回调时读取它的属性，出错时抛那个异常。
    pub(crate) fn to_core(&self, py: Python<'_>) -> PyResult<core::JobRequest> {
        let mut request =
            core::JobRequest::new(self.source.get().to_core(py)?, self.output.to_core());
        request.key = self.key.as_ref().map(|key| core::KeyOverride {
            key: key.key,
            iv: key.iv,
        });
        request.concurrency = self.concurrency.0;
        request.live = self.live.as_ref().map(|live| core::LiveOptions {
            max_duration: live.max_duration,
            stall_timeout: live.stall_timeout,
        });
        Ok(request)
    }
}

impl From<core::LiveOptions> for LiveOptions {
    fn from(live: core::LiveOptions) -> Self {
        LiveOptions {
            max_duration: live.max_duration,
            stall_timeout: live.stall_timeout,
        }
    }
}
