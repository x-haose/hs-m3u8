//! 轨道与分片：音视频分离、独立音频取代变体里混着的、字节范围、标了 EXT-X-GAP 的分片、只差查询串的 init 段。

use std::sync::Arc;

use hs_m3u8_core::{
    Error, HookError, Hooks, MissReason, Missed, Purpose, RequestParts, Unsupported,
};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use super::{TS_A, put_ts_a};
use crate::server::Server;
use crate::{
    assert_output, engine, expected, expected_long, fixture, fixtures, request, run, test_dir,
    track,
};

/// 主播放列表经重定向到子目录（相对地址按最终地址解析），选出视频变体与独立音频 rendition，两个不连续段组。
#[tokio::test(flavor = "multi_thread")]
async fn split_audio_video_with_redirect_and_discontinuity() {
    let dir = test_dir("split");
    let server = Server::start().await;
    for program in ["fmp4_a", "fmp4_b"] {
        for kind in ["video", "audio"] {
            let names = std::fs::read_dir(fixtures().join(program).join(kind)).unwrap();
            for name in names {
                let name = name.unwrap().file_name().into_string().unwrap();
                server.put(
                    &format!("hls/{program}/{kind}/{name}"),
                    fixture(&format!("{program}/{kind}/{name}")),
                );
            }
        }
    }
    let media = |kind: &str, a: usize, b: usize| {
        let mut text = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:1\n");
        text += &format!("#EXT-X-MAP:URI=\"fmp4_a/{kind}/init.mp4\"\n");
        for i in 0..a {
            text += &format!("#EXTINF:1,\nfmp4_a/{kind}/seg{i}.m4s\n");
        }
        text += &format!("#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"fmp4_b/{kind}/init.mp4\"\n");
        for i in 0..b {
            text += &format!("#EXTINF:1,\nfmp4_b/{kind}/seg{i}.m4s\n");
        }
        text + "#EXT-X-ENDLIST\n"
    };
    server.put("hls/video.m3u8", media("video", 2, 1));
    server.put("hls/audio.m3u8", media("audio", 3, 2));
    server.put(
        "hls/master.m3u8",
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"main\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=200000,RESOLUTION=320x180,AUDIO=\"aud\"\nvideo.m3u8\n",
    );
    server.redirect("watch", "hls/master.m3u8");

    // 探测：列出主播放列表、按偏好选出的轨与各轨的媒体播放列表，相对地址按重定向之后的地址解析
    let req = request(server.url("watch"), &dir);
    let probe = engine().probe(&req.source).await.unwrap();
    assert!(!probe.live);
    let master = probe.master.unwrap();
    assert_eq!((master.variants.len(), master.audio.len()), (1, 1));
    let selected = &master.selected;
    assert_eq!(
        (selected.variant.index, selected.variant.bandwidth),
        (0, Some(200_000))
    );
    let audio = selected.audio.as_ref().unwrap();
    assert_eq!((audio.index, audio.name.as_deref()), (0, Some("main")));
    let counts: Vec<(usize, bool)> = probe.tracks.iter().map(|t| (t.segments, t.ended)).collect();
    assert_eq!(counts, [(3, true), (5, true)]);

    let job = engine().start(req).unwrap();
    let progress = job.control().progress();
    let output = job.wait().await.unwrap();
    assert_eq!(progress.borrow().selected.as_ref(), Some(selected));

    let program = |name: &str, video: &[&str], audio: &[&str]| DiscontinuityGroup {
        tracks: vec![
            track(&format!("{name}/video"), Some("init.mp4"), video),
            track(&format!("{name}/audio"), Some("init.mp4"), audio),
        ],
    };
    let want = expected(
        &dir,
        &[Streams::Video, Streams::Audio],
        &[
            program(
                "fmp4_a",
                &["seg0.m4s", "seg1.m4s"],
                &["seg0.m4s", "seg1.m4s", "seg2.m4s"],
            ),
            program("fmp4_b", &["seg0.m4s"], &["seg0.m4s", "seg1.m4s"]),
        ],
    );
    assert_output(&output, &want);
    assert_eq!(output.segments, 8);
}

