//! 来源：地址、选轨偏好、发请求的方式与站点适配回调。

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use hs_m3u8_core as core;
use pyo3::PyTraverseError;
use pyo3::gc::PyVisit;
use pyo3::prelude::*;
use pyo3::types::{PyString, PyTuple};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};

use super::{HttpOptions, Preference};
use crate::convert::Address;
use crate::error::to_pyerr;
use crate::hooks::{self, Hooks};
use crate::record::{field_names, reduce};

/// 来源与访问它的方式；探测（`Engine.probe`）与下载共用。按值比较，回调按对象身份比较。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen, eq, hash, get_all)]
pub struct Source {
    /// 主播放列表或媒体播放列表的地址，只支持 http/https；读到的是解析后的规范写法（主机名小写、补上路径 `/`、百分号编
    /// 码等），可能与传入的不同。续传时可以与上次的不同（换了令牌、主机或路径）：续传先核对这次取到的与已下载的是同一份
    /// 内容：点播每条轨已完成的字节数最多的至多 16 个不连续段组，各取已完成的分片里字节数最大的重新下载（取不到的换次大
    /// 的），与已存的逐字节比较，分片都已下完、地址与所选的轨也与上次相同时不核对；直播见 `LiveOptions`。不是时报
    /// `WorkDirProblem.SourceUnverified`。点播的轨 init 段内容变了时，已完成的分片不沿用，重新下载
    pub url: Address,
    /// 选轨偏好
    pub preference: Preference,
    /// 发请求的方式
    pub http: HttpOptions,
    /// 站点适配回调；None 时不做改动
    pub hooks: Option<Py<Hooks>>,
}

impl Source {
    /// 回调对象的身份：回调一般不定义按值比较。
    fn hooks_identity(&self) -> Option<*mut pyo3::ffi::PyObject> {
        self.hooks.as_ref().map(Py::as_ptr)
    }
}

impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
            && self.preference == other.preference
            && self.http == other.http
            && self.hooks_identity() == other.hooks_identity()
    }
}

impl Eq for Source {}

impl Hash for Source {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.url.hash(state);
        self.preference.hash(state);
        self.http.hash(state);
        self.hooks_identity().hash(state);
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Source {
    /// `preference`、`http` 默认取 `Preference()`、`HttpOptions()`；`url` 解析不了或不是 http/https 时抛
    /// `InvalidInputError`。
    #[new]
    #[pyo3(signature = (
        url,
        *,
        preference = Preference::from(core::hls::Preference::default()),
        http = HttpOptions::from(core::HttpOptions::default()),
        hooks = None,
    ))]
    fn new(
        py: Python<'_>,
        url: Address,
        preference: Preference,
        http: HttpOptions,
        hooks: Option<Py<Hooks>>,
    ) -> PyResult<Self> {
        core::Source::check_url(&url.0).map_err(|error| to_pyerr(py, error))?;
        Ok(Source {
            url,
            preference,
            http,
            hooks,
        })
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let hooks = match &self.hooks {
            Some(hooks) => hooks.bind(py).repr()?.to_string(),
            None => "None".to_owned(),
        };
        Ok(format!(
            "Source(url={}, preference={}, http={}, hooks={hooks})",
            PyString::new(py, &self.url.bare()).repr()?,
            Bound::new(py, self.preference.clone())?.repr()?,
            Bound::new(py, self.http.clone())?.repr()?,
        ))
    }

    #[gen_stub(skip)]
    fn __reduce__<'py>(
        slf: &Bound<'py, Self>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyTuple>)> {
        reduce(
            slf.as_any(),
            field_names!(Source {
                url,
                preference,
                http,
                hooks,
            }),
        )
    }

    /// 回调对象常引用来源（如存着用到它的请求），引用环要靠垃圾回收器经这里找到。
    #[gen_stub(skip)]
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.hooks)
    }
}

impl Source {
    /// 换成 core 的来源；接上回调时读取它的属性，出错时抛那个异常。
    pub(crate) fn to_core(&self, py: Python<'_>) -> PyResult<core::Source> {
        let hooks: Arc<dyn core::Hooks> = match &self.hooks {
            Some(hooks) => hooks::to_core(hooks.bind(py))?,
            None => Arc::new(core::NoHooks),
        };
        Ok(core::Source {
            url: self.url.0.clone(),
            preference: self.preference.to_core(),
            http: self.http.to_core(),
            hooks,
        })
    }
}
