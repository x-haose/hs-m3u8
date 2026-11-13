//! 任务目录格式的兼容：已发布版本写的任务目录样本，当前版本照样续传、只合并与放弃。
//!
//! 样本在 tests/fixtures/workdir/job/<job.json 的格式版本>/<名字>/ 下：`work/` 为当时的版本运行留下的任务目录，点播另有
//! `source/` 为当时的来源（入口为 `index.m3u8`，连同 init 段与分片），续传时原样提供。各样本由 [`write_samples`] 生成，
//! 名字与覆盖的形态见 [`SAMPLES`]。发布后不再改动，`make compat` 对照发布标签核对。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use hs_m3u8_core::{CancellationToken, Error, JobRequest, Unsettled};

use crate::live::{STALL, judged_in, live_request};
use crate::server::Server;
use crate::{engine, fixture, request, run, test_dir};

/// 每个格式版本的样本：
/// - `vod`：主播放列表（变体与音频按属性认）的视频加独立音频 fMP4 点播，音频最后一个分片取不到而失败
/// - `vod-ts`：媒体播放列表来源（不选轨）、带不连续段的 TS 点播，最后一个分片取不到而失败
/// - `vod-muxed`：主播放列表里音频混在视频里的变体（没有独立音频），TS 点播，最后一个分片取不到而失败
/// - `live`：写了稳定 ID 的视频加独立音频 fMP4 直播，录完各轨列出的分片后中断
/// - `live-sessions`：视频加独立音频 TS 直播，中断后音频窗口已前移、续录另起会话 1（视频跳过会话 0 录过的序号，起点
///   写作 `after<序号>`），再中断
const SAMPLES: [&str; 5] = ["vod", "vod-ts", "vod-muxed", "live", "live-sessions"];

/// 各格式版本的全部样本目录，按版本、名字排列；系统生成的 `.DS_Store` 之类不算。最新的版本须有 [`SAMPLES`] 中的
/// 每一个（core 的单元测试核对最新的版本即当前的格式版本）。
fn samples() -> Vec<PathBuf> {
    let root = samples_root();
    let mut versions: Vec<(u32, PathBuf)> = visible_entries(&root)
        .into_iter()
        .map(|dir| {
            (
                dir.file_name().unwrap().to_str().unwrap().parse().unwrap(),
                dir,
            )
        })
        .collect();
    versions.sort();
    let (_, newest) = versions.last().expect("至少有一个版本的样本");
    for name in SAMPLES {
        assert!(
            newest.join(name).is_dir(),
            "缺少样本 {}",
            newest.join(name).display()
        );
    }
    versions
        .iter()
        .flat_map(|(_, dir)| {
            let mut found = visible_entries(dir);
            found.sort();
            found
        })
        .collect()
}

fn samples_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/workdir/job")
}

/// 目录 `dir` 里名字不以 `.` 开头的项。
fn visible_entries(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| !path.file_name().unwrap().to_str().unwrap().starts_with('.'))
        .collect()
}

/// 样本记录的任务类型（job.json 的 `kind`）。
fn kind_of(sample: &Path) -> String {
    let job: serde_json::Value =
        serde_json::from_slice(&std::fs::read(sample.join("work/job.json")).unwrap()).unwrap();
    job["kind"].as_str().unwrap().to_owned()
}

/// 测试目录名：样本的版本与名字。
fn label(sample: &Path) -> String {
    let version = sample
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    let name = sample.file_name().unwrap().to_str().unwrap();
    format!("{version}_{name}")
}

/// 把 `from` 整个复制到 `to`（`to` 不存在）。
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// 目录 `dir` 下的全部文件，键为以 `/` 分隔的相对路径。
fn files_in(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if entry.file_type().unwrap().is_dir() {
            for (path, body) in files_in(&entry.path()) {
                files.insert(format!("{name}/{path}"), body);
            }
        } else {
            files.insert(name, std::fs::read(entry.path()).unwrap());
        }
    }
    files
}

/// 提供样本的来源 `source` 的本地服务。
async fn serve(source: &Path) -> (Server, BTreeMap<String, Vec<u8>>) {
    let server = Server::start().await;
    let files = files_in(source);
    for (path, body) in &files {
        server.put(path, body.clone());
    }
    (server, files)
}

/// 来源里播放列表之外的文件（init 段与分片）被请求的总次数。
fn media_hits(server: &Server, files: &BTreeMap<String, Vec<u8>>) -> usize {
    files
        .keys()
        .filter(|path| !path.ends_with(".m3u8"))
        .map(|path| server.hits(path))
        .sum()
}

