//! 站点适配回调：Python 继承 `Hooks` 并覆盖需要的方法；core 在阻塞线程池里取得 GIL 后调用，没覆盖的方法不调用、
//! 不取 GIL。

use std::fmt;
use std::sync::Arc;

use hs_m3u8_core as core;
use hs_m3u8_core::Url;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyMapping, PyTuple};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};

use crate::record::variants;

variants! {
    /// 请求的用途。
    pub enum Purpose {
        /// 播放列表
        Playlist {},
        /// key
        Key {},
        /// init 段（EXT-X-MAP）
        Init {},
        /// 分片
        Segment {},
    }
}

/// 发出请求前可修改的地址与请求头：`Hooks.on_request` 收到它，返回改过的。`repr` 只显示地址到路径与请求头的
/// 名字：请求头常带 Cookie 与令牌，地址的查询串常带签名。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen, eq, hash, get_all, from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RequestParts {
    /// 请求的地址
    pub url: String,
    /// 本次请求的请求头（已含 `HttpOptions.headers`），按顺序，同名的可以有多个
    pub headers: Vec<(String, String)>,
}

#[gen_stub_pymethods]
#[pymethods]
impl RequestParts {
    #[new]
    #[pyo3(signature = (url, *, headers))]
    fn new(
        url: String,
        #[gen_stub(override_type(
            type_repr = "collections.abc.Mapping[str, str] | collections.abc.Iterable[tuple[str, str]]",
            imports = ("collections.abc",)
        ))]
        headers: &Bound<'_, PyAny>,
    ) -> PyResult<Self> {
        Ok(RequestParts {
            url,
            headers: extract_headers(headers)?,
        })
    }

    fn __repr__(&self) -> String {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        format!(
            "RequestParts(url={:?}, headers={names:?})",
            bare_url(&self.url)
        )
    }
}

/// 站点适配回调的基类：继承它并覆盖需要的方法，没覆盖的不调用。回调在后台线程里执行（不在事件循环里），抛出的
/// 异常使任务失败、不重试（`HookError`，`__cause__` 为原异常）。取消不会打断正在执行的回调，任务等它返回后才结束。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", subclass, frozen)]
pub struct Hooks;

#[gen_stub_pymethods]
#[pymethods]
impl Hooks {
    /// 子类可以定义带参数的 `__init__`，这里的参数一概不用。
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn new(
        #[gen_stub(override_type(type_repr = "builtins.object"))] _args: &Bound<'_, PyTuple>,
        #[gen_stub(override_type(type_repr = "builtins.object"))] _kwargs: Option<
            &Bound<'_, PyDict>,
        >,
    ) -> Self {
        Hooks
    }

    /// 改写拉到的播放列表（如解密、转码），返回改写后的内容，须为 UTF-8 文本。`url` 为重定向之后的地址。
    fn on_playlist<'py>(&self, url: String, body: Bound<'py, PyBytes>) -> Bound<'py, PyBytes> {
        let _ = url;
        body
    }

    /// 发出请求前修改地址与请求头（如签名），返回改过的 `request`。每次尝试（含重试）调用一次；改后的请求不合法
    /// 时任务失败。
    fn on_request(&self, purpose: Purpose, request: RequestParts) -> RequestParts {
        let _ = purpose;
        request
    }

    /// 变换拉到的 key（如站点自定义的 key 加密），返回 16 字节的 key。`url` 为播放列表中的 key 地址。
    fn on_key<'py>(&self, url: String, data: Bound<'py, PyBytes>) -> Bound<'py, PyBytes> {
        let _ = url;
        data
    }

    /// 在解密之前变换分片（如去掉伪装成图片的前缀字节），返回变换后的内容。`url` 为播放列表中的分片地址；init 段
    /// 不经过此回调。
    fn on_segment<'py>(&self, url: String, data: Bound<'py, PyBytes>) -> Bound<'py, PyBytes> {
        let _ = url;
        data
    }
}

/// 回调抛出的 Python 异常，原样放进 core 的 `Error::Hook`，翻译为 `HookError` 时取回作 `__cause__`。
#[derive(Debug)]
pub(crate) struct PyHookError(pub(crate) PyErr);

impl fmt::Display for PyHookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for PyHookError {}

