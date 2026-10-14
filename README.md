# hs-m3u8

HLS（m3u8）下载器。本分支是 Rust 重写：Rust 库、Python 包（`hs-m3u8` 1.0）与桌面应用共用同一个下载引擎。已发布的 Python 0.1.x 在 `master` 分支。

## 现状

| 部分 | 状态 |
|---|---|
| `crates/hls`：播放列表解析、规范化、选轨 | 可用 |
| `crates/core`：下载、解密与校验、续传、直播录制、合并 | 可用，Rust 公开 API 未定型 |
| `crates/remux`：转封装为 MP4（静态链接的精简 FFmpeg） | 可用 |
| `crates/py`：Python 绑定（`hs_m3u8`，Python ≥ 3.12） | 可用，尚未发布到 PyPI |
| 桌面应用（Tauri） | 未开始 |

## 能力

- 点播：TS、fMP4 与打包音频分片，AES-128（含加密的 init 段）、字节范围、不连续段，视频与独立音频 rendition 合并为一个 MP4，不重编码（fMP4 里 H.264、HEVC、AV1、VP9 视频与 AAC、AC-3、E-AC-3、MP3、MP2、FLAC、Opus、ALAC 音频，TS 与打包音频里 H.264、HEVC 视频与 AAC、AC-3、E-AC-3、MP3、MP2 音频；MP3 采样率不低于 16 kHz，TS 与打包音频里的 E-AC-3 每帧 6 块、只有一个独立子流）；也可输出可直接播放的本地 HLS 目录，或两者都要。MP4 里的 MP3、MP2 音轨，苹果基于 AVFoundation 的播放器（QuickTime Player、Safari 等）认不出。来源标了 EXT-X-GAP 的分片不取，在结果里如实报告。
- 续传：分片落盘即完整，中断后再次运行只补缺的；播放列表变了时明确拒绝。
- 下载时校验分片：解密失败、不是媒体数据的内容拒收；空的响应、fMP4 的顶层 box 或 moof 里声明的样本数据超出数据、TS 或打包音频截在包或帧中间（加密的看解出的内容）时，按没写完重新取，直播里仍没写完的记为缺失。恰好截在 TS 包、音频帧或两组 moof/mdat 之间的看不出，合并出的内容比声明的时长短。
- 直播录制：按 RFC 8216 的节奏刷新，窗口滑过、取不到或来源标了 EXT-X-GAP 的分片在结果里如实报告；只录一条轨时编码器重启（序号回退）后另起会话接着录；中断后续录，或不联网只合并已录到的。
- 站点适配回调：改写播放列表、修改请求、变换 key、变换分片；也可直接给出 key 与 IV（EXT-X-KEY 没写 URI 时要靠它）。
- 失败按处理方式分类：改请求再试、来源本身不行、可稍后重试的网络故障等，异常带类型化的原因。
- DRM、SAMPLE-AES、用绝对位置寻址的 fMP4（tfhd 写了 base_data_offset，RFC 8216 不允许）、只有关键帧的播放列表（EXT-X-I-FRAMES-ONLY）、增量更新（EXT-X-SKIP）与 TS 的 init 段不支持，遇到即明确报错。

## 用法（Python）

只有异步接口，脚本用 `asyncio.run`。所有类型、属性与参数都有类型标注与说明，`mypy --strict` 与 basedpyright 严格模式下可用。

给地址与保存路径，下载最高清的变体，存成 MP4：

```python
import asyncio

from hs_m3u8 import download

asyncio.run(download("https://example.com/master.m3u8", "downloads/video.mp4"))
```

站点要求请求头时加 `headers={"Referer": "https://example.com/"}`；中断后再次调用即续传。要先探测清晰度与音轨、看进度、录制直播、调并发、输出本地 HLS 或用站点适配回调，用 `Engine`：

```python
import asyncio

from hs_m3u8 import Engine, JobRequest, OutputOptions, Source, Target


async def main() -> None:
    engine = Engine()
    source = Source("https://example.com/master.m3u8")
    probe = await engine.probe(source)  # 可选的清晰度与音轨、各轨的分片数与时长、是否直播
    if probe.master is not None:
        print("将下载", probe.master.selected.variant.resolution)
    job = engine.start(JobRequest(source, OutputOptions(Target.Mp4("downloads/video.mp4"))))
    output = await job.wait()  # 等待期间可用 job.progress() 看进度，录制直播时用 job.stop() 停止
    print("已保存到", output.mp4.path if output.mp4 else None)


asyncio.run(main())
```

选轨偏好、请求头与代理、站点适配回调在 `Source` 上，输出种类与任务目录在 `OutputOptions` 上，并发、直播录制方式与自定义 key 在 `JobRequest` 上。中断后用同样的请求再次运行即续传；直播用 `job.stop()` 停止并写出已录到的部分。放弃任务用 `Engine.discard(任务目录)`，它会收拾写到一半的输出。数据类、枚举取值与异常都能 pickle（如交给进程池），只保证同一版本之间。

