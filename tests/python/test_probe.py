"""探测：选轨结果与各轨概况、回调、失败时抛出的异常。"""

import asyncio
import gc
import os
import subprocess
import sys
import weakref
from datetime import timedelta

import pytest

from conftest import Server, assert_survives_pickle
from hs_m3u8 import (
    AudioChoice,
    Engine,
    HookError,
    HookKind,
    Hooks,
    HttpError,
    HttpErrorKind,
    HttpOptions,
    IncompleteError,
    InvalidInputError,
    JobRequest,
    LiveOptions,
    Malformed,
    OutputOptions,
    PlaylistError,
    Preference,
    Probe,
    Proxy,
    Purpose,
    RequestParts,
    RetryPolicy,
    SegmentRef,
    SelectError,
    SelectFailure,
    Source,
    SourceUnsupported,
    Target,
    Timeouts,
    VariantChoice,
)

MASTER = """#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="English",LANGUAGE="en",DEFAULT=YES,URI="audio.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="日本語",LANGUAGE="ja",URI="audio_ja.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2",AUDIO="aud"
low.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,AUDIO="aud"
high.m3u8
"""

MEDIA = """#EXTM3U
#EXT-X-TARGETDURATION:2
#EXTINF:1,
seg0.ts
#EXTINF:1.5,
seg1.ts
#EXT-X-ENDLIST
"""

LIVE = """#EXTM3U
#EXT-X-TARGETDURATION:2
#EXTINF:2,
seg0.ts
"""

# 一次就失败，不等重试
NO_RETRY = HttpOptions(retry=RetryPolicy(attempts=1))


def probe(source: Source) -> Probe:
    """在新的事件循环里探测 `source`。"""
    return asyncio.run(Engine().probe(source))


def put_master(server: Server) -> None:
    server.put("master.m3u8", MASTER)
    for name in ["low.m3u8", "high.m3u8", "audio.m3u8", "audio_ja.m3u8"]:
        server.put(name, MEDIA)


def test_master_playlists_list_their_tracks_and_the_selection(server: Server) -> None:
    put_master(server)

    result = probe(Source(server.url("master.m3u8")))

    assert result.master is not None
    low, high = result.master.variants
    assert (low.index, low.bandwidth, low.codecs, low.audio_group) == (0, 800000, ("avc1.4d401e", "mp4a.40.2"), "aud")
    assert low.resolution is not None
    assert (low.resolution.width, low.resolution.height) == (640, 360)
    assert [(a.index, a.language, a.default, a.separate) for a in result.master.audio] == [
        (0, "en", True, True),
        (1, "ja", False, True),
    ]
    assert result.master.selected.variant == high
    assert result.master.selected.audio == result.master.audio[0]
    assert len(result.tracks) == 2
    assert result.tracks[0].segments == 2
    assert result.tracks[0].duration == timedelta(seconds=2.5)
    assert not result.tracks[0].encrypted
    assert result.tracks[0].ended
    assert not result.live
    assert_survives_pickle(result)


def test_preferences_choose_the_variant_and_audio(server: Server) -> None:
    put_master(server)
    preference = Preference(variant=VariantChoice.Index(0), audio=AudioChoice.Language("JA"))

    result = probe(Source(server.url("master.m3u8"), preference=preference))

    assert result.master is not None
    assert result.master.selected.variant.index == 0
    assert result.master.selected.audio is not None
    assert result.master.selected.audio.language == "ja"


def test_media_playlists_have_no_master_and_live_ones_are_reported(server: Server) -> None:
    server.put("media.m3u8", MEDIA)
    server.put("live.m3u8", LIVE)

    media = probe(Source(server.url("media.m3u8")))
    live = probe(Source(server.url("live.m3u8")))

    assert media.master is None
    assert len(media.tracks) == 1
    assert not media.live
    assert live.live
    assert not live.tracks[0].ended


def test_hooks_rewrite_playlists_and_requests(server: Server) -> None:
    server.put("media.m3u8", "不是播放列表")

    class Rewrite(Hooks):
        def on_playlist(self, url: str, body: bytes) -> str:
            assert url.endswith("/media.m3u8")
            assert body == "不是播放列表".encode()
            return MEDIA

        def on_request(self, purpose: Purpose, request: RequestParts) -> RequestParts:
            assert purpose == Purpose.Playlist()
            return RequestParts(request.url, headers=[*request.headers, ("X-Sign", "signed")])

    result = probe(Source(server.url("media.m3u8"), hooks=Rewrite()))

    assert result.tracks[0].segments == 2
    # 请求头名不区分大小写，发出时为小写
    assert ("x-sign", "signed") in server.headers("media.m3u8")


