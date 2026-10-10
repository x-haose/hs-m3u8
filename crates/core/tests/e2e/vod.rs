//! 点播：解密、音视频分离、字节范围、重试、失败与续传、取消、回调、下载前的拒绝。

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockModeEncrypt, KeyIvInit};
use axum::http::StatusCode;
use hs_m3u8_core::{
    Error, HookError, Hooks, HttpError, Integrity, KeyOverride, Purpose, RequestParts, Stage,
    Unsupported, Url, WorkDirProblem,
};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use crate::server::Server;
use crate::{
    assert_output, engine, expected, expected_long, fixture, fixtures, request, run, test_dir,
    track,
};

/// ts_a 的分片（H.264 + AAC 的 TS）。
const TS_A: [&str; 2] = ["seg0.ts", "seg1.ts"];

/// ts_a 两个分片单轨合并的期望输出。
fn expected_ts_a(dir: &Path) -> Vec<u8> {
    let tracks = vec![track("ts_a", None, &TS_A)];
    expected(dir, &[Streams::All], &[DiscontinuityGroup { tracks }])
}

fn encrypt(plain: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
    let mut buf = plain.to_vec();
    buf.resize(plain.len() + 16, 0);
    let len = cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into())
        .encrypt_padded::<Pkcs7>(&mut buf, plain.len())
        .unwrap()
        .len();
    buf.truncate(len);
    buf
}

fn hex(bytes: &[u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// ts_long 的四个分片：第一个用显式 IV，其余三个换了 key 并用媒体序号推出的 IV。
/// 并发 4 时三个分片同时需要第二个 key，它只被拉取一次。
#[tokio::test(flavor = "multi_thread")]
async fn aes128_explicit_and_sequence_iv() {
    let dir = test_dir("aes128");
    let server = Server::start().await;
    let (key0, key1) = ([0x11; 16], [0x22; 16]);
    let iv0 = [0x33; 16];
    server.put("k0", key0);
    server.put("k1", key1);
    // MEDIA-SEQUENCE 为 7：分片 i 的序号为 7 + i
    let mut playlist = format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:7\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"k0\",IV=0x{}\n",
        hex(&iv0)
    );
    let mut served = 0;
    for i in 0..4u64 {
        let plain = fixture(&format!("ts_long/seg{i}.ts"));
        let cipher = match i {
            0 => encrypt(&plain, &key0, &iv0),
            _ => encrypt(&plain, &key1, &u128::from(7 + i).to_be_bytes()),
        };
        served += cipher.len();
        server.put(&format!("s{i}.ts"), cipher);
        if i == 1 {
            playlist += "#EXT-X-KEY:METHOD=AES-128,URI=\"k1\"\n";
        }
        playlist += &format!("#EXTINF:1,\ns{i}.ts\n");
    }
    let playlist = playlist + "#EXT-X-ENDLIST\n";
    served += playlist.len() + key0.len() + key1.len();
    server.put("index.m3u8", playlist);

    let mut req = request(server.url("index.m3u8"), &dir);
    req.concurrency = NonZeroUsize::new(4).unwrap();
    let job = engine().start(req).unwrap();
    let progress = job.control().progress();
    let output = job.wait().await.unwrap();

    assert_output(&output, &expected_long(&dir, &[0, 1, 2, 3], &[4]));
    assert_eq!(output.segments, 4);
    let last = progress.borrow().clone();
    assert_eq!(
        (last.stage, last.segments_done, last.segments_total),
        (Stage::Done, 4, 4)
    );
    // 读到的字节：播放列表、两个 key 与四个分片（密文），各一次
    assert_eq!(last.received, served as u64);
    assert_eq!(last.duration_us, 4_000_000);
    assert_eq!((server.hits("k0"), server.hits("k1")), (1, 1));
}

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
    assert!(!probe.is_live());
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
    assert_eq!(progress.borrow().selection.as_ref(), Some(selected));

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

/// 500 按重试策略重试后成功。
#[tokio::test(flavor = "multi_thread")]
async fn retries_server_errors() {
    let dir = test_dir("retries");
    let server = Server::start().await;
    put_ts_a(&server, "index.m3u8", 1.0);
    server.fail("seg0.ts", 2);

    let output = run(request(server.url("index.m3u8"), &dir)).await.unwrap();

    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
    assert_eq!(server.hits("seg0.ts"), 3);
}

/// ts_a 的两个分片，明文；`duration` 为每个分片声明的时长。
fn put_ts_a(server: &Server, playlist: &str, duration: f64) {
    for name in TS_A {
        server.put(name, fixture(&format!("ts_a/{name}")));
    }
    server.put(
        playlist,
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:{duration},\nseg0.ts\n\
             #EXTINF:{duration},\nseg1.ts\n#EXT-X-ENDLIST\n"
        ),
    );
}