/// 变体 TS 混有音频，又选了独立音频 rendition：只用 rendition 的音频，变体里的音频不进输出。
#[tokio::test(flavor = "multi_thread")]
async fn selected_audio_rendition_replaces_muxed_audio() {
    let dir = test_dir("muxed_audio");
    let server = Server::start().await;
    put_ts_a(&server, "video.m3u8", 1.0);
    let audio = ["seg0.m4s", "seg1.m4s", "seg2.m4s"];
    for name in ["init.mp4"].iter().chain(&audio) {
        server.put(
            &format!("audio/{name}"),
            fixture(&format!("fmp4_a/audio/{name}")),
        );
    }
    let mut playlist =
        String::from("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"audio/init.mp4\"\n");
    for name in audio {
        playlist += &format!("#EXTINF:1,\naudio/{name}\n");
    }
    server.put("audio.m3u8", playlist + "#EXT-X-ENDLIST\n");
    server.put(
        "master.m3u8",
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=320x180,AUDIO=\"aud\"\nvideo.m3u8\n",
    );

    let output = run(request(server.url("master.m3u8"), &dir)).await.unwrap();

    let tracks = vec![
        track("ts_a", None, &TS_A),
        track("fmp4_a/audio", Some("init.mp4"), &audio),
    ];
    let want = expected(
        &dir,
        &[Streams::Video, Streams::Audio],
        &[DiscontinuityGroup { tracks }],
    );
    assert_output(&output, &want);
}

/// init 段与分片都是同一个文件里的字节范围。
#[tokio::test(flavor = "multi_thread")]
async fn byte_ranges() {
    let dir = test_dir("byte_ranges");
    let server = Server::start().await;
    let parts = ["init.mp4", "seg0.m4s", "seg1.m4s"].map(|n| fixture(&format!("fmp4_a/video/{n}")));
    server.put("video.mp4", parts.concat());
    let (init, seg0, seg1) = (parts[0].len(), parts[1].len(), parts[2].len());
    server.put(
        "index.m3u8",
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"video.mp4\",BYTERANGE=\"{init}@0\"\n\
             #EXTINF:1,\n#EXT-X-BYTERANGE:{seg0}@{init}\nvideo.mp4\n\
             #EXTINF:1,\n#EXT-X-BYTERANGE:{seg1}\nvideo.mp4\n#EXT-X-ENDLIST\n"
        ),
    );

    let output = run(request(server.url("index.m3u8"), &dir)).await.unwrap();

    let want = expected(
        &dir,
        &[Streams::All],
        &[DiscontinuityGroup {
            tracks: vec![track(
                "fmp4_a/video",
                Some("init.mp4"),
                &["seg0.m4s", "seg1.m4s"],
            )],
        }],
    );
    assert_output(&output, &want);
    assert_eq!(server.hits("video.mp4"), 3);
}

/// 标了 EXT-X-GAP 的分片：不去取，不算失败；成片里那段留空、保留原时间戳，结果与进度里报出来。
#[tokio::test(flavor = "multi_thread")]
async fn gap_segments_are_skipped_and_reported() {
    let dir = test_dir("vod_gap");
    let server = Server::start().await;
    for i in [0, 2] {
        server.put(
            &format!("seg{i}.ts"),
            fixture(&format!("ts_long/seg{i}.ts")),
        );
    }
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-GAP\n#EXTINF:1,\nseg1.ts\n\
         #EXTINF:1,\nseg2.ts\n#EXT-X-ENDLIST\n",
    );

    let job = engine()
        .start(request(server.url("index.m3u8"), &dir))
        .unwrap();
    let progress = job.control().progress();
    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 2], &[2]));
    let gap = Missed {
        session: 0,
        track: 0,
        first: 1,
        last: 1,
        reason: MissReason::Gap,
    };
    assert_eq!(output.missed, [gap]);
    assert_eq!(server.hits("seg1.ts"), 0);
    let last = progress.borrow().clone();
    assert_eq!(
        (last.segments.done, last.segments.total, last.segments.gap),
        (2, 2, 1)
    );
}