def test_hooks_may_take_arguments_and_rename_parameters(server: Server) -> None:
    server.put("media.m3u8", MEDIA)

    class Signer(Hooks):
        def __init__(self, token: str) -> None:
            self.token = token

        # 参数名与基类的不同：回调按位置调用
        def on_request(self, kind: Purpose, parts: RequestParts) -> RequestParts:
            del kind
            return RequestParts(parts.url, headers=(*parts.headers, ("X-Token", self.token)))

    result = probe(Source(server.url("media.m3u8"), hooks=Signer("t")))

    assert result.tracks[0].segments == 2
    assert ("x-token", "t") in server.headers("media.m3u8")


def test_hooks_referring_back_to_their_request_are_collected() -> None:
    # 回调 → 请求 → 来源 → 回调：请求与来源都要向垃圾回收器报出它们引用的对象
    class Holder(Hooks):
        request: JobRequest | None = None

    hooks = Holder()
    hooks.request = JobRequest(Source("https://example.com/a.m3u8", hooks=hooks), OutputOptions(Target.Mp4("a.mp4")))
    collected = weakref.ref(hooks)
    del hooks
    gc.collect()

    assert collected() is None


def test_hook_failures_carry_the_original_exception(server: Server) -> None:
    server.put("media.m3u8", MEDIA)

    class Broken(Hooks):
        def on_playlist(self, url: str, body: bytes) -> str:
            del url, body
            message = "站点改版了"
            raise ValueError(message)

    with pytest.raises(HookError) as raised:
        probe(Source(server.url("media.m3u8"), hooks=Broken()))

    assert raised.value.hook == HookKind.Playlist()
    assert isinstance(raised.value.__cause__, ValueError)
    assert "站点改版了" in str(raised.value)
    assert not raised.value.retryable


def test_invalid_hook_results_are_reported_without_a_fabricated_cause(server: Server) -> None:
    server.put("media.m3u8", MEDIA)

    class ToFtp(Hooks):
        def on_request(self, purpose: Purpose, request: RequestParts) -> RequestParts:
            del purpose
            return RequestParts("ftp://example.com/media.m3u8", headers=request.headers)

    with pytest.raises(HookError) as unsupported:
        probe(Source(server.url("media.m3u8"), hooks=ToFtp()))

    assert unsupported.value.hook == HookKind.Request()
    assert unsupported.value.__cause__ is None
    assert "http/https" in str(unsupported.value)
    assert server.hits("media.m3u8") == 0


def test_http_failures_report_the_status_and_whether_to_retry(server: Server) -> None:
    server.status("gone.m3u8", 404)
    server.status("busy.m3u8", 503)

    with pytest.raises(HttpError) as gone:
        probe(Source(server.url("gone.m3u8"), http=NO_RETRY))
    with pytest.raises(HttpError) as busy:
        probe(Source(server.url("busy.m3u8"), http=NO_RETRY))

    assert gone.value.kind == HttpErrorKind.Status(404)
    assert gone.value.url == server.url("gone.m3u8")
    assert not gone.value.retryable
    assert gone.value.segment is None
    assert gone.value.leftovers == ()
    assert busy.value.kind == HttpErrorKind.Status(503)
    assert busy.value.retryable
    assert busy.value.retry_after is None


def test_waits_the_server_asks_for_are_reported(server: Server) -> None:
    # 要求的等待超过退避上限（默认 15 秒）：不再重试，直接失败，等待写在异常里
    server.status("limited.m3u8", 429, headers=[("Retry-After", "100")])

    with pytest.raises(HttpError) as limited:
        probe(Source(server.url("limited.m3u8")))

    assert limited.value.kind == HttpErrorKind.Status(429)
    assert limited.value.retryable
    assert limited.value.retry_after == 100
    assert server.hits("limited.m3u8") == 1


def test_self_signed_certificates_are_accepted_only_when_insecure(tls_server: Server) -> None:
    tls_server.put("media.m3u8", MEDIA)

    with pytest.raises(HttpError) as rejected:
        probe(Source(tls_server.url("media.m3u8"), http=NO_RETRY))
    accepted = probe(Source(tls_server.url("media.m3u8"), http=HttpOptions(insecure=True)))

    assert isinstance(rejected.value.kind, HttpErrorKind.Certificate)
    assert not rejected.value.retryable
    assert accepted.tracks[0].segments == 2


def test_malformed_playlists_and_impossible_selections_are_reported(server: Server) -> None:
    server.put("text.m3u8", "hello")
    # 截在多字节字符中间：服务器还没写完，可重试
    server.put("cut.m3u8", (MEDIA + "#EXTINF:1,\n第2段.ts\n").encode()[:-6])
    put_master(server)

    with pytest.raises(PlaylistError) as malformed:
        probe(Source(server.url("text.m3u8")))
    with pytest.raises(IncompleteError) as truncated:
        probe(Source(server.url("cut.m3u8")))
    with pytest.raises(SelectError) as unselectable:
        probe(Source(server.url("master.m3u8"), preference=Preference(variant=VariantChoice.Index(5))))

    assert malformed.value.reason == Malformed.NotAPlaylist()
    assert truncated.value.retryable
    assert truncated.value.url == server.url("cut.m3u8")
    assert unselectable.value.reason == SelectFailure.VariantIndexOutOfRange(index=5, count=2)


