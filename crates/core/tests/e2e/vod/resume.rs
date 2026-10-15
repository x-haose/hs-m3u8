//! 失败与续传：地址变了时核对内容、找回上次的选轨、取消之后续传。

use std::time::Duration;

use hs_m3u8_core::hls::AudioChoice;
use hs_m3u8_core::{Error, HttpErrorKind, Url, WorkDirProblem};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use super::{TS_A, expected_ts_a, put_ts_a};
use crate::server::Server;
use crate::{assert_output, engine, expected, fixture, fixtures, request, run, test_dir, track};

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
                    kind: HttpErrorKind::Status(404),
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
    // 第一次运行下载的，与续传前核对内容时取的
    assert_eq!(server.hits("seg0.ts"), 2);
    // 续传前已完成的分片计入时长与字节数
    let last = progress.borrow().clone();
    assert_eq!(last.duration, Duration::from_secs(2));
    let plain: usize = TS_A
        .map(|n| fixture(&format!("ts_a/{n}")).len())
        .iter()
        .sum();
    assert_eq!(last.bytes, plain as u64);
}

/// 两个分片、各声明 1 秒的点播播放列表，分片地址为 `<dir>/seg<i>.ts`。
fn two_segments_in(dir: &str) -> String {
    format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\n{dir}/seg0.ts\n\
         #EXTINF:1,\n{dir}/seg1.ts\n#EXT-X-ENDLIST\n"
    )
}

/// CDN 在播放列表与分片的路径里放每次会话不同的令牌：续传时地址都变了，分片的身份（地址最后一段）不变；核对内容相同
/// （重新下载已完成的 seg0 一次），照常续传，不重下已完成的。
#[tokio::test(flavor = "multi_thread")]
async fn resume_survives_tokens_in_the_path() {
    let dir = test_dir("resume_path_token");
    let server = Server::start().await;
    for token in ["tok1", "tok2"] {
        server.put(&format!("{token}/index.m3u8"), two_segments_in("."));
        server.put(&format!("{token}/seg0.ts"), fixture("ts_a/seg0.ts"));
    }
    assert!(
        run(request(server.url("tok1/index.m3u8"), &dir))
            .await
            .is_err()
    );

    server.put("tok2/seg1.ts", fixture("ts_a/seg1.ts"));
    let output = run(request(server.url("tok2/index.m3u8"), &dir))
        .await
        .unwrap();
    assert_output(&output, &expected_ts_a(&dir));
    assert_eq!(server.hits("tok2/seg0.ts"), 1);
}

