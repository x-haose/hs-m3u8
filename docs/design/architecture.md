# hs-m3u8 Rust 版架构设计

- 状态：已采纳
- 日期：2026-10-09
- 相关决定：ADR-0001（技术路线）、ADR-0002（FFmpeg）、ADR-0003（仓库与迁移）、ADR-0004（产品范围）、ADR-0005（前端技术栈）

## 1. 目标与非目标

目标：

- 下载 HLS 点播流并原样复制（不重编码）为单个 MP4：TS 与 fMP4 分片、AES-128 加密、字节范围分片、不连续段、视频与独立音频 rendition 合流。
- 正确性优先：每个分片使用自己的 key 与 IV；失败一律上抛；任何时刻中断都可安全续传；产物要么完整、要么不存在。
- 同一核心服务三类使用方：Rust 库、Python 包 `hs-m3u8`、Tauri 桌面应用。
- 站点适配通过回调完成（改写播放列表、修改请求、变换 key、变换分片）。
- 直播录制（无 `#EXT-X-ENDLIST` 的播放列表），排在点播流程完成之后，见 5.9。

非目标：

- DRM（Widevine、FairPlay、PlayReady）与 SAMPLE-AES：识别后明确报不支持。
- 转码。
- HTTP 直链、BT、磁力下载（ADR-0004）。
- 浏览器指纹模拟：v1 不做，以后作为可选功能加入（wreq 0.16.1 仍在 1.0 之前，要求 Rust 1.98）。

## 2. 仓库结构

```
hs-m3u8/
├── Cargo.toml              workspace
├── rust-toolchain.toml     锁定工具链版本
├── crates/
│   ├── hls/                播放列表解析与规范化、选轨、IV 推导；纯计算，无 IO
│   ├── core/               下载任务：HTTP、调度、解密校验、续传、合并编排、进度
│   ├── remux/              FFmpeg 转封装；全项目唯一的 FFI 边界
│   └── py/                 PyO3 绑定（cdylib），模块名 hs_m3u8._native
├── python/hs_m3u8/         Python 包：类型化包装、异常类、.pyi
├── apps/desktop/           Tauri 应用（src-tauri/ 与前端）
├── third_party/ffmpeg/     FFmpeg 构建脚本：固定版本与 configure 参数
├── tests/fixtures/         测试样本（用 ffmpeg testsrc 生成，不含第三方内容）
└── docs/
```

## 3. 分层与依赖方向

```
apps/desktop ─┐
crates/py ────┼──> crates/core ──> crates/hls
              │          └──────> crates/remux
```

- `hls` 不依赖 tokio、reqwest、ffmpeg；`core` 不依赖 tauri、pyo3；`remux` 不依赖 `core`。
- 跨边界一律翻译：`hls` 不暴露第三方解析库的类型；`py` 与 `apps/desktop` 把 `core` 的类型转成各自的表示，`core` 不出现 Python 或前端的概念。
- 以上由检查命令机器判定（第 10 节），不靠人看。

## 4. hls：数据模型与规范化

只做纯计算：输入播放列表文本和它的最终 URL（跟随重定向之后），输出规范化模型。

```rust
// 草案
pub struct MasterPlaylist {
    pub variants: Vec<Variant>,
    pub renditions: Vec<Rendition>,   // EXT-X-MEDIA
}

pub struct Variant {
    pub uri: Url,
    pub bandwidth: u64,               // bit/s，取自 BANDWIDTH
    pub resolution: Option<(u32, u32)>,
    pub codecs: Vec<String>,
    pub audio_group: Option<String>,  // AUDIO 属性
}

pub struct Rendition {
    pub kind: RenditionKind,          // Audio / Subtitles / ClosedCaptions
    pub group_id: String,
    pub name: String,
    pub language: Option<String>,
    pub default: bool,
    pub uri: Option<Url>,             // None 表示该 rendition 混在变体流里
}

pub struct MediaPlaylist {
    pub segments: Vec<Segment>,
    pub ended: bool,                  // 是否有 EXT-X-ENDLIST
}

pub struct Segment {
    pub sequence: u64,                // 媒体序号 = EXT-X-MEDIA-SEQUENCE + 下标
    pub uri: Url,                     // 已按播放列表最终 URL 解析为绝对地址
    pub byte_range: Option<ByteRange>,// 起止偏移已算好
    pub duration_ms: u64,
    pub discontinuity: u32,           // 不连续段序号
    pub key: Option<SegmentKey>,      // None 表示不加密
    pub init: Option<InitSection>,    // fMP4 的 EXT-X-MAP
}

pub struct SegmentKey {
    pub uri: Url,
    pub iv: [u8; 16],                 // 已定值：显式 IV，或缺省时的媒体序号大端编码
}
```

