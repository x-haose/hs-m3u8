"""录制直播，按 Ctrl+C 停止并写出已录到的部分。

用的是 Unified Streaming 公开的直播测试流。录制从播放列表里已列出的分片开始（这个流列出最近约 10 分钟，几十秒内
下完），之后跟着直播实时录；按分片的时长算最多录 15 分钟。按 Ctrl+C 停止刷新，把已列出的分片下完后写出；再按一次
取消，已录到的分片留在任务目录。录制中断（如断网、取消、进程被杀）后再运行一次即续录；不再联网、只想把已录到的
写出来，用 `Engine().merge_recorded(OutputOptions(...))`。
"""

import asyncio
import signal
from datetime import timedelta

from hs_m3u8 import Engine, JobRequest, LiveOptions, OutputOptions, Source, Target

URL = "https://demo.unified-streaming.com/k8s/live/stable/live.isml/.m3u8"


async def main() -> None:
    request = JobRequest(
        Source(URL),
        OutputOptions(Target.Mp4("downloads/live.mp4")),
        live=LiveOptions(max_duration=timedelta(minutes=15)),
    )
    job = Engine().start(request)

    def stop(*_: object) -> None:
        """停止录制；再按 Ctrl+C 时按原来的处理（取消）。"""
        job.stop()
        signal.signal(signal.SIGINT, previous)

    previous = signal.signal(signal.SIGINT, stop)
    print("录制中，按 Ctrl+C 停止")
    try:
        output = await job.wait()
    finally:
        signal.signal(signal.SIGINT, previous)
    if output.live is not None:
        print("结束原因：", output.live.end, "，录了", job.progress().duration)
    for missed in output.missed:
        print("缺失：", missed)
    print("已保存到", output.mp4.path if output.mp4 else None)


asyncio.run(main())
