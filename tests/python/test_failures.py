"""失败在 Python 里的样子：来源与选轨、请求、校验、放不进 MP4 的内容、任务目录各种失败抛的异常类与原因。"""

import asyncio
import json
import socket
from collections.abc import Callable
from pathlib import Path

import pytest

from conftest import Server, fixture
from hs_m3u8 import (
    AudioChoice,
    AudioKey,
    Encryption,
    Engine,
    HlsUnsupported,
    HlsUnsupportedError,
    HsM3u8Error,
    HttpError,
    HttpErrorKind,
    HttpOptions,
    IntegrityError,
    IntegrityFailure,
    JobRequest,
    JobType,
    Malformed,
    Mp4Unsupported,
    Mp4UnsupportedError,
    NotMediaPlaylistError,
    OutputOptions,
    PlaylistError,
    Preference,
    Purpose,
    ResponseTooLargeError,
    RetryPolicy,
    SelectError,
    SelectFailure,
    Source,
    SourceUnsupported,
    SourceUnsupportedError,
    StreamKind,
    Target,
    VariantChoice,
    VariantKey,
    WorkDirError,
    WorkDirProblem,
)

# 一次就失败，不等重试
NO_RETRY = HttpOptions(retry=RetryPolicy(attempts=1))

MEDIA = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n"

MASTER = """#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="English",LANGUAGE="en",DEFAULT=YES,URI="audio.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="other",NAME="Commentary",LANGUAGE="en",URI="other.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,AUDIO="aud"
video.m3u8
"""


def failure(source: Source, directory: Path) -> HsM3u8Error:
    """把 `source` 下载成 `directory` 下的 MP4，返回任务抛出的异常；没抛出时测试失败。

    失败的任务留下的任务目录也在 `directory` 下：各次调用给不同的目录，后一次才不会续到前一次的任务目录上。
    """
    request = JobRequest(source, OutputOptions(Target.Mp4(directory / "out.mp4")))
    with pytest.raises(HsM3u8Error) as raised:
        asyncio.run(Engine().start(request).wait())
    return raised.value


def test_playlists_that_cannot_be_read_or_used_are_reported(server: Server, tmp_path: Path) -> None:
    head = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n"
    key = '#EXT-X-KEY:METHOD={},URI="key"{}\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n'
    malformed: list[tuple[str, bytes | str, Malformed]] = [
        ("empty.m3u8", "", Malformed.Empty()),
        ("bytes.m3u8", b"#EXTM3U\n#EXTINF:1,\n\xff.ts\n", Malformed.NotUtf8()),
        ("mixed.m3u8", head + "#EXT-X-STREAM-INF:BANDWIDTH=1\na.m3u8\n#EXTINF:1,\nb.ts\n", Malformed.Mixed()),
    ]

    def encrypted(encryption: Encryption) -> Callable[[str], SourceUnsupported]:
        return lambda url: SourceUnsupported.Encrypted(url=url, line=3, encryption=encryption)

    unsupported: list[tuple[str, str, Callable[[str], SourceUnsupported]]] = [
        ("sample-aes.m3u8", head + key.format("SAMPLE-AES", ""), encrypted(Encryption.SampleAes())),
        (
            "drm.m3u8",
            head + key.format("SAMPLE-AES", ',KEYFORMAT="com.example.drm"'),
            encrypted(Encryption.Drm("com.example.drm")),
        ),
        ("aes-256.m3u8", head + key.format("AES-256", ""), encrypted(Encryption.Method("AES-256"))),
        ("no-segments.m3u8", head + "#EXT-X-ENDLIST\n", SourceUnsupported.EmptyPlaylist),
        ("no-target.m3u8", "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n", SourceUnsupported.NoTargetDuration),
        (
            "no-key-uri.m3u8",
            head + "#EXT-X-KEY:METHOD=AES-128\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
            SourceUnsupported.KeyWithoutUri,
        ),
    ]
    for path, body, _ in [*malformed, *unsupported]:
        server.put(path, body)
    server.put("seg0.ts", fixture("ts_a/seg0.ts"))

    for path, _, reason in malformed:
        error = failure(Source(server.url(path), http=NO_RETRY), tmp_path / path)
        assert isinstance(error, PlaylistError), path
        assert (error.url, error.reason) == (server.url(path), reason)
    for path, _, expected in unsupported:
        error = failure(Source(server.url(path), http=NO_RETRY), tmp_path / path)
        assert isinstance(error, SourceUnsupportedError), path
        assert error.reason == expected(server.url(path))