规范化规则（RFC 8216，全部有纯计算测试）：

- `EXT-X-KEY` 作用于其后所有分片，直到下一个 `EXT-X-KEY`；`METHOD=NONE` 清除。每个分片都带自己的 key 与 IV。
- 缺省 IV 时用该分片媒体序号的 16 字节大端编码。
- 同一位置有多个 `EXT-X-KEY` 时，只取 `KEYFORMAT` 缺省或为 `identity` 的那个；只剩 DRM 的 KEYFORMAT 时报 `Unsupported(Drm)`；`SAMPLE-AES` 报 `Unsupported(SampleAes)`。
- `EXT-X-MAP` 作用于其后所有分片，直到下一个 `EXT-X-MAP`。
- `EXT-X-BYTERANGE` 省略偏移量时，接着同一资源上一个子区间往后取。
- 所有 URI 按**该播放列表的最终 URL**（跟随重定向之后）解析。

选轨：

```rust
pub fn select(master: &MasterPlaylist, pref: &Preference) -> Result<Selection, SelectError>;
pub struct Selection { pub video: Variant, pub audio: Option<Rendition> }
```

- 默认取最高分辨率，同分辨率取最高码率；用户可以指定。
- 变体带 `AUDIO` 组、且组内 rendition 有 URI 时，选 `DEFAULT=YES` 的那个，或按用户指定的语言选；这就是音视频分流的情况。

解析层：用 `m3u8-rs`（6.0.1）做标签与属性的词法解析，规范化与上述规则自己实现。注意：`m3u8-rs` 只把 KEY 和 MAP 挂在标签后的第一个分片上，往后传播必须由本层完成，并由测试守住。

## 5. core：下载任务

### 5.1 任务与流水线

一个任务把一个来源 URL 变成一个输出文件：

1. **解析**：取主播放列表，选轨，再取媒体播放列表；可调用 `on_playlist` 回调。
2. **计划**：规范化后得到每条轨的分片列表，计算计划摘要（见 5.2）。
3. **下载**：并发拉取分片，取 key，解密并校验，写盘。
4. **合并**：调用 `remux` 生成临时文件，核对包数后改名为输出文件。
5. **收尾**：按选项删除或保留任务目录。

### 5.2 任务目录与续传

```
<输出名>.hsdl/
├── job.json                  任务描述，格式见下
└── tracks/<轨道>/<序号>.seg   已解密且通过校验的分片
```

- `job.json` 是序列化契约：自描述 JSON，带 `format_version`；遇到不认识的版本直接拒绝。内容包括来源 URL、选轨结果、请求配置、计划摘要。写入方式是先写临时文件再改名。
- **分片完成的判定只看最终文件名是否存在**：分片先写 `.part`，校验通过后改名；改名是原子操作，所以存在即完整，不需要逐片记账。
- **计划摘要** = 规范化计划（分片 URL、字节范围、key URL、IV、init URL）的 SHA-256。续传时重新取播放列表并计算；摘要不同即报 `PlanChanged`，不混用旧分片。
- 输出文件已存在：拒绝覆盖，除非调用方明确要求。不再出现「看到同名 mp4 就当作完成」的情况。
- 任务目录位置可配置：库的默认位置在输出文件旁，成功后删除；桌面应用放在应用数据目录，不放在下载目录，避免被网盘同步。