/// 分片 404：任务失败、不生成输出、保留任务目录；播放列表变了不能续传；恢复后续传只补缺的分片。
#[tokio::test(flavor = "multi_thread")]
async fn failure_then_resume() {
    let dir = test_dir("resume");
    let server = Server::start().await;
    put_ts_a(&server, "index.m3u8", 1.0);
    server.remove("seg1.ts");
    let req = request(server.url("index.m3u8"), &dir);

    let err = run(req.clone()).await.unwrap_err();
    match err {
        Error::Segment {
            track: 0,
            sequence: 1,
            cause,
            ..
        } => assert!(
            matches!(
                *cause,
                Error::Http {
                    kind: HttpError::Status(404),
                    ..
                }
            ),
            "{cause}"
        ),
        other => panic!("{other}"),
    }
    assert!(!req.output.target.mp4().unwrap().exists());
    assert!(dir.join("out.hsdl/job.json").exists());

    put_ts_a(&server, "index.m3u8", 0.5);
    assert!(matches!(
        run(req.clone()).await,
        Err(Error::WorkDir {
            problem: WorkDirProblem::PlanChanged,
            ..
        })
    ));

    put_ts_a(&server, "index.m3u8", 1.0);
    let job = engine().start(req).unwrap();
    let progress = job.control().progress();
    let output = job.wait().await.unwrap();
    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
    assert_eq!(server.hits("seg0.ts"), 1);
    // 续传前已完成的分片计入时长与字节数
    let last = progress.borrow().clone();
    assert_eq!(last.duration_us, 2_000_000);
    let plain: usize = TS_A
        .map(|n| fixture(&format!("ts_a/{n}")).len())
        .iter()
        .sum();
    assert_eq!(last.bytes, plain as u64);
}

/// CDN 在路径里放每次会话不同的令牌：续传时分片的地址都变了，但身份（地址最后一段）不变，照常续传。
#[tokio::test(flavor = "multi_thread")]
async fn resume_survives_tokens_in_the_path() {
    let dir = test_dir("resume_path_token");
    let server = Server::start().await;
    let playlist = |token: &str| {
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\n{token}/seg0.ts\n\
             #EXTINF:1,\n{token}/seg1.ts\n#EXT-X-ENDLIST\n"
        )
    };
    server.put("tok1/seg0.ts", fixture("ts_a/seg0.ts"));
    server.put("index.m3u8", playlist("tok1"));
    let req = request(server.url("index.m3u8"), &dir);
    assert!(run(req.clone()).await.is_err());

    for name in TS_A {
        server.put(&format!("tok2/{name}"), fixture(&format!("ts_a/{name}")));
    }
    server.put("index.m3u8", playlist("tok2"));
    let output = run(req).await.unwrap();

    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
    assert_eq!(server.hits("tok2/seg0.ts"), 0);
}

