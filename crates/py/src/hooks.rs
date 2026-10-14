//! 站点适配回调：Python 继承 `Hooks` 并覆盖需要的方法；core 在阻塞线程池里取得 GIL 后调用，没覆盖的方法不调用、
//! 不取 GIL、不进阻塞线程池。

use std::fmt;
use std::sync::Arc;

use hs_m3u8_core as core;
use hs_m3u8_core::Url;
use pyo3::exceptions::{PyBaseException, PyTypeError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyBytes, PyDict, PyString, PyTuple, PyWeakrefMethods, PyWeakrefReference};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};

use crate::convert::{Address, Headers};
use crate::record::{field_names, reduce, variants};

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
    /// 请求的地址；读到的是解析后的规范写法（主机名小写、补上路径 `/`、百分号编码等），可能与传入的不同
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
        reduce(slf.as_any(), field_names!(RequestParts { url, headers }))
    }
}

/// 站点适配回调的基类：继承它并覆盖需要的方法，没覆盖的不调用。回调在后台线程里执行
/// （不在事件循环里），同一个对象的回调会在多个线程上同时被调用（同时下载的分片各占一个线程），有状态的回调要自己
/// 加锁。回调出错使任务失败、不重试（见 `HookError`）。取消不会打断正在执行的回调，任务等它返回后才结束；解释器
/// 退出时也等它返回，所以回调里的网络请求要设超时。
///
/// 每次调用都在新的 Python 线程状态里：`threading.local` 与 `contextvars` 里存的东西不留到下一次调用，要跨调用复用
/// （如每线程一个 HTTP 会话）就存在对象上并自己加锁。回调持着 GIL 时事件循环与其他回调都要等，耗时的计算放进放开
/// GIL 的调用里。`Job` 持有任务所用的回调对象，回调对象可以引用它的 `Job`（如在回调里调用 `Job.stop`）：两者不再被
/// 其他地方引用时，在垃圾回收器回收这个环时一起被回收、任务随之取消（时间不确定，要立即停止就调用 `Job.cancel`）。
/// 子类定义 `__slots__` 时不要写 `__weakref__`：本类已有。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", subclass, frozen, weakref)]
pub struct Hooks;

#[gen_stub_pymethods]
#[pymethods]
impl Hooks {
    /// 子类可以定义带参数的 `__init__`，这里的参数一概不用（由 `__init__` 检查）。
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

    // 签名与构造函数相同：类型检查器与 stubtest 按此核对
    /// 不收参数：没定义 `__init__` 的子类传了参数时抛 `TypeError`。
    #[pyo3(signature = (*args, **kwargs))]
    fn __init__(
        slf: &Bound<'_, Self>,
        #[gen_stub(override_type(type_repr = "typing.Any", imports = ("typing",)))] args: &Bound<
            '_,
            PyTuple,
        >,
        #[gen_stub(override_type(type_repr = "typing.Any", imports = ("typing",)))] kwargs: Option<
            &Bound<'_, PyDict>,
        >,
    ) -> PyResult<()> {
        if args.is_empty() && kwargs.is_none_or(|kwargs| kwargs.is_empty()) {
            return Ok(());
        }
        Err(PyTypeError::new_err(format!(
            "{}() 不收参数；要收参数就在子类里定义 __init__",
            slf.get_type().qualname()?
        )))
    }

    /// 改写拉到的播放列表（如解密、转码），返回改写后的文本；返回 None 表示不改，与没覆盖一样按 UTF-8 解析拉到的
    /// 内容（基类的实现即如此，只改部分地址时其余的交给 `super()`）。`url` 为重定向之后的地址。
    #[pyo3(signature = (url, body, /))]
    fn on_playlist(&self, url: String, body: Bound<'_, PyBytes>) -> Option<String> {
        let _ = (url, body);
        None
    }

    /// 发出请求前修改地址与请求头（如签名），返回改过的请求（`RequestParts` 不可变，改时新建一个）。每次尝试（含
    /// 重试）调用一次，在拿到引擎的请求名额之后、调完立即发出，签名不在排队中过期；改后的请求不合法（不是
    /// http/https，请求头放不进请求或是本库控制的头，见 `HttpOptions`）时任务失败。
    #[pyo3(signature = (purpose, request, /))]
    fn on_request(&self, purpose: Purpose, request: RequestParts) -> RequestParts {
        let _ = purpose;
        request
    }

    /// 变换拉到的 key（如站点自定义的 key 加密），返回 16 字节的 key，长度不对时任务失败（`HookError`）；返回 None
    /// 表示不改，与没覆盖一样用拉到的 key，它不是 16 字节时为 `KeyLengthError`（基类的实现即如此）。`url` 为播放列表
    /// 中的 key 地址。
    #[pyo3(signature = (url, data, /))]
    fn on_key<'py>(&self, url: String, data: Bound<'py, PyBytes>) -> Option<Bound<'py, PyBytes>> {
        let _ = (url, data);
        None
    }

    /// 在解密之前变换 init 段（EXT-X-MAP），同 `on_segment`；`url` 为播放列表中的 init 段地址。
    #[pyo3(signature = (url, data, /))]
    fn on_init<'py>(&self, url: String, data: Bound<'py, PyBytes>) -> Bound<'py, PyBytes> {
        let _ = url;
        data
    }

    /// 在解密之前变换分片（如去掉伪装成图片的前缀字节），返回变换后的内容。`url` 为播放列表中的分片地址；init 段经
    /// `on_init`，空的响应不经过此回调。重试时同一个分片会再经过它，`data` 可能没写完。
    #[pyo3(signature = (url, data, /))]
    fn on_segment<'py>(&self, url: String, data: Bound<'py, PyBytes>) -> Bound<'py, PyBytes> {
        let _ = url;
        data
    }
}

