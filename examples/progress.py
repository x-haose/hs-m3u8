"""用 Engine 开始任务：每秒显示一次进度，失败时按异常的类别给出处理建议。

来源同 download.py（Mux 的测试流，约 60 MB），存到另一个文件。中断后再运行一次即续传，只下缺的分片。
"""

import asyncio

from hs_m3u8 import Engine, HsM3u8Error, Job, JobRequest, Output, OutputOptions, RequestError, Source, Target

URL = "https://test-streams.mux.dev/test_001/stream.m3u8"


async def watch(job: Job) -> Output:
    """等任务结束，期间每秒显示一次进度。"""
    waiting = asyncio.create_task(job.wait())
    while True:
        done, _ = await asyncio.wait({waiting}, timeout=1)
        progress = job.progress()
        megabytes = progress.received / 1_000_000
        print(f"{progress.segments_done}/{progress.segments_total} 个分片，已读 {megabytes:.1f} MB")
        if done:
            return await waiting


async def main() -> None:
    try:
        job = Engine().start(JobRequest(Source(URL), OutputOptions(Target.Mp4("downloads/progress.mp4"))))
        output = await watch(job)
    except RequestError as error:  # 改请求再试：参数不对、输出已存在、选轨偏好与来源不符等
        print("请求不对：", error)
    except HsM3u8Error as error:
        print(error, "（稍后重试可能成功）" if error.retryable else "")
    else:
        if output.mp4 is not None:
            print("已保存到", output.mp4.path)
            for stream in output.mp4.report.streams:
                print("  ", stream.shape, stream.packets, "个包", stream.duration)


asyncio.run(main())