/// 主播放列表里有两个属性完全相同的变体（冗余流）：续传按排位选回上次那个。两者分片文件名相同、计划摘要
/// 也相同，选错了不会报错，只能看续传时的请求落在哪个变体上。
#[tokio::test(flavor = "multi_thread")]
async fn resume_finds_the_same_redundant_variant() {
    let dir = test_dir("resume_redundant");
    let server = Server::start().await;
    put_ts_a_variant(&server, "a");
    put_ts_a_variant(&server, "b");
    server.put(
        "master.m3u8",
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=160x90\na/v.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=160x90\nb/v.m3u8\n",
    );
    let req = request(server.url("master.m3u8"), &dir);
    // 两者相同时按最佳选到后一个；并发为 1，seg1 的请求到达时 seg0 已写完
    let gate = server.gate("b/seg1.ts");
    let job = engine().start(req.clone()).unwrap();
    gate.arrived.notified().await;
    job.control().cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    server.ungate("b/seg1.ts");
    // 中断期间主播放列表多了一个更高的变体：按偏好会选它，续传按记录找回 b，进度里报告的也是 b
    server.put(
        "master.m3u8",
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=160x90\na/v.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=160x90\nb/v.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=9,RESOLUTION=1920x1080\nc/v.m3u8\n",
    );

    let job = engine().start(req).unwrap();
    let progress = job.control().progress();
    let output = job.wait().await.unwrap();

    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
    // b/seg1 第一次运行时到达过一次（挂住后取消），续传时再下一次
    assert_eq!((server.hits("a/seg1.ts"), server.hits("b/seg1.ts")), (0, 2));
    let selected = progress.borrow().selection.clone().unwrap();
    assert_eq!(selected.variant.index, 1);
}

/// 上次一个分片都没下完就中断：目录里没有可续的内容，不按记录的选轨找回（那个变体已从主播放列表里删掉），
/// 按偏好重新选。
#[tokio::test(flavor = "multi_thread")]
async fn selection_is_not_kept_without_completed_segments() {
    let dir = test_dir("resume_nothing_completed");
    let server = Server::start().await;
    put_ts_a_variant(&server, "a");
    put_ts_a_variant(&server, "b");
    let variant = |host: &str, bandwidth: u32| {
        format!("#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},RESOLUTION=160x90\n{host}/v.m3u8\n")
    };
    server.put(
        "master.m3u8",
        format!("#EXTM3U\n{}{}", variant("a", 1), variant("b", 2)),
    );
    let req = request(server.url("master.m3u8"), &dir);
    // 按最佳选到 b，它的第一个分片还在下载时取消
    let gate = server.gate("b/seg0.ts");
    let job = engine().start(req.clone()).unwrap();
    gate.arrived.notified().await;
    job.control().cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    server.ungate("b/seg0.ts");

    server.put("master.m3u8", format!("#EXTM3U\n{}", variant("a", 1)));
    let output = run(req).await.unwrap();

    assert_output(&output, &expected_ts_a(&dir));
    assert_eq!(server.hits("a/seg0.ts"), 1);
}

/// 在 `dir` 下放 ts_a 的两个分片，与引用它们的媒体播放列表 `<dir>/v.m3u8`。
fn put_ts_a_variant(server: &Server, dir: &str) {
    for name in TS_A {
        server.put(&format!("{dir}/{name}"), fixture(&format!("ts_a/{name}")));
    }
    server.put(
        &format!("{dir}/v.m3u8"),
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n\
         #EXT-X-ENDLIST\n",
    );
}

