//! hs-m3u8 的 Python 绑定：把 core 的类型逐个翻译成 Python 类，放在模块 `hs_m3u8._native`，由 Python 包 `hs_m3u8`
//! 再导出。类型存根由 `stub_gen` 从这里的定义生成。

mod engine;
mod error;
mod hooks;
mod info;
mod job;
mod path;
mod record;
mod report;
mod request;

use pyo3::prelude::*;

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    engine::register(module)?;
    error::register(module)?;
    hooks::register(module)?;
    info::register(module)?;
    job::register(module)?;
    report::register(module)?;
    request::register(module)?;
    Ok(())
}

pyo3_stub_gen::reexport_module_members!("hs_m3u8", "hs_m3u8._native");

/// 生成存根所需的类型信息；pyproject.toml 在仓库根。
pub fn stub_info() -> pyo3_stub_gen::Result<pyo3_stub_gen::StubInfo> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    pyo3_stub_gen::StubInfo::from_pyproject_toml(root.join("pyproject.toml"))
}