`examples/` 里是可以直接运行的示例，用的是公开的测试流：`download.py`（最简单的用法）、`progress.py`（进度与失败的分类）、`probe.py`（探测后挑清晰度）、`live.py`（录制直播，Ctrl+C 停止）、`hooks.py`（站点适配回调）、`batch.py`（批量下载）。在仓库里运行（先按「开发」一节装好 Rust 与 uv）：

```bash
make ffmpeg && make py_develop
uv run --no-sync python examples/download.py
```

从 0.1.x 迁移：

| 0.1.x | 1.0 |
|---|---|
| `await M3u8Downloader(m3u8_url, save_path, ...).run()` | `await download(m3u8_url, mp4_path)`；用到 `headers` 以外的参数时写成 `await Engine().start(JobRequest(Source(m3u8_url), OutputOptions(Target.Mp4(mp4_path)), live=None)).wait()` |
| `run()` 不返回结果，分片缺失、合并失败等多数失败只写进日志 | 失败时抛 `HsM3u8Error` 的子类：`RequestError` 要改请求，`retryable` 为真的稍后重试可能成功 |
| MP4 已存在时跳过，同一份列表可以反复运行 | 抛 `OutputExistsError`：要跳过就捕获它，要替换用 `OutputOptions(overwrite=True)` |
| `save_path` 是目录：MP4 写在 `save_path/<目录名>.mp4`，分片在 `save_path/hls` | `Target.Mp4` 给 MP4 文件的路径；分片在任务目录（默认与 MP4 同级、名为 `<主名>.hsdl`） |
| `headers` | `download(..., headers=...)`，或 `HttpOptions(headers=...)` |
| `max_workers` | `JobRequest(concurrency=...)` |
| `key=M3u8Key(key, iv)`，iv 可写十六进制字符串 | `JobRequest(key=KeyOverride(key, iv=...))`，iv 是 16 字节的 `bytes`（十六进制字符串用 `bytes.fromhex(iv.removeprefix("0x"))` 转） |
| `is_decrypt` | 不需要：按播放列表的 `EXT-X-KEY` 解密 |
| `get_m3u8_func`、`m3u8_response_after` | `Hooks.on_playlist`（返回改写后的文本） |
| `m3u8_request_before`、`key_request_before`、`ts_request_before` | `Hooks.on_request`（`purpose` 区分用途） |
| `key_response_after` | `Hooks.on_key` |
| `ts_response_after` | `Hooks.on_segment`，init 段（`EXT-X-MAP`）为 `Hooks.on_init` |
| `run(merge=False)` | `Target.Hls`：可直接播放的本地 HLS 目录 |
| `run(del_hls=False)`（默认，保留分片） | 默认成功后删掉任务目录；要保留用 `OutputOptions(keep_work_dir=True)` |
| Python ≥ 3.10 | Python ≥ 3.12 |

## 用法（Rust，API 未定型）

```rust
use std::num::NonZeroUsize;

use hs_m3u8_core::{Engine, JobRequest, OutputOptions, Source, Target, Url};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = Engine::new(NonZeroUsize::new(32).unwrap());
    let url = Url::parse("https://example.com/master.m3u8")?;
    let output = OutputOptions::new(Target::Mp4("downloads/video.mp4".into()));
    let request = JobRequest::new(Source::new(url), output);
    let job = engine.start(request)?;
    let output = job.wait().await?;
    if let Some(mp4) = output.mp4 {
        println!("已保存到 {}", mp4.path.display());
    }
    Ok(())
}
```

## 开发

需要：Rust（版本由 `rust-toolchain.toml` 固定）、[uv](https://docs.astral.sh/uv/) 0.13.0、cargo-deny 0.20.2、cargo-llvm-cov 0.9.1；macOS 另需 Xcode 命令行工具，Linux 与 Windows 另需 libclang（bindgen 用）；编译 FFmpeg 还要 C 编译器、make、curl 与 xz。Python 开发工具由 uv 按 `uv.lock` 装进 `.venv`（默认 Python 3.12）。

```bash
make ffmpeg       # 下载并编译精简 FFmpeg 静态库到 third_party/ffmpeg/dist，只需一次
make check        # 全部检查：Rust（格式、依赖方向、禁止压制属性、cargo-deny、clippy、文档、测试、覆盖率）与 Python
make py_develop   # 构建扩展模块并装进 .venv
make py_test      # Python 端到端测试
make py_stubs     # 改了绑定的类型后重新生成类型存根（python/hs_m3u8）
```

换 Python 版本另给目录，如 `make py_test VENV=.venv-3.15 UV_PYTHON=3.15`。Windows：在已加载 MSVC 环境的 MSYS2 中运行 `third_party/ffmpeg/build.sh`，其余步骤见 `.github/workflows/rust.yml`。
