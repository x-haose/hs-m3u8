"""
M3U8 下载器
"""

import asyncio
import platform
import posixpath
import shutil
import subprocess
from collections.abc import Callable
from hashlib import md5
from pathlib import Path
from urllib.parse import urljoin, urlparse
from zipfile import ZipFile

import m3u8
from hssp import Net
from hssp.utils import crypto
from loguru import logger


def get_ffmpeg():
    """
    根据平台不同获取不同的ffmpeg可执行文件
    :return: FFmpeg 的可执行文件路径
    """
    current_os = platform.system()
    if current_os != "Windows":
        return "ffmpeg"

    res_path = Path(__file__).parent.parent.parent / "res"
    ffmpeg_bin = res_path / "ffmpeg_win.exe"

    if ffmpeg_bin.exists():
        return str(ffmpeg_bin)

    # ZIP 文件
    ffmpeg_bin_zip = Path(ffmpeg_bin.parent) / f"{ffmpeg_bin.name}.zip"
    if ffmpeg_bin_zip.exists():
        # 解压缩到同一目录
        with ZipFile(ffmpeg_bin_zip, "r") as zip_ref:
            zip_ref.extractall(ffmpeg_bin.parent)

    return ffmpeg_bin


class M3u8Key:
    """
    M3u8key
    """

    def __init__(self, key: bytes, iv: str = None):
        """
        :param key: 密钥
        :param iv: 偏移
        """
        self.key = key
        self.iv = iv or key


