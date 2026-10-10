//! 输出：本地 HLS 目录、MP4 与 HLS 两者都要、替换已有的输出、写 HLS 失败时的清理、路径校验。
//!
//! 本库的 FFmpeg 没有 HLS 读取器，HLS 输出的验证方式：用本库的解析器读回写出的播放列表，按列出的文件
//! 直接合并，结果须与同一任务输出的 MP4 逐字节相同。

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use hs_m3u8_core::hls::{MediaPlaylist, Playlist, parse};
use hs_m3u8_core::{Error, JobRequest, LiveOptions, Output, Target, Url};
use hs_m3u8_remux::{DiscontinuityGroup, Streams, TrackSegments, remux};

use crate::server::Server;
use crate::{assert_output, expected_long, fixture, fixtures, request, run, test_dir};

/// 视频与独立音频分离、两个不连续段组的点播（fmp4_a 后接 fmp4_b），返回主播放列表的地址。
fn put_split_vod(server: &Server) -> Url {
    for program in ["fmp4_a", "fmp4_b"] {
        for kind in ["video", "audio"] {
            for name in std::fs::read_dir(fixtures().join(program).join(kind)).unwrap() {
                let name = name.unwrap().file_name().into_string().unwrap();
                let path = format!("{program}/{kind}/{name}");
                server.put(&path, fixture(&path));
            }
        }
    }
    let media = |kind: &str, a: usize, b: usize| {
        let mut text = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"fmp4_a/{kind}/init.mp4\"\n"
        );
        for i in 0..a {
            text += &format!("#EXTINF:1,\nfmp4_a/{kind}/seg{i}.m4s\n");
        }
        text += &format!("#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"fmp4_b/{kind}/init.mp4\"\n");
        for i in 0..b {
            text += &format!("#EXTINF:1,\nfmp4_b/{kind}/seg{i}.m4s\n");
        }
        text + "#EXT-X-ENDLIST\n"
    };
    server.put("video.m3u8", media("video", 2, 1));
    server.put("audio.m3u8", media("audio", 3, 2));
    server.put(
        "master.m3u8",
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"main\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=200000,RESOLUTION=320x180,CODECS=\"avc1.64000d,mp4a.40.2\",AUDIO=\"aud\"\n\
         video.m3u8\n",
    );
    server.url("master.m3u8")
}

fn read_playlist(path: &Path) -> Playlist {
    let text = std::fs::read_to_string(path).unwrap();
    parse(&text, &Url::from_file_path(path).unwrap()).unwrap()
}

/// 媒体播放列表按不连续段分组，换成本地文件。
fn media_groups(media: &MediaPlaylist) -> Vec<TrackSegments> {
    let mut groups: Vec<TrackSegments> = Vec::new();
    let mut current = None;
    for segment in &media.segments {
        if current != Some(segment.discontinuity) {
            current = Some(segment.discontinuity);
            groups.push(TrackSegments {
                init: segment.init.as_ref().map(|i| i.uri.to_file_path().unwrap()),
                segments: Vec::new(),
            });
        }
        let path = segment.uri.to_file_path().unwrap();
        groups.last_mut().unwrap().segments.push(path);
    }
    groups
}

/// 读回 HLS 目录：各组各轨列出的文件，以及各轨的取流方式。
fn read_hls(dir: &Path) -> (Vec<Streams>, Vec<DiscontinuityGroup>) {
    let tracks: Vec<MediaPlaylist> = match read_playlist(&dir.join("index.m3u8")) {
        Playlist::Media(media) => vec![media],
        Playlist::Master(master) => {
            let audio = master.renditions[0].uri.as_ref().unwrap();
            [&master.variants[0].uri, audio]
                .map(|uri| match read_playlist(&uri.to_file_path().unwrap()) {
                    Playlist::Media(media) => media,
                    Playlist::Master(_) => panic!("应为媒体播放列表"),
                })
                .into()
        }
    };
    for media in &tracks {
        assert!(media.ended, "本地 HLS 应是点播");
    }
    let streams = match tracks.len() {
        1 => vec![Streams::All],
        _ => vec![Streams::Video, Streams::Audio],
    };
    let per_track: Vec<Vec<TrackSegments>> = tracks.iter().map(media_groups).collect();
    let groups = (0..per_track[0].len())
        .map(|g| DiscontinuityGroup {
            tracks: per_track.iter().map(|t| t[g].clone()).collect(),
        })
        .collect();
    (streams, groups)
}

