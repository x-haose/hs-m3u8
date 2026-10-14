//! 本地 HLS：主播放列表与各轨的写法、只要 HLS 时不经 FFmpeg、直播的缺失与会话、本地 HLS 表示不了的内容。

use std::path::PathBuf;

use axum::http::StatusCode;
use hs_m3u8_core::{
    Error, FileCount, HlsUnsupported, LiveOptions, Mp4Unsupported, Target, Unsupported, Url,
    Writing,
};
use hs_m3u8_hls::Playlist;

use super::{assert_hls_matches, both, names_in, read_hls, read_playlist, request_to};
use crate::server::Server;
use crate::{assert_output, engine, expected_long, fixture, fixtures, run, test_dir};

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
    server.put("video.m3u8", split_media("video", 2, 1, "1"));
    server.put("audio.m3u8", split_media("audio", 3, 2, "1"));
    server.put("master.m3u8", SPLIT_MASTER);
    server.url("master.m3u8")
}

const SPLIT_MASTER: &str = "#EXTM3U\n\
     #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"main\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
     #EXT-X-STREAM-INF:BANDWIDTH=200000,RESOLUTION=320x180,CODECS=\"avc1.64000d,mp4a.40.2\",AUDIO=\"aud\"\n\
     video.m3u8\n";

/// 分离点播一条轨的媒体播放列表：fmp4_a 的 `a` 个分片、不连续标记、fmp4_b 的 `b` 个分片，每个声明 `duration` 秒。
fn split_media(kind: &str, a: usize, b: usize, duration: &str) -> String {
    let mut text =
        format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"fmp4_a/{kind}/init.mp4\"\n");
    for i in 0..a {
        text += &format!("#EXTINF:{duration},\nfmp4_a/{kind}/seg{i}.m4s\n");
    }
    text += &format!("#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"fmp4_b/{kind}/init.mp4\"\n");
    for i in 0..b {
        text += &format!("#EXTINF:{duration},\nfmp4_b/{kind}/seg{i}.m4s\n");
    }
    text + "#EXT-X-ENDLIST\n"
}

/// 两者都要：音视频分离、两个不连续段组。HLS 的主播放列表码率按文件算、编码照抄来源，各轨分片按顺序编号、
/// 组间加不连续标记；按它列出的文件合并与 MP4 相同。任务目录与临时名都已删除。写出进度两种输出各给一份。
#[tokio::test(flavor = "multi_thread")]
async fn split_source_writes_both_outputs() {
    let dir = test_dir("output_both");
    let server = Server::start().await;
    let url = put_split_vod(&server);

    let job = engine().start(request_to(url, &dir, both(&dir))).unwrap();
    let progress = job.control().progress();
    let output = job.wait().await.unwrap();

    assert_eq!(names_in(&dir), ["out", "out.mp4"]);
    let hls = dir.join("out");
    assert_eq!(output.hls.as_deref(), Some(hls.as_path()));
    let mp4 = output.mp4.as_ref().unwrap();
    assert_eq!(mp4.report.streams.len(), 2);
    assert_hls_matches(&hls, &mp4.path);
    let Playlist::Master(master) = read_playlist(&hls.join("index.m3u8")) else {
        panic!("音视频分离时入口应为主播放列表");
    };
    // 码率为两条轨分片峰值码率之和（各分片声明 1 秒），编码照抄来源
    let peak = |kind: &str| {
        let sizes = ["fmp4_a", "fmp4_b"].map(|program| {
            std::fs::read_dir(fixtures().join(program).join(kind))
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|e| e == "m4s"))
                .map(|p| std::fs::metadata(p).unwrap().len())
                .max()
                .unwrap()
        });
        sizes.into_iter().max().unwrap() * 8
    };
    let variant = &master.variants[0];
    assert_eq!(
        (variant.bandwidth, variant.codecs.join(",")),
        (
            Some(peak("video") + peak("audio")),
            "avc1.64000d,mp4a.40.2".to_owned()
        )
    );
    assert_eq!(master.renditions[0].language.as_deref(), Some("en"));
    for name in ["0/0.m4s", "0/2.m4s", "1/4.m4s"] {
        assert!(hls.join(name).is_file(), "{name}");
    }
    let (_, groups) = read_hls(&hls);
    let files: usize = groups
        .iter()
        .flat_map(|g| &g.tracks)
        .map(|t| t.segments.len() + usize::from(t.init.is_some()))
        .sum();
    let count = Some(FileCount {
        done: files,
        total: files,
    });
    let writing = Writing {
        mp4: count,
        hls: count,
    };
    assert_eq!(progress.borrow().writing, Some(writing));
}