class M3u8Downloader:
    """
    M3u8 异步下载器，并保留hls文件
    """

    retry_count: int = 0
    retry_max_count: int = 50
    ts_url_list: list = []
    ts_path_list: list = []
    ts_key: M3u8Key = None
    mp4_head_hrl: str = None
    m3u8_md5 = ""

    def __init__(
        self,
        m3u8_url: str,
        save_path: str,
        decrypt=False,
        max_workers=None,
        headers=None,
        get_m3u8_func: Callable = None,
    ):
        """

        Args:
            m3u8_url: m3u8 地址
            save_path: 保存路径
            decrypt: 如果ts被加密，是否解密ts
            max_workers: 最大并发数
            headers: 情求头
            get_m3u8_func: 处理m3u8情求的回调函数。适用于m3u8地址不是真正的地址，
                           而是包含m3u8内容的情求，会把m3u8_url的响应传递给get_m3u8_func，要求返回真正的m3u8内容
        """

        sem = asyncio.Semaphore(max_workers) if max_workers else None
        self.headers = headers
        self.net = Net(sem=sem)
        self.decrypt = decrypt
        self.m3u8_url = urlparse(m3u8_url)
        self.get_m3u8_func = get_m3u8_func
        self.save_dir = Path(save_path) / "hls"
        self.save_name = Path(save_path).name
        self.key_path = self.save_dir / "key.key"

        if not self.save_dir.exists():
            self.save_dir.mkdir(parents=True)

        logger.add(self.save_dir.parent / f"{self.save_name}.log")
        self.logger = logger

    async def run(self, merge=True, del_hls=False):
        await self.start(merge, del_hls)
        await self.net.close()

    async def start(self, merge=True, del_hls=False):
        """
        下载器启动函数
        :param merge: ts下载完后是否合并，默认合并
        :param del_hls: 是否删除hls系列文件，包括.m3u8文件、*.ts、.key文件
        :return:
        """
        mp4_path = self.save_dir.parent / f"{self.save_name}.mp4"
        if Path(mp4_path).exists():
            self.logger.info(f"{mp4_path}已存在")
            if del_hls:
                shutil.rmtree(str(self.save_dir))
            return True

        self.logger.info(
            f"开始下载: 合并ts为mp4={merge}, "
            f"删除hls信息={del_hls}, "
            f"下载地址为：{self.m3u8_url.geturl()}. 保存路径为：{self.save_dir}"
        )

        await self._download()
        self.logger.info("ts下载完成")
        self.ts_path_list = [ts_path for ts_path in self.ts_path_list if ts_path]
        count_1, count_2 = len(self.ts_url_list), len(self.ts_path_list)
        self.logger.info(f"TS应下载数量为：{count_1}, 实际下载数量为：{count_2}")
        if count_1 == 0 or count_2 == 0:
            self.logger.error("ts数量为0，请检查！！！")
            return

        if count_2 != count_1:
            self.logger.error(f"ts下载数量与实际数量不符合！！！应该下载数量为：{count_1}, 实际下载数量为：{count_2}")
            self.logger.error(self.ts_url_list)
            self.logger.error(self.ts_path_list)
            if self.retry_count < self.retry_max_count:
                self.retry_count += 1
                self.logger.error(f"正在进行重试：{self.retry_count}/{self.retry_max_count}")
                return self.start(merge, del_hls)
            return False

        if not merge:
            return True

        if await self.merge():
            self.logger.info("合并成功")
        else:
            self.logger.error(
                f"mp4合并失败. ts应该下载数量为：{count_1}, 实际下载数量为：{count_2}. 保存路径为：{self.save_dir}"
            )
            return False
        if del_hls:
            shutil.rmtree(str(self.save_dir))
        return True

    async def _download(self):
        """
        下载ts文件、m3u8文件、key文件
        :return:
        """
        self.ts_url_list = await self.get_ts_list(self.m3u8_url)
        self.ts_path_list = [None] * len(self.ts_url_list)
        await asyncio.gather(*[self._download_ts(url) for url in self.ts_url_list])

    async def get_ts_list(self, url) -> list[dict]:
        """
        解析m3u8并保存至列表
        :param url:
        :return:
        """
        resp = await self.net.get(url.geturl(), headers=self.headers)
        m3u8_text = self.get_m3u8_func(resp.text) if self.get_m3u8_func else resp.text
        m3u8_obj = m3u8.loads(m3u8_text)
        prefix = f"{url.scheme}://{url.netloc}"
        base_path = posixpath.normpath(url.path + "/..") + "/"
        m3u8_obj.base_uri = urljoin(prefix, base_path)

        # 解析多层m3u8， 默认选取比特率最高的
        ts_url_list = []
        if len(m3u8_obj.playlists) > 0:
            bandwidth = 0
            play_url = ""
            self.logger.info("发现多个播放列表")
            for playlist in m3u8_obj.playlists:
                if int(playlist.stream_info.bandwidth) > bandwidth:
                    bandwidth = int(playlist.stream_info.bandwidth)
                    play_url = playlist.absolute_uri
            self.logger.info(f"选择的播放地址：{play_url}，比特率：{bandwidth}")
            return await self.get_ts_list(urlparse(play_url))

        # 处理具有 #EXT-X-MAP:URI="*.mp4" 的情况
        segment_map_count = len(m3u8_obj.segment_map)
        if segment_map_count > 0:
            if segment_map_count > 1:
                raise ValueError("暂不支持segment_map有多个的情况，请提交issues，并告知m3u8的地址，方便做适配")
            self.mp4_head_hrl = prefix + m3u8_obj.segment_map[0].uri
            m3u8_obj.segment_map[0].uri = "head.mp4"

        # 遍历ts文件
        for index, segments in enumerate(m3u8_obj.segments):
            ts_uri = segments.uri if "http" in m3u8_obj.segments[index].uri else segments.absolute_uri
            m3u8_obj.segments[index].uri = f"{index}.ts"
            ts_url_list.append({"uri": ts_uri, "index": index})

        # 保存解密key
        if len(m3u8_obj.keys) > 0 and m3u8_obj.keys[0]:
            resp = await self.net.get(m3u8_obj.keys[0].absolute_uri, headers=self.headers)
            key_data = resp.content
            self.save_file(key_data, self.key_path)
            self.ts_key = M3u8Key(key=key_data, iv=m3u8_obj.keys[0].iv)
            key = m3u8_obj.segments[0].key
            key.uri = "key.key"
            m3u8_obj.segments[0].key = key

        # 导出m3u8文件
        m3u8_text = m3u8_obj.dumps()
        self.m3u8_md5 = md5(m3u8_text.encode("utf8"), usedforsecurity=False).hexdigest().lower()
        self.save_file(m3u8_text, self.save_dir / f"{self.m3u8_md5}.m3u8")
        self.logger.info("导出m3u8文件成功")

        return ts_url_list

    async def _download_ts(self, ts_item: dict):
        """
        下载ts
        :param ts_item: ts 数据
        :return:
        """
        index = ts_item["index"]
        ts_uri = ts_item["uri"]
        ts_path = self.save_dir / f"{index}.ts"
        if Path(ts_path).exists():
            self.ts_path_list[index] = str(ts_path)
            return
        resp = await self.net.get(ts_item["uri"])
        ts_content = resp.content
        if ts_content is None:
            return

        if self.ts_key and self.decrypt:
            ts_content = crypto.decrypt_aes_256_cbc_pad7(ts_content, self.ts_key.key, self.ts_key.iv)

        self.save_file(ts_content, ts_path)
        self.logger.info(f"{ts_uri}下载成功")
        self.ts_path_list[index] = str(ts_path)

    async def merge(self):
        """
        合并ts文件为mp4文件
        :return:
        """
        self.logger.info("开始合并mp4")
        if len(self.ts_path_list) != len(self.ts_url_list):
            self.logger.error("数量不足拒绝合并！")
            return False

        # 整合后的ts文件路径
        big_ts_path = self.save_dir.parent / f"{self.save_name}.ts"
        if big_ts_path.exists():
            big_ts_path.unlink()

        # mp4路径
        mp4_path = self.save_dir.parent / f"{self.save_name}.mp4"

        # 如果保护mp4的头，则把ts放到后面
        mp4_head_data = b""
        if self.mp4_head_hrl:
            resp = await self.net.get(self.mp4_head_hrl)
            mp4_head_data = resp.content
            mp4_head_file = self.save_dir / "head.mp4"
            mp4_head_file.write_bytes(mp4_head_data)

        # 把ts文件整合到一起
        big_ts_file = big_ts_path.open("ab+")
        big_ts_file.write(mp4_head_data)
        for path in self.ts_path_list:
            with open(path, "rb") as ts_file:
                data = ts_file.read()
                if self.ts_key:
                    data = crypto.decrypt_aes_256_cbc_pad7(data, self.ts_key.key, self.ts_key.iv)
                big_ts_file.write(data)
        big_ts_file.close()
        self.logger.info("ts文件整合完毕")

        # 把大的ts文件转换成mp4文件
        ffmpeg_bin = get_ffmpeg()
        command = (
            f'{ffmpeg_bin} -i "{big_ts_path}" '
            f'-c copy -map 0:v -map 0:a? -bsf:a aac_adtstoasc -threads 32 "{mp4_path}" -y'
        )
        self.logger.info(f"ts整合成功，开始转为mp4。 command：{command}")
        result = subprocess.run(command, shell=True, capture_output=True, text=True)
        if result.returncode != 0:
            logger.error(f"命令执行失败: {result.stderr or result.stdout}")

        if Path(mp4_path).exists():
            big_ts_path.unlink()
        return Path(mp4_path).exists()

    @staticmethod
    def save_file(content: bytes | str, filepath):
        """
        保存内容到文件
        :param content: 内容
        :param filepath: 文件路径
        :return:
        """
        mode = "wb" if isinstance(content, bytes) else "w"
        with open(file=filepath, mode=mode) as file:
            file.write(content)
