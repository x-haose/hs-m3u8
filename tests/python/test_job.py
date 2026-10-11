"""任务：写出输出、进度、取消与续传、直播的停止与只合并、放弃任务，以及失败时异常里的位置。"""

import asyncio
import copy
import errno
import gc
import os
import pickle
import subprocess
import sys
import threading
from datetime import timedelta
from pathlib import Path

import pytest

from conftest import Gate, Server, assert_survives_pickle, fixture
from hs_m3u8 import (
    AudioChoice,
    Engine,
    FileError,
    HookError,
    HookKind,
    Hooks,
    HttpError,
    HttpErrorKind,
    HttpOptions,
    IntegrityError,
    IntegrityFailure,
    InvalidInputError,
    JobCancelledError,
    JobRequest,
    KeyOverride,
    Leftover,
    LeftoverKind,
    LiveEnd,
    LiveOptions,
    MissReason,
    Output,
    OutputExistsError,
    OutputOptions,
    Preference,
    Progress,
    Proxy,
    RequestParts,
    RetryPolicy,
    Shape,
    Source,
    Stage,
    Target,
    Timeouts,
    Unsettled,
    VariantChoice,
    VolumeUnavailableError,
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

# 播放列表写的 IV 不对，key 地址取不到：只有自定义的 key 与 IV 解得开
ENCRYPTED = f"""#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-KEY:METHOD=AES-128,URI="key.bin",IV=0x{bytes(16).hex()}
#EXTINF:2,
seg0.ts
#EXTINF:2,
seg1.ts
#EXT-X-ENDLIST
"""

# 第二个分片取不到（404），记为缺失
LIVE_WITH_MISSING = """#EXTM3U
#EXT-X-TARGETDURATION:1
#EXTINF:1,
seg0.ts
#EXTINF:1,
missing.ts
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


def discard(work_dir: Path) -> tuple[Leftover, ...]:
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
    assert (output.segments, output.hls, output.live, output.leftovers) == (2, None, None, ())
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
    assert discard(work_dir) == ()
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


@pytest.mark.skipif(sys.platform == "win32", reason="Windows 上文件之下的路径报不存在，不是「不是目录」")
def test_file_errors_carry_the_os_error(server: Server, tmp_path: Path) -> None:
    put_media(server, VOD)
    blocker = tmp_path / "file"
    blocker.write_bytes(b"")
    work_dir = blocker / "work"
    request = JobRequest(
        Source(server.url("index.m3u8")),
        OutputOptions(Target.Mp4(tmp_path / "out.mp4"), work_dir=work_dir),
    )

    with pytest.raises(FileError) as raised:
        run(request)

    assert raised.value.path == work_dir
    cause = raised.value.__cause__
    assert isinstance(cause, NotADirectoryError)
    assert (cause.errno, cause.filename) == (errno.ENOTDIR, str(work_dir))


def test_existing_outputs_are_reported_and_replaced_on_request(server: Server, tmp_path: Path) -> None:
    put_media(server, VOD)
    target = tmp_path / "out.mp4"
    target.write_bytes(b"old")

    with pytest.raises(OutputExistsError) as raised:
        run(one_at_a_time(server, Target.Mp4(target)))
    assert raised.value.path == target
    assert server.hits("index.m3u8") == 0

    replace = OutputOptions(Target.Mp4(target), overwrite=True)
    assert run(JobRequest(Source(server.url("index.m3u8")), replace)).segments == 2
    assert target.read_bytes() != b"old"
    assert not replace.resolved_work_dir().exists()

    keep = OutputOptions(Target.Mp4(tmp_path / "kept.mp4"), keep_work_dir=True)
    assert run(JobRequest(Source(server.url("index.m3u8")), keep)).segments == 2
    assert keep.resolved_work_dir().is_dir()


def test_key_overrides_decrypt_with_their_iv(server: Server, tmp_path: Path) -> None:
    # 样本由 tests/fixtures/media/encrypt.sh 生成：key 为 00 01 … 0f，IV 为 10 11 … 1f；播放列表写的 IV 不对
    key, iv = bytes(range(16)), bytes(range(16, 32))
    server.put("index.m3u8", ENCRYPTED)
    server.put("seg0.ts", fixture("aes/seg0.ts"))
    server.put("seg1.ts", fixture("aes/seg1.ts"))
    output = OutputOptions(Target.Mp4(tmp_path / "out.mp4"))

    with pytest.raises(IntegrityError) as wrong_iv:
        run(JobRequest(Source(server.url("index.m3u8")), output, key=KeyOverride(key)))
    decrypted = run(JobRequest(Source(server.url("index.m3u8")), output, key=KeyOverride(key, iv=iv)))

    assert isinstance(wrong_iv.value.reason, IntegrityFailure.UnrecognizedSegment)
    assert decrypted.segments == 2
    assert server.hits("key.bin") == 0


def test_key_hooks_return_the_key_and_other_lengths_fail_the_hook(server: Server, tmp_path: Path) -> None:
    # key 地址给的是逐字节取反的 key；播放列表写对 IV（样本见上一个测试）
    key, iv = bytes(range(16)), bytes(range(16, 32))
    server.put("index.m3u8", ENCRYPTED.replace(f"IV=0x{bytes(16).hex()}", f"IV=0x{iv.hex()}"))
    server.put("key.bin", bytes(b ^ 0xFF for b in key))
    server.put("seg0.ts", fixture("aes/seg0.ts"))
    server.put("seg1.ts", fixture("aes/seg1.ts"))
    output = OutputOptions(Target.Mp4(tmp_path / "out.mp4"))

    class Invert(Hooks):
        def on_key(self, url: str, data: bytes) -> bytes:
            del url
            return bytes(b ^ 0xFF for b in data)

    class Truncate(Hooks):
        def on_key(self, url: str, data: bytes) -> bytes:
            del url
            return data[:15]

    with pytest.raises(HookError) as truncated:
        run(JobRequest(Source(server.url("index.m3u8"), hooks=Truncate()), output))
    decrypted = run(JobRequest(Source(server.url("index.m3u8"), hooks=Invert()), output))

    assert truncated.value.hook == HookKind.Key()
    assert truncated.value.__cause__ is None
    assert truncated.value.key_url == server.url("key.bin")
    assert decrypted.segments == 2


def test_segment_hooks_transform_segments_before_they_are_checked(server: Server, tmp_path: Path) -> None:
    put_media(server, VOD)
    for name in ["seg0.ts", "seg1.ts"]:
        server.put(name, b"\x89PNG" + fixture(f"ts_a/{name}"))
    called: list[str] = []

    class StripDisguise(Hooks):
        def on_segment(self, url: str, data: bytes) -> bytes:
            called.append(url)
            return data.removeprefix(b"\x89PNG")

    output = run(
        JobRequest(
            Source(server.url("index.m3u8"), hooks=StripDisguise()), OutputOptions(Target.Mp4(tmp_path / "a.mp4"))
        )
    )

    assert output.segments == 2
    assert sorted(called) == [server.url("seg0.ts"), server.url("seg1.ts")]


def test_live_recordings_end_at_endlist_and_report_missing_segments(server: Server, tmp_path: Path) -> None:
    server.put("index.m3u8", LIVE_WITH_MISSING)
    server.put("seg0.ts", fixture("ts_a/seg0.ts"))
    gate = server.gate("seg0.ts")
    request = one_at_a_time(server, Target.Mp4(tmp_path / "out.mp4"))

    async def record() -> tuple[Output, Progress]:
        job = Engine().start(request)
        await arrived(gate)
        server.put("index.m3u8", LIVE_WITH_MISSING + "#EXT-X-ENDLIST\n")
        gate.release()
        output = await job.wait()
        return output, job.progress()

    output, progress = asyncio.run(record())

    assert output.live is not None
    assert output.live.end == LiveEnd.EndList()
    assert [(m.first, m.last, m.reason) for m in output.live.missed] == [
        (1, 1, MissReason.Failed(HttpErrorKind.Status(404)))
    ]
    assert (progress.segments_done, progress.segments_failed, progress.segments_expired) == (1, 1, 0)
    # 读到的字节含播放列表与取不到的请求，多于写进任务目录的分片
    assert progress.received > progress.bytes > 0


# 在子进程里跑：任务的回调还在执行（等 stdin）时脚本就结束了。脚本自己登记的 atexit 处理在本库的之前运行（后登记的
# 先运行），打印 exiting 后，测试才放行回调；回调写出标记文件即执行完了
UNFINISHED_HOOK_SCRIPT = """
import asyncio, atexit, sys, threading
from hs_m3u8 import Engine, Hooks, JobRequest, OutputOptions, Source, Target

started = threading.Event()

class Slow(Hooks):
    def on_playlist(self, url: str, body: bytes) -> str:
        started.set()
        sys.stdin.readline()
        with open(sys.argv[3], "w") as marker:
            marker.write("done")
        return body.decode()

atexit.register(print, "exiting", flush=True)

async def main() -> None:
    request = JobRequest(Source(sys.argv[1], hooks=Slow()), OutputOptions(Target.Mp4(sys.argv[2])))
    job = Engine().start(request)
    await asyncio.to_thread(started.wait)
    del job

asyncio.run(main())
"""


def test_interpreter_exit_waits_for_running_hooks(server: Server, tmp_path: Path) -> None:
    put_media(server, VOD)
    marker = tmp_path / "marker"
    script = [sys.executable, "-c", UNFINISHED_HOOK_SCRIPT, server.url("index.m3u8"), str(tmp_path / "a.mp4")]
    lines: list[str] = []

    with subprocess.Popen([*script, str(marker)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True) as child:
        stdin, stdout = child.stdin, child.stdout
        assert stdin is not None
        assert stdout is not None
        try:
            reader = threading.Thread(target=lambda: lines.append(stdout.readline()))
            reader.start()
            reader.join(ARRIVAL)
            stdin.write("go\n")
            stdin.close()
            code = child.wait(ARRIVAL)
        finally:
            child.kill()

    assert lines == ["exiting\n"]
    assert code == 0
    assert marker.read_text() == "done"


@pytest.mark.skipif(sys.platform == "win32", reason="Windows 没有 fork")
# 测的就是在多线程进程里 fork；子进程只回收对象、等管道、退出，不做会死锁的事
@pytest.mark.filterwarnings("ignore:This process .* is multi-threaded:DeprecationWarning")
def test_forked_children_hold_no_work_dir_locks_and_leave_parent_jobs_alone(server: Server, tmp_path: Path) -> None:
    put_media(server, VOD)
    gate = server.gate("seg1.ts")
    request = one_at_a_time(server, Target.Mp4(tmp_path / "out.mp4"))
    # 子进程等父进程续传完再退出（父进程退出时管道也会关上）：它活着时父进程照样能续传
    parent_done, wait_parent = os.pipe()
    children: list[int] = []

    async def main() -> Output:
        engine = Engine()
        job = engine.start(request)
        await arrived(gate)
        pid = os.fork()
        if pid == 0:
            # 子进程：回收继承来的任务不碰父进程的运行时，没有无法抛出的异常；退出码 0 为是
            code = 1
            try:
                unraisable: list[object] = []
                sys.unraisablehook = unraisable.append
                del job
                gc.collect()
                os.read(parent_done, 1)
                code = 3 if unraisable else 0
            finally:
                os._exit(code)
        children.append(pid)
        job.cancel()
        with pytest.raises(JobCancelledError):
            await job.wait()
        gate.release()
        return await engine.start(request).wait()

    try:
        output = asyncio.run(main())
    finally:
        os.write(wait_parent, b"\0")
        exit_codes = [os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]) for pid in children]

    assert output.segments == 2
    assert exit_codes == [0]


@pytest.mark.skipif(sys.platform != "win32", reason="只有 Windows 造得出读不到的根（不存在的盘符）")
def test_unreachable_work_dirs_fail_or_are_reported_as_asked() -> None:
    letter = next(letter for letter in "ZYXWVUTSRQPONMLKJIHGFED" if not Path(f"{letter}:\\").exists())
    work_dir = Path(f"{letter}:\\job.hsdl")

    with pytest.raises(VolumeUnavailableError):
        discard(work_dir)
    leftovers = asyncio.run(Engine().discard(work_dir, unsettled=Unsettled.Report()))

    assert [(leftover.path, leftover.kind) for leftover in leftovers] == [(work_dir, LeftoverKind.WorkDir())]


def test_requests_survive_pickle_by_value(tmp_path: Path) -> None:
    source = Source(
        "https://example.com/a.m3u8?token=t",
        preference=Preference(variant=VariantChoice.Index(1), audio=AudioChoice.Language("en")),
        http=HttpOptions(
            headers=[("Cookie", "a"), ("Cookie", "b")],
            proxy=Proxy.Url("socks5h://127.0.0.1:1080"),
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
    with pytest.raises(InvalidInputError):
        JobRequest(source, output, concurrency=-1)
    override = KeyOverride(key, iv=key)
    assert (override.key, override.iv) == (key, key)
    assert repr(override) == "KeyOverride(key=..., iv=...)"
    assert "concurrency=" in repr(JobRequest(source, output))
