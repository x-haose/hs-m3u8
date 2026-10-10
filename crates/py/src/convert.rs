//! Python 与 core 之间换手的值：路径、请求头与地址。

use std::path::PathBuf;

use hs_m3u8_core as core;
use hs_m3u8_core::Url;
use pyo3::prelude::*;
use pyo3::types::PyMapping;
use pyo3_stub_gen::{PyStubType, TypeInfo};

/// 对外的路径字段与参数用它，不直接用 `PathBuf`：存根生成把 `PathBuf` 的入参写成不带类型参数的 `os.PathLike`。
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct PyPath(pub(crate) PathBuf);

impl From<PathBuf> for PyPath {
    fn from(path: PathBuf) -> Self {
        PyPath(path)
    }
}

impl FromPyObject<'_, '_> for PyPath {
    type Error = PyErr;

    fn extract(object: Borrowed<'_, '_, PyAny>) -> PyResult<Self> {
        object.extract::<PathBuf>().map(PyPath)
    }
}

impl<'py> IntoPyObject<'py> for PyPath {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        self.0.into_pyobject(py)
    }
}

impl<'py> IntoPyObject<'py> for &PyPath {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        (&self.0).into_pyobject(py)
    }
}

impl PyStubType for PyPath {
    fn type_output() -> TypeInfo {
        TypeInfo::with_module("pathlib.Path", "pathlib".into())
    }

    fn type_input() -> TypeInfo {
        TypeInfo::builtin("str") | TypeInfo::with_module("os.PathLike[str]", "os".into())
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