/// core 的回调：只调用 Python 子类覆盖了的方法。
struct PyHooks {
    object: Py<Hooks>,
    playlist: bool,
    request: bool,
    key: bool,
    segment: bool,
}

/// 把 Python 的 `Hooks` 实例接到 core。
pub(crate) fn to_core(object: &Bound<'_, Hooks>) -> PyResult<Arc<dyn core::Hooks>> {
    let class = object.get_type();
    let base = object.py().get_type::<Hooks>();
    let overridden =
        |name: &str| -> PyResult<bool> { Ok(!class.getattr(name)?.is(&base.getattr(name)?)) };
    Ok(Arc::new(PyHooks {
        object: object.clone().unbind(),
        playlist: overridden("on_playlist")?,
        request: overridden("on_request")?,
        key: overridden("on_key")?,
        segment: overridden("on_segment")?,
    }))
}

impl PyHooks {
    /// 调用 `method(url, data)`，返回的须是 `bytes`。
    fn transform(&self, method: &str, url: &Url, data: &[u8]) -> Result<Vec<u8>, core::HookError> {
        Python::attach(|py| {
            let result =
                self.object
                    .call_method1(py, method, (url.as_str(), PyBytes::new(py, data)))?;
            Ok(result.cast_bound::<PyBytes>(py)?.as_bytes().to_vec())
        })
        .map_err(|failure: PyErr| Box::new(PyHookError(failure)) as core::HookError)
    }
}

impl core::Hooks for PyHooks {
    fn on_playlist(&self, url: &Url, body: Vec<u8>) -> Result<Vec<u8>, core::HookError> {
        if !self.playlist {
            return Ok(body);
        }
        self.transform("on_playlist", url, &body)
    }

    fn on_request(
        &self,
        purpose: core::Purpose,
        request: &mut core::RequestParts,
    ) -> Result<(), core::HookError> {
        if !self.request {
            return Ok(());
        }
        let parts = RequestParts {
            url: request.url.to_string(),
            headers: request.headers.clone(),
        };
        let changed = Python::attach(|py| {
            let result =
                self.object
                    .call_method1(py, "on_request", (Purpose::from(purpose), parts))?;
            let changed: RequestParts = result.extract(py)?;
            let url = Url::parse(&changed.url).map_err(|cause| {
                PyValueError::new_err(format!(
                    "on_request 返回的地址无法解析（{cause}）：{}",
                    bare_url(&changed.url)
                ))
            })?;
            Ok(core::RequestParts {
                url,
                headers: changed.headers,
            })
        })
        .map_err(|failure: PyErr| Box::new(PyHookError(failure)) as core::HookError)?;
        *request = changed;
        Ok(())
    }

    fn on_key(&self, url: &Url, data: Vec<u8>) -> Result<Vec<u8>, core::HookError> {
        if !self.key {
            return Ok(data);
        }
        self.transform("on_key", url, &data)
    }

    fn on_segment(&self, url: &Url, data: Vec<u8>) -> Result<Vec<u8>, core::HookError> {
        if !self.segment {
            return Ok(data);
        }
        self.transform("on_segment", url, &data)
    }
}

impl From<core::Purpose> for Purpose {
    fn from(purpose: core::Purpose) -> Self {
        match purpose {
            core::Purpose::Playlist => Purpose::Playlist {},
            core::Purpose::Key => Purpose::Key {},
            core::Purpose::Init => Purpose::Init {},
            core::Purpose::Segment => Purpose::Segment {},
        }
    }
}

/// 请求头入参：`Mapping[str, str]`，或 `(名, 值)` 的序列（同名的可以有多个）。
pub(crate) fn extract_headers(value: &Bound<'_, PyAny>) -> PyResult<Vec<(String, String)>> {
    if let Ok(mapping) = value.cast::<PyMapping>() {
        return mapping.items()?.extract();
    }
    value
        .try_iter()?
        .map(|item| item?.extract::<(String, String)>())
        .collect()
}

/// 地址只到路径（见 `core::bare_url`）；解析不了时整个隐去。
pub(crate) fn bare_url(url: &str) -> String {
    match Url::parse(url) {
        Ok(url) => core::bare_url(&url),
        Err(_) => "<无法解析的地址>".to_owned(),
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Purpose>()?;
    module.add_class::<RequestParts>()?;
    module.add_class::<Hooks>()?;
    Ok(())
}
