//! 定义对外类型的宏：不可变、可比较、可哈希的数据类与枚举，带按字段列出的 `__repr__`，可以 pickle 与 copy。
//!
//! pickle 与 copy 经 `__reduce__` 按关键字调用构造函数重建（见 [`partial`]），所以每个类都有构造函数。

use std::ops::BitOr;

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyString, PyTuple};
use pyo3_stub_gen::TypeInfo;

use crate::convert::bare_url;

/// 数据类：字段只读（同名属性）。构造函数各参数只能按关键字传、都必须给；要默认值或校验的写 `@custom_new`，自己定义
/// 构造函数。
macro_rules! record {
    (
        @custom_new
        $(#[$meta:meta])*
        pub struct $name:ident {
            $(
                $(#[$field_meta:meta])*
                pub $field:ident : $ty:ty
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[::pyo3_stub_gen::derive::gen_stub_pyclass]
        #[::pyo3::pyclass(module = "hs_m3u8._native", frozen, eq, hash, get_all, from_py_object)]
        #[derive(Clone, PartialEq, Eq, Hash)]
        pub struct $name { $( $(#[$field_meta])* pub $field: $ty ),* }

        #[::pyo3_stub_gen::derive::gen_stub_pymethods]
        #[::pyo3::pymethods]
        impl $name {
            fn __repr__(slf: &::pyo3::Bound<'_, Self>) -> ::pyo3::PyResult<String> {
                $crate::record::fields_repr(slf.as_any(), stringify!($name), &[$(stringify!($field)),*])
            }

            #[gen_stub(skip)]
            fn __reduce__<'py>(
                slf: &::pyo3::Bound<'py, Self>,
            ) -> ::pyo3::PyResult<(::pyo3::Bound<'py, ::pyo3::PyAny>, ::pyo3::Bound<'py, ::pyo3::types::PyTuple>)> {
                $crate::record::reduce(slf.as_any(), &[$(stringify!($field)),*])
            }
        }
    };
    (
        $(#[$meta:meta])*
        pub struct $name:ident {
            $(
                $(#[$field_meta:meta])*
                pub $field:ident : $ty:ty
            ),* $(,)?
        }
    ) => {
        $crate::record::record! {
            @custom_new
            $(#[$meta])*
            pub struct $name { $( $(#[$field_meta])* pub $field: $ty ),* }
        }

        #[::pyo3_stub_gen::derive::gen_stub_pymethods]
        #[::pyo3::pymethods]
        impl $name {
            /// 各参数即同名属性。
            #[new]
            #[pyo3(signature = (*, $($field),*))]
            fn new($($field: $ty),*) -> Self {
                $name { $($field),* }
            }
        }
    };
}

/// 枚举：每种取值是一个子类（不带数据的也是，如 `Stage.Writing()`），用 `match` 或 `isinstance` 分辨。存根里，属性与
/// 返回值的类型写成各取值类的并集（如 `Stage.Preparing | ... | Stage.Done`），类型检查器才能判断 `match` 是否穷尽；
/// 参数的类型写基类，接受任一取值。
macro_rules! variants {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident {
                    $(
                        $(#[$field_meta:meta])*
                        $field:ident : $ty:ty
                    ),* $(,)?
                }
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[::pyo3_stub_gen::derive::gen_stub_pyclass_complex_enum(skip_stub_type)]
        #[::pyo3::pyclass(module = "hs_m3u8._native", frozen, eq, hash, from_py_object)]
        #[derive(Clone, PartialEq, Eq, Hash)]
        pub enum $name {
            $(
                $(#[$variant_meta])*
                $variant { $( $(#[$field_meta])* $field: $ty ),* }
            ),*
        }

        impl ::pyo3_stub_gen::PyStubType for $name {
            fn type_output() -> ::pyo3_stub_gen::TypeInfo {
                $crate::record::union(&[$(concat!(stringify!($name), ".", stringify!($variant))),*])
            }

            fn type_input() -> ::pyo3_stub_gen::TypeInfo {
                ::pyo3_stub_gen::TypeInfo::unqualified(stringify!($name))
            }
        }

        #[::pyo3_stub_gen::derive::gen_stub_pymethods]
        #[::pyo3::pymethods]
        impl $name {
            fn __repr__(slf: &::pyo3::Bound<'_, Self>) -> ::pyo3::PyResult<String> {
                $crate::record::variant_repr(slf.as_any())
            }

            #[gen_stub(skip)]
            fn __reduce__<'py>(
                slf: &::pyo3::Bound<'py, Self>,
            ) -> ::pyo3::PyResult<(::pyo3::Bound<'py, ::pyo3::PyAny>, ::pyo3::Bound<'py, ::pyo3::types::PyTuple>)> {
                let fields = $crate::record::variant_fields(slf.as_any())?;
                let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
                $crate::record::reduce(slf.as_any(), &fields)
            }
        }
    };
}

pub(crate) use {record, variants};

/// 各取值类的并集，如 `Stage.Preparing | Stage.Done`；都定义在本模块里，存根里直接写类名。
pub(crate) fn union(variants: &[&str]) -> TypeInfo {
    variants
        .iter()
        .map(|variant| TypeInfo::unqualified(variant))
        .reduce(BitOr::bitor)
        .expect("枚举至少有一种取值")
}

/// `名字(字段=repr, ...)`，按 `fields` 的顺序。名为 `url` 的字段是地址，常带令牌，只显示到路径（属性本身是完整的）。
pub(crate) fn fields_repr(
    object: &Bound<'_, PyAny>,
    name: &str,
    fields: &[&str],
) -> PyResult<String> {
    let mut parts = Vec::with_capacity(fields.len());
    for field in fields {
        let value = object.getattr(*field)?;
        let shown = if *field == "url" {
            PyString::new(object.py(), &bare_url(&value.extract::<String>()?)).repr()?
        } else {
            value.repr()?
        };
        parts.push(format!("{field}={shown}"));
    }
    Ok(format!("{name}({})", parts.join(", ")))
}

/// 枚举取值的 `__repr__`：`类型.取值(字段=repr, ...)`。
pub(crate) fn variant_repr(object: &Bound<'_, PyAny>) -> PyResult<String> {
    let name = object.get_type().qualname()?.to_string();
    let fields = variant_fields(object)?;
    let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
    fields_repr(object, &name, &fields)
}

/// 枚举取值的字段，取自取值类的 `__match_args__`。
pub(crate) fn variant_fields(object: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    object.get_type().getattr("__match_args__")?.extract()
}

/// 数据类与枚举的 `__reduce__`：`(partial(...), ())`，见 [`partial`]。
pub(crate) fn reduce<'py>(
    object: &Bound<'py, PyAny>,
    fields: &[&str],
) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyTuple>)> {
    Ok((partial(object, fields)?, PyTuple::empty(object.py())))
}

/// `functools.partial(type(object), 字段=值, ...)`：再按位置补上其余参数调用即重建 `object`。
pub(crate) fn partial<'py>(
    object: &Bound<'py, PyAny>,
    fields: &[&str],
) -> PyResult<Bound<'py, PyAny>> {
    static PARTIAL: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    let py = object.py();
    let partial = PARTIAL.import(py, "functools", "partial")?;
    let kwargs = PyDict::new(py);
    for field in fields {
        kwargs.set_item(*field, object.getattr(*field)?)?;
    }
    partial.call((object.get_type(),), Some(&kwargs))
}
