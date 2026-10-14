# hs-m3u8

HLS（m3u8）下载：并发拉取、AES-128 解密与校验、断点续传、直播录制，输出 MP4 或可直接播放的本地 HLS。下载引擎用 Rust 写成，Python 包与 Rust 库共用；合并 MP4 用静态链接的精简 FFmpeg，不需要另装 FFmpeg。

## 安装

```bash
pip install hs-m3u8
```

需要 CPython 3.12 及以上（自由线程版不支持）。预编译的 wheel 覆盖 Linux x86_64 与 aarch64（glibc 2.28 及以上）、macOS 11 及以上的 Apple 芯片机型、Windows x86_64；其他平台从源码构建，见「从源码构建」。

## 能力

- 点播：TS、fMP4 与打包音频分片，AES-128（含加密的 init 段）、字节范围、不连续段；视频与独立音频 rendition 合并为一个 MP4，不重编码，也可输出可直接播放的本地 HLS 目录，或两者都要。来源标了 EXT-X-GAP 的分片不取，在结果里如实报告。
- 合并成 MP4 的编码：
  - fMP4 里：H.264、HEVC、AV1、VP9 视频，AAC、AC-3、E-AC-3、MP3、MP2、FLAC、Opus、ALAC 音频；
  - TS 与打包音频里：H.264、HEVC 视频，AAC、AC-3、E-AC-3、MP3、MP2 音频；
  - MP3 采样率不低于 16 kHz，TS 与打包音频里的 E-AC-3 每帧 6 块、只有一个独立子流；
  - 每条轨只取第一路视频与第一路音频（如 TS 里有多路音频时只取第一路），本地 HLS 原样保留全部；
  - MP4 里的 MP3、MP2 音轨，苹果基于 AVFoundation 的播放器（QuickTime Player、Safari 等）认不出。
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

