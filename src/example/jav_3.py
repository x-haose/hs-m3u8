import asyncio

from hs_m3u8 import M3u8Downloader


async def main():
    url = "https://surrit.com/85f671be-4ebc-4cad-961e-a8d339483cc6/playlist.m3u8"
    name = "CUS-2413"
    dl = M3u8Downloader(m3u8_url=url, save_path=f"../../downloads/{name}", max_workers=64)
    await dl.run(del_hls=False, merge=True)


if __name__ == "__main__":
    asyncio.run(main())
