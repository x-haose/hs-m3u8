# hs-m3u8

m3u8 视频下载工具。支持大部分的m3u8视频下载。后续增加UI界面。

## 功能

- aes解密
- 自动选择高分辨m3u8
- 合并MP4
- 可选择保留ts文件
- 合并使用 PyAV（wheel 内置 FFmpeg），无需另装 ffmpeg

## 计划

- 增加cli功能，通过终端执行命令去下载
- 增加支持curl参数功能。直接在curl里面读取请求头及cookie
- 编写详细文档
- 选择一个合适的技术栈，增加UI界面

## 安装

### pip包安装

```shell
pip install hs-m3u8
```

### 克隆代码安装

见下面开发文档

## 使用

```python
url = "https://surrit.com/6d3bb2b2-d707-4b79-adf0-89542cb1383c/playlist.m3u8"
name = "SDAB-129"
dl = M3u8Downloader(
    m3u8_url=url,
    save_path=f"downloads/{name}",
    max_workers=64
)
await dl.run(del_hls=False, merge=True)
```

- del_hls 为True时会删除ts、m3u8、key等文件，否则会经过处理后保留，以便直接使用
- merge 为True时会自动合并为mp4

## 开发

### 先安装uv

uv网站：https://docs.astral.sh/uv/

### 使用uv 安装包及虚拟环境

```bash
uv sync
```