/// 取消在途任务；运行期间同一任务目录不能被第二个任务使用；取消后续传。
#[tokio::test(flavor = "multi_thread")]
async fn cancel_lock_and_resume() {
    let dir = test_dir("cancel");
    let server = Server::start().await;
    put_ts_a(&server, "index.m3u8", 1.0);
    let gate = server.gate("seg1.ts");
    let req = request(server.url("index.m3u8"), &dir);

    let job = engine().start(req.clone()).unwrap();
    // 并发为 1：seg1 的请求到达时 seg0 已写完
    gate.arrived.notified().await;
    let second = run(req.clone()).await.unwrap_err();
    assert!(
        matches!(
            &second,
            Error::WorkDir {
                problem: WorkDirProblem::Locked,
                ..
            }
        ),
        "{second}"
    );
    job.control().cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    assert!(!req.output.target.mp4().unwrap().exists());

    server.ungate("seg1.ts");
    let output = run(req).await.unwrap();
    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
    assert_eq!(server.hits("seg0.ts"), 1);
}

/// key 不对：解密后的填充或内容校验失败，不写盘。
#[tokio::test(flavor = "multi_thread")]
async fn wrong_key_fails_integrity() {
    let dir = test_dir("wrong_key");
    let server = Server::start().await;
    let iv = [0u8; 16];
    server.put("key", [0x01; 16]);
    server.put(
        "seg0.ts",
        encrypt(&fixture("ts_a/seg0.ts"), &[0x02; 16], &iv),
    );
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n\
         #EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );

    let err = run(request(server.url("index.m3u8"), &dir))
        .await
        .unwrap_err();
    match err {
        Error::Segment { cause, .. } => assert!(
            matches!(
                *cause,
                Error::Integrity {
                    kind: Integrity::Padding | Integrity::UnrecognizedSegment(_),
                    ..
                }
            ),
            "{cause}"
        ),
        other => panic!("{other}"),
    }
    let written = std::fs::read_dir(dir.join("out.hsdl/tracks/0"))
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(written, 0, "校验失败的分片不应写盘");
}

/// 自定义 key：分片用它与给定的 IV 解密；播放列表里的 key 地址（这里 404）不请求，分片自己的 IV
/// （由媒体序号推出）不用。
#[tokio::test(flavor = "multi_thread")]
async fn key_override_replaces_the_playlist_key_and_iv() {
    let dir = test_dir("key_override");
    let server = Server::start().await;
    let (key, iv) = ([0x3C; 16], [0x7E; 16]);
    for name in TS_A {
        server.put(name, encrypt(&fixture(&format!("ts_a/{name}")), &key, &iv));
    }
    server.status("key", StatusCode::NOT_FOUND);
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n\
         #EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n",
    );

    let mut req = request(server.url("index.m3u8"), &dir);
    req.key = Some(KeyOverride { key, iv: Some(iv) });
    let output = run(req).await.unwrap();

    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
    assert_eq!(server.hits("key"), 0);
}

/// 站点适配：改写播放列表、给每个请求签名、解开变换过的 key、去掉分片前的伪装字节。
struct SiteHooks;

const DISGUISE: &[u8] = b"\x89PNG\r\n\x1a\n";

impl Hooks for SiteHooks {
    fn on_playlist(&self, _url: &Url, body: Vec<u8>) -> Result<Vec<u8>, HookError> {
        Ok(String::from_utf8(body)?
            .replace("{segment}", "seg")
            .into_bytes())
    }

    fn on_request(&self, _purpose: Purpose, request: &mut RequestParts) -> Result<(), HookError> {
        request.headers.push(("x-token".into(), "secret".into()));
        Ok(())
    }

    fn on_key(&self, _url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError> {
        Ok(data.iter().map(|b| b ^ 0xFF).collect())
    }

    fn on_segment(&self, _url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError> {
        data.strip_prefix(DISGUISE)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| "缺少伪装前缀".into())
    }
}

/// 经站点适配回调（服务器要求签名请求头、key 变换过、分片带伪装前缀）下载：成片与明文样本合并的一致。
#[tokio::test(flavor = "multi_thread")]
async fn hooks_adapt_site() {
    let dir = test_dir("hooks");
    let server = Server::start().await;
    server.require_header("x-token", "secret");
    let key = [0x5A; 16];
    server.put("key", key.map(|b| b ^ 0xFF));
    for (i, name) in TS_A.iter().enumerate() {
        let iv = (i as u128).to_be_bytes();
        let cipher = encrypt(&fixture(&format!("ts_a/{name}")), &key, &iv);
        let body = [DISGUISE, cipher.as_slice()].concat();
        server.put(name, body);
    }
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n\
         #EXTINF:1,\n{segment}0.ts\n#EXTINF:1,\n{segment}1.ts\n#EXT-X-ENDLIST\n",
    );