仓库的 [examples](https://github.com/x-haose/hs-m3u8/tree/master/examples) 目录里是可以直接运行的示例，用的是公开的测试流：`download.py`（最简单的用法）、`progress.py`（进度与失败的分类）、`probe.py`（探测后挑清晰度）、`live.py`（录制直播，Ctrl+C 停止）、`hooks.py`（站点适配回调）、`batch.py`（批量下载）。

## 从 0.1.x 迁移

| 0.1.x | 1.0 |
|---|---|
| `await M3u8Downloader(m3u8_url, save_path, ...).run()` | `await download(m3u8_url, mp4_path)`；用到 `headers` 以外的参数时写成 `await Engine().start(JobRequest(Source(m3u8_url), OutputOptions(Target.Mp4(mp4_path)), live=None)).wait()` |
| `run()` 不返回结果；分片取不到等抛 hssp 的 `RequestException`，key 不对等抛 `ValueError`，空的播放列表与合并结果为空只写进日志 | 失败一律抛 `HsM3u8Error` 的子类，原来 `except RequestException`、`except ValueError` 的地方改为 `except HsM3u8Error`：`RequestError` 要改请求，`retryable` 为真的稍后重试可能成功 |
| MP4 已存在时跳过，同一份列表可以反复运行 | 抛 `OutputExistsError`：要跳过就捕获它，要替换用 `OutputOptions(overwrite=True)` |
| `save_path` 是目录：MP4 写在 `save_path/<目录名>.mp4`，分片在 `save_path/hls` | `Target.Mp4` 给 MP4 文件的路径；分片在任务目录（默认与 MP4 同级、名为 `<主名>.hsdl`） |
| `run()`（默认 `del_hls=False`）：MP4 之外留下可直接播放的 HLS 目录 | `Target.Both(mp4=..., hls=...)`；只要 MP4（`del_hls=True`）用 `Target.Mp4`。`OutputOptions(keep_work_dir=True)` 留下的是续传用的任务目录，不能直接播放 |
| `run(merge=False)` | `Target.Hls`：可直接播放的本地 HLS 目录 |
| `headers` | `download(..., headers=...)`，或 `HttpOptions(headers=...)` |
| `max_workers` | `JobRequest(concurrency=...)`；所有任务合计另受 `Engine(max_requests=...)` 限制，并发调大时一并调大 |
| `key=M3u8Key(key, iv)`，key 任意长度，iv 可写十六进制字符串 | `JobRequest(key=KeyOverride(key, iv=...))`，key 须 16 字节，iv 是 16 字节的 `bytes`（十六进制字符串用 `bytes.fromhex(iv.removeprefix("0x"))` 转） |
| `is_decrypt` | 不需要：按播放列表的 `EXT-X-KEY` 解密 |
| `get_m3u8_func`、`m3u8_response_after` | `Hooks.on_playlist`（返回改写后的文本） |
| `m3u8_request_before`、`key_request_before`、`ts_request_before` | `Hooks.on_request`（`purpose` 区分用途）：只改地址与请求头，不能改请求方法、请求体、代理、超时与重试，也不能直接给出响应 |
| `key_response_after` | `Hooks.on_key` |
| `ts_response_after` | `Hooks.on_segment`，init 段（`EXT-X-MAP`）为 `Hooks.on_init` |
| 回调可以是协程 | 回调是普通函数，在后台线程里并发调用，有状态的回调要自己加锁（见 `Hooks`） |
| 不校验 TLS 证书 | 默认校验；来源可信而证书有问题时设 `HttpOptions(insecure=True)` |
| 代理只看环境变量 | 默认 `Proxy.System()` 还看 macOS 与 Windows 的系统设置，系统设置用了 PAC 时报 `SystemProxyError`，改用 `Proxy.Direct()` 或 `Proxy.Url(...)` |
| 日志（loguru、`<save_path>/<名>.log`） | 不写日志：进度看 `job.progress()`，失败看异常 |
| TS 里的各种编码都合并成 MP4 | 放不进 MP4 的编码（如 TS 里的 Opus、MPEG-2 视频）报 `Mp4UnsupportedError`，改用 `Target.Hls` |
| `M3u8Key`、`start()`、`merge()`、`ts_to_mp4()` | 已移除，对应的用法见上面各行 |
| 纯 Python，各平台都能装 | 预编译的 wheel 只覆盖「安装」一节列的平台，其他平台从源码构建 |
| Python ≥ 3.10 | Python ≥ 3.12 |

## 用法（Rust，API 未定型）

Rust 库没有发布到 crates.io，可用 git 依赖（`hs-m3u8-core`）。

```rust no_run
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

## 许可证

本项目按 MIT 许可证发布。wheel 里的扩展模块静态链接了 FFmpeg（LGPL 2.1 或更高版本）与若干 Rust crate：声明、各许可证的全文、FFmpeg 源码的取得方式与换用修改过的 FFmpeg 的做法见 `LICENSES/NOTICE.txt`（wheel 里在 `*.dist-info/licenses/` 下，源码分发包里在 `LICENSES/` 下）。

## 从源码构建

没有预编译 wheel 的平台上，pip 会下载源码分发包构建。需要 Rust（rustup 按 `rust-toolchain.toml` 装对应的版本）、C 编译器、make 与 xz，Linux 与 Windows 另需 libclang；FFmpeg 要先单独编译（源码分发包里带着 FFmpeg 的源码包，不必另外下载）：

```bash
pip download --no-binary :all: --no-deps hs-m3u8
tar -xzf hs_m3u8-*.tar.gz
cd hs_m3u8-*/
bash third_party/ffmpeg/build.sh
pip install .
```

Windows 上在已加载 MSVC 环境的 MSYS2 里运行 `build.sh`。已按 `build.sh` 的参数编译安装过 FFmpeg 时，把环境变量 `FFMPEG_DIR` 设为它的安装目录，再直接 `pip install hs-m3u8` 即可。

## 开发

| 目录 | 内容 |
|---|---|
| `crates/hls` | 播放列表解析、规范化、选轨 |
| `crates/core` | 下载、解密与校验、续传、直播录制、合并（Rust 公开 API 未定型） |
| `crates/remux` | 转封装为 MP4（静态链接的精简 FFmpeg） |
| `crates/py` | Python 绑定（`hs_m3u8`） |

桌面应用（Tauri）未开始。

需要：Rust（版本由 `rust-toolchain.toml` 固定）、[uv](https://docs.astral.sh/uv/)（版本见 `pyproject.toml` 的 `required-version`）、cargo-deny 与 cargo-llvm-cov（版本见 `.github/workflows/rust.yml`）；macOS 另需 Xcode 命令行工具，Linux 与 Windows 另需 libclang（bindgen 用）；编译 FFmpeg 还要 C 编译器、make、curl 与 xz。Python 开发工具由 uv 按 `uv.lock` 装进 `.venv`（默认 Python 3.12）。

```bash
make ffmpeg       # 下载并编译精简 FFmpeg 静态库到 third_party/ffmpeg/dist，只需一次
make check        # 全部检查：Rust（格式、依赖方向、禁止压制属性、cargo-deny、随发布包的声明、clippy、文档、测试、覆盖率）与 Python
make py_develop   # 构建扩展模块并装进 .venv
make py_test      # Python 端到端测试
make py_stubs     # 改了绑定的类型后重新生成类型存根（python/hs_m3u8）
make licenses     # 依赖或 FFmpeg 的版本变了之后重新生成 LICENSES 下的声明
uv run --no-sync python examples/download.py   # 运行示例（先 make ffmpeg 与 make py_develop）
```

换 Python 版本另给目录，如 `make py_test VENV=.venv-3.15 UV_PYTHON=3.15`。Windows：在已加载 MSVC 环境的 MSYS2 中运行 `third_party/ffmpeg/build.sh`，其余步骤见 `.github/workflows/rust.yml`。发布用的 sdist 与各平台 wheel 由 `.github/workflows/wheels.yml` 构建并测试。