/// 地址变了（这里只是查询串），新地址列出的分片名、时长都与记录的相同，内容却不同（只靠查询串区分的另一个视频，或
/// 同一视频的另一档）：计划摘要相同也不续传，报 SourceUnverified，别的分片一个也不下；原来的地址照常续传。
#[tokio::test(flavor = "multi_thread")]
async fn different_content_at_a_new_address_is_not_resumed() {
    let dir = test_dir("resume_other_content");
    let server = Server::start().await;
    let url = |id: u32| Url::parse(&format!("{}?id={id}", server.url("index.m3u8"))).unwrap();
    server.put("v1/seg0.ts", fixture("ts_a/seg0.ts"));
    server.put("index.m3u8", two_segments_in("v1"));
    assert!(run(request(url(1), &dir)).await.is_err());

    server.put("v2/seg0.ts", fixture("ts_b/seg0.ts"));
    server.put("v2/seg1.ts", fixture("ts_a/seg1.ts"));
    server.put("index.m3u8", two_segments_in("v2"));
    let err = run(request(url(2), &dir)).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::WorkDir {
                problem: WorkDirProblem::SourceUnverified,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(server.hits("v2/seg1.ts"), 0);

    server.put("v1/seg1.ts", fixture("ts_a/seg1.ts"));
    server.put("index.m3u8", two_segments_in("v1"));
    let output = run(request(url(1), &dir)).await.unwrap();
    assert_output(&output, &expected_ts_a(&dir));
    assert_eq!(server.hits("v1/seg0.ts"), 2);
}

/// 两个视频共用单独成组的片尾（逐字节相同），主体的分片名、时长也相同：按组核对，主体那组比较出内容不同，报
/// SourceUnverified，不因片尾相同而续传。
#[tokio::test(flavor = "multi_thread")]
async fn a_shared_ending_does_not_vouch_for_the_rest() {
    let dir = test_dir("resume_shared_ending");
    let server = Server::start().await;
    let url = |id: u32| Url::parse(&format!("{}?id={id}", server.url("index.m3u8"))).unwrap();
    let playlist = |dir: &str| {
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\n{dir}/seg0.ts\n#EXTINF:1,\n{dir}/seg1.ts\n\
             #EXT-X-DISCONTINUITY\n#EXTINF:1,\ncommon/end.ts\n#EXT-X-ENDLIST\n"
        )
    };
    server.put("common/end.ts", fixture("ts_b/seg0.ts"));
    server.put("v1/seg0.ts", fixture("ts_a/seg0.ts"));
    server.put("index.m3u8", playlist("v1"));
    // 合并 MP4 时先下各组的第一个分片：seg0 与片尾都已完成，v1/seg1 取不到
    assert!(run(request(url(1), &dir)).await.is_err());
    assert_eq!(server.hits("common/end.ts"), 1);

    server.put("v2/seg0.ts", fixture("ts_long/seg0.ts"));
    server.put("v2/seg1.ts", fixture("ts_long/seg1.ts"));
    server.put("index.m3u8", playlist("v2"));
    let err = run(request(url(2), &dir)).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::WorkDir {
                problem: WorkDirProblem::SourceUnverified,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(server.hits("v2/seg1.ts"), 0);
}

/// 每个分片自成一组（每片前都有 EXT-X-DISCONTINUITY）：续传前至多核对已完成字节数最多的 16 组，不把已完成的都重下
/// 一遍。
#[tokio::test(flavor = "multi_thread")]
async fn verification_checks_at_most_sixteen_groups_per_track() {
    let dir = test_dir("resume_many_groups");
    let server = Server::start().await;
    let mut playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n".to_owned();
    for i in 0..17 {
        playlist += &format!("#EXT-X-DISCONTINUITY\n#EXTINF:1,\n{i}/seg.ts\n");
        server.put(&format!("{i}/seg.ts"), fixture("ts_long/seg0.ts"));
    }
    playlist += "#EXT-X-DISCONTINUITY\n#EXTINF:1,\nlast/seg.ts\n#EXT-X-ENDLIST\n";
    server.put("index.m3u8", playlist);
    assert!(run(request(server.url("index.m3u8"), &dir)).await.is_err());

    server.put("last/seg.ts", fixture("ts_long/seg0.ts"));
    run(request(server.url("index.m3u8"), &dir)).await.unwrap();
    let rechecked: usize = (0..17)
        .map(|i| server.hits(&format!("{i}/seg.ts")) - 1)
        .sum();
    assert_eq!(rechecked, 16);
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
    let selected = progress.borrow().selected.clone().unwrap();
    assert_eq!(selected.variant.index, 1);
}

/// 同一音频组里两条属性完全相同的音轨（同组、同语言、同名），按下标选了后一条：续传按排位找回同一条，
/// 不换成前一条（两条的分片文件名与 init 段内容相同，换了也不会报错，只会把两条音轨拼在一起）。
#[tokio::test(flavor = "multi_thread")]
async fn resume_finds_the_same_identical_audio_rendition() {
    let dir = test_dir("resume_identical_audio");
    let server = Server::start().await;
    let media = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"init.mp4\"\n\
                 #EXTINF:1,\nseg0.m4s\n#EXTINF:1,\nseg1.m4s\n#EXT-X-ENDLIST\n";
    for (dir, fixtures) in [
        ("v", "fmp4_a/video"),
        ("a", "fmp4_a/audio"),
        ("b", "fmp4_b/audio"),
    ] {
        for name in ["init.mp4", "seg0.m4s", "seg1.m4s"] {
            server.put(
                &format!("{dir}/{name}"),
                fixture(&format!("{fixtures}/{name}")),
            );
        }
        server.put(&format!("{dir}/index.m3u8"), media);
    }
    server.put(
        "master.m3u8",
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"a/index.m3u8\"\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",URI=\"b/index.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=200000,RESOLUTION=320x180,AUDIO=\"aud\"\nv/index.m3u8\n",
    );
    let mut req = request(server.url("master.m3u8"), &dir);
    req.source.preference.audio = AudioChoice::Index(1);
    server.remove("b/seg1.m4s");
    assert!(run(req.clone()).await.is_err());
    server.put("b/seg1.m4s", fixture("fmp4_b/audio/seg1.m4s"));

    let output = run(req).await.unwrap();

    let group = DiscontinuityGroup {
        tracks: vec![
            track("fmp4_a/video", Some("init.mp4"), &["seg0.m4s", "seg1.m4s"]),
            track("fmp4_b/audio", Some("init.mp4"), &["seg0.m4s", "seg1.m4s"]),
        ],
    };
    let want = expected(&dir, &[Streams::Video, Streams::Audio], &[group]);
    assert_output(&output, &want);
    assert_eq!(server.hits("a/seg1.m4s"), 0);
}

/// 变体写了稳定 ID：中断期间它的码率变了、前面多了更高的变体，续传按稳定 ID 找回它（按属性就找不到了）。
#[tokio::test(flavor = "multi_thread")]
async fn resume_finds_the_variant_by_its_stable_id() {
    let dir = test_dir("resume_stable_id");
    let server = Server::start().await;
    put_ts_a_variant(&server, "a");
    put_ts_a_variant(&server, "c");
    let variant = |bandwidth: u32| {
        format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},RESOLUTION=160x90,STABLE-VARIANT-ID=\"a\"\n\
             a/v.m3u8\n"
        )
    };
    server.put("master.m3u8", format!("#EXTM3U\n{}", variant(1)));
    let req = request(server.url("master.m3u8"), &dir);
    server.remove("a/seg1.ts");
    assert!(matches!(run(req.clone()).await, Err(Error::Segment { .. })));
    server.put("a/seg1.ts", fixture("ts_a/seg1.ts"));
    server.put(
        "master.m3u8",
        format!(
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=9,RESOLUTION=1920x1080\nc/v.m3u8\n{}",
            variant(2)
        ),
    );

    let output = run(req).await.unwrap();

    assert_output(&output, &expected_ts_a(&dir));
    assert_eq!(server.hits("c/v.m3u8"), 0);
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
    // 第一次运行下载的，与续传前核对内容时取的
    assert_eq!(server.hits("seg0.ts"), 2);
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

    // 两个样本的 init 段只差视频轨的 handler 名，配错时成片也可能逐字节相同，所以另核对目录里的 init 段
    let mut inits: Vec<Vec<u8>> =
        std::fs::read_dir(req.output.resolved_work_dir().unwrap().join("tracks/0"))
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
