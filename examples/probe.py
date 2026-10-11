"""先探测，列出可选的清晰度与音轨，再挑一路下载。

用的是 Apple 公开的 bipbop 测试流：24 路清晰度、三组音轨（AAC、AC-3、E-AC-3）。默认选的最高画质配的是 AC-3 音轨，
MP4 放不进（会报 Mp4UnsupportedError，可改为输出本地 HLS）；这里挑音频为 AAC（CODECS 写作 mp4a.40.2）的变体里码率
最低的一路，约 40 MB。
"""

import asyncio

from hs_m3u8 import Engine, JobRequest, OutputOptions, Preference, Source, Target, VariantChoice

URL = "https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_ts/master.m3u8"


async def main() -> None:
    """列出探测结果，选音频为 AAC 的变体里码率最低的下载。"""
    engine = Engine()
    probe = await engine.probe(Source(URL))
    if probe.master is None:
        print("来源本身是媒体播放列表，没有可选的清晰度")
        return
    for variant in probe.master.variants:
        size = f"{variant.resolution.width}x{variant.resolution.height}" if variant.resolution else "分辨率未写"
        rate = f"{variant.bandwidth // 1000} kbps" if variant.bandwidth is not None else "码率未写"
        print(variant.index, size, rate, ",".join(variant.codecs))
    for audio in probe.master.audio:
        print("音轨", audio.index, audio.group, audio.language, audio.name)
    print("默认会选：", probe.master.selected.variant)

    aac = [(v.bandwidth, v.index) for v in probe.master.variants if v.bandwidth is not None and "mp4a.40.2" in v.codecs]
    if not aac:
        print("没有写了码率、音频为 AAC 的变体")
        return
    _, index = min(aac)
    source = Source(URL, preference=Preference(variant=VariantChoice.Index(index)))
    output = await engine.start(JobRequest(source, OutputOptions(Target.Mp4("downloads/bipbop.mp4")))).wait()
    print("已保存到", output.mp4.path if output.mp4 else None)


asyncio.run(main())
