//! 请求：来源、选轨偏好、发请求的方式、输出与任务的配置。构造时按 core 的规则检查各值，不合要求时抛
//! `InvalidInputError`；只有输出路径（相对路径按开始任务时的当前目录补全）在开始任务时检查。

mod http;
mod job;
mod output;
mod select;
mod source;

use pyo3::prelude::*;

pub(crate) use self::http::{HttpOptions, Proxy, RetryPolicy, Timeouts};
pub(crate) use self::job::{JobRequest, KeyOverride, LiveOptions};
pub(crate) use self::output::{OutputOptions, Target, Unsettled};
pub(crate) use self::select::{AudioChoice, AudioKey, Preference, VariantChoice, VariantKey};
pub(crate) use self::source::Source;

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<VariantChoice>()?;
    module.add_class::<VariantKey>()?;
    module.add_class::<AudioChoice>()?;
    module.add_class::<AudioKey>()?;
    module.add_class::<Preference>()?;
    module.add_class::<Timeouts>()?;
    module.add_class::<RetryPolicy>()?;
    module.add_class::<Proxy>()?;
    module.add_class::<HttpOptions>()?;
    module.add_class::<Source>()?;
    module.add_class::<Target>()?;
    module.add_class::<OutputOptions>()?;
    module.add_class::<KeyOverride>()?;
    module.add_class::<LiveOptions>()?;
    module.add_class::<JobRequest>()?;
    module.add_class::<Unsettled>()?;
    Ok(())
}
