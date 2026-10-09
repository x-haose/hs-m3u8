# hs-m3u8

HLS（m3u8）下载器。本分支是 Rust 重写：Rust 库、Python 包（`hs-m3u8` 1.0）与桌面应用共用同一个下载引擎。已发布的 Python 0.1.x 在 `master` 分支。

## 现状

| 部分 | 状态 |
|---|---|
| `crates/hls`：播放列表解析、规范化、选轨 | 可用 |
| `crates/core`：下载、解密与校验、续传、直播录制、合并 | 可用，公开 API 未定型 |
| `crates/remux`：转封装为 MP4（静态链接的精简 FFmpeg） | 可用 |
| Python 绑定（PyO3） | 未开始 |
| 桌面应用（Tauri） | 未开始 |

设计见 [docs/design/architecture.md](docs/design/architecture.md)，主要取舍见 [docs/adr](docs/adr)。

## 核心库能力

- 点播：TS 与 fMP4 分片、AES-128、字节范围、不连续段，视频与独立音频 rendition 合并为一个 MP4，不重编码。
- 续传：分片落盘即完整，中断后再次运行只补缺的；播放列表变了时明确拒绝。
- 直播录制：按 RFC 8216 的节奏刷新，窗口滑过或取不到的分片在结果里如实报告；中断后续录。
- 站点适配回调：改写播放列表、修改请求、变换 key、变换分片。
- DRM 与 SAMPLE-AES 不支持，遇到即明确报错。

## 用法（Rust，API 未定型）

```rust
use std::num::NonZeroUsize;

use hs_m3u8_core::{Engine, JobRequest, Url};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = Engine::new(NonZeroUsize::new(32).unwrap());
    let url = Url::parse("https://example.com/master.m3u8")?;
    let job = engine.start(JobRequest::new(url, "downloads/video.mp4".into()))?;
    let output = job.wait().await?;
    println!("已保存到 {}", output.path.display());
    Ok(())
}
```

## 开发

需要：Rust（版本由 `rust-toolchain.toml` 固定）、cargo-deny 0.20.2、cargo-llvm-cov 0.9.1；macOS 另需 Xcode 命令行工具。

```bash
make ffmpeg     # 下载并编译精简 FFmpeg 静态库到 third_party/ffmpeg/dist，只需一次
make rs_check   # 格式、依赖方向、禁止压制属性、cargo-deny、clippy、测试、覆盖率
```

Windows：在已加载 MSVC 环境的 MSYS2 中运行 `third_party/ffmpeg/build.sh`，其余步骤见 `.github/workflows/rust.yml`。

Python 0.1.x 部分用 [uv](https://docs.astral.sh/uv/)：`uv sync` 安装依赖，`make py_check` 检查。