    let mut req = request(server.url("index.m3u8"), &dir);
    req.source.hooks = Arc::new(SiteHooks);
    // 探测经同样的回调：带上签名请求头，播放列表经改写
    let probe = engine().probe(&req.source).await.unwrap();
    assert_eq!(probe.master, None);
    assert_eq!(
        (probe.tracks[0].segments, probe.tracks[0].encrypted),
        (2, true)
    );
    let output = run(req).await.unwrap();

    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
}

/// init 段地址的签名每次会话不同：续传时分片仍对应同一个 init 段（init 段按内容命名）。
#[tokio::test(flavor = "multi_thread")]
async fn signed_init_urls_resume_with_the_right_init() {
    let dir = test_dir("signed_init");
    let server = Server::start().await;
    for program in ["fmp4_a", "fmp4_b"] {
        for name in ["init.mp4", "seg0.m4s", "seg1.m4s"] {
            if let Ok(data) = std::fs::read(fixtures().join(program).join("video").join(name)) {
                server.put(&format!("{program}/{name}"), data);
            }
        }
    }
    // 三组：a、a、b；第 1 次运行两组 a 的 init 签名不同，第 2 次相同
    let playlist = |sig: [u32; 3]| {
        let mut text = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:1\n");
        for (group, (program, segments)) in [("fmp4_a", 2), ("fmp4_a", 2), ("fmp4_b", 1)]
            .into_iter()
            .enumerate()
        {
            if group > 0 {
                text += "#EXT-X-DISCONTINUITY\n";
            }
            text += &format!("#EXT-X-MAP:URI=\"{program}/init.mp4?s={}\"\n", sig[group]);
            for i in 0..segments {
                text += &format!("#EXTINF:1,\n{program}/seg{i}.m4s\n");
            }
        }
        text + "#EXT-X-ENDLIST\n"
    };
    server.put("index.m3u8", playlist([1, 2, 1]));
    server.remove("fmp4_b/seg0.m4s");
    let req = request(server.url("index.m3u8"), &dir);
    assert!(run(req.clone()).await.is_err());

    server.put("index.m3u8", playlist([1, 1, 1]));
    server.put("fmp4_b/seg0.m4s", fixture("fmp4_b/video/seg0.m4s"));
    let mut req = req;
    req.output.keep_work_dir = true;
    let output = run(req.clone()).await.unwrap();

    // 两个样本的 init 段只差 SPS/PPS，配错时成片也可能逐字节相同，所以另核对目录里的 init 段
    let mut inits: Vec<Vec<u8>> =
        std::fs::read_dir(req.output.resolved_work_dir().join("tracks/0"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
            .map(|p| std::fs::read(p).unwrap())
            .collect();
    inits.sort();
    let mut want_inits = vec![
        fixture("fmp4_a/video/init.mp4"),
        fixture("fmp4_b/video/init.mp4"),
    ];
    want_inits.sort();
    assert_eq!(inits, want_inits);
    let video = |program: &str, segments: &[&str]| DiscontinuityGroup {
        tracks: vec![track(
            &format!("{program}/video"),
            Some("init.mp4"),
            segments,
        )],
    };
    let a = ["seg0.m4s", "seg1.m4s"];
    let want = expected(
        &dir,
        &[Streams::All],
        &[
            video("fmp4_a", &a),
            video("fmp4_a", &a),
            video("fmp4_b", &["seg0.m4s"]),
        ],
    );
    assert_eq!(
        std::fs::read(&output.mp4.as_ref().unwrap().path).unwrap(),
        want
    );
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

/// 来源地址带用户名、密码与令牌时，错误信息与调试输出里都不出现它们（`unwrap` 与日志会打印后者）。
#[tokio::test(flavor = "multi_thread")]
async fn errors_do_not_reveal_credentials_or_tokens() {
    let dir = test_dir("redaction");
    let server = Server::start().await;
    let mut url = server.url("gone.m3u8?token=SECRET");
    url.set_username("user").unwrap();
    url.set_password(Some("PASSWD")).unwrap();
    let err = run(request(url, &dir)).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Http {
                kind: HttpError::Status(404),
                ..
            }
        ),
        "{err}"
    );

    let unsupported = Url::parse("ftp://user:PASSWD@h.example/x?token=SECRET").unwrap();
    let Err(invalid) = engine().start(request(unsupported, &dir)) else {
        panic!("ftp 地址应被拒绝");
    };

    // 探测结果、进度与可修改的请求的调试输出同样不带：界面与日志常直接打印它们
    server.put(
        "master.m3u8",
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8?token=SECRET\n",
    );
    server.put(
        "v.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts?token=SECRET\n#EXT-X-ENDLIST\n",
    );
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    let req = request(server.url("master.m3u8?token=SECRET"), &dir);
    let probe = engine().probe(&req.source).await.unwrap();
    let job = engine().start(req).unwrap();
    let progress = job.control().progress();
    job.wait().await.unwrap();
    let parts = RequestParts {
        url: Url::parse("https://user:PASSWD@h.example/x?token=SECRET").unwrap(),
        headers: vec![("cookie".into(), "SECRET".into())],
    };

    for text in [
        err.to_string(),
        format!("{err:?}"),
        invalid.to_string(),
        format!("{invalid:?}"),
        format!("{probe:?}"),
        format!("{:?}", *progress.borrow()),
        format!("{parts:?}"),
    ] {
        assert!(
            !text.contains("PASSWD") && !text.contains("SECRET"),
            "{text}"
        );
    }
}

/// 下载前就能判定的失败：输出已存在、不录制直播时遇到直播、各轨不连续段不一致。
#[tokio::test(flavor = "multi_thread")]
async fn rejected_before_download() {
    let dir = test_dir("rejected");
    let server = Server::start().await;

    let req = request(server.url("index.m3u8"), &dir);
    std::fs::write(req.output.target.mp4().unwrap(), b"").unwrap();
    assert!(matches!(
        run(req.clone()).await,
        Err(Error::OutputExists(_))
    ));
    std::fs::remove_file(req.output.target.mp4().unwrap()).unwrap();

    server.put(
        "live.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n",
    );
    let mut req = request(server.url("live.m3u8"), &dir);
    req.live = None;
    let err = run(req).await.unwrap_err();
    assert!(
        matches!(err, Error::Unsupported(Unsupported::Live)),
        "{err}"
    );

    server.put(
        "video.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nv0.ts\n#EXT-X-DISCONTINUITY\n\
         #EXTINF:1,\nv1.ts\n#EXT-X-ENDLIST\n",
    );
    server.put(
        "audio.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\na0.aac\n#EXTINF:1,\na1.aac\n#EXT-X-ENDLIST\n",
    );
    server.put(
        "master.m3u8",
        "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"x\",URI=\"audio.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"a\"\nvideo.m3u8\n",
    );
    let err = run(request(server.url("master.m3u8"), &dir))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            Error::Unsupported(Unsupported::DiscontinuityMismatch { track: 1, first, found })
                if *first == [0, 1] && *found == [0]
        ),
        "{err}"
    );
    // 下载前失败不留下任务目录与输出
    assert!(!dir.join("out.hsdl").exists());
    assert_eq!(server.hits("v0.ts") + server.hits("a0.aac"), 0);
}
