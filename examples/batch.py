"""批量下载一组地址：所有任务共用请求上限，已存在的跳过，失败的按能否重试分开记下。

三个来源都是公开的测试流：Mux 的 TS、Unified Streaming 的《钢铁之泪》、JW Player 的 AES-128 加密流（解密由本库
按播放列表的 EXT-X-KEY 自动完成）。每个都选码率最低的变体，合计约 60 MB。
"""

import asyncio
from pathlib import Path

from hs_m3u8 import (
    Engine,
    HsM3u8Error,
    JobRequest,
    OutputExistsError,
    OutputOptions,
    Preference,
    Source,
    Target,
)

SOURCES = {
    "test_001": "https://test-streams.mux.dev/test_001/stream.m3u8",
    "tears_of_steel": "https://demo.unified-streaming.com/k8s/features/stable/video/tears-of-steel/tears-of-steel.ism/.m3u8",
    "oceans_aes": "https://playertest.longtailvideo.com/adaptive/oceans_aes/oceans_aes.m3u8",
}


async def lowest(engine: Engine, url: str) -> Preference:
    """写了分辨率与码率的变体里码率最低的那个（没写分辨率的多为纯音频）；没有这样的变体时用默认的选法。"""
    probe = await engine.probe(Source(url))
    if probe.master is None:
        return Preference()
    video = [(v.bandwidth, v) for v in probe.master.variants if v.resolution is not None and v.bandwidth is not None]
    if not video:
        return Preference()
    _, lowest = min(video, key=lambda pair: pair[0])
    return Preference(variant=lowest.choice())


async def download_one(engine: Engine, name: str, url: str) -> str:
    """下载一个来源，返回一行结果。已下完的不再探测；来源意外是直播时报错，不一直录下去。"""
    path = Path("downloads/batch") / f"{name}.mp4"
    if path.exists():
        return f"{name}：已存在，跳过"
    try:
        source = Source(url, preference=await lowest(engine, url))
        output = await engine.start(JobRequest(source, OutputOptions(Target.Mp4(path)), live=None)).wait()
    except OutputExistsError:  # 探测期间别处写出了同一个文件
        return f"{name}：已存在，跳过"
    except HsM3u8Error as error:
        kind = "稍后重试可能成功" if error.retryable else "不是临时故障"
        return f"{name}：失败（{kind}）：{error}"
    return f"{name}：{output.segments} 个分片，{output.bytes / 1_000_000:.1f} MB"


async def main() -> None:
    engine = Engine(max_requests=32)  # 所有点播任务合计最多 32 个在途的分片、key 与 init 段请求
    results = await asyncio.gather(*(download_one(engine, name, url) for name, url in SOURCES.items()))
    print("\n".join(results))


asyncio.run(main())
