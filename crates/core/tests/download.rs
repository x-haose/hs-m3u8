//! core 的端到端测试：测试内起本地 HTTP 服务提供 HLS（分片取自 tests/fixtures/media），跑完整任务。
//! 输出与直接用 remux 合并同一批样本文件的结果逐字节比较，解密、顺序或分组的任何错误都会暴露。

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockModeEncrypt, KeyIvInit};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use hs_m3u8_core::{
    Engine, Error, Hooks, HttpError, Integrity, JobRequest, Output, RequestParts, RetryPolicy,
    Stage, Unsupported, Url,
};
use hs_m3u8_remux::{DiscontinuityGroup, Streams, TrackSegments, remux};
use tokio::sync::Notify;

// ---------- 样本 ----------

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media")
}

fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(path)).unwrap()
}

/// 样本目录中的一条轨：`init` 为 init 段文件名，`segments` 为分片文件名。
fn track(dir: &str, init: Option<&str>, segments: &[&str]) -> TrackSegments {
    let dir = fixtures().join(dir);
    TrackSegments {
        init: init.map(|i| dir.join(i)),
        segments: segments.iter().map(|s| dir.join(s)).collect(),
    }
}

/// 直接合并样本文件得到的期望输出。
fn expected(dir: &Path, streams: &[Streams], groups: &[DiscontinuityGroup]) -> Vec<u8> {
    let path = dir.join("expected.mp4");
    remux(streams, groups, &path).unwrap();
    std::fs::read(path).unwrap()
}

/// ts_a 的分片（H.264 + AAC 的 TS）。
const TS_A: [&str; 2] = ["seg0.ts", "seg1.ts"];

/// ts_a 两个分片单轨合并的期望输出。
fn expected_ts_a(dir: &Path) -> Vec<u8> {
    let tracks = vec![track("ts_a", None, &TS_A)];
    expected(dir, &[Streams::ALL], &[DiscontinuityGroup { tracks }])
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

/// 每个测试独立的空目录。
fn test_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("download")
        .join(test);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------- 测试服务 ----------

enum Entry {
    Body(Vec<u8>),
    Redirect(String),
}

/// 收到请求后先通知测试，再等测试放行。
#[derive(Default)]
struct Gate {
    arrived: Notify,
    release: Notify,
}

#[derive(Default)]
struct ServerState {
    entries: Mutex<HashMap<String, Entry>>,
    hits: Mutex<HashMap<String, usize>>,
    /// 路径 → 还要返回 500 的次数
    failures: Mutex<HashMap<String, usize>>,
    gates: Mutex<HashMap<String, Arc<Gate>>>,
    /// 所有请求都必须带的请求头
    required_header: Mutex<Option<(String, String)>>,
}

struct Server {
    base: Url,
    state: Arc<ServerState>,
}

impl Server {
    async fn start() -> Server {
        let state = Arc::new(ServerState::default());
        let app = axum::Router::new()
            .fallback(serve)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server { base, state }
    }

    fn url(&self, path: &str) -> Url {
        self.base.join(path).unwrap()
    }

    fn put(&self, path: &str, body: impl Into<Vec<u8>>) {
        let entry = Entry::Body(body.into());
        self.state
            .entries
            .lock()
            .unwrap()
            .insert(path.into(), entry);
    }

    fn redirect(&self, from: &str, to: &str) {
        let entry = Entry::Redirect(to.into());
        self.state
            .entries
            .lock()
            .unwrap()
            .insert(from.into(), entry);
    }

    fn fail(&self, path: &str, times: usize) {
        self.state
            .failures
            .lock()
            .unwrap()
            .insert(path.into(), times);
    }

    fn gate(&self, path: &str) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        self.state
            .gates
            .lock()
            .unwrap()
            .insert(path.into(), gate.clone());
        gate
    }

    fn ungate(&self, path: &str) {
        let gate = self.state.gates.lock().unwrap().remove(path).unwrap();
        gate.release.notify_one();
    }

    fn require_header(&self, name: &str, value: &str) {
        *self.state.required_header.lock().unwrap() = Some((name.into(), value.into()));
    }

    fn hits(&self, path: &str) -> usize {
        self.state
            .hits
            .lock()
            .unwrap()
            .get(path)
            .copied()
            .unwrap_or(0)
    }
}