/// 按 HLS 目录列出的文件合并，须与 `mp4` 逐字节相同。
fn assert_hls_matches(hls: &Path, mp4: &Path) {
    let (streams, groups) = read_hls(hls);
    let merged = hls.with_extension("check.mp4");
    remux(&streams, &groups, &merged).unwrap();
    assert_eq!(std::fs::read(merged).unwrap(), std::fs::read(mp4).unwrap());
}

fn both(dir: &Path) -> Target {
    Target::Both {
        mp4: dir.join("out.mp4"),
        hls: dir.join("out"),
    }
}

/// 输出目标为 `target`，任务目录取默认值。
fn request_to(url: Url, dir: &Path, target: Target) -> JobRequest {
    let mut req = request(url, dir);
    req.output.target = target;
    req
}

/// 两者都要：音视频分离、两个不连续段组。HLS 的主播放列表照抄来源的变体属性，各轨分片按顺序编号、
/// 组间加不连续标记；按它列出的文件合并与 MP4 相同。任务目录已删除。
#[tokio::test(flavor = "multi_thread")]
async fn split_source_writes_both_outputs() {
    let dir = test_dir("output_both");
    let server = Server::start().await;
    let url = put_split_vod(&server);

    let output = run(request_to(url, &dir, both(&dir))).await.unwrap();

    let hls = dir.join("out");
    assert_eq!(output.hls.as_deref(), Some(hls.as_path()));
    let mp4 = output.mp4.as_ref().unwrap();
    assert_eq!(mp4.report.streams.len(), 2);
    assert_hls_matches(&hls, &mp4.path);
    let Playlist::Master(master) = read_playlist(&hls.join("index.m3u8")) else {
        panic!("音视频分离时入口应为主播放列表");
    };
    let variant = &master.variants[0];
    assert_eq!(
        (variant.bandwidth, variant.codecs.join(",")),
        (Some(200_000), "avc1.64000d,mp4a.40.2".to_owned())
    );
    assert_eq!(master.renditions[0].language.as_deref(), Some("en"));
    for name in ["0/0.m4s", "0/2.m4s", "1/4.m4s"] {
        assert!(hls.join(name).is_file(), "{name}");
    }
    assert!(!dir.join("out.hsdl").exists());
    assert!(!dir.join("out.part").exists());
}

/// 只要 HLS：不经 FFmpeg，MP4 合并不支持的编码（这里是 MP3 音频）照样输出；分片原样放进目录。
#[tokio::test(flavor = "multi_thread")]
async fn hls_only_keeps_codecs_the_mp4_cannot_take() {
    let dir = test_dir("output_hls_only");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_mp3/seg0.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:1.5,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let hls = dir.join("out");

    let req = request_to(server.url("index.m3u8"), &dir, Target::Hls(hls.clone()));
    let output = run(req).await.unwrap();

    assert_eq!(output.mp4, None);
    let Playlist::Media(media) = read_playlist(&hls.join("index.m3u8")) else {
        panic!("单轨时入口应为媒体播放列表");
    };
    let segment = &media.segments[0];
    assert_eq!(segment.uri.to_file_path().unwrap(), hls.join("0/0.ts"));
    assert_eq!(segment.duration_us, 1_500_000);
    assert_eq!(
        std::fs::read(hls.join("0/0.ts")).unwrap(),
        fixture("ts_mp3/seg0.ts")
    );
    assert!(!dir.join("out.hsdl").exists());
}

