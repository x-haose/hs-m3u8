包里的许可证文件对不上所声明许可证（见 scripts/licenses.py 的 `uncovered`）的 crate 补的许可证原文，由
scripts/licenses.py 收录进 LICENSES/THIRD-PARTY-RUST.txt，在那里标为本项目补的。目录名为 `<crate>-<版本>`。
文件取自该版本发布时的提交（包里 `.cargo_vcs_info.json` 记的）在源码仓库根目录的许可证文件（包所在目录没有时）；
源码里也没有的，取 SPDX License List（spdx/license-list-data v3.29.0 的 text/）的标准文本，逐条写明。

Python 扩展模块依赖的：

- ffmpeg-sys-next 9.0.0：包里与源码仓库都没有许可证文件，Cargo.toml 写的是 WTFPL；取同一作者的 ffmpeg-next 9.0.0
  包里的 LICENSE（WTFPL 原文）
- pyo3-stub-gen 0.23.1：Jij-Inc/pyo3-stub-gen@5c1f0ea0cc7c7bccbd32c600b6a0ffd30eeabf93
- rustpython-ast、rustpython-parser、rustpython-parser-core、rustpython-parser-vendored 0.4.0：
  RustPython/Parser@8dd2aea26778d8d6917770f8e32bea1b9cdc0ae8
- unic-char-property、unic-char-range、unic-common、unic-ucd-version 0.9.0：
  open-i18n/rust-unic@5878605364af97a3358368a6eaef02104af2e016
- unic-emoji-char、unic-ucd-ident 0.9.0：open-i18n/rust-unic@8a6ce83063d90b91ae2ce59eddb803edd393fca9
- unicode_names2 1.3.0：许可证为 `(MIT OR Apache-2.0) AND Unicode-DFS-2016`，包里与源码仓库
  （progval/unicode_names2@598db07f37aef3cf8c2d7a1673c43b4989ae2f1f）只有 MIT 与 Apache-2.0 的文件；LICENSE-UNICODE
  为 SPDX 的 Unicode-DFS-2016 标准文本

这些 crate 升级后这里没有新版本的目录、包里的文件又对不上时，scripts/licenses.py 即失败，照上面的办法取新版本的文件。
