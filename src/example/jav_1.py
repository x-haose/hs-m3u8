import asyncio

from hs_m3u8 import M3u8Downloader


async def main():
    url = "https://surrit.com/6d3bb2b2-d707-4b79-adf0-89542cb1383c/playlist.m3u8"
    name = "SDAB-129"
    dl = M3u8Downloader(m3u8_url=url, save_path=f"../../downloads/{name}", max_workers=64)
    await dl.run(del_hls=False, merge=True)


if __name__ == "__main__":
    asyncio.run(main())
