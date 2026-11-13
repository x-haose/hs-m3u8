//! 任务服务的端到端测试：真实的 SQLite 文件、测试内的本地 HLS 服务（分片取自 tests/fixtures/media/ts_a）。
//! 服务自带 tokio 运行时，在测试自己的运行时外面建、外面丢弃；等事件与本地服务在测试的运行时里。

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{StatusCode, Uri};
use hs_m3u8_app::{Event, OutputSpec, Service, Status, Store, StoredPreference, TaskId, TaskSpec};
use hs_m3u8_core::Url;
use hs_m3u8_core::hls::Preference;
use tokio::runtime::Runtime;
use tokio::sync::{Notify, mpsc};

const PLAYLIST: &str =
    "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n";

/// 本地 HLS 服务：index.m3u8 与 ts_a 的两个分片；`gate` 打开前 seg1.ts 的请求挂着不回。
struct Server {
    base: Url,
    gate: Arc<Gate>,
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Notify,
}

impl Gate {
    async fn pass(&self) {
        loop {
            let opened = self.opened.notified();
            if *self.open.lock().unwrap() {
                return;
            }
            opened.await;
        }
    }

    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_waiters();
    }
}

#[derive(Clone)]
struct Files {
    bodies: Arc<HashMap<String, Vec<u8>>>,
    gate: Arc<Gate>,
}

async fn serve(State(files): State<Files>, uri: Uri) -> Result<Vec<u8>, StatusCode> {
    let path = uri.path().trim_start_matches('/');
    if path == "seg1.ts" {
        files.gate.pass().await;
    }
    files.bodies.get(path).cloned().ok_or(StatusCode::NOT_FOUND)
}

impl Server {
    async fn start(gate_open: bool) -> Server {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media/ts_a");
        let bodies = HashMap::from([
            ("index.m3u8".to_owned(), PLAYLIST.as_bytes().to_vec()),
            (
                "seg0.ts".to_owned(),
                std::fs::read(fixtures.join("seg0.ts")).unwrap(),
            ),
            (
                "seg1.ts".to_owned(),
                std::fs::read(fixtures.join("seg1.ts")).unwrap(),
            ),
        ]);
        let gate = Arc::new(Gate::default());
        if gate_open {
            gate.open();
        }
        let files = Files {
            bodies: Arc::new(bodies),
            gate: gate.clone(),
        };
        let app = axum::Router::new().fallback(serve).with_state(files);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server { base, gate }
    }
}

/// 每个测试独立的空目录。
fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("app")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spec(url: Url, mp4: PathBuf) -> TaskSpec {
    TaskSpec {
        title: "测试".into(),
        url,
        preference: StoredPreference::try_from(&Preference::default()).unwrap(),
        headers: Vec::new(),
        output: OutputSpec::Mp4(mp4),
        live: false,
    }
}

fn service(store: &Path) -> (Service, mpsc::UnboundedReceiver<Event>) {
    Service::new(Store::open(store).unwrap(), NonZeroUsize::new(8).unwrap()).unwrap()
}

/// 等到任务 `id` 停下，返回它的状态；中途的进度事件交给 `on_progress`。
async fn ended(
    events: &mut mpsc::UnboundedReceiver<Event>,
    id: TaskId,
    mut on_progress: impl FnMut(&hs_m3u8_core::Progress),
) -> Status {
    loop {
        match events.recv().await.expect("服务还在，事件通道不会关闭") {
            Event::Progress { id: at, progress } if at == id => on_progress(&progress),
            Event::Ended { id: at, status, .. } if at == id => return status,
            Event::StoreFailed { error, .. } => panic!("状态没写进存储：{error}"),
            _ => {}
        }
    }
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// 新建即开始，下完写出 MP4、删掉任务目录，存储里记为已完成。
#[test]
fn a_task_downloads_and_is_recorded_as_done() {
    let dir = test_dir("done");
    let rt = runtime();
    let server = rt.block_on(Server::start(true));
    let (service, mut events) = service(&dir.join("tasks.sqlite"));
    let mp4 = dir.join("out.mp4");

    let id = service
        .create(
            spec(server.base.join("index.m3u8").unwrap(), mp4.clone()),
            1,
        )
        .unwrap();
    let status = rt.block_on(ended(&mut events, id, |_| {}));

    assert_eq!(status, Status::Done);
    assert!(mp4.is_file());
    assert!(!dir.join("out.hsdl").exists());
    assert_eq!(service.tasks().unwrap()[0].status, Status::Done);
}

/// 第一个分片下完后暂停：停下为暂停、任务目录保留；关掉应用重开后继续，只补下没完成的分片。
#[test]
fn a_paused_task_resumes_after_reopening() {
    let dir = test_dir("pause");
    let store = dir.join("tasks.sqlite");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let mp4 = dir.join("out.mp4");
    let id = {
        let (service, mut events) = service(&store);
        let id = service
            .create(
                spec(server.base.join("index.m3u8").unwrap(), mp4.clone()),
                1,
            )
            .unwrap();
        let mut paused = false;
        let status = rt.block_on(ended(&mut events, id, |progress| {
            if progress.segments.done == 1 && !paused {
                service.pause(id);
                paused = true;
            }
        }));
        assert_eq!(status, Status::Paused);
        assert!(dir.join("out.hsdl").is_dir());
        id
    };

    server.gate.open();
    let (service, mut events) = service(&store);
    assert_eq!(service.tasks().unwrap()[0].status, Status::Paused);
    service.resume(id).unwrap();
    let status = rt.block_on(ended(&mut events, id, |_| {}));

    assert_eq!(status, Status::Done);
    assert!(mp4.is_file());
}

/// 运行中放弃：先停下，再删掉任务目录与存储里的记录。
#[test]
fn discarding_a_running_task_removes_it() {
    let dir = test_dir("discard");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let (service, mut events) = service(&dir.join("tasks.sqlite"));
    let id = service
        .create(
            spec(server.base.join("index.m3u8").unwrap(), dir.join("out.mp4")),
            1,
        )
        .unwrap();
    rt.block_on(async {
        loop {
            if let Some(Event::Progress { progress, .. }) = events.recv().await
                && progress.segments.done == 1
            {
                break;
            }
        }
    });

    let leftovers = rt.block_on(service.discard(id)).unwrap().unwrap();

    assert_eq!(leftovers, []);
    assert!(!dir.join("out.hsdl").exists());
    assert!(service.tasks().unwrap().is_empty());
}
