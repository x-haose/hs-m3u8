//! 直播录制：播放列表按请求次数逐步变化，模拟刷新、窗口滑动、结束、中断与服务器前后矛盾。
//! 样本 ts_long 为 4 个时间戳连续的 1 秒分片；TARGETDURATION 设为 0.1 秒，刷新间隔随之很短。

mod deciding;
mod recording;
mod resume;

use std::path::Path;
use std::time::Duration;

use hs_m3u8_core::{
    Error, JobRequest, LiveEnd, LiveOptions, LiveReport, MissReason, Missed, Progress, Resume, Url,
};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use crate::server::Server;
use crate::{engine, expected, fixture, request, track};

/// 序号即 ts_long 分片编号的直播播放列表；`indices` 须连续。
fn playlist(indices: &[u64], end: bool) -> String {
    media_playlist("0.1", "", indices, end)
}

/// 同 [`playlist`]，分片路径为 `<dir>seg<i>.ts`，与 [`put_long`] 的 `dir` 对应。
fn playlist_in(dir: &str, indices: &[u64], end: bool) -> String {
    media_playlist("0.1", dir, indices, end)
}

/// 同 [`playlist`]，TARGETDURATION 为 `target` 秒。
fn playlist_with_target(target: &str, indices: &[u64], end: bool) -> String {
    media_playlist(target, "", indices, end)
}

fn media_playlist(target: &str, dir: &str, indices: &[u64], end: bool) -> String {
    let mut text = format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:{}\n",
        indices.first().copied().unwrap_or(0)
    );
    for i in indices {
        text += &format!("#EXTINF:1,\n{dir}seg{i}.ts\n");
    }
    if end {
        text += "#EXT-X-ENDLIST\n";
    }
    text
}

/// 在 `dir` 下放 ts_long 的分片 `indices`（路径 `<dir>seg<i>.ts`）。
fn put_long(server: &Server, dir: &str, indices: &[u64]) {
    for i in indices {
        server.put(
            &format!("{dir}seg{i}.ts"),
            fixture(&format!("ts_long/seg{i}.ts")),
        );
    }
}

fn live_request(url: Url, dir: &Path, stall_timeout: Duration) -> JobRequest {
    let mut req = request(url, dir);
    req.live = Some(LiveOptions {
        max_duration: None,
        stall_timeout,
        resume: Resume::Continue,
    });
    req
}

const STALL: Duration = Duration::from_secs(2);

fn report(end: LiveEnd, session_count: usize, missed: Vec<Missed>) -> Option<LiveReport> {
    Some(LiveReport {
        end,
        session_count,
        missed,
    })
}

/// 第 0 个会话第 0 条轨的缺失区间。
fn missed(first: u64, last: u64, reason: MissReason) -> Missed {
    Missed {
        session: 0,
        track: 0,
        first,
        last,
        reason,
    }
}

/// 运行到 `until` 成立后取消；任务以取消结束，不生成输出，任务目录保留。
async fn interrupt(req: &JobRequest, until: impl FnMut(&Progress) -> bool) {
    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.progress();
    progress.wait_for(until).await.unwrap();
    job.cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    assert!(!req.output.exists());
}

/// 视频加独立音频 rendition 的直播源：放 fmp4_a 的 init 段与分片，两条媒体播放列表按 `video` / `audio`
/// 依次返回（每项为 (分片数, init 签名, 是否结束)）。返回两条轨可用的分片文件名。
fn split_source(
    server: &Server,
    video: &[(usize, u32, bool)],
    audio: &[(usize, u32, bool)],
) -> (Vec<&'static str>, Vec<&'static str>) {
    let video_segments = vec!["seg0.m4s", "seg1.m4s"];
    let audio_segments = vec!["seg0.m4s", "seg1.m4s", "seg2.m4s"];
    for (kind, names) in [("video", &video_segments), ("audio", &audio_segments)] {
        for name in ["init.mp4"].iter().chain(names.iter()) {
            server.put(
                &format!("{kind}/{name}"),
                fixture(&format!("fmp4_a/{kind}/{name}")),
            );
        }
    }
    let media = |kind: &str, &(count, sig, end): &(usize, u32, bool)| {
        let mut text = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:0.1\n#EXT-X-MAP:URI=\"{kind}/init.mp4?sig={sig}\"\n"
        );
        for i in 0..count {
            text += &format!("#EXTINF:1,\n{kind}/seg{i}.m4s\n");
        }
        if end {
            text += "#EXT-X-ENDLIST\n";
        }
        text
    };
    server.put_sequence(
        "video.m3u8",
        video.iter().map(|v| media("video", v)).collect(),
    );
    server.put_sequence(
        "audio.m3u8",
        audio.iter().map(|a| media("audio", a)).collect(),
    );
    put_split_master(server);
    (video_segments, audio_segments)
}

/// 主播放列表：一个视频变体 video.m3u8，音频 rendition audio.m3u8。
fn put_split_master(server: &Server) {
    server.put(
        "master.m3u8",
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=320x180,AUDIO=\"aud\"\nvideo.m3u8\n",
    );
}

/// 视频与音频都取自 ts_long 的期望输出：`groups` 的每一项为一组的（视频, 音频）分片编号。
fn expected_split_long(dir: &Path, groups: &[(&[u64], &[u64])]) -> Vec<u8> {
    let names =
        |indices: &[u64]| -> Vec<String> { indices.iter().map(|i| format!("seg{i}.ts")).collect() };
    let groups: Vec<DiscontinuityGroup> = groups
        .iter()
        .map(|(video, audio)| {
            let tracks = [names(video), names(audio)].map(|names| {
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                track("ts_long", None, &names)
            });
            DiscontinuityGroup {
                tracks: tracks.into(),
            }
        })
        .collect();
    expected(dir, &[Streams::Video, Streams::Audio], &groups)
}

/// fmp4_a 视频与音频各取若干个分片、合成一组的期望输出。
fn expected_split(dir: &Path, video: &[&str], audio: &[&str]) -> Vec<u8> {
    let tracks = vec![
        track("fmp4_a/video", Some("init.mp4"), video),
        track("fmp4_a/audio", Some("init.mp4"), audio),
    ];
    expected(
        dir,
        &[Streams::Video, Streams::Audio],
        &[DiscontinuityGroup { tracks }],
    )
}