/// 已有的 HLS 目录：为空可以直接写；有内容时不覆盖就拒绝；覆盖时只替换本库写出的，里面有别的文件仍拒绝。
#[tokio::test(flavor = "multi_thread")]
async fn existing_hls_directories_are_replaced_only_when_written_by_the_library() {
    let dir = test_dir("output_overwrite");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let hls = dir.join("out");
    std::fs::create_dir(&hls).unwrap();
    let req = request_to(server.url("index.m3u8"), &dir, Target::Hls(hls.clone()));
    run(req.clone()).await.unwrap();

    let exists = |result: Result<Output, Error>, path: &Path| match result {
        Err(Error::OutputExists(p)) => assert_eq!(p, path),
        other => panic!("应报输出已存在：{other:?}"),
    };
    exists(run(req.clone()).await, &hls);

    let mut replace = req;
    replace.output.overwrite = true;
    run(replace.clone()).await.unwrap();
    assert!(hls.join("0/0.ts").is_file());

    std::fs::write(hls.join("0/notes.txt"), "别人的文件").unwrap();
    exists(run(replace).await, &hls);
    assert!(hls.join("0/notes.txt").is_file());
}

/// 两者都要时写 HLS 失败（`<目录>.part` 被别人占用）：刚写的 MP4 删掉，任务目录保留；腾出来后再运行，
/// 已下载的分片不重下。
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_hls_output_removes_the_new_mp4() {
    let dir = test_dir("output_hls_failed");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let stage = dir.join("out.part");
    std::fs::create_dir(&stage).unwrap();
    std::fs::write(stage.join("notes.txt"), "别人的文件").unwrap();
    let req = request_to(server.url("index.m3u8"), &dir, both(&dir));

    match run(req.clone()).await {
        Err(Error::OutputExists(path)) => assert_eq!(path, stage),
        other => panic!("应报输出已存在：{other:?}"),
    }
    assert!(!dir.join("out.mp4").exists());
    assert!(dir.join("out.hsdl/job.json").exists());

    std::fs::remove_dir_all(&stage).unwrap();
    let output = run(req).await.unwrap();
    assert_hls_matches(&dir.join("out"), &output.mp4.unwrap().path);
    assert_eq!(server.hits("seg0.ts"), 1);
}