/// 只要 HLS：不经 FFmpeg，MP4 合并不支持的编码（这里是 TS 里的 Opus 音频）照样输出；分片原样放进目录。
#[tokio::test(flavor = "multi_thread")]
async fn hls_only_keeps_codecs_the_mp4_cannot_take() {
    let dir = test_dir("output_hls_only");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_opus/seg0.ts"));
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
        fixture("ts_opus/seg0.ts")
    );
    assert!(!dir.join("out.hsdl").exists());
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

/// 编码放不进 MP4（TS 里的 Opus 音频）：下了第一个分片、核对出来就报错，其余分片不下，任务目录保留；改为输出 HLS 后用同
/// 一个默认任务目录接着下，已下的不重下（续传前核对内容时取过一次）。HLS 目录路径带结尾的分隔符也照常。
#[tokio::test(flavor = "multi_thread")]
async fn switching_to_hls_after_an_unsupported_codec_reuses_the_download() {
    let dir = test_dir("output_switch");
    let server = Server::start().await;
    let mut playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n".to_owned();
    for i in 0..3 {
        server.put(&format!("seg{i}.ts"), fixture("ts_opus/seg0.ts"));
        playlist += &format!("#EXTINF:1.5,\nseg{i}.ts\n");
    }
    server.put("index.m3u8", playlist + "#EXT-X-ENDLIST\n");
    let hits = || [0, 1, 2].map(|i| server.hits(&format!("seg{i}.ts")));
    let url = server.url("index.m3u8");
    let mp4 = request_to(url.clone(), &dir, Target::Mp4(dir.join("out.mp4")));
    match run(mp4).await {
        Err(Error::Unsupported(Unsupported::Mp4(Mp4Unsupported::Codec {
            codec: "opus", ..
        }))) => {}
        other => panic!("TS 里的 Opus 音频应放不进 MP4：{other:?}"),
    }
    assert_eq!(hits(), [1, 0, 0]);
    assert_eq!(names_in(&dir), ["out.hsdl"]);
    assert!(dir.join("out.hsdl/job.json").exists());

    let mut hls = dir.join("out").into_os_string();
    hls.push(std::path::MAIN_SEPARATOR_STR);
    let output = run(request_to(url, &dir, Target::Hls(hls.into())))
        .await
        .unwrap();

    assert!(dir.join("out/0/2.ts").is_file());
    assert_eq!(output.leftovers, []);
    assert!(!dir.join("out.hsdl").exists());
    assert_eq!(hits(), [2, 1, 1]);
}

/// 打包音频前的 ID3 标签很长（带封面图）：下载时整片校验通过，本地 HLS 按标签长度跳过后识别出 AAC。
#[tokio::test(flavor = "multi_thread")]
async fn long_id3_tags_before_packed_audio_are_skipped() {
    let dir = test_dir("output_long_id3");
    let server = Server::start().await;
    let body = vec![0u8; 70 * 1024];
    let size = body.len();
    let syncsafe = [size >> 21, size >> 14, size >> 7, size].map(|b| (b & 0x7F) as u8);
    let tag = [b"ID3\x04\x00\x00".as_slice(), &syncsafe, &body].concat();
    // 帧头声明帧长 16 字节的一个 ADTS 帧
    let adts = [&[0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC][..], &[0; 9]].concat();
    server.put("seg0.aac", [tag.as_slice(), &adts].concat());
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.aac\n#EXT-X-ENDLIST\n",
    );
    let hls = dir.join("out");

    run(request_to(
        server.url("index.m3u8"),
        &dir,
        Target::Hls(hls.clone()),
    ))
    .await
    .unwrap();

    assert!(hls.join("0/0.aac").is_file());
}

