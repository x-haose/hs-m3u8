//! Python 与 core 之间换手的值：路径、序列、请求头、计数、时长与地址。传入时在这里校验，存根的写法也在这里定。

use std::path::PathBuf;
use std::time::Duration;

use hs_m3u8_core as core;
use hs_m3u8_core::Url;
use pyo3::exceptions::{PyOverflowError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyMapping, PyString, PyTuple};
use pyo3_stub_gen::{PyStubType, TypeInfo};

use crate::error::invalid_input;

/// 路径：传入时接受 `str | os.PathLike[str]`，返回 `pathlib.Path`。不直接用 `PathBuf`：存根生成把它的入参写成
/// 不带类型参数的 `os.PathLike`。
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

/// 序列：交给 Python 的是 `tuple`，改不了，也就没有「改了却没生效」；传入时接受列表、元组等序列（不接受 `str`）。
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct Seq<T>(pub(crate) Vec<T>);

impl<T> Default for Seq<T> {
    fn default() -> Self {
        Seq(Vec::new())
    }
}

impl<T> FromIterator<T> for Seq<T> {
    fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Self {
        Seq(items.into_iter().collect())
    }
}

impl<'py, T> FromPyObject<'_, 'py> for Seq<T>
where
    T: FromPyObjectOwned<'py>,
{
    type Error = PyErr;

    fn extract(object: Borrowed<'_, 'py, PyAny>) -> PyResult<Self> {
        object.extract::<Vec<T>>().map(Seq)
    }
}

impl<'py, T> IntoPyObject<'py> for Seq<T>
where
    T: IntoPyObject<'py>,
{
    type Target = PyTuple;
    type Output = Bound<'py, PyTuple>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        PyTuple::new(py, self.0)
    }
}

impl<'a, 'py, T> IntoPyObject<'py> for &'a Seq<T>
where
    &'a T: IntoPyObject<'py>,
{
    type Target = PyTuple;
    type Output = Bound<'py, PyTuple>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        PyTuple::new(py, &self.0)
    }
}

impl<T: PyStubType> PyStubType for Seq<T> {
    fn type_output() -> TypeInfo {
        let item = T::type_output();
        let mut import = item.import;
        import.insert("builtins".into());
        TypeInfo {
            name: format!("builtins.tuple[{}, ...]", item.name),
            source_module: None,
            import,
            type_refs: item.type_refs,
        }
    }

    fn type_input() -> TypeInfo {
        let item = T::type_input();
        let mut import = item.import;
        import.insert("collections.abc".into());
        TypeInfo {
            name: format!("collections.abc.Sequence[{}]", item.name),
            source_module: None,
            import,
            type_refs: item.type_refs,
        }
    }
}

/// 请求头：按顺序，同名的可以有多个。传入时接受 `Mapping[str, str]` 或 `(名, 值)` 的序列，交给 Python 的是
/// `tuple[tuple[str, str], ...]`。
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub(crate) struct Headers(pub(crate) Vec<(String, String)>);

impl FromPyObject<'_, '_> for Headers {
    type Error = PyErr;

    fn extract(object: Borrowed<'_, '_, PyAny>) -> PyResult<Self> {
        // str 与 bytes 也能迭代，空串会被当作没有请求头
        if object.is_instance_of::<PyString>() || object.is_instance_of::<PyBytes>() {
            return Err(PyTypeError::new_err(
                "请求头应为映射或 (名, 值) 的序列，不能是 str 或 bytes",
            ));
        }
        if let Ok(mapping) = object.cast::<PyMapping>() {
            return mapping.items()?.extract().map(Headers);
        }
        object
            .try_iter()?
            .map(|item| item?.extract::<(String, String)>())
            .collect::<PyResult<_>>()
            .map(Headers)
    }
}

impl<'py> IntoPyObject<'py> for &Headers {
    type Target = PyTuple;
    type Output = Bound<'py, PyTuple>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        PyTuple::new(py, &self.0)
    }
}

impl PyStubType for Headers {
    fn type_output() -> TypeInfo {
        Seq::<(String, String)>::type_output()
    }

    fn type_input() -> TypeInfo {
        let pair = <(String, String)>::type_input();
        let mut import = pair.import;
        import.insert("collections.abc".into());
        TypeInfo {
            name: format!(
                "collections.abc.Mapping[builtins.str, builtins.str] | collections.abc.Iterable[{}]",
                pair.name
            ),
            source_module: None,
            import,
            type_refs: pair.type_refs,
        }
    }
}

impl Headers {
    /// 各请求头的名字（`repr` 只显示名字：值常带 Cookie 与令牌）。
    pub(crate) fn names(&self) -> Vec<&str> {
        self.0.iter().map(|(name, _)| name.as_str()).collect()
    }
}

/// 传入的计数与下标：负数或超出范围时抛 `InvalidInputError`，与「该至少为 1 而为 0」同类，不是 PyO3 默认的
/// `OverflowError`。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Count<T>(pub T);

macro_rules! count {
    ($($ty:ty),*) => {$(
        impl FromPyObject<'_, '_> for Count<$ty> {
            type Error = PyErr;

            fn extract(object: Borrowed<'_, '_, PyAny>) -> PyResult<Self> {
                match object.extract::<$ty>() {
                    Ok(value) => Ok(Count(value)),
                    Err(failure) if failure.is_instance_of::<PyOverflowError>(object.py()) => {
                        Err(invalid_input(
                            object.py(),
                            format!("应为 0 到 {} 的整数：{}", <$ty>::MAX, object.repr()?),
                        ))
                    }
                    Err(failure) => Err(failure),
                }
            }
        }
    )*};
}

count!(u32, usize);

impl<'py, T: IntoPyObject<'py>> IntoPyObject<'py> for Count<T> {
    type Target = T::Target;
    type Output = T::Output;
    type Error = T::Error;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        self.0.into_pyobject(py)
    }
}

impl<'py, T: Copy + IntoPyObject<'py>> IntoPyObject<'py> for &Count<T> {
    type Target = T::Target;
    type Output = T::Output;
    type Error = T::Error;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        self.0.into_pyobject(py)
    }
}

impl<T: PyStubType> PyStubType for Count<T> {
    fn type_output() -> TypeInfo {
        T::type_output()
    }

    fn type_input() -> TypeInfo {
        T::type_input()
    }
}

/// 传入的时长（`datetime.timedelta`）：为负时抛 `InvalidInputError`，不是 PyO3 默认的 `ValueError`。
#[derive(Clone, Copy)]
pub(crate) struct Span(pub(crate) Duration);

impl FromPyObject<'_, '_> for Span {
    type Error = PyErr;

    fn extract(object: Borrowed<'_, '_, PyAny>) -> PyResult<Self> {
        match object.extract::<Duration>() {
            Ok(duration) => Ok(Span(duration)),
            Err(failure) if failure.is_instance_of::<PyValueError>(object.py()) => Err(
                invalid_input(object.py(), format!("时长不能为负：{}", object.repr()?)),
            ),
            Err(failure) => Err(failure),
        }
    }
}

impl PyStubType for Span {
    fn type_output() -> TypeInfo {
        Duration::type_output()
    }
}

/// 地址只到路径（见 `core::bare_url`）；解析不了时整个隐去。
pub(crate) fn bare_url(url: &str) -> String {
    match Url::parse(url) {
        Ok(url) => core::bare_url(&url),
        Err(_) => "<无法解析的地址>".to_owned(),
    }
}
