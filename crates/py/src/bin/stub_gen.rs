//! 生成 `python/hs_m3u8/_native.pyi` 与 `python/hs_m3u8/__init__.py`（位置取自仓库根的 pyproject.toml）。

fn main() -> pyo3_stub_gen::Result<()> {
    hs_m3u8_py::stub_info()?.generate()
}
