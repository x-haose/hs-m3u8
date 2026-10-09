# ADR-0002：合并（转封装）使用静态链接的精简 FFmpeg

- 状态：已采纳
- 日期：2026-10-09

## 背景

下载完成后要把分片原样复制（不重编码）进一个 MP4：TS（H.264/HEVC + AAC）、fMP4（`#EXT-X-MAP`），以及视频轨与独立音频轨合流。0.1.8 用 PyAV 做这件事，曾因未复制编码参数导致 fMP4 合并结果不可解码却报告成功（0.1.9 修复）。

## 决定

- 自行从源码构建 FFmpeg（当前 n9.0.2，2026-09-18），只启用转封装需要的组件，静态链接；许可证为 LGPL 2.1+。
- Rust 侧用 `ffmpeg-next` 9.0.0，通过 `FFMPEG_DIR` 指向自建的静态库（不用其 `build` 特性：它克隆 `release/<版本>` 分支并编译完整库，无法传入自定义 configure 参数）。
- FFmpeg 相关代码集中在单独的 crate（见架构设计 `remux`），是全项目唯一的 FFI 边界。

configure 参数（验证原型实际使用）：

```
--enable-static --disable-shared --disable-programs --disable-doc --disable-autodetect --disable-network
--disable-everything --disable-avdevice --disable-avfilter --disable-swscale --disable-swresample
--enable-protocol=file --enable-demuxer=mpegts,mov,aac --enable-muxer=mp4
--enable-parser=h264,hevc,aac --enable-decoder=h264,hevc,aac
--enable-bsf=aac_adtstoasc,extract_extradata --enable-pic
```

## 考虑过的方案

| 方案 | 结论 |
|---|---|
| ffmpeg-next + 自建精简静态 FFmpeg | 采用。macOS 上已验证（见下） |
| ffmpeg-next 的 `build` 特性 | 编译完整 FFmpeg，不能裁剪组件，体积与许可证不可控 |
| rsmpeg 0.18.0 | 仓库自 2025-08-24 无活动，不支持 FFmpeg 9 |
| 纯 Rust（mpeg2ts 解封装 + muxide / shiguredo_mp4 封装） | ADTS→ASC、Annex B→AVCC、时间戳不连续都要自写；对口的 hls-transmux 0.9.0 首发仅 3 个月、1 star |
| GStreamer 0.25.4 | 需要用户预装 GStreamer 系统库，违背单文件分发 |
| 打包 ffmpeg 命令行 | 外部进程编排；0.1.x 曾在仓库里放过约 115 MB 的 ffmpeg 压缩包 |

`ffmpeg-next` 的 README 写明处于维护模式（maintenance-only），是本方案的主要风险；它只是薄绑定，必要时可换 `rusty_ffmpeg` 一类的 sys 层绑定，影响局限在 remux crate 内。

## 验证结果（macOS arm64，2026-10-07）

- FFmpeg 精简构建 16 秒；验证程序 release 二进制 4.0 MB，运行时只依赖系统 framework。
- TS → MP4：视频 4000 帧、音频 3750 帧全部可解码。
- 视频 rendition + 独立音频 rendition（fMP4）→ 一个 MP4：结果与 TS 合流逐帧一致。
- X 的真实 fMP4（init + 36 段）：3263 帧全部可解码。
- HEVC：可解码；需显式设 codec tag 为 `hvc1`（`codec_tag=0` 时 muxer 给 `hev1`，QuickTime 不播）。
- moov 前置（`movflags=+faststart`）生效。
- 时间戳回退（模拟 `EXT-X-DISCONTINUITY`）：muxer 明确报错，未静默。

## 实现约束（验证中发现的坑）

1. macOS 上 bindgen 需显式传 `--sysroot=$(xcrun --show-sdk-path)`（`BINDGEN_EXTRA_CLANG_ARGS`），否则找不到系统头文件。
2. 不用 `ffmpeg-next` 的 `Input::packets()` 迭代器：它把 I/O 错误当作读到末尾、静默跳过 `InvalidData` 包。直接调用 `Packet::read` 并上抛每个错误。
3. 输出先写临时文件，成功后改名；失败时删除临时文件。
4. `EXT-X-DISCONTINUITY` 处时间戳会重置，需按不连续段组累加偏移后再写入。
5. HEVC 输出流 codec tag 设为 `hvc1`。
6. 合并后核对每条输出流的包数与输入一致，不一致即失败（0.1.8 的 fMP4 回归会被这条拦下）。

## 后果

- CI 需为每个目标平台构建并缓存 FFmpeg 静态库（以 FFmpeg 版本 + configure 参数为缓存键）。**Windows 尚未验证**（需 MSYS2 环境编译 FFmpeg、MSVC 链接），是开工后第一个要过的风险。
- 静态链接 LGPL 库：需满足 LGPL 2.1 第 6 节「用户能用修改过的库重新链接」。应用以 MIT 开源并提供 FFmpeg 构建脚本可满足，发布前按条款核对。

## 依据的版本

FFmpeg n9.0.2（2026-09-18）、ffmpeg-next / ffmpeg-sys-next 9.0.0（2026-08-05）、rsmpeg 0.18.0+ffmpeg.8.0（2025-08-24）、rusty_ffmpeg 0.17.0+ffmpeg.8.1（2026-04-10）、mpeg2ts 0.6.1、muxide 0.2.5、shiguredo_mp4 2026.5.0、hls-transmux 0.9.0、gstreamer 0.25.4。
