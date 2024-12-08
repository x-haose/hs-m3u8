# hs-m3u8

m3u8 视频下载工具。支持大部分的m3u8视频下载。后续增加UI界面。

## 功能

- aes解密
- 自动选择高分辨m3u8
- 合并MP4
- 可选择保留ts文件
- 内置Windows平台ffmpeg可执行文件（由于Linux及Mac下权限问题，需自行安装ffmpeg文件）

## 使用

```python
url = "https://surrit.com/6d3bb2b2-d707-4b79-adf0-89542cb1383c/playlist.m3u8"
name = "SDAB-129"
dl = M3u8Downloader(
    url=url,
    save_path=f"downloads/{name}",
    max_workers=64
)
await dl.run(del_hls=False, merge=True)
```

- del_hls 为True时会删除ts、m3u8、key等文件，否则会经过处理后保留，以便直接使用
- merge 为True时会自动合并为mp4

## 安装

### rye 安装

```bash
rye sync
```

### pip 安装
该`requirements.lock`文件是在Mac环境在生成的，不同系统环境下可能会遇到不同的效果，如果使用请使用`rye`安装

```bash
pip install -r requirements.lock
```