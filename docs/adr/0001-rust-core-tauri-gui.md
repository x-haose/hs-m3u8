# ADR-0001：Rust 核心 + Tauri 2 桌面应用 + Python 绑定

- 状态：已采纳
- 日期：2026-10-09

## 背景

0.1.x 是单文件 Python 库（`src/hs_m3u8/main.py`），能完成大部分站点的下载，但有两类问题：

1. 功能缺口：不支持音视频分离（主播放列表里视频与音频是两条独立的媒体播放列表，X/Twitter 即如此）；没有 GUI。
2. 正确性问题（0.1.8 代码审查与离线实测确认）：所有分片共用第一个 key 的 IV、缺省 IV 时使用随机 IV、ts 请求头被当作 URL 参数发送、`max_workers` 对分片请求不生效、合并失败留下半截 mp4 且下次视为已完成、失败不向调用方返回等。

GUI 要分发给他人使用，美观与分发体验是一等需求。

## 决定

1. 下载核心用 Rust 重写。
2. 桌面应用用 Tauri 2（当前 2.12.x 稳定版），前端技术栈见 ADR-0005；界面设计在写 GUI 时另行确定。
3. 保留 Python 绑定（PyO3 + maturin），以 PyPI 包名 `hs-m3u8` 继续发布，服务现有用户与按站点编写的适配脚本；最低 Python 3.11。
4. 不做 Go 绑定。
5. 发布平台：macOS 只发 arm64；Windows x64；Linux x64。macOS 签名与公证在购买开发者账号后补上；Windows 不购买代码签名证书。

## 考虑过的方案

### 语言：继续用 Python vs Rust 核心

- 继续用 Python（配 hs-net 网络库）：站点适配脚本天然是 Python，PyAV 的 wheel 内置 FFmpeg，代码最少。但桌面分发要打包解释器、PyAV 与 GUI 框架，体积与打包稳定性差，这是用户明确拒绝的原因。
- Rust 核心：桌面端是单个原生二进制；分片状态、密钥/IV、失败路径能用类型与 `Result` 表达成编译期约束，正对 0.1.x 的问题类别。代价是 hs-net 无法复用，FFmpeg 需要自行静态链接（见 ADR-0002）。

GUI 要分发给他人，故选 Rust 核心。

### GUI：Slint / Tauri / GPUI / Iced

同一界面（侧栏、任务列表、新建任务抽屉、三套主题、模拟进度）用 Slint、Tauri、GPUI 各实现一份对比（2026-10-07）。三者认真设计后视觉水准相当，差别在：

| | Slint 1.18.1 | Tauri 2.12.1 | GPUI（gpui-component 0.7.1） |
|---|---|---|---|
| 产物 | 17 MB | 3.97 MB（.app） | 14 MB |
| 中文排版 | `font-family` 只接受单个字体名，无回退列表、无语言标签、无 OpenType 特性（读 1.18.1 源码核实） | CSS 完整支持 | 正常 |
| 原生窗口融合 | 透明标题栏依赖不稳定的 winit 钩子并锁死精确版本；红绿灯位置不可调；无毛玻璃 | 配置与 API 可达；毛玻璃在 2.x 需私有 API | 可达 |
| 依赖风险 | 低（1.x） | 低（2.x） | 高：依赖个人发布的快照 `gpui-pre`，4 周内 14 个公开 API 删除或改签名，且未声明需要 Rust 1.95 |
| 内存 | 未测 | 约 253 MB RSS（四进程合计） | 未测 |

选 Tauri：中文混排、等宽数字、原生窗口融合都在正式 API 内可达，界面迭代有热更新。接受的代价：内存在 250 MB 量级；三平台 WebView 内核不同，需逐平台验证；Linux 的 WebKitGTK 在 NVIDIA 显卡上有已知问题；前端多一门 TypeScript。

Iced 0.14.0 自述 experimental、无无障碍、虚拟列表未发布，未进入对比。

### Tauri 2 还是 3

Tauri 3.0 截至 2026-10-01 为 alpha.4，里程碑无截止日期、完成度 25%（2026-09-21），alpha 间多次破坏性变更，最低 Rust 1.95。3.0 的变化对本项目：

- 可选 CEF 运行时：第三方实测同一应用安装包 20.10 MiB → 341.90 MiB、内存 352.6 → 751.0 MiB，不采用。
- wry 运行时不再依赖 macOS 私有 API（alpha.4）：只对上架 App Store 有意义，当前不以上架为目标。

GUI 开发排在核心与 Python 绑定之后，届时再评估是否迁移到 3.0；Tauri 壳保持薄（命令、事件、窗口配置），迁移面小。

### 多语言绑定：x-iztro 模式

x-iztro 是纯计算核心 + 无状态 JSON 桥接，Go 经 wasm32-wasip1 + wazero 调用。下载器的核心是长时间运行的网络 IO，需要进度事件、取消与回调；wasm32-wasip1 没有发起出站连接的接口，FFmpeg 也无法放入。Go 只能走 cgo + C ABI。没有具体的 Go 使用方，不做。

## 后果

- 0.1.x 只做严重问题的修复，不再加功能；Rust 版 Python 绑定以 1.0.0 发布，API 不兼容 0.1.x。
- 资源嗅探（从网页里发现 m3u8）放在 GUI 之后：Tauri 2 的 `on_web_resource_request` 只作用于 `tauri://` 协议，看不到外部请求；可行路线是内嵌浏览器 + 注入脚本（`initialization_script_for_all_frames`）+ `cookies_for_url`，worker 内请求不可见，需原型验证。
- 本机与 CI 的 Rust 工具链用 `rust-toolchain.toml` 锁定（当前稳定版 1.99.0，2026-09-28）。
- Python 3.10 已于 2026-10-01 停止支持（devguide），故最低版本取 3.11，用 PyO3 的 `abi3-py311` 每平台出一个 wheel。
- 未签名的代价：macOS 首次打开会被系统拦截，需在「系统设置 → 隐私与安全性」中放行；Windows 会出现 SmartScreen 提示，需点「更多信息 → 仍要运行」。自动更新包的签名密钥由我们自行生成，与上述证书无关，必须配置。
- 只发 arm64 意味着 Intel 芯片的 Mac 无法使用。

## 依据的版本（2026-10-07～08 查 crates.io / PyPI / GitHub）

tauri 2.12.1（2026-09-30）、tauri 3.0.0-alpha.4（2026-10-01）、slint 1.18.1、gpui-component 0.7.1 / gpui-pre 0.3.8、iced 0.14.0、pyo3 0.29.3、maturin 1.15.0、uniffi 0.32.2（Go 仅第三方 uniffi-bindgen-go，对应 uniffi 0.31）。