敏感信息（请求头的值、Cookie、自定义 key、URL 中的令牌）：

- 与其他任务配置一起明文写入 `job.json`，文件权限为仅本人可读写（Unix 0600；Windows 用户目录默认如此）。
- 不加密：密钥若写死在开源代码里等于公开，若随机生成后存在本机，能读到数据的人同样能读到密钥；有效的加密需要系统钥匙串，复杂度不值得。
- 日志与错误信息中不输出请求头的值、Cookie 与 key 内容。

### 5.3 调度、超时、重试、取消

- `Engine` 持有全局并发上限（跨任务的在途分片数）；每个任务另有自己的上限。信号量覆盖整个分片请求，包括响应体读取。
- 每个请求都有连接超时、读空闲超时和总时长上限。
- 可重试：连接错误、超时、5xx、408、429（遵守 `Retry-After`）。退避方式为指数加随机抖动，次数可配置。
- 不可重试：其余 4xx、解密或校验失败、回调报错。
- 任一分片最终失败：取消该任务所有在途请求，任务以 `SegmentFailed { sequence, url, cause }` 结束；已完成的分片保留，可以续传。
- 取消：每个任务一个 `CancellationToken`，暂停就是取消后保留目录。所有并发单元由任务的 `JoinSet` 持有，任务结束时全部回收。
- 运行时：`core` 不自建 tokio 运行时，使用调用方的（Tauri、`pyo3-async-runtimes` 都基于 tokio）。

### 5.4 HTTP

- `reqwest` 0.13.5（rustls）。每个任务的请求配置包括请求头、Cookie、User-Agent、代理。
- 证书校验默认开启；关闭必须显式写 `insecure: true`。0.1.x 依赖的 hssp 默认关闭校验，这里纠正。
- 字节范围分片用 `Range` 请求头；响应必须是 206 且长度一致，否则视为错误。
- key 按 key URL 去重，只取一次，缓存在任务内存里。

### 5.5 解密与校验

- AES-128-CBC 加 PKCS#7，使用分片自己的 key 与 IV（`aes` 0.9.3 + `cbc` 0.2.1）。
- 校验（不通过即失败，不写盘）：
  - 有 `Content-Length` 时长度必须一致；
  - 解密后去填充必须合法；key 或 IV 错误时这一步几乎必然失败；
  - TS 分片：偏移 0 和 188 处都必须是同步字节 `0x47`；
  - fMP4 分片：开头必须是合法的 box 头（`styp`、`moof` 等）。

0.1.x 的「IV 错了只坏前 16 字节、仍报告成功」这类问题，会在这一层被拦下。

### 5.6 进度

- `watch` 通道发布进度快照：已完成分片数、总分片数、已下载字节、速度、当前阶段。消费者慢只会跳过中间值，不会阻塞下载。
- 最终结果通过任务句柄的 `Result` 返回，不经进度通道传递。

### 5.7 扩展点（回调）

```rust
// 草案；默认实现均为原样返回
pub trait Hooks: Send + Sync {
    fn on_playlist(&self, url: &Url, text: String) -> Result<String, HookError>;
    fn on_request(&self, req: &mut RequestParts) -> Result<(), HookError>;   // 可改 URL 与请求头
    fn on_key(&self, key_url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError>;
    fn on_segment(&self, url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError>;
}
```

- `on_key` 用于站点自定义 key 加密（如 `qiqiuyun.py` 的 key 变换）。
- `on_segment` 在解密之前调用，用于去掉分片前的伪装字节（例如伪装成 PNG 的 TS）。
- 回调出错时任务失败，错误不可重试。
- 回调持有强引用，不会像 0.1.x（blinker 弱引用）那样被垃圾回收后静默失效。

### 5.8 错误模型

