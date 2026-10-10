"""任务：写出输出、进度、取消与续传、直播的停止与只合并、放弃任务，以及失败时异常里的位置。"""

import asyncio
import copy
import pickle
from datetime import timedelta
from pathlib import Path

import pytest

from conftest import Gate, Server, assert_survives_pickle, fixture
from hs_m3u8 import (
    AudioChoice,
    Engine,
    HttpError,
    HttpErrorKind,
    HttpOptions,
    InvalidInputError,
    JobCancelledError,
    JobRequest,
    KeyOverride,
    Leftover,
    LiveEnd,
    LiveOptions,
    Output,
    OutputExistsError,
    OutputOptions,
    Preference,
    Progress,
    RequestParts,
    RetryPolicy,
    Shape,
    Source,
    Stage,
    Target,
    Timeouts,
    Unsettled,
    VariantChoice,
)

VOD = """#EXTM3U
#EXT-X-TARGETDURATION:2
#EXTINF:2,
seg0.ts
#EXTINF:2,
seg1.ts
#EXT-X-ENDLIST
"""

LIVE = """#EXTM3U
#EXT-X-TARGETDURATION:2
#EXTINF:2,
seg0.ts
#EXTINF:2,
seg1.ts
"""

# 等请求到达的上限，秒；只在出错时才会等满
ARRIVAL = 10.0


def put_media(server: Server, playlist: str) -> None:
    server.put("index.m3u8", playlist)
    server.put("seg0.ts", fixture("ts_a/seg0.ts"))
    server.put("seg1.ts", fixture("ts_a/seg1.ts"))


def one_at_a_time(server: Server, target: Target) -> JobRequest:
    """依次下载分片的请求：前一个下完才请求下一个，停住某个分片时之前的都已完成。"""
    return JobRequest(Source(server.url("index.m3u8")), OutputOptions(target), concurrency=1)


def run(request: JobRequest) -> Output:
    """在新的事件循环里跑完任务。"""

    async def main() -> Output:
        return await Engine().start(request).wait()

    return asyncio.run(main())


def discard(work_dir: Path) -> list[Leftover]:
    return asyncio.run(Engine().discard(work_dir))


async def arrived(gate: Gate) -> None:
    assert await asyncio.to_thread(gate.arrived.wait, ARRIVAL)


def assert_exception_survives_pickle(error: HttpError) -> None:
    """`error` 经 pickle、copy 重建后类型、说明、注记与各属性都不变（如进程池把子进程里的异常传回）。"""
    error.add_note("注记")
    attributes = ("url", "kind", "retryable", "retry_after", "segment", "key_url", "leftovers")
    for rebuilt in (pickle.loads(pickle.dumps(error)), copy.copy(error), copy.deepcopy(error)):
        assert type(rebuilt) is HttpError
        assert (rebuilt.args, rebuilt.__notes__) == (error.args, error.__notes__)
        assert [getattr(rebuilt, name) for name in attributes] == [getattr(error, name) for name in attributes]


def test_downloads_write_the_mp4_and_report_its_streams(server: Server, tmp_path: Path) -> None:
    put_media(server, VOD)
    target = tmp_path / "out.mp4"

    async def main() -> tuple[Output, Progress]:
        job = Engine().start(JobRequest(Source(server.url("index.m3u8")), OutputOptions(Target.Mp4(target))))
        output = await job.wait()
        return output, job.progress()

    output, progress = asyncio.run(main())

    assert output.mp4 is not None
    assert output.mp4.path == target
    assert target.is_file()
    assert {type(stream.shape) for stream in output.mp4.report.streams} == {Shape.Video, Shape.Audio}
    assert all(stream.packets > 0 for stream in output.mp4.report.streams)
    assert (output.segments, output.hls, output.live, output.leftovers) == (2, None, None, [])
    assert not (tmp_path / "out.hsdl").exists()
    assert progress.stage == Stage.Done()
    assert (progress.segments_done, progress.segments_total) == (2, 2)


def test_both_outputs_are_written_and_waits_get_the_same_result(server: Server, tmp_path: Path) -> None:
    put_media(server, VOD)
    target = Target.Both(mp4=tmp_path / "a.mp4", hls=tmp_path / "a")

    async def main() -> tuple[Output, Output]:
        job = Engine().start(JobRequest(Source(server.url("index.m3u8")), OutputOptions(target)))
        return await job.wait(), await job.wait()

    first, second = asyncio.run(main())

    assert first == second
    assert_survives_pickle(first)
    assert first.hls == tmp_path / "a"
    assert (tmp_path / "a" / "index.m3u8").is_file()
    assert (tmp_path / "a.mp4").is_file()


def test_cancelled_jobs_keep_their_segments_and_resume(server: Server, tmp_path: Path) -> None:
    put_media(server, VOD)
    gate = server.gate("seg1.ts")
    request = one_at_a_time(server, Target.Mp4(tmp_path / "out.mp4"))

    async def cancel() -> Progress:
        job = Engine().start(request)
        await arrived(gate)
        progress = job.progress()
        job.cancel()
        gate.release()
        with pytest.raises(JobCancelledError):
            await job.wait()
        return progress

    progress = asyncio.run(cancel())

    assert progress.stage == Stage.Downloading()
    assert (progress.segments_done, progress.segments_total) == (1, 2)
    assert_survives_pickle(progress)
    assert request.output.resolved_work_dir().is_dir()
    assert run(request).segments == 2
    assert server.hits("seg0.ts") == 1