/// 点播样本照当时的来源续传（本地服务的端口每次不同，即换了地址，先核对内容）：输出与同一来源从头下载的逐字节相同，
/// 请求的 init 段与分片比从头下载的少（已完成的除核对内容时重下的之外不重下）。
#[tokio::test(flavor = "multi_thread")]
async fn vod_samples_resume() {
    for sample in samples().into_iter().filter(|s| kind_of(s) == "vod") {
        let source = sample.join("source");
        let fresh_dir = test_dir(&format!("samples_fresh_{}", label(&sample)));
        let (fresh_server, files) = serve(&source).await;
        let fresh = run(request(fresh_server.url("index.m3u8"), &fresh_dir))
            .await
            .unwrap();

        let dir = test_dir(&format!("samples_resume_{}", label(&sample)));
        copy_dir(&sample.join("work"), &dir.join("out.hsdl"));
        let (server, _) = serve(&source).await;
        let output = run(request(server.url("index.m3u8"), &dir)).await.unwrap();

        let mp4 = |output: &hs_m3u8_core::Output| {
            std::fs::read(&output.mp4.as_ref().unwrap().path).unwrap()
        };
        assert!(
            mp4(&output) == mp4(&fresh),
            "{}：输出不同",
            sample.display()
        );
        assert_eq!(output.missed, [], "{}", sample.display());
        let (resumed, from_scratch) = (
            media_hits(&server, &files),
            media_hits(&fresh_server, &files),
        );
        assert!(
            resumed < from_scratch,
            "{}：续传请求 {resumed} 次，从头 {from_scratch} 次",
            sample.display()
        );
    }
}

/// 直播样本不联网只合并：任务目录里的分片都认得、都合并进去，会话数与分片名里的一致。
#[tokio::test(flavor = "multi_thread")]
async fn live_samples_merge() {
    for sample in samples().into_iter().filter(|s| kind_of(s) == "live") {
        let dir = test_dir(&format!("samples_merge_{}", label(&sample)));
        let work = sample.join("work");
        copy_dir(&work, &dir.join("out.hsdl"));
        let names: Vec<String> = files_in(&work.join("tracks"))
            .into_keys()
            .filter(|path| path.ends_with(".seg"))
            .collect();
        let sessions: BTreeSet<&str> = names
            .iter()
            .map(|path| path.rsplit('/').next().unwrap().split('-').next().unwrap())
            .collect();
        let req = request("http://127.0.0.1:9/index.m3u8".parse().unwrap(), &dir);

        let job = engine().merge_recorded(req.output).unwrap();
        let progress = job.control().progress();
        let output = job.wait().await.unwrap();

        assert!(output.mp4.unwrap().path.is_file());
        assert_eq!(
            progress.borrow().segments.done,
            names.len(),
            "{}",
            sample.display()
        );
        assert_eq!(
            output.live.unwrap().session_count,
            sessions.len(),
            "{}",
            sample.display()
        );
    }
}

/// 放弃样本：认得其中全是本库写的文件，整个删掉。
#[tokio::test(flavor = "multi_thread")]
async fn samples_are_discarded() {
    for sample in samples() {
        let dir = test_dir(&format!("samples_discard_{}", label(&sample)));
        let work = dir.join("out.hsdl");
        copy_dir(&sample.join("work"), &work);

        let leftovers = engine()
            .discard(&work, Unsettled::Fail, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(leftovers, [], "{}", sample.display());
        assert!(!work.exists());
    }
}

// ---------- 生成 ----------

/// 用当前版本生成 [`SAMPLES`]，放进 tests/fixtures/workdir/job/<格式版本>/；那个版本的样本已有时失败（发布后不再改动）。
/// 升了 job.json 的格式版本、发布之前运行一次：
/// `cargo test -p hs-m3u8-core --test e2e -- --ignored --exact samples::write_samples`。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "只在升了格式版本时手动运行"]
async fn write_samples() {
    let dir = test_dir("samples_write");

    let mut source = Source::start().await;
    put_fmp4_split(&mut source, "", true);
    failed_vod(&dir, "vod", source, "audio/seg2.m4s").await;

    let mut source = Source::start().await;
    put_long(&mut source, "", 4);
    source.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n\
         #EXT-X-DISCONTINUITY\n#EXTINF:1,\nseg2.ts\n#EXTINF:1,\nseg3.ts\n#EXT-X-ENDLIST\n",
    );
    failed_vod(&dir, "vod-ts", source, "seg3.ts").await;

    let mut source = Source::start().await;
    put_long(&mut source, "v/", 4);
    source.put("v.m3u8", judged_in("v/", &[0, 1, 2, 3], true));
    source.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=150000,RESOLUTION=160x90,CODECS=\"avc1.64000c,mp4a.40.2\"\nv.m3u8\n",
    );
    failed_vod(&dir, "vod-muxed", source, "v/seg3.ts").await;

    let mut source = Source::start().await;
    put_fmp4_split(
        &mut source,
        ",STABLE-VARIANT-ID=\"v\"|,STABLE-RENDITION-ID=\"a\"",
        false,
    );
    let req = live_request(source.server.url("index.m3u8"), &dir.join("live"), STALL);
    interrupt(&req, 5).await;
    save(&req, "live", None);

    let server = Server::start().await;
    server.put("index.m3u8", split_master("", "v.m3u8", "a.m3u8"));
    for kind in ["v/", "a/"] {
        for i in 0..4 {
            server.put(
                &format!("{kind}seg{i}.ts"),
                fixture(&format!("ts_long/seg{i}.ts")),
            );
        }
    }
    let req = live_request(server.url("index.m3u8"), &dir.join("live-sessions"), STALL);
    server.put("v.m3u8", judged_in("v/", &[0, 1], false));
    server.put("a.m3u8", judged_in("a/", &[0, 1], false));
    interrupt(&req, 4).await;
    server.put("v.m3u8", judged_in("v/", &[0, 1, 2], false));
    server.put("a.m3u8", judged_in("a/", &[2, 3], false));
    interrupt(&req, 7).await;
    let work = req.output.resolved_work_dir().unwrap();
    let video = files_in(&work.join("tracks/0"));
    assert!(
        video.keys().any(|name| name.starts_with("1-after1-2-")),
        "{video:?}"
    );
    save(&req, "live-sessions", None);
}

