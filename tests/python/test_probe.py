"""探测：选轨结果与各轨概况、回调、失败时抛出的异常。"""

import asyncio
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
    InvalidInputError,
    LiveOptions,
    Malformed,
    PlaylistError,
    Preference,
    Probe,
    Purpose,
    RequestParts,
    RetryPolicy,
    SegmentRef,
    SelectError,
    SelectFailure,
    Source,
    SourceUnsupported,
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
        def on_playlist(self, url: str, body: bytes) -> bytes:
            assert url.endswith("/media.m3u8")
            assert body == "不是播放列表".encode()
            return MEDIA.encode()

        def on_request(self, purpose: Purpose, request: RequestParts) -> RequestParts:
            assert purpose == Purpose.Playlist()
            return RequestParts(request.url, headers=[*request.headers, ("X-Sign", "signed")])

    result = probe(Source(server.url("media.m3u8"), hooks=Rewrite()))

    assert result.tracks[0].segments == 2
    # 请求头名不区分大小写，发出时为小写
    assert ("x-sign", "signed") in server.headers("media.m3u8")


def test_hooks_may_take_arguments_rename_parameters_and_be_set_on_the_instance(server: Server) -> None:
    server.put("media.m3u8", "不是播放列表")

    class Signer(Hooks):
        def __init__(self, token: str) -> None:
            self.token = token

        # 参数名与基类的不同：回调按位置调用
        def on_request(self, kind: Purpose, parts: RequestParts) -> RequestParts:
            del kind
            return RequestParts(parts.url, headers=(*parts.headers, ("X-Token", self.token)))

    def rewrite(url: str, body: bytes) -> bytes:
        del url, body
        return MEDIA.encode()

    signer = Signer("t")
    vars(signer)["on_playlist"] = rewrite
    result = probe(Source(server.url("media.m3u8"), hooks=signer))

    assert result.tracks[0].segments == 2
    assert ("x-token", "t") in server.headers("media.m3u8")


def test_hook_failures_carry_the_original_exception(server: Server) -> None:
    server.put("media.m3u8", MEDIA)

    class Broken(Hooks):
        def on_playlist(self, url: str, body: bytes) -> bytes:
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

    class Unparsable(Hooks):
        def on_request(self, purpose: Purpose, request: RequestParts) -> RequestParts:
            del purpose
            return RequestParts("不是地址", headers=request.headers)

    with pytest.raises(HookError) as unparsable:
        probe(Source(server.url("media.m3u8"), hooks=Unparsable()))

    assert unparsable.value.hook == HookKind.Request()
    assert unparsable.value.__cause__ is None
    assert "无法解析" in str(unparsable.value)
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


def test_malformed_playlists_and_impossible_selections_are_reported(server: Server) -> None:
    server.put("text.m3u8", "hello")
    put_master(server)

    with pytest.raises(PlaylistError) as malformed:
        probe(Source(server.url("text.m3u8")))
    with pytest.raises(SelectError) as unselectable:
        probe(Source(server.url("master.m3u8"), preference=Preference(variant=VariantChoice.Index(5))))

    assert malformed.value.reason == Malformed.NotAPlaylist()
    assert unselectable.value.reason == SelectFailure.VariantIndexOutOfRange(index=5, count=2)


def test_invalid_arguments_are_reported_before_any_request(server: Server) -> None:
    # 为 0、为负、超出范围的计数与为负的时长都是参数错误，不是 OverflowError、ValueError
    with pytest.raises(InvalidInputError):
        Engine(max_requests=0)
    with pytest.raises(InvalidInputError):
        Engine(max_requests=-1)
    with pytest.raises(InvalidInputError):
        RetryPolicy(attempts=0)
    with pytest.raises(InvalidInputError):
        RetryPolicy(attempts=2**40)
    with pytest.raises(InvalidInputError):
        VariantChoice.Index(-1)
    with pytest.raises(InvalidInputError):
        Timeouts(connect=timedelta(seconds=-1))
    with pytest.raises(InvalidInputError):
        LiveOptions(stall_timeout=timedelta(seconds=-1))
    url = server.url("a.m3u8")
    unusable = [
        Source("ftp://example.com/a.m3u8"),
        Source("不是地址"),
        Source(url, http=HttpOptions(proxy="sock5://127.0.0.1:9")),
        Source(url, http=HttpOptions(proxy="不是地址")),
    ]
    for source in unusable:
        with pytest.raises(InvalidInputError):
            probe(source)
    assert server.hits("a.m3u8") == 0


def test_reprs_hide_header_values_and_credentials() -> None:
    options = HttpOptions(headers={"Cookie": "session=secret"}, proxy="http://user:password@proxy.example:8080")
    request = RequestParts("https://example.com/a.ts?token=secret", headers=[("Cookie", "secret")])
    segment = SegmentRef(track=0, sequence=1, url="https://user:password@example.com/a.ts?token=secret")
    empty = SourceUnsupported.EmptyPlaylist(url="https://example.com/a.m3u8?token=secret")

    for value in [options, Source("https://user:password@example.com/a.m3u8?token=secret"), request, segment, empty]:
        text = repr(value)
        assert "secret" not in text
        assert "password" not in text
    assert "Cookie" in repr(options)
    assert "proxy.example:8080" in repr(options)
