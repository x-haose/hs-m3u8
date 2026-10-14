//! Python 与 core 之间转换的值：路径、序列、请求头、计数、时长与地址。传入时在这里校验，存根的写法也在这里定。

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

use hs_m3u8_core as core;
use hs_m3u8_core::Url;
use pyo3::exceptions::{PyOverflowError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyMapping, PyString, PyTuple};
use pyo3_stub_gen::{PyStubType, TypeInfo};

use crate::error::{invalid_input, to_pyerr};

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

/// 序列：交给 Python 的是 `tuple`，改不了，也就没有「改了却没生效」；传入时接受列表、元组等序列（不接受 `str` 与
/// 二进制缓冲区，如 `bytes`、`bytearray`、`memoryview`：它们也是序列，会被拆成一个个字符或整数）。
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
        if is_buffer(object)? {
            return Err(PyTypeError::new_err(
                "应为序列，不能是 bytes、bytearray、memoryview 等二进制缓冲区",
            ));
        }
        object.extract::<Vec<T>>().map(Seq)
    }
}

/// 是二进制缓冲区（`collections.abc.Buffer`，如 `bytes`、`bytearray`、`memoryview`）。
fn is_buffer(object: Borrowed<'_, '_, PyAny>) -> PyResult<bool> {
    static BUFFER: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    object.is_instance(BUFFER.import(object.py(), "collections.abc", "Buffer")?)
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

/// 请求头：按顺序，同名的可以有多个。传入时接受 `Mapping[str, str]` 或 `(名, 值)` 的可迭代对象，交给 Python 的是
/// `tuple[tuple[str, str], ...]`。
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub(crate) struct Headers(pub(crate) Vec<(String, String)>);

impl FromPyObject<'_, '_> for Headers {
    type Error = PyErr;

    fn extract(object: Borrowed<'_, '_, PyAny>) -> PyResult<Self> {
        // str 与二进制缓冲区也能迭代，空的会被当作没有请求头
        if object.is_instance_of::<PyString>() || is_buffer(object)? {
            return Err(PyTypeError::new_err(
                "请求头应为映射或 (名, 值) 的可迭代对象，不能是 str 或 bytes 等二进制缓冲区",
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

/// 传入的计数与下标：负数或超出范围时抛 `InvalidInputError`，不是 PyO3 默认的 `OverflowError`。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Count<T>(pub(crate) T);

/// 传入的至少为 1 的计数：为 0、为负或超出范围时抛 `InvalidInputError`。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Positive<T>(pub(crate) T);

/// 「应为 `min` 到 `max` 的整数」的 `InvalidInputError`。
fn out_of_range(object: Borrowed<'_, '_, PyAny>, min: u8, max: impl std::fmt::Display) -> PyErr {
    match object.repr() {
        Ok(repr) => invalid_input(object.py(), format!("应为 {min} 到 {max} 的整数：{repr}")),
        Err(failure) => failure,
    }
}

macro_rules! counts {
    ($($raw:ty => $nonzero:ty),*) => {$(
        impl Count<$raw> {
            /// 取不小于 `min` 的整数。
            fn at_least(object: Borrowed<'_, '_, PyAny>, min: u8) -> PyResult<$raw> {
                match object.extract::<$raw>() {
                    Ok(value) if value >= <$raw>::from(min) => Ok(value),
                    Ok(_) => Err(out_of_range(object, min, <$raw>::MAX)),
                    Err(failure) if failure.is_instance_of::<PyOverflowError>(object.py()) => {
                        Err(out_of_range(object, min, <$raw>::MAX))
                    }
                    Err(failure) => Err(failure),
                }
            }
        }

        impl FromPyObject<'_, '_> for Count<$raw> {
            type Error = PyErr;

            fn extract(object: Borrowed<'_, '_, PyAny>) -> PyResult<Self> {
                Self::at_least(object, 0).map(Count)
            }
        }

        impl FromPyObject<'_, '_> for Positive<$nonzero> {
            type Error = PyErr;

            fn extract(object: Borrowed<'_, '_, PyAny>) -> PyResult<Self> {
                let value = Count::<$raw>::at_least(object, 1)?;
                Ok(Positive(<$nonzero>::new(value).expect("at_least(1) 保证不为 0")))
            }
        }

        impl<'py> IntoPyObject<'py> for Positive<$nonzero> {
            type Target = PyAny;
            type Output = Bound<'py, PyAny>;
            type Error = PyErr;

            fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
                Ok(self.0.get().into_pyobject(py)?.into_any())
            }
        }

        impl<'py> IntoPyObject<'py> for &Positive<$nonzero> {
            type Target = PyAny;
            type Output = Bound<'py, PyAny>;
            type Error = PyErr;

            fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
                (*self).into_pyobject(py)
            }
        }

        impl PyStubType for Positive<$nonzero> {
            fn type_output() -> TypeInfo {
                <$raw>::type_output()
            }
        }
    )*};
}

counts!(u32 => NonZeroU32, u64 => NonZeroU64, usize => NonZeroUsize);

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

/// 服务器要求的等待（Retry-After）换成整秒，向上取整：按它等不会比要求的短。
pub(crate) fn wait_seconds(wait: Duration) -> u64 {
    wait.as_secs() + u64::from(wait.subsec_nanos() > 0)
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

/// 地址：传入时解析，解析不了抛 `InvalidInputError`（说明里不带原文：解析不了就没法只显示到路径，原样带上可能泄露
/// 令牌）；交给 Python 的是 `str`。
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct Address(pub(crate) Url);

impl Address {
    /// 只到路径（见 `core::bare_url`），`repr` 用它：地址常带令牌。
    pub(crate) fn bare(&self) -> String {
        core::bare_url(&self.0)
    }
}

impl FromPyObject<'_, '_> for Address {
    type Error = PyErr;

    fn extract(object: Borrowed<'_, '_, PyAny>) -> PyResult<Self> {
        let text = object.extract::<String>()?;
        Url::parse(&text)
            .map(Address)
            .map_err(|cause| invalid_input(object.py(), format!("地址无法解析（{cause}）")))
    }
}

impl<'py> IntoPyObject<'py> for &Address {
    type Target = PyString;
    type Output = Bound<'py, PyString>;
    type Error = std::convert::Infallible;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        Ok(PyString::new(py, self.0.as_str()))
    }
}

impl<'py> IntoPyObject<'py> for Address {
    type Target = PyString;
    type Output = Bound<'py, PyString>;
    type Error = std::convert::Infallible;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        (&self).into_pyobject(py)
    }
}

impl PyStubType for Address {
    fn type_output() -> TypeInfo {
        String::type_output()
    }
}

/// 代理地址：同 [`Address`]，另按 `core::Proxy::check_url` 检查 HTTP 库用不用得上，用不上时同样抛
/// `InvalidInputError`。
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProxyAddress(pub(crate) Address);

impl FromPyObject<'_, '_> for ProxyAddress {
    type Error = PyErr;

    fn extract(object: Borrowed<'_, '_, PyAny>) -> PyResult<Self> {
        let address = object.extract::<Address>()?;
        core::Proxy::check_url(&address.0).map_err(|error| to_pyerr(object.py(), error))?;
        Ok(ProxyAddress(address))
    }
}

impl<'py> IntoPyObject<'py> for &ProxyAddress {
    type Target = PyString;
    type Output = Bound<'py, PyString>;
    type Error = std::convert::Infallible;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        (&self.0).into_pyobject(py)
    }
}

impl<'py> IntoPyObject<'py> for ProxyAddress {
    type Target = PyString;
    type Output = Bound<'py, PyString>;
    type Error = std::convert::Infallible;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        self.0.into_pyobject(py)
    }
}

impl PyStubType for ProxyAddress {
    fn type_output() -> TypeInfo {
        String::type_output()
    }
}