/// 直播：中途一个分片取不到（缺失），之后续录另起了会话。HLS 里缺失处不加标记、保留时间戳，会话之间加
/// 不连续标记；按它列出的文件合并与 MP4 相同。
#[tokio::test(flavor = "multi_thread")]
async fn live_holes_and_sessions_in_hls() {
    let dir = test_dir("output_live");
    let server = Server::start().await;
    for i in [0, 2, 3] {
        server.put(
            &format!("seg{i}.ts"),
            fixture(&format!("ts_long/seg{i}.ts")),
        );
    }
    server.status("seg1.ts", StatusCode::NOT_FOUND);
    let window = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n\
                  #EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXTINF:1,\nseg2.ts\n";
    server.put_sequence(
        "live.m3u8",
        vec![window.to_owned(), format!("{window}#EXT-X-ENDLIST\n")],
    );
    let mut req = request_to(server.url("live.m3u8"), &dir, both(&dir));
    req.live = Some(LiveOptions::default());
    // 第一次：录到 seg0、seg2（seg1 缺失）；保留目录供续录
    let mut first = req.clone();
    first.output.target = Target::Mp4(dir.join("first.mp4"));
    first.output.work_dir = Some(dir.join("out.hsdl"));
    first.output.keep_work_dir = true;
    run(first).await.unwrap();
    // 第二次：编码器重启后的新节目，序号从 0 重来、内容不同，另起会话
    server.put("seg0.ts", fixture("ts_long/seg3.ts"));
    server.put(
        "live.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );

    let output = run(req).await.unwrap();

    assert_eq!(output.live.as_ref().unwrap().session_count, 2);
    let hls = dir.join("out");
    let Playlist::Media(media) = read_playlist(&hls.join("index.m3u8")) else {
        panic!("单轨时入口应为媒体播放列表");
    };
    let seen: Vec<(PathBuf, u64)> = media
        .segments
        .iter()
        .map(|s| (s.uri.to_file_path().unwrap(), s.discontinuity))
        .collect();
    let want: Vec<(PathBuf, u64)> = [("0/0.ts", 0), ("0/1.ts", 0), ("0/2.ts", 1)]
        .map(|(name, d)| (hls.join(name), d))
        .into();
    assert_eq!(seen, want);
    assert_hls_matches(&hls, &output.mp4.as_ref().unwrap().path);
    assert_output(&output, &expected_long(&dir, &[0, 2, 3], &[2, 1]));
}

/// 合并 MP4 时编码不受支持（MP3 音频）而失败，任务目录保留；改为输出 HLS 后用同一个默认任务目录，
/// 不重新下载。HLS 目录路径带结尾的分隔符也照常。
#[tokio::test(flavor = "multi_thread")]
async fn switching_to_hls_after_an_unsupported_codec_reuses_the_download() {
    let dir = test_dir("output_switch");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_mp3/seg0.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:1.5,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let url = server.url("index.m3u8");
    let mp4 = request_to(url.clone(), &dir, Target::Mp4(dir.join("out.mp4")));
    match run(mp4).await {
        Err(Error::Remux(_)) => {}
        other => panic!("MP3 音频应合并失败：{other:?}"),
    }
    assert!(dir.join("out.hsdl/job.json").exists());

    let mut hls = dir.join("out").into_os_string();
    hls.push(std::path::MAIN_SEPARATOR_STR);
    let output = run(request_to(url, &dir, Target::Hls(hls.into())))
        .await
        .unwrap();

    assert!(dir.join("out/0/0.ts").is_file());
    assert_eq!(output.cleanup_error, None);
    assert!(!dir.join("out.hsdl").exists());
    assert_eq!(server.hits("seg0.ts"), 1);
}

/// 输出经符号链接落在任务目录里（按字面比较看不出）：删除任务目录时只删本库写的文件，输出留下，
/// 没删完的原因记在 cleanup_error 里。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn outputs_inside_the_work_dir_survive_its_removal() {
    let dir = test_dir("output_alias");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let work = dir.join("work");
    std::fs::create_dir(&work).unwrap();
    std::os::unix::fs::symlink(&work, dir.join("link")).unwrap();
    let target = Target::Mp4(dir.join("link/out.mp4"));
    let mut req = request_to(server.url("index.m3u8"), &dir, target);
    req.output.work_dir = Some(work.clone());

    let output = run(req).await.unwrap();

    assert!(work.join("out.mp4").is_file());
    let cleanup = output.cleanup_error.expect("留有输出，任务目录没删完");
    assert!(cleanup.contains("out.mp4"), "{cleanup}");
    for name in ["job.json", "lock", "tracks"] {
        assert!(!work.join(name).exists(), "{name} 应已删除");
    }
}

/// 输出与任务目录的路径相同或互相包含时，下载前即拒绝：任务目录成功后会被整个删除。
#[tokio::test(flavor = "multi_thread")]
async fn overlapping_paths_are_rejected() {
    let dir = test_dir("output_paths");
    let url = Url::parse("http://127.0.0.1:9/index.m3u8").unwrap();
    let cases = [
        (both(&dir), Some(dir.join("out"))),
        (Target::Hls(dir.join("work/hls")), Some(dir.join("work"))),
        // 按字面规整 `..` 之后才看得出在任务目录里
        (
            Target::Mp4(dir.join("x/../work/out.mp4")),
            Some(dir.join("work")),
        ),
        (
            Target::Both {
                mp4: dir.join("out/a.mp4"),
                hls: dir.join("out"),
            },
            None,
        ),
    ];
    for (target, work_dir) in cases {
        let mut req = request_to(url.clone(), &dir, target.clone());
        req.output.work_dir = work_dir;
        match run(req).await {
            Err(Error::InvalidInput(_)) => {}
            other => panic!("{target:?} 应被拒绝：{other:?}"),
        }
    }
}