def test_invalid_arguments_are_rejected_when_constructed() -> None:
    # 为 0、为负、超出范围的计数，为负的时长，解析不了的地址：参数错误，不是 OverflowError、ValueError
    with pytest.raises(InvalidInputError):
        Engine(max_requests=0)
    with pytest.raises(InvalidInputError):
        RetryPolicy(attempts=-1)
    with pytest.raises(InvalidInputError):
        RetryPolicy(attempts=2**40)
    with pytest.raises(InvalidInputError):
        VariantChoice.Index(-1)
    with pytest.raises(InvalidInputError):
        LiveOptions(stall_timeout=timedelta(seconds=-1))
    with pytest.raises(InvalidInputError):
        Source("不是地址")
    # 按 core 的规则检查的，也在构造时报
    with pytest.raises(InvalidInputError):
        Source("ftp://example.com/a.m3u8")
    with pytest.raises(InvalidInputError):
        Preference(audio=AudioChoice.Language("en\n"))
    with pytest.raises(InvalidInputError):
        Timeouts(connect=timedelta(0))
    with pytest.raises(InvalidInputError):
        Proxy.Url("sock5://127.0.0.1:9")
    with pytest.raises(InvalidInputError):
        LiveOptions(max_duration=timedelta(0))
    # 二进制缓冲区也是整数的序列，类型检查器放行，却不是要的序列
    for buffer in (b"\x01", bytearray(b"\x01"), memoryview(b"\x01")):
        with pytest.raises(TypeError):
            SourceUnsupported.DiscontinuityMismatch(track=1, first=buffer, found=())


# 在子进程里跑：系统代理的环境变量只影响这个进程。直连照常探测；按系统设置时经代理，请求失败时退出码为 3，代理设置
# 用不了时为 4
SYSTEM_PROXY_SCRIPT = """
import asyncio, sys
from hs_m3u8 import Engine, HttpError, HttpOptions, InvalidInputError, Proxy, RetryPolicy, Source

async def main(url: str, proxy: Proxy) -> None:
    await Engine().probe(Source(url, http=HttpOptions(proxy=proxy, retry=RetryPolicy(attempts=1))))

asyncio.run(main(sys.argv[1], Proxy.Direct()))
try:
    asyncio.run(main(sys.argv[1], Proxy.System()))
except HttpError:
    sys.exit(3)
except InvalidInputError:
    sys.exit(4)
"""


# 子进程跑完的上限，秒；只在出错时才会等满
CHILD_LIMIT = 60.0


def run_with_system_proxy(server: Server, proxy: str) -> int:
    """在子进程里先直连、再按系统设置（环境变量为 `proxy`）探测，返回退出码。"""
    env = {**os.environ, "HTTP_PROXY": proxy, "http_proxy": proxy, "NO_PROXY": "", "no_proxy": ""}
    script = [sys.executable, "-c", SYSTEM_PROXY_SCRIPT, server.url("a.m3u8")]
    return subprocess.run(script, env=env, check=False, timeout=CHILD_LIMIT).returncode


def test_direct_connections_ignore_the_system_proxy(server: Server) -> None:
    # 系统代理指向测试服务器自己：经代理的请求以完整地址作请求目标到达，查不到内容，回 404
    server.put("a.m3u8", MEDIA)

    code = run_with_system_proxy(server, server.url(""))

    assert code == 3
    assert server.hits("a.m3u8") == 1
    assert server.hits(server.url("a.m3u8")) == 1


def test_unusable_system_proxies_fail_instead_of_connecting_directly(server: Server) -> None:
    server.put("a.m3u8", MEDIA)

    code = run_with_system_proxy(server, "sock5://127.0.0.1:9")

    assert code == 4
    assert server.hits("a.m3u8") == 1


def test_reprs_hide_header_values_and_credentials() -> None:
    options = HttpOptions(
        headers={"Cookie": "session=secret"}, proxy=Proxy.Url("http://user:password@proxy.example:8080")
    )
    request = RequestParts("https://example.com/a.ts?token=secret", headers=[("Cookie", "secret")])
    segment = SegmentRef(track=0, sequence=1, url="https://user:password@example.com/a.ts?token=secret")
    empty = SourceUnsupported.EmptyPlaylist(url="https://example.com/a.m3u8?token=secret")

    for value in [options, Source("https://user:password@example.com/a.m3u8?token=secret"), request, segment, empty]:
        text = repr(value)
        assert "secret" not in text
        assert "password" not in text
    assert "Cookie" in repr(options)
    assert "proxy.example:8080" in repr(options)
