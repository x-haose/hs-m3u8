"""多条音轨、字幕与 MKV：探测时字幕能不能下，选几条音轨与字幕合并成 MKV、另存字幕文件，进度里的各轨。"""

import asyncio
from pathlib import Path

import pytest

from conftest import Server, assert_survives_pickle, fixture
from hs_m3u8 import (
    AudioChoice,
    Container,
    Engine,
    FetchFailure,
    InvalidInputError,
    JobRequest,
    MediaReport,
    MergedOutput,
    MergeUnsupported,
    MergeUnsupportedError,
    Mp4Output,
    Mp4Unsupported,
    Mp4UnsupportedError,
    Output,
    OutputOptions,
    Preference,
    Probe,
    Progress,
    RenditionChoice,
    Selected,
    Shape,
    Source,
    SubtitleFileFormat,
    SubtitleFormat,
    SubtitleOutput,
    SubtitleProblem,
    Subtitles,
    SubtitleSupport,
    Target,
    TrackKind,
)


def put_tracks(server: Server) -> None:
    """放好来源：fmp4_a 的视频与两条音频，三条字幕。

    音频 `en` 为默认、`de`；字幕 `zh-Hans` 为 WebVTT 文本，`en` 的分片是 fMP4（默认），`ja` 的媒体播放列表
    取不到（404）。
    """
    for kind, folder, count in (("video", "v", 2), ("audio", "a1", 3), ("audio", "a2", 3)):
        server.put(f"{folder}/init.mp4", fixture(f"fmp4_a/{kind}/init.mp4"))
        playlist = f'#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI="{folder}/init.mp4"\n'
        for i in range(count):
            server.put(f"{folder}/seg{i}.m4s", fixture(f"fmp4_a/{kind}/seg{i}.m4s"))
            playlist += f"#EXTINF:1,\n{folder}/seg{i}.m4s\n"
        server.put(f"{folder}.m3u8", playlist + "#EXT-X-ENDLIST\n")
    header = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n"
    server.put("zh/0.vtt", f"{header}00:00.200 --> 00:01.400\n你好\n")
    server.put("zh/1.vtt", f"{header}00:01.500 --> 00:01.900\n再见\n")
    server.put(
        "zh.m3u8", "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nzh/0.vtt\n#EXTINF:1,\nzh/1.vtt\n#EXT-X-ENDLIST\n"
    )
    server.put(
        "en.m3u8",
        '#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI="en/init.mp4"\n#EXTINF:1,\nen/0.m4s\n#EXT-X-ENDLIST\n',
    )
    server.status("ja.m3u8", 404)
    server.put(
        "master.m3u8",
        "#EXTM3U\n"
        '#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="English",LANGUAGE="en",DEFAULT=YES,CHANNELS="2",URI="a1.m3u8"\n'
        '#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="Deutsch",LANGUAGE="de",URI="a2.m3u8"\n'
        '#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="简体",LANGUAGE="zh-Hans",URI="zh.m3u8"\n'
        '#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",DEFAULT=YES,URI="en.m3u8"\n'
        '#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="日本語",LANGUAGE="ja",URI="ja.m3u8"\n'
        '#EXT-X-STREAM-INF:BANDWIDTH=200000,RESOLUTION=320x180,CODECS="avc1.64000c,mp4a.40.2,wvtt",'
        'AUDIO="aud",SUBTITLES="subs"\nv.m3u8\n',
    )


def probe(source: Source) -> Probe:
    async def main() -> Probe:
        return await Engine().probe(source)

    return asyncio.run(main())


def run(request: JobRequest) -> tuple[Output, Progress]:
    async def main() -> tuple[Output, Progress]:
        job = Engine().start(request)
        output = await job.wait()
        return output, job.progress()

    return asyncio.run(main())


def test_probe_reports_which_subtitles_can_be_downloaded(server: Server) -> None:
    """字幕组里每一条都读：fMP4 的标为格式不支持，取不到的给出原因，都不让探测失败；默认选到的 fMP4 字幕列为跳过。"""
    put_tracks(server)
    preference = Preference(subtitles=Subtitles.Default())

    result = probe(Source(server.url("master.m3u8"), preference=preference))

    assert result.master is not None
    support = [subtitle.support for subtitle in result.master.subtitles]
    assert support[0] == SubtitleSupport.Supported()
    assert support[1] == SubtitleSupport.Unsupported(SubtitleFormat.Fmp4(("wvtt",)))
    match support[2]:
        case SubtitleSupport.Unreadable(problem=SubtitleProblem.Fetch(failure=FetchFailure.Http())):
            pass
        case other:
            pytest.fail(f"取不到的字幕应为 Unreadable(Fetch)：{other!r}")
    selected = result.master.selected
    assert selected.subtitles == ()
    assert [s.language for s in selected.skipped_subtitles] == ["en"]
    assert [a.channels for a in result.master.audio] == ["2", None]
    assert result.master.variants[0].subtitle_group == "subs"
    assert_survives_pickle(result)