```rust
// 草案
pub enum Error {
    InvalidInput(InvalidInput),          // 调用方输入错误：URL、请求头、选轨偏好、输出路径
    Unsupported(Unsupported),            // DRM、SAMPLE-AES、无可用轨
    Upstream { url: Url, cause: UpstreamCause, retryable: bool }, // HTTP 状态、网络、超时
    SegmentFailed { sequence: u64, url: Url, cause: Box<Error> },
    Integrity(IntegrityError),           // 长度不符、去填充失败、同步字节错、合并后包数不一致
    PlanChanged,                         // 续传时播放列表已变
    Hook(HookError),
    Io(std::io::Error),
    Remux(RemuxError),
}
```

不变量被违反（代码本不该产生的状态）用 panic 中止当次任务，不降级。

### 5.9 直播录制

- 判定：媒体播放列表没有 `#EXT-X-ENDLIST` 即为直播；`PLAYLIST-TYPE:EVENT` 可从头录制，其余从当前窗口开始。
- 刷新：按 target duration 的节奏重新取媒体播放列表；内容未变化时按 RFC 8216 的要求放慢刷新。按媒体序号去重，把新分片追加进计划。
- 漏段：窗口已滑过、尚未下载的分片无法补回。在该处插入不连续标记，通过进度事件发出漏段通知，并写入最终结果（漏掉的序号区间）。不静默跳过。
- 结束：出现 `#EXT-X-ENDLIST`、调用方停止、或达到调用方设定的时长时，停止刷新，等待在途分片完成后合并。
- 中断：直播任务续传只能把已录到的分片合并成片，不再刷新；计划摘要校验不适用于直播，改为校验来源 URL 与选轨一致。
- 选轨在开始时确定；录制中主播放列表的变体变化不跟随。

## 6. remux

```rust
// 草案
pub struct TrackInput {
    pub kind: TrackKind,                 // Video / Audio
    pub pieces: Vec<Piece>,              // 按播放顺序
}
pub struct Piece {
    pub discontinuity: u32,
    pub init: Option<PathBuf>,           // fMP4 init 段
    pub segments: Vec<PathBuf>,
}
pub fn remux(tracks: &[TrackInput], output_tmp: &Path) -> Result<RemuxReport, RemuxError>;
```

- 遵守 ADR-0002 的实现约束：用 `Packet::read`、不连续段累加时间戳偏移、HEVC 标 `hvc1`、moov 前置、核对包数。
- 分片读取方式待原型确定：FFmpeg `concat` 协议，或自定义 `AVIOContext` 顺序读取。目标是不先拼成一个大文件（0.1.x 需要双倍磁盘空间）。

## 7. Python 绑定

```python
# 草案
import hs_m3u8

async def main():
    dl = hs_m3u8.Downloader(max_concurrency=32)
    job = dl.start(
        "https://example.com/master.m3u8",
        output="downloads/日日是好日.mp4",
        headers={"Referer": "https://example.com/"},
        on_key=decrypt_key,          # (key_url: str, data: bytes) -> bytes
        on_request=None,             # (req: hs_m3u8.Request) -> None，可改 url 与 headers
        on_playlist=None,            # (url: str, text: str) -> str
        on_segment=None,             # (url: str, data: bytes) -> bytes
        keep_hls=False,
    )
    async for p in job.progress():
        print(p.done_segments, p.total_segments, p.bytes_per_sec)
    path = await job                 # 失败抛 hs_m3u8.DownloadError 的子类

hs_m3u8.download("https://...", output="a.mp4")   # 同步版本
```

- PyO3 0.29.3 + `pyo3-async-runtimes` 0.29.0（tokio）+ maturin 1.15.0；`abi3-py311` wheel（最低 Python 3.11），FFmpeg 静态链接进扩展模块。
- Python 回调在阻塞线程池中获取 GIL 后执行，不占用 tokio 工作线程。
- `core::Error` 在边界上翻译为 Python 异常层级，`retryable` 等字段作为异常属性保留。
- 0.1.x 参数对应关系：`key` → `key=hs_m3u8.Key(...)`；`get_m3u8_func` → `on_playlist`；`*_request_before` → `on_request`；`key_response_after` → `on_key`；`ts_response_after` → `on_segment`；`del_hls` → `keep_hls`；`merge=False` → `keep_hls=True` 并且只下载不合并。

