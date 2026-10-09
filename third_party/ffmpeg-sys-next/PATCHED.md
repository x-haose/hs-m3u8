# ffmpeg-sys-next 9.0.0（本地修改版）

来源：crates.io 上的 ffmpeg-sys-next 9.0.0 原样副本（WTFPL），经根目录 `Cargo.toml` 的 `[patch.crates-io]` 替换上游版本。

与上游的差异（均在 `build.rs`）：

1. macOS 静态链接的 framework 列表去掉 `QTKit`。QTKit 自 macOS 10.15 起已移除，当前 SDK 中的
   `QTKit.tbd` 没有 arm64，链接时 ld 报 `missing required architecture arm64`。上游 issue：
   https://github.com/zmwangx/rust-ffmpeg-sys/issues/114
2. bindgen 排除 `malloc`、`realloc`、`memcmp`、`memcpy`、`memmove`、`memset`、`strlen`、`bcmp`。这些 libc
   符号经 FFmpeg 头文件间接引入，生成的声明把 `size_t` 写成 `u64`，与标准库的定义（`usize`）不一致，
   触发 `suspicious_runtime_symbol_definitions`；上游作为 crates.io 依赖时该警告被 cargo 压下，改为本地路径依赖后才出现。

上游发布包含以上两处修正的版本后，删除本目录与 `[patch.crates-io]` 条目。
