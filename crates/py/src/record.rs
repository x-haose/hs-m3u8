//! 定义对外类型的宏：不可变、可比较、可哈希的数据类与枚举，带按字段列出的 `__repr__`。

use pyo3::prelude::*;
use pyo3::types::PyTuple;

/// 数据类：字段只读（同名属性）。
macro_rules! record {
    (
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
        }
    };
}

/// 枚举：每种取值是一个子类（不带数据的也是，如 `Stage.Writing()`），用 `match` 或 `isinstance` 分辨。
macro_rules! variants {
    (
        $(#[$meta:meta])*
        pub enum $name:ident { $($body:tt)* }
    ) => {
        $(#[$meta])*
        #[::pyo3_stub_gen::derive::gen_stub_pyclass_complex_enum]
        #[::pyo3::pyclass(module = "hs_m3u8._native", frozen, eq, hash, from_py_object)]
        #[derive(Clone, PartialEq, Eq, Hash)]
        pub enum $name { $($body)* }

        #[::pyo3_stub_gen::derive::gen_stub_pymethods]
        #[::pyo3::pymethods]
        impl $name {
            fn __repr__(slf: &::pyo3::Bound<'_, Self>) -> ::pyo3::PyResult<String> {
                $crate::record::variant_repr(slf.as_any())
            }
        }
    };
}

pub(crate) use {record, variants};

/// `名字(字段=repr, ...)`，按 `fields` 的顺序。
pub(crate) fn fields_repr(
    object: &Bound<'_, PyAny>,
    name: &str,
    fields: &[&str],
) -> PyResult<String> {
    let mut parts = Vec::with_capacity(fields.len());
    for field in fields {
        parts.push(format!("{field}={}", object.getattr(*field)?.repr()?));
    }
    Ok(format!("{name}({})", parts.join(", ")))
}

/// 枚举取值的 `__repr__`：`类型.取值(字段=repr, ...)`，字段取自取值类的 `__match_args__`。
pub(crate) fn variant_repr(object: &Bound<'_, PyAny>) -> PyResult<String> {
    let variant = object.get_type();
    let name = variant.qualname()?.to_string();
    let fields = variant.getattr("__match_args__")?.cast_into::<PyTuple>()?;
    let fields: Vec<String> = fields.extract()?;
    let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
    fields_repr(object, &name, &fields)
}
