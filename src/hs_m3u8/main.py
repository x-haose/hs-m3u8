"""
M3U8 下载器
"""

import asyncio
import posixpath
import shutil
from collections.abc import Callable
from hashlib import md5
from pathlib import Path
from typing import Any
from urllib.parse import ParseResult, urljoin, urlparse

import av
import m3u8
from hssp import Net
from hssp.models.net import RequestModel
from hssp.network.response import Response
from hssp.utils import crypto
from loguru import logger


class M3u8Key:
    """
    M3u8key
    """

    def __init__(self, key: bytes, iv: str | bytes | None = None):
        """
        Args:
            key: 密钥
            iv: 偏移，可以是十六进制字符串(0x开头)、bytes或None
        """
        try:
            if isinstance(iv, str) and iv.startswith("0x"):
                iv = bytes.fromhex(iv[2:])
            elif isinstance(iv, str):
                iv = bytes.fromhex(iv)
        except ValueError as e:
            raise ValueError(f"iv {iv!r} 值不对: {e}") from e

        if iv and len(iv) != 16:
            raise ValueError(f"iv {iv} 长度不等于16")

        self.key = key
        self.iv = iv


class M3u8Downloader:
    """
    M3u8 异步下载器，并保留hls文件
    """

    def __init__(
        self,
        m3u8_url: str,
        save_path: str,
        is_decrypt: bool = False,
        max_workers: int | None = None,
        headers: dict[str, Any] | None = None,
        key: M3u8Key | None = None,
        get_m3u8_func: Callable | None = None,
        m3u8_request_before: Callable[[RequestModel], RequestModel] | None = None,
        m3u8_response_after: Callable[[Response], Response] | None = None,
        key_request_before: Callable[[RequestModel], RequestModel] | None = None,
        key_response_after: Callable[[Response], Response] | None = None,
        ts_request_before: Callable[[RequestModel], RequestModel] | None = None,
        ts_response_after: Callable[[Response], Response] | None = None,
    ):
        """

        Args:
            m3u8_url: m3u8 地址
            save_path: 保存路径
            is_decrypt: 如果ts被加密，是否解密ts
            max_workers: 最大并发数
            headers: 情求头
            get_m3u8_func: 处理m3u8情求的回调函数。适用于m3u8地址不是真正的地址，
                           而是包含m3u8内容的情求，会把m3u8_url的响应传递给get_m3u8_func，要求返回真正的m3u8内容
           m3u8_request_before: m3u8请求前的回调函数
           m3u8_response_after: m3u8响应后的回调函数
           key_request_before: key请求前的回调函数
           key_response_after: key响应后的回调函数
           ts_request_before: ts请求前的回调函数
           ts_response_after: ts响应后的回调函数
        """

        sem = asyncio.Semaphore(max_workers) if max_workers else None
        self.headers = headers

        # m3u8 内容的请求器
        self.m3u8_net = Net(sem=sem)
        if m3u8_request_before:
            self.m3u8_net.request_before_signal.connect(m3u8_request_before)
        if m3u8_response_after:
            self.m3u8_net.response_after_signal.connect(m3u8_response_after)

        # 加密key的请求器
        self.key_net = Net()
        if key_request_before:
            self.key_net.request_before_signal.connect(key_request_before)
        if key_response_after:
            self.key_net.response_after_signal.connect(key_response_after)

        # ts内容的请求器
        self.ts_net = Net()
        if ts_request_before:
            self.ts_net.request_before_signal.connect(ts_request_before)
        if ts_response_after:
            self.ts_net.response_after_signal.connect(ts_response_after)

        self.is_decrypt = is_decrypt
        self.m3u8_url = urlparse(m3u8_url)
        self.get_m3u8_func = get_m3u8_func
        self.save_dir = Path(save_path) / "hls"
        self.save_name = Path(save_path).name
        self.key_path = self.save_dir / "key.key"
        self.custom_key = key

        # 实例变量初始化
        self.retry_count: int = 0
        self.retry_max_count: int = 50
        self.ts_url_list: list = []
        self.ts_path_list: list = []
        self.ts_key: M3u8Key | None = None
        self.mp4_head_url: str | None = None
        self.m3u8_md5: str = ""

        if not self.save_dir.exists():
            self.save_dir.mkdir(parents=True)

        logger.add(self.save_dir.parent / f"{self.save_name}.log")
        self.logger = logger

    async def run(self, merge=True, del_hls=False):
        await self.start(merge, del_hls)
        await self.m3u8_net.close()
        await self.key_net.close()
        await self.ts_net.close()

    async def start(self, merge=True, del_hls=False):
        """
        下载器启动函数
        Args:
            merge: ts下载完后是否合并，默认合并
            del_hls: 是否删除hls系列文件，包括.m3u8文件、*.ts、.key文件

        Returns:

        """
        mp4_path = self.save_dir.parent / f"{self.save_name}.mp4"
        mp4_path = mp4_path.absolute()
        if mp4_path.exists():
            self.logger.info(f"{mp4_path}已存在")
            if del_hls and self.save_dir.exists():
                shutil.rmtree(str(self.save_dir))
            return True

        self.logger.info(
            f"开始下载: 合并ts为mp4={merge}, "
            f"删除hls信息={del_hls}, "
            f"下载地址为：{self.m3u8_url.geturl()}. 保存路径为：{self.save_dir.absolute()}"
        )

        await self._download()
        self.logger.info("ts下载完成")
        self.ts_path_list = [ts_path for ts_path in self.ts_path_list if ts_path]
        count_1, count_2 = len(self.ts_url_list), len(self.ts_path_list)
        self.logger.info(f"TS应下载数量为：{count_1}, 实际下载数量为：{count_2}")
        if count_1 == 0 or count_2 == 0:
            self.logger.error("ts数量为0，请检查！！！")
            return None

        if count_2 != count_1:
            self.logger.error(f"ts下载数量与实际数量不符合！！！应该下载数量为：{count_1}, 实际下载数量为：{count_2}")
            self.logger.error(self.ts_url_list)
            self.logger.error(self.ts_path_list)
            if self.retry_count < self.retry_max_count:
                self.retry_count += 1
                self.logger.error(f"正在进行重试：{self.retry_count}/{self.retry_max_count}")
                return await self.start(merge, del_hls)
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
        Returns:

        """
        self.ts_url_list = await self.get_ts_list(self.m3u8_url)
        self.ts_path_list = [None] * len(self.ts_url_list)
        await asyncio.gather(*[self._download_ts(url) for url in self.ts_url_list])

    async def get_ts_list(self, url: ParseResult) -> list[dict]:
        """
        解析m3u8并保存至列表
        Args:
            url: m3u8地址

        Returns:

        """
        resp = await self.m3u8_net.get(url.geturl(), headers=self.headers)
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
            self.mp4_head_url = prefix + m3u8_obj.segment_map[0].uri
            m3u8_obj.segment_map[0].uri = "head.mp4"

        # 遍历ts文件
        for index, segments in enumerate(m3u8_obj.segments):
            ts_uri = segments.uri if "http" in m3u8_obj.segments[index].uri else segments.absolute_uri
            m3u8_obj.segments[index].uri = f"{index}.ts"
            ts_url_list.append({"uri": ts_uri, "index": index})

        # 保存解密key
        if len(m3u8_obj.keys) > 0 and m3u8_obj.keys[0]:
            iv = m3u8_obj.keys[0].iv
            if not self.custom_key:
                resp = await self.key_net.get(m3u8_obj.keys[0].absolute_uri, headers=self.headers)
                key_data = resp.content
            else:
                key_data = self.custom_key.key
                iv = self.custom_key.iv or iv

            self.save_file(key_data, self.key_path)
            self.ts_key = M3u8Key(key=key_data, iv=iv)
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
        Args:
            ts_item: ts数据

        Returns:

        """
        index = ts_item["index"]
        ts_uri = ts_item["uri"]
        ts_path = self.save_dir / f"{index}.ts"

        if ts_path.exists():
            self.ts_path_list[index] = str(ts_path)
            return

        resp = await self.ts_net.get(ts_item["uri"], self.headers)
        ts_content = resp.content
        if ts_content is None:
            return

        if self.ts_key and self.is_decrypt:
            ts_content = crypto.decrypt_aes_256_cbc(ts_content, self.ts_key.key, self.ts_key.iv)

        self.save_file(ts_content, ts_path)
        self.logger.info(f"{ts_uri}下载成功")
        self.ts_path_list[index] = str(ts_path)

    async def merge(self) -> bool:
        """
        合并ts文件为mp4文件

        Returns:
            返回是否合并成功
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

        # 如果有mp4的头，则把ts放到后面
        mp4_head_data = b""
        if self.mp4_head_url:
            resp = await self.ts_net.get(self.mp4_head_url)
            mp4_head_data = resp.content
            mp4_head_file = self.save_dir / "head.mp4"
            mp4_head_file.write_bytes(mp4_head_data)

        # 把ts文件整合到一起
        with big_ts_path.open("ab+") as big_ts_file:
            big_ts_file.write(mp4_head_data)
            for path in self.ts_path_list:
                with open(path, "rb") as ts_file:
                    data = ts_file.read()
                    if self.ts_key:
                        data = crypto.decrypt_aes_256_cbc(data, self.ts_key.key, self.ts_key.iv)
                    big_ts_file.write(data)
        self.logger.info("ts文件整合完毕")

        # 把大的ts文件转换成mp4文件
        self.logger.info(f"ts整合成功，开始转为mp4。 ts路径：{big_ts_path} mp4路径：{mp4_path}")
        result = self.ts_to_mp4(big_ts_path, mp4_path)
        if not result:
            self.logger.error("ts转mp4失败！")
            return False

        # 删除完整ts文件
        big_ts_path.unlink()
        return True

    @staticmethod
    def save_file(content: bytes | str, filepath: Path | str):
        """
        保存内容到文件
        Args:
            content: 内容
            filepath: 文件路径

        Returns:

        """
        mode = "wb" if isinstance(content, bytes) else "w"
        with open(file=filepath, mode=mode) as file:
            file.write(content)

    @staticmethod
    def ts_to_mp4(ts_path: Path, mp4_path: Path) -> bool:
        """
        将 TS 转为 MP4 (stream copy,不重编码)
        Args:
            ts_path: ts 视频文件路径
            mp4_path: mp4 视频文件路径

        Returns:
            返回是否转换成功：mp4路径存在并且是一个文件并且大小大于0
        """
        if not ts_path.exists():
            raise FileNotFoundError("ts文件不存在")

        if not mp4_path.parent.exists():
            mp4_path.parent.mkdir(parents=True)

        with av.open(str(ts_path)) as input_container, av.open(str(mp4_path), "w") as output_container:
            # 映射视频流
            out_stream = None
            if input_container.streams.video:
                in_stream = input_container.streams.video[0]
                out_stream = output_container.add_stream(in_stream.codec_context.name)
                out_stream.width = in_stream.codec_context.width
                out_stream.height = in_stream.codec_context.height
                out_stream.pix_fmt = in_stream.codec_context.pix_fmt
                if in_stream.average_rate:
                    out_stream.rate = in_stream.average_rate

            # 映射音频流 (如果存在)
            out_audio = None
            if input_container.streams.audio:
                in_audio = input_container.streams.audio[0]
                out_audio = output_container.add_stream(in_audio.codec_context.name)
                out_audio.rate = in_audio.codec_context.sample_rate  # type: ignore
                if in_audio.codec_context.layout:
                    out_audio.layout = in_audio.codec_context.layout  # type: ignore
                if in_audio.codec_context.format:
                    out_audio.format = in_audio.codec_context.format  # type: ignore

            # Stream copy - 直接复制数据包,不重编码
            for packet in input_container.demux():
                if packet.dts is None:
                    continue

                if packet.stream.type == "video" and out_stream:
                    packet.stream = out_stream
                    output_container.mux(packet)
                elif packet.stream.type == "audio" and out_audio:
                    packet.stream = out_audio
                    output_container.mux(packet)

        return mp4_path.exists() and mp4_path.is_file() and mp4_path.stat().st_size > 0
