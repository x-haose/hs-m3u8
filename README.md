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

- 点播：TS 与 fMP4 分片、AES-128、字节范围、不连续段，视频与独立音频 rendition 合并为一个 MP4，不重编码；也可输出可直接播放的本地 HLS 目录，或两者都要。
- 续传：分片落盘即完整，中断后再次运行只补缺的；播放列表变了时明确拒绝。
- 直播录制：按 RFC 8216 的节奏刷新，窗口滑过或取不到的分片在结果里如实报告；中断后续录，或不联网只合并已录到的。
- 站点适配回调：改写播放列表、修改请求、变换 key、变换分片；也可直接给出 key 与 IV。
- 失败按处理方式分类：改请求再试、来源本身不行、可稍后重试的网络故障等，异常带类型化的原因。
- DRM 与 SAMPLE-AES 不支持，遇到即明确报错。

## 用法（Python）

只有异步接口，脚本用 `asyncio.run`。所有类型、属性与参数都有类型标注与说明，`mypy --strict` 与 basedpyright 严格模式下可用。

```python
import asyncio

from hs_m3u8 import (
    Engine,
    HsM3u8Error,
    Hooks,
    HttpOptions,
    JobRequest,
    OutputOptions,
    Purpose,
    RequestError,
    RequestParts,
    Source,
    Target,
)


class Signer(Hooks):
    """站点适配：给每个请求加上签名头。没覆盖的回调不调用。"""

    def __init__(self, token: str) -> None:
        self.token = token

    def on_request(self, purpose: Purpose, request: RequestParts) -> RequestParts:
        return RequestParts(request.url, headers=(*request.headers, ("X-Sign", self.token)))


async def main() -> None:
    engine = Engine()
    source = Source(
        "https://example.com/master.m3u8",
        http=HttpOptions(headers={"User-Agent": "Mozilla/5.0"}),
        hooks=Signer("..."),
    )

    probe = await engine.probe(source)  # 可选的清晰度与音轨、时长、是否直播
    if probe.master is not None:
        print("将下载", probe.master.selected.variant.resolution)

    job = engine.start(JobRequest(source, OutputOptions(Target.Mp4("downloads/video.mp4"))))
    waiting = asyncio.create_task(job.wait())
    while True:  # 每秒显示一次进度，任务一结束就不再等
        done, _ = await asyncio.wait({waiting}, timeout=1)
        progress = job.progress()
        print(f"{progress.segments_done}/{progress.segments_total}")
        if done:
            break
    try:
        output = await waiting
    except RequestError as error:  # 改请求再试：输出已存在、选轨偏好与来源不符等
        print("请求不对：", error)
    except HsM3u8Error as error:
        print(error, "（稍后重试可能成功）" if error.retryable else "")
    else:
        if output.mp4 is not None:
            print("已保存到", output.mp4.path)


asyncio.run(main())
```

中断后用同样的请求再次运行即续传；直播用 `job.stop()` 停止并写出已录到的部分。放弃任务用 `Engine.discard(任务目录)`，它会收拾写到一半的输出。数据类、枚举取值与异常都能 pickle（如交给进程池），只保证同一版本之间。

从 0.1.x 迁移：

| 0.1.x | 1.0 |
|---|---|
| `await M3u8Downloader(m3u8_url, save_path, ...).run()` | `await Engine().start(JobRequest(Source(m3u8_url), OutputOptions(Target.Mp4(mp4_path)))).wait()` |
| `run()` 不返回结果，分片缺失、合并失败等多数失败只写进日志 | 失败时 `wait()` 抛 `HsM3u8Error` 的子类：`RequestError` 要改请求，`retryable` 为真的稍后重试可能成功 |
| MP4 已存在时跳过，同一份列表可以反复运行 | 抛 `OutputExistsError`：要跳过就捕获它，要替换用 `OutputOptions(overwrite=True)` |
| `save_path` 是目录：MP4 写在 `save_path/<目录名>.mp4`，分片在 `save_path/hls` | `Target.Mp4` 给 MP4 文件的路径；分片在任务目录（默认与 MP4 同级、名为 `<主名>.hsdl`） |
| `headers` | `HttpOptions(headers=...)` |
| `max_workers` | `JobRequest(concurrency=...)` |
| `key=M3u8Key(key, iv)`，iv 可写十六进制字符串 | `JobRequest(key=KeyOverride(key, iv=...))`，iv 是 16 字节的 `bytes`（十六进制字符串用 `bytes.fromhex(iv.removeprefix("0x"))` 转） |
| `is_decrypt` | 不需要：按播放列表的 `EXT-X-KEY` 解密 |
| `get_m3u8_func`、`m3u8_response_after` | `Hooks.on_playlist`（返回改写后的文本） |
| `m3u8_request_before`、`key_request_before`、`ts_request_before` | `Hooks.on_request`（`purpose` 区分用途） |
| `key_response_after` | `Hooks.on_key` |
| `ts_response_after` | `Hooks.on_segment` |
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

需要：Rust（版本由 `rust-toolchain.toml` 固定）、[uv](https://docs.astral.sh/uv/) 0.13.0、cargo-deny 0.20.2、cargo-llvm-cov 0.9.1；macOS 另需 Xcode 命令行工具。Python 开发工具由 uv 按 `uv.lock` 装进 `.venv`（默认 Python 3.12）。

```bash
make ffmpeg       # 下载并编译精简 FFmpeg 静态库到 third_party/ffmpeg/dist，只需一次
make check        # 全部检查：Rust（格式、依赖方向、禁止压制属性、cargo-deny、clippy、文档、测试、覆盖率）与 Python
make py_develop   # 构建扩展模块并装进 .venv
make py_test      # Python 端到端测试
make py_stubs     # 改了绑定的类型后重新生成类型存根（python/hs_m3u8）
```

换 Python 版本另给目录，如 `make py_test VENV=.venv-3.15 UV_PYTHON=3.15`。Windows：在已加载 MSVC 环境的 MSYS2 中运行 `third_party/ffmpeg/build.sh`，其余步骤见 `.github/workflows/rust.yml`。