/// 独立音频有一整段都标了 EXT-X-GAP（音频编码器中断）：这段合并不了，两条轨这段的分片都不取，报为缺失，其余照常
/// 合并；音频全部标了 GAP 时没有一段合并得了，下载前报出是哪条播放列表。
#[tokio::test(flavor = "multi_thread")]
async fn a_group_with_only_gaps_in_one_track_is_left_out() {
    let dir = test_dir("vod_gap_group");
    let server = Server::start().await;
    for kind in ["video", "audio"] {
        for name in ["init.mp4", "seg0.m4s", "seg1.m4s"] {
            server.put(
                &format!("fmp4_a/{kind}/{name}"),
                fixture(&format!("fmp4_a/{kind}/{name}")),
            );
        }
    }
    let media = |kind: &str, gap: &str| {
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"fmp4_a/{kind}/init.mp4\"\n\
             {gap}#EXTINF:1,\nfmp4_a/{kind}/seg0.m4s\n{gap}#EXTINF:1,\nfmp4_a/{kind}/seg1.m4s\n\
             #EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"fmp4_b/{kind}/init.mp4\"\n\
             #EXTINF:1,\nfmp4_b/{kind}/seg0.m4s\n#EXT-X-ENDLIST\n"
        )
    };
    let audio_gaps =
        media("audio", "").replace("#EXTINF:1,\nfmp4_b", "#EXT-X-GAP\n#EXTINF:1,\nfmp4_b");
    server.put("video.m3u8", media("video", ""));
    server.put("audio.m3u8", audio_gaps);
    server.put(
        "master.m3u8",
        "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"x\",URI=\"audio.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"a\"\nvideo.m3u8\n",
    );

    let output = run(request(server.url("master.m3u8"), &dir)).await.unwrap();

    let group = DiscontinuityGroup {
        tracks: vec![
            track("fmp4_a/video", Some("init.mp4"), &["seg0.m4s", "seg1.m4s"]),
            track("fmp4_a/audio", Some("init.mp4"), &["seg0.m4s", "seg1.m4s"]),
        ],
    };
    let want = expected(&dir, &[Streams::Video, Streams::Audio], &[group]);
    assert_output(&output, &want);
    let missed = |track, reason| Missed {
        session: 0,
        track,
        first: 2,
        last: 2,
        reason,
    };
    assert_eq!(
        output.missed,
        [
            missed(0, MissReason::Unmergeable),
            missed(1, MissReason::Gap)
        ]
    );
    assert_eq!(
        server.hits("fmp4_b/video/init.mp4") + server.hits("fmp4_b/video/seg0.m4s"),
        0
    );

    let dir = test_dir("vod_gap_track");
    server.put(
        "audio.m3u8",
        media("audio", "#EXT-X-GAP\n")
            .replace("#EXTINF:1,\nfmp4_b", "#EXT-X-GAP\n#EXTINF:1,\nfmp4_b"),
    );
    let err = run(request(server.url("master.m3u8"), &dir))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            Error::Unsupported(Unsupported::NoMergeableGroup(url)) if **url == server.url("audio.m3u8")
        ),
        "{err}"
    );
    assert!(!dir.join("out.hsdl").exists());
}

/// 把 init 段地址 `init?p=<目录>` 改写为 `<目录>/init.mp4`：模拟按查询串区分内容的站点。
struct InitByQuery;

impl Hooks for InitByQuery {
    fn on_request(&self, purpose: Purpose, request: &mut RequestParts) -> Result<(), HookError> {
        if purpose == Purpose::Init
            && let Some(program) = request.url.query().and_then(|q| q.strip_prefix("p="))
        {
            request.url = request.url.join(&format!("{program}/init.mp4"))?;
            request.url.set_query(None);
        }
        Ok(())
    }
}

/// 两个 init 段的地址只差查询串（站点按查询串区分内容）：各拉各的，不当成同一个。
#[tokio::test(flavor = "multi_thread")]
async fn inits_differing_only_in_query_are_distinct() {
    let dir = test_dir("init_query");
    let server = Server::start().await;
    for program in ["fmp4_a", "fmp4_b"] {
        for name in ["init.mp4", "seg0.m4s"] {
            server.put(
                &format!("{program}/{name}"),
                fixture(&format!("{program}/video/{name}")),
            );
        }
    }
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"init?p=fmp4_a\"\n\
         #EXTINF:1,\nfmp4_a/seg0.m4s\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init?p=fmp4_b\"\n\
         #EXTINF:1,\nfmp4_b/seg0.m4s\n#EXT-X-ENDLIST\n",
    );
    let mut req = request(server.url("index.m3u8"), &dir);
    req.source.hooks = Arc::new(InitByQuery);

    let output = run(req).await.unwrap();

    let program = |name: &str| DiscontinuityGroup {
        tracks: vec![track(
            &format!("{name}/video"),
            Some("init.mp4"),
            &["seg0.m4s"],
        )],
    };
    let want = expected(
        &dir,
        &[Streams::All],
        &[program("fmp4_a"), program("fmp4_b")],
    );
    assert_output(&output, &want);
    assert_eq!(
        (
            server.hits("fmp4_a/init.mp4"),
            server.hits("fmp4_b/init.mp4")
        ),
        (1, 1)
    );
}
