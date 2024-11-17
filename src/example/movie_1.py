import asyncio

from hs_m3u8 import M3u8Downloader


async def main():
    url = "https://v3.dious.cc/20220422/EZWdBGuQ/index.m3u8"
    name = "日日是好日"
    dl = M3u8Downloader(m3u8_url=url, save_path=f"../../downloads/{name}", max_workers=64)
    await dl.run(del_hls=False, merge=True)


if __name__ == "__main__":
    asyncio.run(main())