/// 生成样本时的来源：放进本地服务，同时记下，点播样本连同任务目录存下。
struct Source {
    server: Server,
    files: BTreeMap<String, Vec<u8>>,
}

impl Source {
    async fn start() -> Self {
        Source {
            server: Server::start().await,
            files: BTreeMap::new(),
        }
    }

    fn put(&mut self, path: &str, body: impl Into<Vec<u8>>) {
        let body = body.into();
        self.server.put(path, body.clone());
        self.files.insert(path.to_owned(), body);
    }
}

/// 视频加独立音频的来源：fmp4_a 的 init 段与分片，入口 index.m3u8 为主播放列表，video.m3u8、audio.m3u8 列出全部分片；
/// `stable_ids` 见 [`split_master`] 的 `extra`；`end` 为是否点播。
fn put_fmp4_split(source: &mut Source, stable_ids: &str, end: bool) {
    const SEGMENTS: [(&str, usize); 2] = [("video", 2), ("audio", 3)];
    for (kind, count) in SEGMENTS {
        source.put(
            &format!("{kind}/init.mp4"),
            fixture(&format!("fmp4_a/{kind}/init.mp4")),
        );
        let mut playlist =
            format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"{kind}/init.mp4\"\n");
        for i in 0..count {
            let name = format!("{kind}/seg{i}.m4s");
            source.put(&name, fixture(&format!("fmp4_a/{name}")));
            playlist += &format!("#EXTINF:1,\n{name}\n");
        }
        if end {
            playlist += "#EXT-X-ENDLIST\n";
        }
        source.put(&format!("{kind}.m3u8"), playlist);
    }
    source.put(
        "index.m3u8",
        split_master(stable_ids, "video.m3u8", "audio.m3u8"),
    );
}

/// 一个视频变体加一个独立音频 rendition 的主播放列表；`extra` 为两行额外的属性，以 `|` 分开。
fn split_master(extra: &str, video: &str, audio: &str) -> String {
    let (variant, rendition) = extra.split_once('|').unwrap_or_default();
    format!(
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"{audio}\"{rendition}\n\
         #EXT-X-STREAM-INF:BANDWIDTH=200000,RESOLUTION=320x180,CODECS=\"avc1.64000c,mp4a.40.2\",AUDIO=\"aud\"{variant}\n\
         {video}\n"
    )
}

/// ts_long 的前 `count` 个分片，路径 `<dir>seg<i>.ts`。
fn put_long(source: &mut Source, dir: &str, count: u64) {
    for i in 0..count {
        source.put(
            &format!("{dir}seg{i}.ts"),
            fixture(&format!("ts_long/seg{i}.ts")),
        );
    }
}

/// 点播：`missing` 取不到，任务失败、留下任务目录，连同完整的来源（续传时 `missing` 取得到）存为样本 `name`。
async fn failed_vod(dir: &Path, name: &str, source: Source, missing: &str) {
    source.server.status(missing, StatusCode::NOT_FOUND);
    let req = request(source.server.url("index.m3u8"), &dir.join(name));
    let failed = run(req.clone()).await;
    assert!(matches!(failed, Err(Error::Segment { .. })), "{failed:?}");
    save(&req, name, Some(&source.files));
}

/// 运行到已完成 `done` 个分片后取消，任务目录保留。
async fn interrupt(req: &JobRequest, done: usize) {
    let job = engine().start(req.clone()).unwrap();
    let mut progress = job.control().progress();
    progress
        .wait_for(|p| p.segments.done == done)
        .await
        .unwrap();
    job.control().cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
}

/// 把 `req` 的任务目录（锁文件除外）存为它的格式版本的样本 `name`，有 `source` 时存进 `source/`。
fn save(req: &JobRequest, name: &str, source: Option<&BTreeMap<String, Vec<u8>>>) {
    let work = req.output.resolved_work_dir().unwrap();
    let job: serde_json::Value =
        serde_json::from_slice(&std::fs::read(work.join("job.json")).unwrap()).unwrap();
    let version = job["format_version"].as_u64().unwrap();
    let target = samples_root().join(version.to_string()).join(name);
    assert!(!target.exists(), "{} 已存在", target.display());
    std::fs::create_dir_all(&target).unwrap();
    copy_dir(&work, &target.join("work"));
    std::fs::remove_file(target.join("work/lock")).unwrap();
    for (path, body) in source.into_iter().flatten() {
        let file = target.join("source").join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, body).unwrap();
    }
}
