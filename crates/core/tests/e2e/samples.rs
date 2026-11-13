//! 任务目录格式的兼容：已发布版本写的任务目录样本，当前版本照样续传、只合并与放弃。
//!
//! 样本在 tests/fixtures/workdir/job/<job.json 的格式版本>/<名字>/ 下：`work/` 为当时的版本运行留下的任务目录（vod 另加的
//! `.part` 除外），`source/` 为接着它续传、续录时的来源（入口为 `index.m3u8`，连同 init 段与分片；直播的播放列表已结束），
//! 原样提供。各样本由 [`write_samples`] 生成，名字与覆盖的形态见 [`SAMPLES`]。发布后不再改动，`make compat` 对照发布标签
//! 核对；//! 其中的文件名都认得、覆盖了文件名的各种写法由 core 的单元测试核对。

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use hs_m3u8_core::{CancellationToken, Error, JobRequest, Unsettled};

use crate::live::{STALL, live_request};
use crate::server::Server;
use crate::{engine, fixture, request, run, test_dir};

/// 每个格式版本的样本：
/// - `vod`：主播放列表（变体与音频按属性认）的视频加独立音频 fMP4 点播，音频最后一个分片取不到而失败；另有进程在写
///   job.json 与一个分片时被杀会留下的 `.part`，按本库写文件的方式另加（见 [`leave_partial_files`]）
/// - `vod-ts`：媒体播放列表来源（不选轨）、带不连续段的 TS 点播，最后一个分片取不到而失败
/// - `vod-muxed`：主播放列表里音频混在视频里的变体（没有独立音频），TS 点播，最后一个分片取不到而失败
/// - `live`：写了稳定 ID 的视频加独立音频 fMP4 直播，录完各轨列出的分片后中断
/// - `live-sessions`：视频加独立音频 TS 直播，会话 0 里有不连续段；中断后音频窗口已前移、续录另起会话 1（视频跳过会话
///   0 录过的序号，起点写作 `after<序号>`），再中断。续录的来源里视频多出一个没录过的分片（见 [`NEW_ON_CONTINUE`]）
const SAMPLES: [&str; 5] = ["vod", "vod-ts", "vod-muxed", "live", "live-sessions"];

/// 直播样本续录的来源里没录过的分片数：续录时都要录进来。
const NEW_ON_CONTINUE: [(&str, usize); 2] = [("live", 0), ("live-sessions", 1)];

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

/// 目录 `dir` 里名字不以 `.` 开头的项（`.DS_Store` 之类不算）。
fn visible_entries(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| !path.file_name().unwrap().to_str().unwrap().starts_with('.'))
        .collect()
}

/// 直播样本里分片的文件名（以 `/` 分隔、相对 `tracks/` 的路径）与其中的会话编号。
fn recorded(sample: &Path) -> (Vec<String>, BTreeSet<String>) {
    let names: Vec<String> = files_in(&sample.join("work/tracks"))
        .into_keys()
        .filter(|path| path.ends_with(".seg"))
        .collect();
    let sessions = names
        .iter()
        .map(|path| {
            path.rsplit('/')
                .next()
                .unwrap()
                .split('-')
                .next()
                .unwrap()
                .to_owned()
        })
        .collect();
    (names, sessions)
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

/// 把 `from` 整个复制到 `to`（`to` 不存在），名字以 `.` 开头的项除外。
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir(to).unwrap();
    for path in visible_entries(from) {
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, target).unwrap();
        }
    }
}

