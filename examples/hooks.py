"""站点适配回调：继承 Hooks，覆盖需要的方法。

真实站点常见的适配：
- on_request：按用途或地址改请求，如给分片请求签名，每次尝试（含重试）调用一次；所有请求都带的固定请求头用
  `HttpOptions(headers=...)`，不必写回调；
- on_playlist：改写拉到的播放列表，如解密、转码，或像这里一样只留前几个分片做预览；
- on_key、on_segment：站点把 key 加了一层、给分片加了伪装成图片的前缀时还原（这里的来源不需要，没有覆盖）。
回调在后台线程里执行，回调里的网络请求要设超时。用的是 Mux 公开的测试流，它不校验签名；签名的算法按站点的规则写，
这里只演示加在哪里。
"""

import asyncio
import hashlib
import time
from typing import override

from hs_m3u8 import Engine, Hooks, JobRequest, OutputOptions, Purpose, RequestParts, Source, Target

URL = "https://test-streams.mux.dev/test_001/stream.m3u8"


class Preview(Hooks):
    """给分片请求签名，只下载前 `segments` 个分片。"""

    def __init__(self, segments: int) -> None:
        """`segments`：媒体播放列表里保留的分片数。"""
        self.segments = segments

    @override
    def on_request(self, purpose: Purpose, request: RequestParts) -> RequestParts:
        """分片请求带上 5 分钟后过期的时间戳与签名；播放列表、key 与 init 段的请求不改。"""
        if not isinstance(purpose, Purpose.Segment):
            return request
        expires = str(int(time.time()) + 300)
        signature = hashlib.sha256(f"{request.url}|{expires}".encode()).hexdigest()
        headers = (*request.headers, ("X-Expires", expires), ("X-Signature", signature))
        return RequestParts(request.url, headers=headers)

    @override
    def on_playlist(self, url: str, body: bytes) -> str | None:
        """媒体播放列表只留前几个分片并标上结束；主播放列表不改（返回 None）。"""
        text = body.decode()
        if "#EXTINF" not in text:
            return None
        kept: list[str] = []
        segments = 0
        for line in text.splitlines():
            if line == "#EXT-X-ENDLIST":
                continue
            kept.append(line)
            if line and not line.startswith("#"):
                segments += 1
                if segments == self.segments:
                    break
        return "\n".join([*kept, "#EXT-X-ENDLIST", ""])


async def main() -> None:
    source = Source(URL, hooks=Preview(segments=3))
    output = await Engine().start(JobRequest(source, OutputOptions(Target.Mp4("downloads/preview.mp4")))).wait()
    print("下载了", output.segments, "个分片，已保存到", output.mp4.path if output.mp4 else None)


asyncio.run(main())
