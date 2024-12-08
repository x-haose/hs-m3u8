import asyncio

from hs_m3u8 import M3u8Downloader


async def main():
    url = "https://v4.qrssv.com/202412/05/CCu6EzN8tR20/video/index.m3u8"
    name = "毒液:最后一舞 HD-索尼"
    dl = M3u8Downloader(m3u8_url=url, save_path=f"../../downloads/{name}", max_workers=64)
    await dl.run(del_hls=False, merge=True)


if __name__ == "__main__":
    asyncio.run(main())