def test_several_audio_tracks_and_subtitles_go_into_matroska_and_srt(server: Server, tmp_path: Path) -> None:
    """全部音频、点名的一条字幕合并成 MKV，字幕封进去并另存为 SRT；进度里各轨的种类与分片数。"""
    put_tracks(server)
    zh = probe(Source(server.url("master.m3u8"))).master
    assert zh is not None
    preference = Preference(
        audio=AudioChoice.All(),
        subtitles=Subtitles.These((zh.subtitles[0].choice(),)),
    )
    target = tmp_path / "out.mkv"
    request = JobRequest(
        Source(server.url("master.m3u8"), preference=preference),
        OutputOptions(
            Target.File(path=target, container=Container.Matroska()),
            subtitles=SubtitleOutput(files=SubtitleFileFormat.Srt()),
        ),
    )

    output, progress = run(request)

    assert output.mp4 is None
    assert output.merged is not None
    assert output.merged.container == Container.Matroska()
    assert target.is_file()
    kinds = [type(stream.shape) for stream in output.merged.report.streams]
    assert kinds == [Shape.Video, Shape.Audio, Shape.Audio, Shape.Subtitle]
    assert output.merged.report.streams[3].shape == Shape.Subtitle("subrip")
    assert output.merged.report.unused == ()
    [srt] = output.subtitle_files
    assert srt.path == tmp_path / "out.zh-Hans.srt"
    assert srt.cues == 2
    assert "你好" in srt.path.read_text(encoding="utf-8")
    assert [track.kind for track in progress.tracks] == [
        TrackKind.Video(),
        TrackKind.Audio(),
        TrackKind.Audio(),
        TrackKind.Subtitle(),
    ]
    assert [track.segments.done for track in progress.tracks] == [2, 3, 3, 2]
    assert progress.segments.done == 8
    assert progress.selected is not None
    assert [a.language for a in progress.selected.audios] == ["en", "de"]
    assert progress.selected.audio == progress.selected.audios[0]
    assert_survives_pickle(output)
    assert_survives_pickle(progress)


def test_a_single_choice_is_the_same_preference_either_way(server: Server, tmp_path: Path) -> None:
    """`AudioChoice.Language(x)` 与只有同一项的 `AudioChoice.These` 是同一偏好。

    两者选出的相同、任务目录认作同一偏好：中断后换另一种写法照样续传；两者 `==` 不相等。
    """
    put_tracks(server)
    old = Preference(audio=AudioChoice.Language("de"))
    new = Preference(audio=AudioChoice.These((RenditionChoice.Language("de"),)))
    assert old != new
    work_dir = tmp_path / "work"
    options = OutputOptions(Target.Mp4(tmp_path / "out.mp4"), work_dir=work_dir, keep_work_dir=True)

    first, _ = run(JobRequest(Source(server.url("master.m3u8"), preference=old), options))
    second, _ = run(
        JobRequest(
            Source(server.url("master.m3u8"), preference=new),
            OutputOptions(Target.Mp4(tmp_path / "again.mp4"), work_dir=work_dir),
        )
    )

    assert first.mp4 is not None
    assert second.mp4 is not None
    assert first.mp4.report.streams == second.mp4.report.streams
    assert server.hits("a2/seg0.m4s") == 1


def test_empty_choices_are_rejected_when_built() -> None:
    """逐个指定一项也没给：构造 `Preference` 时即报错（一条也不要用 `Off`）。"""
    with pytest.raises(InvalidInputError):
        Preference(audio=AudioChoice.These(()))
    with pytest.raises(InvalidInputError):
        Preference(subtitles=Subtitles.These(()))
    assert Preference() == Preference(audio=AudioChoice.Default(), subtitles=Subtitles.Off())


def test_merge_names_are_the_mp4_names() -> None:
    """`MergeUnsupported`、`MergeUnsupportedError` 与 1.0 的名字是同一个类。"""
    assert MergeUnsupported is Mp4Unsupported
    assert MergeUnsupportedError is Mp4UnsupportedError
    too_long = MergeUnsupported.SubtitleTooLong(group=0, track=3, bytes=70000)
    assert isinstance(too_long, Mp4Unsupported)
    assert_survives_pickle(too_long)


def test_results_built_the_old_way_fill_in_the_new_fields(server: Server, tmp_path: Path) -> None:
    """按 1.0 的写法构造 `Selected`、`Output` 时，新字段由旧字段得出；两边都给了而对不上时报错。"""
    put_tracks(server)
    master = probe(Source(server.url("master.m3u8"))).master
    assert master is not None
    variant, audio = master.variants[0], master.audio[0]
    assert Selected(variant=variant, audio=audio).audios == (audio,)
    assert Selected(variant=variant, audio=None).muxed_audio
    with pytest.raises(InvalidInputError):
        Selected(variant=variant, audio=audio, audios=(master.audio[1],))

    report = MediaReport(streams=())
    mp4 = Mp4Output(path=tmp_path / "out.mp4", report=report)
    old = Output(mp4=mp4, hls=None, segments=0, bytes=0, leftovers=(), missed=(), live=None)
    assert old.merged == MergedOutput(path=tmp_path / "out.mp4", container=Container.Mp4(), report=report)
    mkv = MergedOutput(path=tmp_path / "out.mkv", container=Container.Matroska(), report=report)
    with pytest.raises(InvalidInputError):
        Output(mp4=mp4, hls=None, segments=0, bytes=0, leftovers=(), missed=(), live=None, merged=mkv)