def test_syntax_errors_say_which_line(server: Server, tmp_path: Path) -> None:
    server.put("index.m3u8", "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:abc,\nseg0.ts\n")

    error = failure(Source(server.url("index.m3u8")), tmp_path)

    assert isinstance(error, PlaylistError)
    assert isinstance(error.reason, Malformed.Syntax)
    assert error.reason.line == 3


def test_a_master_where_a_media_playlist_belongs_is_reported(server: Server, tmp_path: Path) -> None:
    server.put("master.m3u8", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nnested.m3u8\n")
    server.put("nested.m3u8", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nmedia.m3u8\n")

    error = failure(Source(server.url("master.m3u8")), tmp_path)

    assert isinstance(error, NotMediaPlaylistError)
    assert error.url == server.url("nested.m3u8")


def test_selections_the_master_cannot_satisfy_are_reported(server: Server, tmp_path: Path) -> None:
    server.put("master.m3u8", MASTER)
    for name in ["video.m3u8", "audio.m3u8", "other.m3u8"]:
        server.put(name, MEDIA)
    absent_variant = VariantKey.Attributes(bandwidth=1, resolution=None, codecs=[], audio_group=None, occurrence=0)
    absent_audio = AudioKey.Stable(id="de", occurrence=0)
    cases: list[tuple[Preference, SelectFailure]] = [
        (Preference(variant=VariantChoice.Matching(absent_variant)), SelectFailure.VariantNotFound()),
        (
            Preference(audio=AudioChoice.Language("fr")),
            SelectFailure.AudioLanguageNotFound(group="aud", language="fr", available=["en"]),
        ),
        (Preference(audio=AudioChoice.Index(5)), SelectFailure.AudioIndexOutOfRange(index=5, count=2)),
        (Preference(audio=AudioChoice.Matching(absent_audio)), SelectFailure.AudioNotFound()),
        (Preference(audio=AudioChoice.Index(1)), SelectFailure.NotVariantAudio(index=1, group="aud")),
    ]

    for index, (preference, reason) in enumerate(cases):
        error = failure(Source(server.url("master.m3u8"), preference=preference), tmp_path / str(index))
        assert isinstance(error, SelectError), reason
        assert error.reason == reason


def test_request_failures_say_what_went_wrong(server: Server, tmp_path: Path) -> None:
    # 重定向到自己：跟到上限即失败
    server.status("loop.m3u8", 302, headers=[("Location", "/loop.m3u8")])
    # 字节范围请求：服务器不认 Range，返回整个文件与 200；或者返回 206，而 Content-Range 与请求的不符
    ranged = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-BYTERANGE:100@0\n#EXTINF:1,\n{}\n#EXT-X-ENDLIST\n"
    server.put("ignored.m3u8", ranged.format("whole.ts"))
    server.put("whole.ts", fixture("ts_a/seg0.ts"))
    server.put("mismatch.m3u8", ranged.format("other.ts"))
    server.status("other.ts", 206, headers=[("Content-Range", "bytes 100-199/1000")])

    redirected = failure(Source(server.url("loop.m3u8"), http=NO_RETRY), tmp_path / "loop")
    ignored = failure(Source(server.url("ignored.m3u8"), http=NO_RETRY), tmp_path / "ignored")
    mismatched = failure(Source(server.url("mismatch.m3u8"), http=NO_RETRY), tmp_path / "mismatch")

    assert isinstance(redirected, HttpError)
    assert isinstance(redirected.kind, HttpErrorKind.Redirect)
    assert isinstance(ignored, HttpError)
    assert ignored.kind == HttpErrorKind.RangeIgnored(200)
    assert ignored.segment is not None
    assert ignored.segment.url == server.url("whole.ts")
    assert isinstance(mismatched, HttpError)
    assert mismatched.kind == HttpErrorKind.RangeMismatch("bytes 100-199/1000")


def test_unreachable_servers_are_connection_failures(tmp_path: Path) -> None:
    # 刚关掉的端口：连接被拒
    with socket.socket() as closed:
        closed.bind(("127.0.0.1", 0))
        port = closed.getsockname()[1]
    error = failure(Source(f"http://127.0.0.1:{port}/index.m3u8", http=NO_RETRY), tmp_path)

    assert isinstance(error, HttpError)
    assert isinstance(error.kind, HttpErrorKind.Connect)
    assert error.retryable


def put_media(server: Server, name: str, groups: list[list[str]], duration: float = 1) -> Source:
    """放一个点播的媒体播放列表，返回它的来源。

    `groups` 为各不连续段组的分片（tests/fixtures/media 下的路径），组间加 EXT-X-DISCONTINUITY；每个分片声明
    `duration` 秒。
    """
    lines = ["#EXTM3U", "#EXT-X-TARGETDURATION:2"]
    for index, segments in enumerate(groups):
        if index > 0:
            lines.append("#EXT-X-DISCONTINUITY")
        for path in segments:
            served = f"{name}/{len(lines)}-{Path(path).name}"
            server.put(served, fixture(path))
            lines += [f"#EXTINF:{duration},", served.removeprefix(f"{name}/")]
    lines.append("#EXT-X-ENDLIST")
    server.put(f"{name}/index.m3u8", "\n".join(lines) + "\n")
    return Source(server.url(f"{name}/index.m3u8"), http=NO_RETRY)


def test_content_an_mp4_cannot_hold_is_reported_with_where_it_is(server: Server, tmp_path: Path) -> None:
    audio = StreamKind.Audio()
    cases: list[tuple[str, list[list[str]], float, Mp4Unsupported]] = [
        ("opus", [["ts_opus/seg0.ts"]], 1, Mp4Unsupported.Codec(group=0, track=0, kind=audio, codec="opus")),
        ("mp3", [["ts_mp3_8k/seg0.ts"]], 1, Mp4Unsupported.Mp3SampleRate(group=0, track=0, sample_rate=8000)),
        ("eac3", [["ts_eac3_short/seg0.ts"]], 1, Mp4Unsupported.Eac3ShortFrames(group=0, track=0)),
        (
            "config",
            [["ts_aac_stereo/seg0.ts"], ["ts_he_aac/seg0.ts"]],
            1,
            Mp4Unsupported.ConfigChanged(group=1, track=0, kind=audio),
        ),
        (
            "pid",
            [["ts_pid_change/seg0.ts", "ts_pid_change/seg1.ts"]],
            1,
            Mp4Unsupported.StreamChangedInGroup(group=0, track=0, kind=audio),
        ),
        (
            "repeat",
            [["ts_repeat/seg0.ts", "ts_repeat/seg1.ts"]],
            1,
            Mp4Unsupported.DtsNotIncreasing(group=0, track=0, kind=StreamKind.Video()),
        ),
        ("gop", [["ts_long_gop/seg1.ts"]], 1, Mp4Unsupported.NoKeyframe(group=0, track=0)),
        ("format", [["ts_a/seg0.ts", "packed/aac/seg0.aac"]], 1, Mp4Unsupported.FormatChangedInGroup(group=0, track=0)),
        # 音频在第 8.5 秒才出现，声明的时长为 0 时探测不到
        ("late", [["ts_audio_late_in_segment/seg0.ts"]], 0, Mp4Unsupported.ParamsUnknown(group=0, track=0, kind=audio)),
    ]

    for name, groups, duration, reason in cases:
        error = failure(put_media(server, name, groups, duration), tmp_path / name)
        assert isinstance(error, Mp4UnsupportedError), name
        assert error.reason == reason, name


def test_changes_inside_a_group_say_which_group_and_track(server: Server, tmp_path: Path) -> None:
    resized = failure(put_media(server, "resized", [["ts_a/seg0.ts"], ["ts_small/seg0.ts"]]), tmp_path / "resized")
    changed = failure(
        put_media(server, "changed", [["ts_param_change/seg0.ts", "ts_param_change/seg1.ts"]]), tmp_path / "changed"
    )
    jumped = failure(put_media(server, "jumped", [["ts_jump/seg0.ts", "ts_jump/seg1.ts"]]), tmp_path / "jumped")

    assert isinstance(resized, Mp4UnsupportedError)
    assert isinstance(resized.reason, Mp4Unsupported.ParamsChanged)
    assert (resized.reason.group, resized.reason.track) == (1, 0)
    assert isinstance(changed, Mp4UnsupportedError)
    assert isinstance(changed.reason, Mp4Unsupported.ParamsChangedInGroup)
    assert (changed.reason.group, changed.reason.track) == (0, 0)
    assert isinstance(jumped, Mp4UnsupportedError)
    assert isinstance(jumped.reason, Mp4Unsupported.TimestampJump)
    assert (jumped.reason.group, jumped.reason.track, jumped.reason.segment) == (0, 0, 1)


def put_split(server: Server, audio_duration: float = 1, *, bandwidth: str = "BANDWIDTH=200000,") -> Source:
    """放一个视频与独立音频分离的点播（fmp4_a），音频分片各声明 `audio_duration` 秒；返回主播放列表的来源。"""
    master = (
        "#EXTM3U\n"
        '#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="a",DEFAULT=YES,URI="audio.m3u8"\n'
        f'#EXT-X-STREAM-INF:{bandwidth}RESOLUTION=320x180,AUDIO="aud"\n'
        "video.m3u8\n"
    )
    server.put("master.m3u8", master)
    for kind, duration in [("video", 1), ("audio", audio_duration)]:
        server.put(f"{kind}/init.mp4", fixture(f"fmp4_a/{kind}/init.mp4"))
        lines = ["#EXTM3U", "#EXT-X-TARGETDURATION:1", f'#EXT-X-MAP:URI="{kind}/init.mp4"']
        for index in range(2):
            server.put(f"{kind}/seg{index}.m4s", fixture(f"fmp4_a/{kind}/seg{index}.m4s"))
            lines += [f"#EXTINF:{duration},", f"{kind}/seg{index}.m4s"]
        server.put(f"{kind}.m3u8", "\n".join([*lines, "#EXT-X-ENDLIST"]) + "\n")
    return Source(server.url("master.m3u8"), http=NO_RETRY)


def test_content_that_fails_its_checks_is_rejected(server: Server, tmp_path: Path) -> None:
    video = fixture("fmp4_a/video/seg0.m4s")
    cases: list[tuple[str, bytes, str]] = [
        ("page", b"<html><body>not found</body></html>" * 8, ""),
        ("not-fmp4", fixture("ts_a/seg0.ts"), "init.mp4"),
        # CDN 在分片末尾加了换行
        ("trailing", video + b"\n", "init.mp4"),
    ]
    server.put("init.mp4", fixture("fmp4_a/video/init.mp4"))
    raised: dict[str, HsM3u8Error] = {}
    for name, body, init in cases:
        server.put(f"{name}.bin", body)
        map_tag = f'#EXT-X-MAP:URI="{init}"\n' if init else ""
        server.put(
            f"{name}.m3u8", f"#EXTM3U\n#EXT-X-TARGETDURATION:1\n{map_tag}#EXTINF:1,\n{name}.bin\n#EXT-X-ENDLIST\n"
        )
        raised[name] = failure(Source(server.url(f"{name}.m3u8"), http=NO_RETRY), tmp_path / name)

    reasons: dict[str, IntegrityFailure] = {}
    for name, *_ in cases:
        error = raised[name]
        assert isinstance(error, IntegrityError), name
        assert error.url == server.url(f"{name}.bin")
        assert not error.retryable
        reasons[name] = error.reason
    assert isinstance(reasons["page"], IntegrityFailure.UnrecognizedSegment)
    assert isinstance(reasons["not-fmp4"], IntegrityFailure.NotFmp4)
    assert reasons["trailing"] == IntegrityFailure.Fmp4TrailingBytes(at=len(video), len=1)


def test_oversized_key_responses_are_refused(server: Server, tmp_path: Path) -> None:
    body = bytes(2 << 20)
    server.put(
        "index.m3u8",
        '#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=AES-128,URI="huge.key"\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n',
    )
    server.put("seg0.ts", fixture("aes/seg0.ts"))
    server.put("huge.key", body)

    error = failure(Source(server.url("index.m3u8"), http=NO_RETRY), tmp_path)

    assert isinstance(error, ResponseTooLargeError)
    assert (error.url, error.purpose) == (server.url("huge.key"), Purpose.Key())
    assert error.limit < len(body)
    assert not error.retryable


def test_untimed_packed_audio_beside_video_cannot_be_aligned(server: Server, tmp_path: Path) -> None:
    master = (
        "#EXTM3U\n"
        '#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="a",DEFAULT=YES,URI="audio.m3u8"\n'
        '#EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO="aud"\nvideo.m3u8\n'
    )
    server.put("master.m3u8", master)
    server.put("video.m3u8", MEDIA)
    server.put("seg0.ts", fixture("ts_a/seg0.ts"))
    server.put("seg1.ts", fixture("ts_a/seg1.ts"))
    # 打包音频样本不带 ID3 时间戳
    server.put("audio.m3u8", MEDIA.replace(".ts", ".aac"))
    server.put("seg0.aac", fixture("packed/aac/seg0.aac"))
    server.put("seg1.aac", fixture("packed/aac/seg1.aac"))

    error = failure(Source(server.url("master.m3u8"), http=NO_RETRY), tmp_path)

    assert isinstance(error, Mp4UnsupportedError)
    assert error.reason == Mp4Unsupported.UntimedPackedAudio(group=0, track=1)


def test_local_hls_needs_a_bandwidth_for_split_tracks(server: Server, tmp_path: Path) -> None:
    source = put_split(server, audio_duration=0, bandwidth="")
    request = JobRequest(source, OutputOptions(Target.Hls(tmp_path / "hls")))

    with pytest.raises(HlsUnsupportedError) as raised:
        asyncio.run(Engine().start(request).wait())

    assert raised.value.reason == HlsUnsupported.BandwidthUnknown()


def test_work_dirs_that_cannot_be_used_say_why(server: Server, tmp_path: Path) -> None:
    server.put("index.m3u8", MEDIA)
    server.put("seg0.ts", fixture("ts_a/seg0.ts"))
    server.put("seg1.ts", fixture("ts_a/seg1.ts"))
    source = Source(server.url("index.m3u8"), http=NO_RETRY)
    for name, content in [
        ("foreign", None),
        ("corrupt", b"not json"),
        ("newer", json.dumps({"kind": "vod", "format_version": 99}).encode()),
    ]:
        work = tmp_path / name / "out.hsdl"
        work.mkdir(parents=True)
        if content is None:
            (work / "notes.txt").write_text("别人的文件", encoding="utf-8")
        else:
            (work / "job.json").write_bytes(content)

    foreign = failure(source, tmp_path / "foreign")
    corrupt = failure(source, tmp_path / "corrupt")
    newer = failure(source, tmp_path / "newer")

    assert isinstance(foreign, WorkDirError)
    assert foreign.problem == WorkDirProblem.NotEmpty()
    assert isinstance(corrupt, WorkDirError)
    assert isinstance(corrupt.problem, WorkDirProblem.Corrupt)
    assert isinstance(newer, WorkDirError)
    assert isinstance(newer.problem, WorkDirProblem.UnsupportedVersion)
    assert (newer.problem.file, newer.problem.found) == ("job.json", 99)
    assert newer.problem.supported < 99


def test_resuming_with_a_different_request_is_refused(server: Server, tmp_path: Path) -> None:
    # 第二个分片取不到：任务失败，任务目录里留下第一个
    server.put("index.m3u8", MEDIA)
    server.put("seg0.ts", fixture("ts_a/seg0.ts"))
    source = Source(server.url("index.m3u8"), http=NO_RETRY)
    failure(source, tmp_path)
    output = OutputOptions(Target.Mp4(tmp_path / "out.mp4"))

    merged = asyncio.run(merge_failure(output))
    server.put("index.m3u8", MEDIA.replace("#EXT-X-ENDLIST\n", ""))
    live = failure(source, tmp_path)
    server.put("index.m3u8", MEDIA.replace("#EXT-X-ENDLIST", "#EXTINF:1,\nseg2.ts\n#EXT-X-ENDLIST"))
    replanned = failure(source, tmp_path)

    assert isinstance(merged, WorkDirError)
    assert merged.problem == WorkDirProblem.NotLiveRecording()
    assert isinstance(live, WorkDirError)
    assert live.problem == WorkDirProblem.KindMismatch(recorded=JobType.Vod(), current=JobType.Live())
    assert isinstance(replanned, WorkDirError)
    assert replanned.problem == WorkDirProblem.PlanChanged()


async def merge_failure(output: OutputOptions) -> HsM3u8Error:
    """只合并 `output` 的任务目录里已录的，返回抛出的异常；没抛出时测试失败。"""
    with pytest.raises(HsM3u8Error) as raised:
        await Engine().merge_recorded(output).wait()
    return raised.value


def test_resuming_a_selection_that_changed_is_refused(server: Server, tmp_path: Path) -> None:
    source = put_split(server)
    # 第二个音频分片取不到：任务失败，任务目录里留下已完成的分片
    server.status("audio/seg1.m4s", 404)
    failure(source, tmp_path)

    other = Source(server.url("master.m3u8"), http=NO_RETRY, preference=Preference(audio=AudioChoice.Language("en")))
    preference = failure(other, tmp_path)
    tracks = failure(Source(server.url("video.m3u8"), http=NO_RETRY), tmp_path)
    server.put("master.m3u8", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nvideo.m3u8\n")
    gone = failure(source, tmp_path)

    assert isinstance(preference, WorkDirError)
    assert preference.problem == WorkDirProblem.PreferenceChanged()
    assert isinstance(tracks, WorkDirError)
    assert tracks.problem == WorkDirProblem.TracksMismatch()
    assert isinstance(gone, WorkDirError)
    assert gone.problem == WorkDirProblem.SelectionGone()
