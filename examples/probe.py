"""先探测，列出可选的清晰度与音轨，再按分辨率挑一个变体下载。

用的是 Apple 公开的 bipbop 测试流（10 分钟）：24 个变体，音轨分 AAC、AC-3、E-AC-3 三组。这里挑高度不超过 270 的
变体里码率最高的那个（配 AC-3 音轨），约 56 MB。
"""

import asyncio

from hs_m3u8 import Engine, JobRequest, OutputOptions, Preference, Source, Target

URL = "https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_ts/master.m3u8"
MAX_HEIGHT = 270


async def main() -> None:
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

    fits = [
        (v.bandwidth, v.index)
        for v in probe.master.variants
        if v.resolution is not None and v.resolution.height <= MAX_HEIGHT and v.bandwidth is not None
    ]
    if not fits:
        print(f"没有写了码率、高度不超过 {MAX_HEIGHT} 的变体")
        return
    _, index = max(fits)
    # 按身份选：探测之后主播放列表重排了变体也选到这一个
    source = Source(URL, preference=Preference(variant=probe.master.variants[index].choice()))
    output = await engine.start(JobRequest(source, OutputOptions(Target.Mp4("downloads/bipbop.mp4")))).wait()
    print("已保存到", output.mp4.path if output.mp4 else None)


asyncio.run(main())
