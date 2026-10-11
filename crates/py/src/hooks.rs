//! 站点适配回调：Python 继承 `Hooks` 并覆盖需要的方法；core 在阻塞线程池里取得 GIL 后调用，没覆盖的方法不调用、
//! 不取 GIL。

use std::fmt;
use std::sync::Arc;

use hs_m3u8_core as core;
use hs_m3u8_core::Url;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyString, PyTuple};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};

use crate::convert::{Address, Headers};
use crate::record::{reduce, variants};

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
    pub url: Address,
    /// 本次请求的请求头（已含 `HttpOptions.headers`），按顺序，同名的可以有多个
    pub headers: Headers,
}

#[gen_stub_pymethods]
#[pymethods]
impl RequestParts {
    /// 各参数即同名属性；`url` 解析不了时抛 `InvalidInputError`。
    #[new]
    #[pyo3(signature = (url, *, headers))]
    fn new(url: Address, headers: Headers) -> Self {
        RequestParts { url, headers }
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(format!(
            "RequestParts(url={}, headers={})",
            PyString::new(py, &self.url.bare()).repr()?,
            PyTuple::new(py, self.headers.names())?.repr()?,
        ))
    }

    #[gen_stub(skip)]
    fn __reduce__<'py>(
        slf: &Bound<'py, Self>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyTuple>)> {
        reduce(slf.as_any(), &["url", "headers"])
    }
}

/// 站点适配回调的基类：继承它并覆盖需要的方法，没覆盖的不调用；在实例上赋值的函数也算覆盖。回调在后台线程里执行
/// （不在事件循环里），同一个对象的回调会在多个线程上同时被调用（同时下载的分片各占一个线程），有状态的回调要自己
/// 加锁。回调出错使任务失败、不重试（见 `HookError`）。取消不会打断正在执行的回调，任务等它返回后才结束；解释器
/// 退出时也等它返回，所以回调里的网络请求要设超时。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", subclass, frozen)]
pub struct Hooks;

#[gen_stub_pymethods]
#[pymethods]
impl Hooks {
    /// 子类可以定义带参数的 `__init__`，这里的参数一概不用。
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    #[gen_stub(override_return_type(type_repr = "typing.Self", imports = ("typing",)))]
    fn new(
        #[gen_stub(override_type(type_repr = "typing.Any", imports = ("typing",)))] _args: &Bound<
            '_,
            PyTuple,
        >,
        #[gen_stub(override_type(type_repr = "typing.Any", imports = ("typing",)))] _kwargs: Option<
            &Bound<'_, PyDict>,
        >,
    ) -> Self {
        Hooks
    }

    /// 改写拉到的播放列表（如解密、转码），返回改写后的文本。`url` 为重定向之后的地址。基类的实现按 UTF-8 解码。
    #[pyo3(signature = (url, body, /))]
    fn on_playlist<'py>(
        &self,
        url: String,
        body: Bound<'py, PyBytes>,
    ) -> PyResult<Bound<'py, PyString>> {
        let _ = url;
        PyString::from_encoded_object(&body, Some(c"utf-8"), Some(c"strict"))
    }

    /// 发出请求前修改地址与请求头（如签名），返回改过的请求（`RequestParts` 不可变，改时新建一个）。每次尝试（含
    /// 重试）调用一次；改后的请求不合法时任务失败。
    #[pyo3(signature = (purpose, request, /))]
    fn on_request(&self, purpose: Purpose, request: RequestParts) -> RequestParts {
        let _ = purpose;
        request
    }

    /// 变换拉到的 key（如站点自定义的 key 加密），返回 16 字节的 key，否则任务失败。`url` 为播放列表中的 key 地址。
    #[pyo3(signature = (url, data, /))]
    fn on_key<'py>(&self, url: String, data: Bound<'py, PyBytes>) -> Bound<'py, PyBytes> {
        let _ = url;
        data
    }

    /// 在解密之前变换分片（如去掉伪装成图片的前缀字节），返回变换后的内容。`url` 为播放列表中的分片地址；init 段
    /// 不经过此回调。
    #[pyo3(signature = (url, data, /))]
    fn on_segment<'py>(&self, url: String, data: Bound<'py, PyBytes>) -> Bound<'py, PyBytes> {
        let _ = url;
        data
    }
}