## 8. 桌面应用（GUI 阶段细化）

- Rust 侧持有 `Engine` 和任务存储（SQLite，rusqlite 0.40.2）。命令：新建、暂停、继续、删除、列表；事件：进度，每个任务限频推送。
- 事务边界在应用的领域层；存储层接收已开启的连接或事务，不自己开事务。
- 前端技术栈见 ADR-0005；界面布局与主题（倾向明亮下载器风）在 GUI 阶段设计。
- 以后接入外部下载器（ADR-0004）时，多协议任务管理在这一层用任务类型区分，核心库不变。
- 资源嗅探：内嵌浏览器 + 注入脚本 + `cookies_for_url`。远程页面调用的上报命令只接收候选地址，内容按不可信数据校验。需先做原型。

## 9. 并发与生命周期约定

- 每个并发单元都有归属者（任务的 `JoinSet`）、退出条件（完成、失败或取消）和超时。
- 取消信号一路传到每个请求与回调调用。
- 不用 sleep 做同步；退避等待使用可被取消的定时器。

## 10. 测试与质量门

测试只写两类（项目约定）：

- 纯计算：`hls` 的规范化规则（KEY/MAP/BYTERANGE/不连续传播、IV 推导、URL 解析、选轨）、计划摘要、AES 解密（已知向量）、分片校验。
- 编解码与端到端：
  - `remux` 用仓库内的小样本（ffmpeg testsrc 生成，几百 KB）覆盖 TS、fMP4、分流、HEVC、不连续，断言每条流的包数和帧数；
  - 端到端在测试内起本地 HTTP 服务提供 HLS 样本（含 AES-128、字节范围、重定向，注入 500 与超时），跑完整任务后核对输出帧数，并覆盖中途取消再续传；
  - Python 绑定做一个端到端冒烟。

不写用假件拼流程、复述实现的测试。

检查命令（`make check`，零告警才算通过）：

1. `cargo fmt --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`
4. `cargo deny check`：许可证白名单（LGPL 只允许 FFmpeg）、安全公告
5. 依赖边界检查：`cargo tree` 断言第 3 节的依赖方向
6. Python：maturin 构建 + 端到端冒烟
7. 前端（GUI 阶段）：`tsc --noEmit`、lint、构建

覆盖率：`hls` 与 `core` 行覆盖 ≥ 80%。

## 11. CI 与发布

- GitHub Actions 三平台矩阵：macOS arm64、Windows x64、Linux x64；FFmpeg 静态库单独构建并缓存。
- PyPI：每个平台一个 abi3 wheel。
- 桌面：tauri-action 出安装包；自动更新用 tauri-plugin-updater，更新签名密钥自行生成并存于 CI 密钥。macOS 签名与公证在购买开发者账号后接入；Windows 不签名。

## 12. 待验证风险与待定问题

开工后按顺序验证：

1. Windows 上 FFmpeg 静态构建与链接（MSYS2 + MSVC）。
2. 不连续段时间戳偏移，以及 concat 协议或自定义 AVIO 的读取方式。
3. 高并发下 Python 回调的 GIL 竞争（`on_segment` 每个分片调用一次）。
4. 资源嗅探的注入脚本（GUI 阶段）。

待定：

1. 桌面应用的产品名（发布前定；PyPI 包名仍为 `hs-m3u8`）。

## 依据的版本（2026-10-08 查 crates.io）

tokio 1.53.2、tokio-util 0.7.19、reqwest 0.13.5、m3u8-rs 6.0.1、aes 0.9.3、cbc 0.2.1、url 2.5.8、thiserror 2.0.21、serde 1.0.229、serde_json 1.0.151、sha2 0.11.0、rusqlite 0.40.2、pyo3 0.29.3、pyo3-async-runtimes 0.29.0、maturin 1.15.0、ffmpeg-next 9.0.0、tauri 2.12.1、cargo-deny 0.20.2。