async fn serve(State(state): State<Arc<ServerState>>, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/').to_owned();
    *state.hits.lock().unwrap().entry(path.clone()).or_default() += 1;

    if let Some((name, value)) = state.required_header.lock().unwrap().clone()
        && headers.get(&name).and_then(|v| v.to_str().ok()) != Some(&value)
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let gate = state.gates.lock().unwrap().get(&path).cloned();
    if let Some(gate) = gate {
        gate.arrived.notify_one();
        gate.release.notified().await;
    }
    if let Some(left) = state.failures.lock().unwrap().get_mut(&path)
        && *left > 0
    {
        *left -= 1;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let body = match state.entries.lock().unwrap().get(&path) {
        None => return StatusCode::NOT_FOUND.into_response(),
        Some(Entry::Redirect(to)) => {
            return (StatusCode::FOUND, [(header::LOCATION, format!("/{to}"))]).into_response();
        }
        Some(Entry::Body(body)) => body.clone(),
    };
    match headers.get(header::RANGE).and_then(|r| r.to_str().ok()) {
        None => body.into_response(),
        Some(range) => {
            let (start, end) = range
                .strip_prefix("bytes=")
                .and_then(|r| r.split_once('-'))
                .unwrap();
            let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
            (StatusCode::PARTIAL_CONTENT, body[start..=end].to_vec()).into_response()
        }
    }
}

// ---------- 任务 ----------

fn request(url: Url, dir: &Path) -> JobRequest {
    let mut request = JobRequest::new(url, dir.join("out.mp4"));
    request.retry = RetryPolicy {
        attempts: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    request.concurrency = NonZeroUsize::new(1).unwrap();
    request
}

fn engine() -> Engine {
    Engine::new(NonZeroUsize::new(8).unwrap())
}

async fn run(request: JobRequest) -> Result<Output, Error> {
    engine().start(request)?.wait().await
}

/// 成功的任务：输出与期望逐字节相同，任务目录已删除。
fn assert_output(output: &Output, expected: &[u8]) {
    assert_eq!(std::fs::read(&output.path).unwrap(), expected);
    assert_eq!(output.cleanup_error, None);
    let mut work_dir = output.path.clone().into_os_string();
    work_dir.push(".hsdl");
    assert!(!Path::new(&work_dir).exists());
}

/// ts_a 的两个分片，第一个用显式 IV、第二个换了 key 并用媒体序号推出的 IV。
#[tokio::test(flavor = "multi_thread")]
async fn aes128_explicit_and_sequence_iv() {
    let dir = test_dir("aes128");
    let server = Server::start().await;
    let (key0, key1) = ([0x11; 16], [0x22; 16]);
    let iv0 = [0x33; 16];
    // MEDIA-SEQUENCE 为 7，第二个分片序号 8
    let iv1 = 8u128.to_be_bytes();
    server.put("k0", key0);
    server.put("k1", key1);
    server.put("a.ts", encrypt(&fixture("ts_a/seg0.ts"), &key0, &iv0));
    server.put("b.ts", encrypt(&fixture("ts_a/seg1.ts"), &key1, &iv1));
    server.put(
        "index.m3u8",
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:7\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"k0\",IV=0x{}\n#EXTINF:1,\na.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"k1\"\n#EXTINF:1,\nb.ts\n#EXT-X-ENDLIST\n",
            hex(&iv0)
        ),
    );

    let mut req = request(server.url("index.m3u8"), &dir);
    req.concurrency = NonZeroUsize::new(4).unwrap();
    let job = engine().start(req).unwrap();
    let progress = job.progress();
    let output = job.wait().await.unwrap();

    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
    assert_eq!(output.segments, 2);
    let last = *progress.borrow();
    assert_eq!(
        (last.stage, last.segments_done, last.segments_total),
        (Stage::Done, 2, 2)
    );
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

    let output = run(request(server.url("watch"), &dir)).await.unwrap();

    let program = |name: &str, video: &[&str], audio: &[&str]| DiscontinuityGroup {
        tracks: vec![
            track(&format!("{name}/video"), Some("init.mp4"), video),
            track(&format!("{name}/audio"), Some("init.mp4"), audio),
        ],
    };
    let want = expected(
        &dir,
        &[Streams::VIDEO, Streams::AUDIO],
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
        &[Streams::VIDEO, Streams::AUDIO],
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
        &[Streams::ALL],
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
    server.state.entries.lock().unwrap().remove("seg1.ts");
    let req = request(server.url("index.m3u8"), &dir);

    let err = run(req.clone()).await.unwrap_err();
    match err {
        Error::Segment {
            sequence: 1,
            source,
            ..
        } => assert!(
            matches!(
                *source,
                Error::Http {
                    kind: HttpError::Status(404),
                    ..
                }
            ),
            "{source}"
        ),
        other => panic!("{other}"),
    }
    assert!(!req.output.exists());
    assert!(dir.join("out.mp4.hsdl/job.json").exists());

    put_ts_a(&server, "index.m3u8", 0.5);
    assert!(matches!(run(req.clone()).await, Err(Error::PlanChanged)));

    put_ts_a(&server, "index.m3u8", 1.0);
    let output = run(req).await.unwrap();
    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
    assert_eq!(server.hits("seg0.ts"), 1);
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
        matches!(&second, Error::WorkDir { reason, .. } if reason.contains("另一个任务")),
        "{second}"
    );
    job.cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
    assert!(!req.output.exists());

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
        Error::Segment { source, .. } => assert!(
            matches!(
                *source,
                Error::Integrity {
                    kind: Integrity::Padding | Integrity::NotTs(_),
                    ..
                }
            ),
            "{source}"
        ),
        other => panic!("{other}"),
    }
    assert!(!dir.join("out.mp4.hsdl/tracks/0/0.seg").exists());
}