/// 回调抛出的 Python 异常（或返回值的类型不对时提取出错的异常），原样放进 core 的 `Error::Hook`，翻译为 `HookError`
/// 时取回作 `__cause__`。
#[derive(Debug)]
pub(crate) struct PyHookError(pub(crate) PyErr);

impl fmt::Display for PyHookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for PyHookError {}

fn hook_error(failure: PyErr) -> core::HookError {
    Box::new(PyHookError(failure))
}

/// 回调返回的内容不合要求；回调本身没有抛异常，`HookError` 没有 `__cause__`。
#[derive(Debug)]
struct InvalidReturn(String);

impl fmt::Display for InvalidReturn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidReturn {}

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
    let base = object.py().get_type::<Hooks>();
    // 与调用时一样从实例上取：没覆盖时取到的是绑定到它的基类方法，子类的方法、实例上赋值的函数都与它不等
    let overridden = |name: &str| -> PyResult<bool> {
        let inherited = base
            .getattr(name)?
            .call_method1("__get__", (object, object.get_type()))?;
        Ok(!object.getattr(name)?.eq(inherited)?)
    };
    Ok(Arc::new(PyHooks {
        object: object.clone().unbind(),
        playlist: overridden("on_playlist")?,
        request: overridden("on_request")?,
        key: overridden("on_key")?,
        segment: overridden("on_segment")?,
    }))
}

impl PyHooks {
    /// 调用 `method(url, data)`，用 `take` 取回结果；返回值的类型不对时报 `take` 抛的异常。
    fn call<T>(
        &self,
        method: &str,
        url: &Url,
        data: &[u8],
        take: impl FnOnce(&Bound<'_, PyAny>) -> PyResult<T>,
    ) -> Result<T, core::HookError> {
        Python::attach(|py| {
            let result =
                self.object
                    .call_method1(py, method, (url.as_str(), PyBytes::new(py, data)))?;
            take(result.bind(py))
        })
        .map_err(hook_error)
    }

    /// 调用 `method(url, data)`，返回的须是 `bytes`。
    fn transform(&self, method: &str, url: &Url, data: &[u8]) -> Result<Vec<u8>, core::HookError> {
        self.call(method, url, data, |result| {
            Ok(result.cast::<PyBytes>()?.as_bytes().to_vec())
        })
    }
}

impl core::Hooks for PyHooks {
    fn on_playlist(&self, url: &Url, body: &[u8]) -> Result<Option<String>, core::HookError> {
        if !self.playlist {
            return Ok(None);
        }
        self.call("on_playlist", url, body, |result| {
            result.extract::<String>()
        })
        .map(Some)
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
            url: Address(request.url.clone()),
            headers: Headers(request.headers.clone()),
        };
        let changed = Python::attach(|py| -> PyResult<RequestParts> {
            let result =
                self.object
                    .call_method1(py, "on_request", (Purpose::from(purpose), parts))?;
            Ok(result.extract::<RequestParts>(py)?)
        })
        .map_err(hook_error)?;
        *request = core::RequestParts {
            url: changed.url.0,
            headers: changed.headers.0,
        };
        Ok(())
    }

    fn on_key(&self, url: &Url, data: &[u8]) -> Result<Option<[u8; 16]>, core::HookError> {
        if !self.key {
            return Ok(None);
        }
        let key = self.transform("on_key", url, data)?;
        let length = key.len();
        <[u8; 16]>::try_from(key)
            .map(Some)
            .map_err(|_| -> core::HookError {
                Box::new(InvalidReturn(format!(
                    "on_key 返回的 key 应为 16 字节，实际 {length} 字节"
                )))
            })
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

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Purpose>()?;
    module.add_class::<RequestParts>()?;
    module.add_class::<Hooks>()?;
    Ok(())
}