/// 目录 `dir` 下的全部文件（名字以 `.` 开头的项除外），键为以 `/` 分隔的相对路径。
fn files_in(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for path in visible_entries(dir) {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        if path.is_dir() {
            for (inner, body) in files_in(&path) {
                files.insert(format!("{name}/{inner}"), body);
            }
        } else {
            files.insert(name, std::fs::read(&path).unwrap());
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
        copy_dir(&sample.join("work"), &dir.join("out.hsdl"));
        let (names, sessions) = recorded(&sample);
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

/// 直播样本接着录（本地服务的端口每次不同，即换了地址，核对过录到的才接着录）：接着各轨最近的会话录、不另起会话，
/// 来源里没录过的分片（见 [`NEW_ON_CONTINUE`]）都录进来，已录的都合并进去。
#[tokio::test(flavor = "multi_thread")]
async fn live_samples_continue() {
    for sample in samples().into_iter().filter(|s| kind_of(s) == "live") {
        let dir = test_dir(&format!("samples_continue_{}", label(&sample)));
        copy_dir(&sample.join("work"), &dir.join("out.hsdl"));
        let (names, sessions) = recorded(&sample);
        let (server, _) = serve(&sample.join("source")).await;
        let name = sample.file_name().unwrap().to_str().unwrap();
        let (_, new) = NEW_ON_CONTINUE.iter().find(|(n, _)| *n == name).unwrap();

        let job = engine()
            .start(live_request(server.url("index.m3u8"), &dir, STALL))
            .unwrap();
        let progress = job.control().progress();
        let output = job.wait().await.unwrap();

        assert_eq!(
            (progress.borrow().segments.done, output.missed),
            (names.len() + new, vec![]),
            "{}",
            sample.display()
        );
        let live = output.live.unwrap();
        assert_eq!(
            (live.session_count, live.new_sessions),
            (sessions.len(), vec![]),
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

    let mut source = SampleSource::start().await;
    put_fmp4_split(&mut source, ("", ""), true);
    let req = failed_vod(&dir, &source, "audio/seg2.m4s").await;
    leave_partial_files(&req.output.resolved_work_dir().unwrap());
    save(&req, "vod", &source.files);

    let mut source = SampleSource::start().await;
    put_long(&mut source, "", 4);
    source.put("index.m3u8", ts_playlist("", 0..4, 0, &[2], true));
    let req = failed_vod(&dir, &source, "seg3.ts").await;
    save(&req, "vod-ts", &source.files);

    let mut source = SampleSource::start().await;
    put_long(&mut source, "v/", 4);
    source.put("v.m3u8", ts_playlist("v/", 0..4, 0, &[], true));
    source.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=150000,RESOLUTION=160x90,CODECS=\"avc1.64000c,mp4a.40.2\"\nv.m3u8\n",
    );
    let req = failed_vod(&dir, &source, "v/seg3.ts").await;
    save(&req, "vod-muxed", &source.files);

    let mut source = SampleSource::start().await;
    let stable_ids = (",STABLE-VARIANT-ID=\"v\"", ",STABLE-RENDITION-ID=\"a\"");
    put_fmp4_split(&mut source, stable_ids, false);
    let req = live_request(source.server.url("index.m3u8"), &dir.join("live"), STALL);
    interrupt(&req, 5).await;
    put_fmp4_split(&mut source, stable_ids, true);
    save(&req, "live", &source.files);

    let mut source = SampleSource::start().await;
    source.put("index.m3u8", split_master(("", ""), "v.m3u8", "a.m3u8"));
    put_long(&mut source, "v/", 4);
    put_long(&mut source, "a/", 4);
    let req = live_request(
        source.server.url("index.m3u8"),
        &dir.join("live-sessions"),
        STALL,
    );
    source.put("v.m3u8", ts_playlist("v/", 0..2, 0, &[1], false));
    source.put("a.m3u8", ts_playlist("a/", 0..2, 0, &[1], false));
    interrupt(&req, 4).await;
    source.put("v.m3u8", ts_playlist("v/", 0..3, 0, &[1], false));
    source.put("a.m3u8", ts_playlist("a/", 2..4, 1, &[], false));
    interrupt(&req, 7).await;
    let video = files_in(&req.output.resolved_work_dir().unwrap().join("tracks/0"));
    let parts = |name: &String| name.split('-').map(str::to_owned).collect::<Vec<_>>();
    assert!(
        video.keys().any(|n| parts(n)[..3] == ["1", "after1", "2"]),
        "{video:?}"
    );
    assert!(video.keys().any(|n| parts(n)[3] != "0"), "{video:?}");
    source.put("v.m3u8", ts_playlist("v/", 0..4, 0, &[1], true));
    source.put("a.m3u8", ts_playlist("a/", 2..4, 1, &[], true));
    save(&req, "live-sessions", &source.files);
}

/// 生成样本时的来源：放进本地服务，同时记下各路径最后放进的内容，连同任务目录存下。
struct SampleSource {
    server: Server,
    files: BTreeMap<String, Vec<u8>>,
}

impl SampleSource {
    async fn start() -> Self {
        SampleSource {
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
/// `extra` 见 [`split_master`]；`end` 为是否点播。
fn put_fmp4_split(source: &mut SampleSource, extra: (&str, &str), end: bool) {
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
        split_master(extra, "video.m3u8", "audio.m3u8"),
    );
}

/// 一个视频变体加一个独立音频 rendition 的主播放列表；`extra` 为变体与 rendition 两行额外的属性。
fn split_master(extra: (&str, &str), video: &str, audio: &str) -> String {
    let (variant, rendition) = extra;
    format!(
        "#EXTM3U\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"{audio}\"{rendition}\n\
         #EXT-X-STREAM-INF:BANDWIDTH=200000,RESOLUTION=320x180,CODECS=\"avc1.64000c,mp4a.40.2\",AUDIO=\"aud\"{variant}\n\
         {video}\n"
    )
}

/// ts_long 的前 `count` 个分片，路径 `<dir>seg<i>.ts`。
fn put_long(source: &mut SampleSource, dir: &str, count: u64) {
    for i in 0..count {
        source.put(
            &format!("{dir}seg{i}.ts"),
            fixture(&format!("ts_long/seg{i}.ts")),
        );
    }
}

/// ts_long 的分片组成的 TS 媒体播放列表，每片 1 秒：分片路径 `<dir>seg<i>.ts`，媒体序号即 `sequences` 里的 i；
/// `discontinuity_sequence` 为第一个分片所在的不连续段编号，`breaks` 里的分片前面加 EXT-X-DISCONTINUITY；`end` 为是否
/// 已结束。
fn ts_playlist(
    dir: &str,
    sequences: Range<u64>,
    discontinuity_sequence: u64,
    breaks: &[u64],
    end: bool,
) -> String {
    let mut text = format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{}\n\
         #EXT-X-DISCONTINUITY-SEQUENCE:{discontinuity_sequence}\n",
        sequences.start
    );
    for i in sequences {
        if breaks.contains(&i) {
            text += "#EXT-X-DISCONTINUITY\n";
        }
        text += &format!("#EXTINF:1,\n{dir}seg{i}.ts\n");
    }
    if end {
        text += "#EXT-X-ENDLIST\n";
    }
    text
}

/// 点播：`missing` 取不到，任务失败、留下任务目录（在 `dir` 下另建一个目录），返回它的请求；存样本时用完整的来源
/// （续传时 `missing` 取得到）。
async fn failed_vod(dir: &Path, source: &SampleSource, missing: &str) -> JobRequest {
    source.server.status(missing, StatusCode::NOT_FOUND);
    let req = request(
        source.server.url("index.m3u8"),
        &dir.join(missing.replace('/', "_")),
    );
    let failed = run(req.clone()).await;
    assert!(matches!(failed, Err(Error::Segment { .. })), "{failed:?}");
    req
}

/// 在任务目录 `work` 里放下进程在写文件时被杀会留下的 `.part`：job.json 的与第 0 条轨第一个分片的，各写了前一半。
fn leave_partial_files(work: &Path) {
    let job = work.join("job.json");
    let segment = files_in(&work.join("tracks/0"))
        .into_keys()
        .find(|name| name.ends_with(".seg"))
        .unwrap();
    for path in [job, work.join("tracks/0").join(segment)] {
        let data = std::fs::read(&path).unwrap();
        let mut partial = path.into_os_string();
        partial.push(".part");
        std::fs::write(partial, &data[..data.len() / 2]).unwrap();
    }
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

/// 把 `req` 的任务目录（锁文件除外）与来源 `source` 存为它的格式版本的样本 `name`。
fn save(req: &JobRequest, name: &str, source: &BTreeMap<String, Vec<u8>>) {
    let work = req.output.resolved_work_dir().unwrap();
    let job: serde_json::Value =
        serde_json::from_slice(&std::fs::read(work.join("job.json")).unwrap()).unwrap();
    let version = job["format_version"].as_u64().unwrap();
    let target = samples_root().join(version.to_string()).join(name);
    assert!(!target.exists(), "{} 已存在", target.display());
    std::fs::create_dir_all(&target).unwrap();
    copy_dir(&work, &target.join("work"));
    std::fs::remove_file(target.join("work/lock")).unwrap();
    for (path, body) in source {
        let file = target.join("source").join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, body).unwrap();
    }
}