/// 站点适配：改写播放列表、给每个请求签名、解开变换过的 key、去掉分片前的伪装字节。
struct SiteHooks;

const DISGUISE: &[u8] = b"\x89PNG\r\n\x1a\n";

impl Hooks for SiteHooks {
    fn on_playlist(&self, _url: &Url, text: String) -> Result<String, String> {
        Ok(text.replace("{segment}", "seg"))
    }

    fn on_request(&self, request: &mut RequestParts) -> Result<(), String> {
        request.headers.push(("x-token".into(), "secret".into()));
        Ok(())
    }

    fn on_key(&self, _url: &Url, data: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(data.iter().map(|b| b ^ 0xFF).collect())
    }

    fn on_segment(&self, _url: &Url, data: Vec<u8>) -> Result<Vec<u8>, String> {
        data.strip_prefix(DISGUISE)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| "缺少伪装前缀".to_owned())
    }
}

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
    req.hooks = Arc::new(SiteHooks);
    let output = run(req).await.unwrap();

    let want = expected_ts_a(&dir);
    assert_output(&output, &want);
}

/// 下载前就能判定的失败：输出已存在、直播、各轨不连续段不一致。
#[tokio::test(flavor = "multi_thread")]
async fn rejected_before_download() {
    let dir = test_dir("rejected");
    let server = Server::start().await;

    let req = request(server.url("index.m3u8"), &dir);
    std::fs::write(&req.output, b"").unwrap();
    assert!(matches!(
        engine().start(req.clone()),
        Err(Error::OutputExists(_))
    ));
    std::fs::remove_file(&req.output).unwrap();

    server.put(
        "live.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n",
    );
    let err = run(request(server.url("live.m3u8"), &dir))
        .await
        .unwrap_err();
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
    assert!(!dir.join("out.mp4.hsdl").exists());
    assert_eq!(server.hits("v0.ts") + server.hits("a0.aac"), 0);
}