/// 同一条轨先 TS 后 fMP4（EXT-X-MAP 只作用于其后的分片）：本地 HLS 照常写出。
#[tokio::test(flavor = "multi_thread")]
async fn hls_accepts_ts_before_fmp4() {
    let dir = test_dir("output_ts_then_fmp4");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put("init.mp4", fixture("fmp4_a/video/init.mp4"));
    server.put("seg0.m4s", fixture("fmp4_a/video/seg0.m4s"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-DISCONTINUITY\n\
         #EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:1,\nseg0.m4s\n#EXT-X-ENDLIST\n",
    );
    let hls = dir.join("out");

    run(request_to(
        server.url("index.m3u8"),
        &dir,
        Target::Hls(hls.clone()),
    ))
    .await
    .unwrap();

    assert!(hls.join("0/0.ts").is_file() && hls.join("0/1.m4s").is_file());
}

/// 音频分片声明的时长都为 0，算不出峰值码率：主播放列表用来源写的 BANDWIDTH；来源也没写时报错，两者都要时
/// 在合并 MP4 之前就报。
#[tokio::test(flavor = "multi_thread")]
async fn bandwidth_falls_back_to_the_source_and_fails_without_it() {
    let dir = test_dir("output_bandwidth");
    let server = Server::start().await;
    let url = put_split_vod(&server);
    server.put("audio.m3u8", split_media("audio", 3, 2, "0"));

    let hls = dir.join("out");
    run(request_to(url.clone(), &dir, Target::Hls(hls.clone())))
        .await
        .unwrap();
    let Playlist::Master(master) = read_playlist(&hls.join("index.m3u8")) else {
        panic!("应为主播放列表");
    };
    assert_eq!(master.variants[0].bandwidth, Some(200_000));

    server.put("master.m3u8", SPLIT_MASTER.replace("BANDWIDTH=200000,", ""));
    let other = Target::Both {
        mp4: dir.join("other.mp4"),
        hls: dir.join("other"),
    };
    match run(request_to(url, &dir, other)).await {
        Err(Error::Unsupported(Unsupported::Hls(HlsUnsupported::BandwidthUnknown))) => {}
        other => panic!("应报算不出码率：{other:?}"),
    }
    assert!(!dir.join("other.mp4").exists() && !dir.join("other").exists());
}

/// 直播续录时服务器从 fMP4 换成了 TS，另起会话：同一条轨有的组用 init 段、有的不用，本地 HLS 无法表示，
/// 合并 MP4 之前就报不支持，什么也不写，任务目录保留。
#[tokio::test(flavor = "multi_thread")]
async fn hls_cannot_mix_fmp4_and_ts_in_one_track() {
    let dir = test_dir("output_mixed_init");
    let server = Server::start().await;
    server.put("init.mp4", fixture("fmp4_a/video/init.mp4"));
    server.put("seg0.m4s", fixture("fmp4_a/video/seg0.m4s"));
    server.put("seg0.ts", fixture("ts_long/seg0.ts"));
    server.put(
        "live.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:1,\nseg0.m4s\n",
    );
    let mut req = request_to(server.url("live.m3u8"), &dir, both(&dir));
    req.live = Some(LiveOptions::default());
    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.segments.done == 1).await.unwrap();
    job.control().cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    server.put(
        "live.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );

    match run(req).await {
        Err(Error::Unsupported(Unsupported::Hls(HlsUnsupported::MixedInit { track: 0 }))) => {}
        other => panic!("应报不支持：{other:?}"),
    }
    assert!(!dir.join("out.mp4").exists() && !dir.join("out").exists());
    assert!(dir.join("out.hsdl/job.json").exists());
}
