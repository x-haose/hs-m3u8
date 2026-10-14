"""`download`：按默认设置下载一次。"""

import os
from collections.abc import Iterable, Mapping

from hs_m3u8._native import Engine, HttpOptions, JobRequest, Output, OutputOptions, Source, Target


async def download(
    url: str,
    path: str | os.PathLike[str],
    *,
    headers: Mapping[str, str] | Iterable[tuple[str, str]] | None = None,
) -> Output:
    """下载点播来源 `url`，存成 MP4 文件 `path`：选最高清的变体（见 `VariantChoice.Best`）与它的默认音频。

    `headers` 附加到所有请求，如站点要求的 Referer、Cookie；其余设置都取默认值。返回的 `Output.mp4` 不为 None，不在成片
    里的分片（来源标了 EXT-X-GAP 的，及因此合并不了的）列在 `Output.missed`。失败时抛 `HsM3u8Error` 的子类：MP4 已存在
    时为 `OutputExistsError`，来源是直播时为 `LiveNotEnabledError`。中断或失败后，已下完的分片留在 MP4 旁的任务目录
    `<主名>.hsdl` 里，再次调用即续传。被取消时等任务结束才抛出（见 `Job.wait`）；取消晚于换上 MP4 时 MP4 已写好，再次
    调用报 `OutputExistsError`。录制直播、设代理或并发、输出本地 HLS、用站点适配回调时用 `Engine`。
    """
    source = Source(url, http=HttpOptions(headers=headers))
    request = JobRequest(source, OutputOptions(Target.Mp4(path)), live=None)
    return await Engine().start(request).wait()
