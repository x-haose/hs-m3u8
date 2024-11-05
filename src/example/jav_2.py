import asyncio

from hs_m3u8 import M3u8Downloader


def get_m3u8(resp_text: str):
    """
    获取m3u8真实文本
    Args:
        resp_text: m3u8_url 获取到的响应文本

    Returns:
        返回真正的m3u8文本

    """
    return resp_text


async def main():
    url = "https://surrit.com/6d3bb2b2-d707-4b79-adf0-89542cb1383c/playlist.m3u8"
    name = "SDAB-129"
    headers = {
        "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
        "(KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36"
    }
    dl = M3u8Downloader(
        m3u8_url=url, save_path=f"../../downloads/{name}", headers=headers, max_workers=64, get_m3u8_func=get_m3u8
    )
    await dl.run(del_hls=False, merge=True)


if __name__ == "__main__":
    asyncio.run(main())