/// 回调抛出的 Python 异常（或返回值的类型不对时提取出错的异常），原样放进 core 的 `Error::Hook`，翻译为 `HookError`
/// 时取回作 `__cause__`。存异常对象而不是 `PyErr`：垃圾回收器要经 `Job.__traverse__` 找到它。
#[derive(Debug)]
pub(crate) struct PyHookError(pub(crate) Py<PyBaseException>);

impl fmt::Display for PyHookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Python::attach(|py| fmt::Display::fmt(self.0.bind(py), f))
    }
}

impl std::error::Error for PyHookError {}

fn hook_error(py: Python<'_>, failure: PyErr) -> core::HookError {
    Box::new(PyHookError(failure.into_value(py)))
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

/// core 的回调：只调用 Python 子类覆盖了的方法。对回调对象只持弱引用，由 `Job` 持有并报给垃圾回收器、探测时由协程
/// 持有到探测结束：回调对象常引用它的 `Job`，core 持强引用时这个环回收不了。
struct PyHooks {
    weak: Py<PyWeakrefReference>,
    playlist: bool,
    request: bool,
    key: bool,
    init: bool,
    segment: bool,
}

/// 把 Python 的 `Hooks` 实例接到 core。
pub(crate) fn to_core(object: &Bound<'_, Hooks>) -> PyResult<Arc<dyn core::Hooks>> {
    static GETATTR_STATIC: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    let py = object.py();
    let getattr_static = GETATTR_STATIC.import(py, "inspect", "getattr_static")?;
    let base = py.get_type::<Hooks>();
    // 按调用时的查找顺序（实例、类、各基类）取出原物，不经描述符、不比较值：是基类的那个即没覆盖
    let overridden = |name: &str| -> PyResult<bool> {
        let found = getattr_static.call1((object, name))?;
        Ok(!found.is(getattr_static.call1((&base, name))?))
    };
    Ok(Arc::new(PyHooks {
        weak: PyWeakrefReference::new(object.as_any())?.unbind(),
        playlist: overridden("on_playlist")?,
        request: overridden("on_request")?,
        key: overridden("on_key")?,
        init: overridden("on_init")?,
        segment: overridden("on_segment")?,
    }))
}

/// 回调对象已被回收：只在持有它的 `Job` 被回收（任务随之取消）之后，还在收尾的任务调用回调时发生，结果没人取。
#[derive(Debug)]
struct Collected;

impl fmt::Display for Collected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("回调对象已被回收（任务已随 Job 被回收而取消）")
    }
}

impl std::error::Error for Collected {}

impl PyHooks {
    /// 回调对象；已被回收时为 [`Collected`]。
    fn object<'py>(&self, py: Python<'py>) -> Result<Bound<'py, PyAny>, core::HookError> {
        self.weak
            .bind(py)
            .upgrade()
            .ok_or_else(|| Box::new(Collected) as core::HookError)
    }

    /// 调用 `method(url, data)`，用 `take` 取回结果；返回值的类型不对时报 `take` 抛的异常（取 `bytes` 时用
    /// `cast`，不接受 bytearray 等可以转换的类型）。
    fn call<T>(
        &self,
        method: &str,
        url: &Url,
        data: &[u8],
        take: impl FnOnce(&Bound<'_, PyAny>) -> PyResult<T>,
    ) -> Result<T, core::HookError> {
        Python::attach(|py| {
            self.object(py)?
                .call_method1(method, (url.as_str(), PyBytes::new(py, data)))
                .and_then(|result| take(&result))
                .map_err(|failure| hook_error(py, failure))
        })
    }
}

impl core::Hooks for PyHooks {
    fn implements(&self, hook: core::HookKind) -> bool {
        match hook {
            core::HookKind::Playlist => self.playlist,
            core::HookKind::Request => self.request,
            core::HookKind::Key => self.key,
            core::HookKind::Init => self.init,
            core::HookKind::Segment => self.segment,
        }
    }

    fn on_playlist(&self, url: &Url, body: &[u8]) -> Result<Option<String>, core::HookError> {
        if !self.playlist {
            return Ok(None);
        }
        self.call("on_playlist", url, body, |result| {
            result.extract::<Option<String>>()
        })
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
        let changed = Python::attach(|py| {
            self.object(py)?
                .call_method1("on_request", (Purpose::from(purpose), parts))
                .and_then(|result| Ok(result.extract::<RequestParts>()?))
                .map_err(|failure| hook_error(py, failure))
        })?;
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
        let key = self.call("on_key", url, data, |result| {
            if result.is_none() {
                return Ok(None);
            }
            Ok(Some(result.cast::<PyBytes>()?.as_bytes().to_vec()))
        })?;
        let Some(key) = key else {
            return Ok(None);
        };
        let length = key.len();
        <[u8; 16]>::try_from(key)
            .map(Some)
            .map_err(|_| -> core::HookError {
                Box::new(InvalidReturn(format!(
                    "on_key 返回的 key 应为 16 字节，实际 {length} 字节"
                )))
            })
    }

    fn on_init(&self, url: &Url, data: Vec<u8>) -> Result<Vec<u8>, core::HookError> {
        if !self.init {
            return Ok(data);
        }
        self.call("on_init", url, &data, |result| {
            Ok(result.cast::<PyBytes>()?.as_bytes().to_vec())
        })
    }

    fn on_segment(&self, url: &Url, data: Vec<u8>) -> Result<Vec<u8>, core::HookError> {
        if !self.segment {
            return Ok(data);
        }
        self.call("on_segment", url, &data, |result| {
            Ok(result.cast::<PyBytes>()?.as_bytes().to_vec())
        })
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
