//! 路径：传入时接受 `str | os.PathLike[str]`，返回 `pathlib.Path`。存根按这两种写法生成。

use std::path::PathBuf;

use pyo3::prelude::*;
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