def test_cancelling_the_wait_cancels_the_job(server: Server, tmp_path: Path) -> None:
    put_media(server, VOD)
    gate = server.gate("seg1.ts")
    request = one_at_a_time(server, Target.Mp4(tmp_path / "out.mp4"))

    async def main() -> None:
        job = Engine().start(request)
        waiting = asyncio.create_task(job.wait())
        await arrived(gate)
        waiting.cancel()
        with pytest.raises(asyncio.CancelledError):
            await waiting
        with pytest.raises(JobCancelledError):
            await asyncio.wait_for(job.wait(), ARRIVAL)

    asyncio.run(main())

    assert gate.abandoned.wait(ARRIVAL)
    assert request.output.resolved_work_dir().is_dir()


def test_jobs_no_longer_referenced_are_cancelled(server: Server, tmp_path: Path) -> None:
    put_media(server, VOD)
    gate = server.gate("seg1.ts")
    request = one_at_a_time(server, Target.Mp4(tmp_path / "out.mp4"))

    async def main() -> None:
        job = Engine().start(request)
        await arrived(gate)
        del job

    asyncio.run(main())

    assert gate.abandoned.wait(ARRIVAL)
    assert not (tmp_path / "out.mp4").exists()


def test_failed_segments_say_which_and_the_task_can_be_discarded(server: Server, tmp_path: Path) -> None:
    put_media(server, VOD)
    server.status("seg1.ts", 404)
    request = one_at_a_time(server, Target.Mp4(tmp_path / "out.mp4"))

    with pytest.raises(HttpError) as raised:
        run(request)

    assert raised.value.kind == HttpErrorKind.Status(404)
    segment = raised.value.segment
    assert segment is not None
    assert (segment.track, segment.sequence, segment.url) == (0, 1, server.url("seg1.ts"))
    assert_exception_survives_pickle(raised.value)
    work_dir = request.output.resolved_work_dir()
    assert work_dir.is_dir()
    assert discard(work_dir) == []
    assert not work_dir.exists()


def test_live_recordings_stop_on_request_and_merge_what_was_recorded(server: Server, tmp_path: Path) -> None:
    put_media(server, LIVE)
    gate = server.gate("seg1.ts")
    request = one_at_a_time(server, Target.Mp4(tmp_path / "out.mp4"))

    async def stop() -> Output:
        job = Engine().start(request)
        await arrived(gate)
        job.stop()
        gate.release()
        return await job.wait()

    output = asyncio.run(stop())

    assert output.live is not None
    assert output.live.end == LiveEnd.Stopped()
    assert (output.live.session_count, output.segments) == (1, 2)


def test_interrupted_recordings_can_be_merged_without_the_network(server: Server, tmp_path: Path) -> None:
    put_media(server, LIVE)
    gate = server.gate("seg1.ts")
    request = one_at_a_time(server, Target.Mp4(tmp_path / "out.mp4"))

    async def interrupt() -> None:
        job = Engine().start(request)
        await arrived(gate)
        job.cancel()
        gate.release()
        with pytest.raises(JobCancelledError):
            await job.wait()

    async def merge() -> Output:
        return await Engine().merge_recorded(request.output).wait()

    asyncio.run(interrupt())
    output = asyncio.run(merge())

    assert output.live is not None
    assert output.live.end is None
    assert output.segments == 1


def test_existing_outputs_are_reported_with_their_path(server: Server, tmp_path: Path) -> None:
    put_media(server, VOD)
    target = tmp_path / "out.mp4"
    target.write_bytes(b"old")

    with pytest.raises(OutputExistsError) as raised:
        run(one_at_a_time(server, Target.Mp4(target)))

    assert raised.value.path == target
    assert server.hits("index.m3u8") == 0


def test_requests_survive_pickle_by_value(tmp_path: Path) -> None:
    source = Source(
        "https://example.com/a.m3u8?token=t",
        preference=Preference(variant=VariantChoice.Index(1), audio=AudioChoice.Language("en")),
        http=HttpOptions(
            headers=[("Cookie", "a"), ("Cookie", "b")],
            proxy="socks5h://127.0.0.1:1080",
            insecure=True,
            timeouts=Timeouts(connect=timedelta(seconds=3)),
            retry=RetryPolicy(attempts=2),
        ),
    )
    output = OutputOptions(
        Target.Both(mp4=tmp_path / "a.mp4", hls=tmp_path / "a"),
        work_dir=tmp_path / "work",
        overwrite=True,
        keep_work_dir=True,
    )
    request = JobRequest(
        source,
        output,
        key=KeyOverride(bytes(range(16)), iv=bytes(16)),
        concurrency=3,
        live=LiveOptions(max_duration=timedelta(minutes=5)),
    )

    for value in (request, Unsettled.Report(), RequestParts("https://example.com/a.ts", headers={"a": "b"})):
        assert_survives_pickle(value)
    assert request != JobRequest(source, output)
    assert hash(request) == hash(pickle.loads(pickle.dumps(request)))


def test_invalid_job_arguments_are_reported_and_keys_stay_out_of_reprs(server: Server, tmp_path: Path) -> None:
    source = Source(server.url("index.m3u8"))
    output = OutputOptions(Target.Mp4(tmp_path / "out.mp4"))
    key = bytes(range(16))

    with pytest.raises(InvalidInputError):
        KeyOverride(bytes(15))
    with pytest.raises(InvalidInputError):
        KeyOverride(key, iv=bytes(3))
    with pytest.raises(InvalidInputError):
        JobRequest(source, output, concurrency=0)
    override = KeyOverride(key, iv=key)
    assert (override.key, override.iv) == (key, key)
    assert repr(override) == "KeyOverride(key=..., iv=...)"
    assert "concurrency=16" in repr(JobRequest(source, output))
