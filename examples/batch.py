"""批量下载一组地址：所有任务共用请求上限，已存在的跳过，失败的按能否重试分开记下。

三个来源都是公开的测试流：Mux 的 TS、Unified Streaming 的《钢铁之泪》、JW Player 的 AES-128 加密流（解密由本库
按播放列表的 EXT-X-KEY 自动完成）。每个都选码率最低的一路，合计约 60 MB。
"""

import asyncio

from hs_m3u8 import (
    Engine,
    HsM3u8Error,
    JobRequest,
    OutputExistsError,
    OutputOptions,
    Preference,
    Source,
    Target,
    VariantChoice,
)

SOURCES = {
    "test_001": "https://test-streams.mux.dev/test_001/stream.m3u8",
    "tears_of_steel": "https://demo.unified-streaming.com/k8s/features/stable/video/tears-of-steel/tears-of-steel.ism/.m3u8",
    "oceans_aes": "https://playertest.longtailvideo.com/adaptive/oceans_aes/oceans_aes.m3u8",
}


async def lowest(engine: Engine, url: str) -> Preference:
    """写了分辨率与码率的变体里码率最低的那一路（没写分辨率的多为纯音频）；没有这样的变体时用默认的选法。"""
    probe = await engine.probe(Source(url))
    if probe.master is None:
        return Preference()
    video = [
        (v.bandwidth, v.index) for v in probe.master.variants if v.resolution is not None and v.bandwidth is not None
    ]
    if not video:
        return Preference()
    _, index = min(video)
    return Preference(variant=VariantChoice.Index(index))


async def download(engine: Engine, name: str, url: str) -> str:
    """下载一个来源，返回一行结果。"""
    try:
        source = Source(url, preference=await lowest(engine, url))
        request = JobRequest(source, OutputOptions(Target.Mp4(f"downloads/batch/{name}.mp4")))
        output = await engine.start(request).wait()
    except OutputExistsError:
        return f"{name}：已存在，跳过"
    except HsM3u8Error as error:
        again = "稍后重试" if error.retryable else "不再重试"
        return f"{name}：失败（{again}）：{error}"
    return f"{name}：{output.segments} 个分片，{output.bytes / 1_000_000:.1f} MB"


async def main() -> None:
    """同时下载全部来源，所有任务合计最多 32 个在途请求。"""
    engine = Engine(max_requests=32)
    results = await asyncio.gather(*(download(engine, name, url) for name, url in SOURCES.items()))
    print("\n".join(results))


asyncio.run(main())
