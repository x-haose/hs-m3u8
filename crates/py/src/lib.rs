//! hs-m3u8 的 Python 绑定：把 core 的类型逐个翻译成 Python 类，放在模块 `hs_m3u8._native`，由 Python 包 `hs_m3u8`
//! 再导出。类型存根由 `stub_gen` 从这里的定义生成。
//!
//! 默认值是整数、时长或 `Some(...)` 的构造函数（`Engine`、`Timeouts`、`RetryPolicy`、`LiveOptions`、`JobRequest`），
//! 存根里的签名手写在类型旁边，默认值写 `...`：PyO3 的运行时签名把这类默认值显示为 `...`，生成的存根却写出具体的值
//! （时长写成 `datetime....`，语法都不对），stubtest 核对不过。其余不是字面量的默认值两边都写 `...`，不必手写。

mod convert;
mod engine;
mod error;
mod hooks;
mod info;
mod job;
mod record;
mod report;
mod request;
mod runtime;

use pyo3::prelude::*;

// 没在自由线程版上测过：导入时让解释器重新开启 GIL（解释器同时给出警告）
#[pymodule(gil_used = true)]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    engine::register(module)?;
    error::register(module)?;
    hooks::register(module)?;
    info::register(module)?;
    job::register(module)?;
    report::register(module)?;
    request::register(module)?;
    runtime::register(module)?;
    Ok(())
}

pyo3_stub_gen::reexport_module_members!("hs_m3u8", "hs_m3u8._native");
// `download` 写成 Python 的协程（python/hs_m3u8/_download.py）：它是最常用的入口，忘了 await 时 Python 给出「never
// awaited」警告，PyO3 的协程没有这个警告
pyo3_stub_gen::reexport_module_members!("hs_m3u8" from "hs_m3u8._download"; "download");
pyo3_stub_gen::reexport_module_members!("hs_m3u8" from "hs_m3u8._version"; "__version__");

/// 生成存根所需的类型信息；pyproject.toml 在仓库根。
pub fn stub_info() -> pyo3_stub_gen::Result<pyo3_stub_gen::StubInfo> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    pyo3_stub_gen::StubInfo::from_pyproject_toml(root.join("pyproject.toml"))
}
