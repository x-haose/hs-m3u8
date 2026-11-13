//! 任务目录格式的兼容：已发布版本写的任务目录样本，当前版本照样续传、只合并与放弃。
//!
//! 样本在 tests/fixtures/workdir/job/<job.json 的格式版本>/ 下，每个版本两个由当时的版本运行生成的任务目录（见
//! [`write_samples`]）：`vod` 为主播放列表（变体与音频按属性认）的视频加独立音频 fMP4 点播，音频最后一个分片取不到而
//! 失败；`live` 为写了稳定 ID 的同样来源的直播，录完各轨列出的分片后中断。发布后不再改动，`make check` 对照发布标签
//! 核对。

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use hs_m3u8_core::{CancellationToken, Error, JobRequest, LiveOptions, Unsettled};
use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use crate::server::Server;
use crate::{assert_output, engine, expected, fixture, request, run, test_dir, track};

const VIDEO: [&str; 2] = ["seg0.m4s", "seg1.m4s"];
const AUDIO: [&str; 3] = ["seg0.m4s", "seg1.m4s", "seg2.m4s"];

/// 各格式版本的样本目录 `<版本>/<name>`。
fn samples(name: &str) -> Vec<PathBuf> {
    let root = samples_root();
    let mut found: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path().join(name))
        .collect();
    found.sort();
    assert!(!found.is_empty(), "{} 下没有样本", root.display());
    for sample in &found {
        assert!(sample.is_dir(), "缺少样本 {}", sample.display());
    }
    found
}

fn samples_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workdir/job")
}

/// 把 `from` 整个复制到 `to`（`to` 不存在），跳过名为 `skip` 的文件。
fn copy_dir(from: &Path, to: &Path, skip: &str) {
    std::fs::create_dir(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target, skip);
        } else if entry.file_name() != skip {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// 视频加独立音频的来源：fmp4_a 的 init 段与分片，媒体播放列表 video.m3u8、audio.m3u8 列出全部分片；`master` 为主播
/// 放列表的变体与音频 rendition 两行额外的属性，`end` 为是否点播（带 EXT-X-ENDLIST）。
fn put_source(server: &Server, master: (&str, &str), end: bool) {
    for (kind, names) in [("video", &VIDEO[..]), ("audio", &AUDIO[..])] {
        let mut playlist =
            format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"{kind}/init.mp4\"\n");
        for name in ["init.mp4"].iter().chain(names) {
            server.put(
                &format!("{kind}/{name}"),
                fixture(&format!("fmp4_a/{kind}/{name}")),
            );
        }
        for name in names {
            playlist += &format!("#EXTINF:1,\n{kind}/{name}\n");
        }
        if end {
            playlist += "#EXT-X-ENDLIST\n";
        }
        server.put(&format!("{kind}.m3u8"), playlist);
    }
    let (variant, audio) = master;
    server.put(
        "master.m3u8",
        format!(
            "#EXTM3U\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"audio.m3u8\"{audio}\n\
             #EXT-X-STREAM-INF:BANDWIDTH=200000,RESOLUTION=320x180,CODECS=\"avc1.64000c,mp4a.40.2\",AUDIO=\"aud\"{variant}\n\
             video.m3u8\n"
        ),
    );
}

const STABLE_IDS: (&str, &str) = (",STABLE-VARIANT-ID=\"v\"", ",STABLE-RENDITION-ID=\"a\"");

fn live(mut req: JobRequest) -> JobRequest {
    req.live = Some(LiveOptions {
        max_duration: None,
        stall_timeout: std::time::Duration::from_secs(2),
    });
    req
}

/// fmp4_a 全部分片合成一组的期望输出。
fn expected_all(dir: &Path) -> Vec<u8> {
    let tracks = vec![
        track("fmp4_a/video", Some("init.mp4"), &VIDEO),
        track("fmp4_a/audio", Some("init.mp4"), &AUDIO),
    ];
    expected(
        dir,
        &[Streams::Video, Streams::Audio],
        &[DiscontinuityGroup { tracks }],
    )
}

