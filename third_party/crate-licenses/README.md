包里没有附许可证文件的 crate 的许可证原文，由 scripts/licenses.py 收录进 LICENSES/THIRD-PARTY-RUST.txt。目录名为
`<crate>-<版本>`，文件取自该版本发布时的提交（包里 `.cargo_vcs_info.json` 记的）在源码仓库根目录的许可证文件：

- pyo3-stub-gen 0.23.1：Jij-Inc/pyo3-stub-gen@5c1f0ea0cc7c7bccbd32c600b6a0ffd30eeabf93
- rustpython-ast、rustpython-parser、rustpython-parser-core、rustpython-parser-vendored 0.4.0：
  RustPython/Parser@8dd2aea26778d8d6917770f8e32bea1b9cdc0ae8
- unic-char-property、unic-char-range、unic-common、unic-ucd-version 0.9.0：
  open-i18n/rust-unic@5878605364af97a3358368a6eaef02104af2e016
- unic-emoji-char、unic-ucd-ident 0.9.0：open-i18n/rust-unic@8a6ce83063d90b91ae2ce59eddb803edd393fca9
- ffmpeg-sys-next 9.0.0：包里与源码仓库都没有许可证文件，Cargo.toml 写的是 WTFPL；取同一作者的 ffmpeg-next 9.0.0
  包里的 LICENSE（WTFPL 原文）

这些 crate 升级后这里没有新版本的目录，scripts/licenses.py 即失败，照上面的办法取新版本的文件。
