"""最简单的用法：给地址和保存路径，下载最高清的变体，存成 MP4。

用的是 Mux 公开给开发者测试的流（Big Buck Bunny 的片段，8 分半，最高 360p，约 60 MB）。中断后再运行一次即续传，只下
缺的分片；已经下完时再运行抛 OutputExistsError。站点要求请求头时加 `headers={"Referer": ...}`。看进度、选清晰度、
录制直播等见同目录的其他示例。
"""

import asyncio

from hs_m3u8 import download

output = asyncio.run(download("https://test-streams.mux.dev/test_001/stream.m3u8", "downloads/test_001.mp4"))
print("已保存到", output.mp4.path if output.mp4 else None)