/// 点播样本换了地址（本地服务的端口每次不同）续传：核对内容时每条轨重下一个已完成的分片，其余已完成的不重下，补上
/// 缺的音频分片，输出完整。
#[tokio::test(flavor = "multi_thread")]
async fn vod_samples_resume() {
    for sample in samples("vod") {
        let dir = test_dir(&format!("samples_vod_{}", version_of(&sample)));
        copy_dir(&sample, &dir.join("out.hsdl"), "");
        let server = Server::start().await;
        put_source(&server, ("", ""), true);

        let output = run(request(server.url("master.m3u8"), &dir)).await.unwrap();

        assert_output(&output, &expected_all(&dir));
        let hits = |kind: &str, names: &[&str]| -> usize {
            names
                .iter()
                .map(|n| server.hits(&format!("{kind}/{n}")))
                .sum()
        };
        assert_eq!(
            (
                hits("video", &VIDEO),
                hits("audio", &AUDIO[..2]),
                hits("audio", &AUDIO[2..])
            ),
            (1, 1, 1),
            "{}",
            sample.display()
        );
    }
}

/// 直播样本不联网只合并，输出完整。
#[tokio::test(flavor = "multi_thread")]
async fn live_samples_merge() {
    for sample in samples("live") {
        let dir = test_dir(&format!("samples_live_{}", version_of(&sample)));
        copy_dir(&sample, &dir.join("out.hsdl"), "");
        let req = request("http://127.0.0.1:9/master.m3u8".parse().unwrap(), &dir);

        let output = engine()
            .merge_recorded(req.output)
            .unwrap()
            .wait()
            .await
            .unwrap();

        assert_output(&output, &expected_all(&dir));
    }
}

/// 放弃样本：认得其中全是本库写的文件，整个删掉。
#[tokio::test(flavor = "multi_thread")]
async fn samples_are_discarded() {
    for sample in [samples("vod"), samples("live")].concat() {
        let name = sample.file_name().unwrap().to_str().unwrap();
        let dir = test_dir(&format!("samples_discard_{name}_{}", version_of(&sample)));
        let work = dir.join("out.hsdl");
        copy_dir(&sample, &work, "");

        let leftovers = engine()
            .discard(&work, Unsettled::Fail, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(leftovers, [], "{}", sample.display());
        assert!(!work.exists());
    }
}

/// 样本所属的格式版本，即上一级目录名。
fn version_of(sample: &Path) -> &str {
    sample
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
}

/// 用当前版本生成样本，放进 tests/fixtures/workdir/job/<格式版本>/；那个版本的样本已有时失败（发布后不再改动）。升了
/// job.json 的格式版本、发布之前运行一次：
/// `cargo test -p hs-m3u8-core --test e2e -- --ignored --exact samples::write_samples`。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "只在升了格式版本时手动运行"]
async fn write_samples() {
    let dir = test_dir("samples_write");
    let server = Server::start().await;

    put_source(&server, ("", ""), true);
    server.status("audio/seg2.m4s", StatusCode::NOT_FOUND);
    let vod = dir.join("vod");
    let req = request(server.url("master.m3u8"), &vod);
    assert!(matches!(run(req.clone()).await, Err(Error::Segment { .. })));
    save(&req.output.resolved_work_dir().unwrap(), "vod");

    put_source(&server, STABLE_IDS, false);
    let live_dir = dir.join("live");
    let req = live(request(server.url("master.m3u8"), &live_dir));
    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.control().progress();
    progress
        .wait_for(|p| p.segments.done == VIDEO.len() + AUDIO.len())
        .await
        .unwrap();
    job.control().cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    save(&req.output.resolved_work_dir().unwrap(), "live");
}

/// 把任务目录 `work`（锁文件除外）存为它的格式版本的样本 `name`。
fn save(work: &Path, name: &str) {
    let job: serde_json::Value =
        serde_json::from_slice(&std::fs::read(work.join("job.json")).unwrap()).unwrap();
    let version = job["format_version"].as_u64().unwrap();
    let target = samples_root().join(version.to_string()).join(name);
    assert!(!target.exists(), "{} 已存在", target.display());
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    copy_dir(work, &target, "lock");
}
