//! 测试用 HTTP 服务：按路径返回内容，可注入 500、指定状态码、404、重定向、按请求次数变化的内容与阻塞点。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use hs_m3u8_core::Url;
use tokio::sync::Notify;

enum Entry {
    Body(Vec<u8>),
    /// 放入后的第 n 次请求返回第 n 个，之后一直返回最后一个
    Sequence(Vec<Vec<u8>>),
    /// 放入后的第 n 次请求重定向到第 n 个地址，之后一直是最后一个
    Redirect(Vec<String>),
    Status(StatusCode),
}

/// 收到请求后先通知测试，再等测试放行。
#[derive(Default)]
pub(crate) struct Gate {
    pub arrived: Notify,
    pub release: Notify,
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

pub(crate) struct Server {
    base: Url,
    state: Arc<ServerState>,
}

impl Server {
    pub(crate) async fn start() -> Server {
        let state = Arc::new(ServerState::default());
        let app = axum::Router::new()
            .fallback(serve)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server { base, state }
    }

    pub(crate) fn url(&self, path: &str) -> Url {
        self.base.join(path).unwrap()
    }

    pub(crate) fn put(&self, path: &str, body: impl Into<Vec<u8>>) {
        let entry = Entry::Body(body.into());
        self.state
            .entries
            .lock()
            .unwrap()
            .insert(path.into(), entry);
    }

    /// 依次返回 `bodies`：放入后的第 n 次请求得到第 n 个，之后一直是最后一个。
    pub(crate) fn put_sequence(&self, path: &str, bodies: Vec<String>) {
        let entry = Entry::Sequence(bodies.into_iter().map(String::into_bytes).collect());
        self.insert_counted(path, entry);
    }

    /// 之后请求该路径返回 404。
    pub(crate) fn remove(&self, path: &str) {
        self.state.entries.lock().unwrap().remove(path);
    }

    pub(crate) fn redirect(&self, from: &str, to: &str) {
        self.redirect_sequence(from, vec![to.into()]);
    }

    /// 放入后第 n 次请求 `from` 重定向到 `targets` 的第 n 个，之后一直是最后一个。
    pub(crate) fn redirect_sequence(&self, from: &str, targets: Vec<String>) {
        self.insert_counted(from, Entry::Redirect(targets));
    }

    /// 放入按请求次数变化的内容，请求次数从此刻重新计。
    fn insert_counted(&self, path: &str, entry: Entry) {
        let mut entries = self.state.entries.lock().unwrap();
        self.state.hits.lock().unwrap().remove(path);
        entries.insert(path.into(), entry);
    }

    /// 之后请求该路径返回 `status`。
    pub(crate) fn status(&self, path: &str, status: StatusCode) {
        let entry = Entry::Status(status);
        self.state
            .entries
            .lock()
            .unwrap()
            .insert(path.into(), entry);
    }

    pub(crate) fn fail(&self, path: &str, times: usize) {
        self.state
            .failures
            .lock()
            .unwrap()
            .insert(path.into(), times);
    }

    pub(crate) fn gate(&self, path: &str) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        self.state
            .gates
            .lock()
            .unwrap()
            .insert(path.into(), gate.clone());
        gate
    }

    pub(crate) fn ungate(&self, path: &str) {
        let gate = self.state.gates.lock().unwrap().remove(path).unwrap();
        gate.release.notify_one();
    }

    pub(crate) fn require_header(&self, name: &str, value: &str) {
        *self.state.required_header.lock().unwrap() = Some((name.into(), value.into()));
    }

    pub(crate) fn hits(&self, path: &str) -> usize {
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
    let hits = {
        let mut all = state.hits.lock().unwrap();
        let hits = all.entry(path.clone()).or_default();
        *hits += 1;
        *hits
    };

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

    let nth = |len: usize| (hits - 1).min(len - 1);
    let body = match state.entries.lock().unwrap().get(&path) {
        None => return StatusCode::NOT_FOUND.into_response(),
        Some(Entry::Status(status)) => return status.into_response(),
        Some(Entry::Redirect(targets)) => {
            let to = &targets[nth(targets.len())];
            return (StatusCode::FOUND, [(header::LOCATION, format!("/{to}"))]).into_response();
        }
        Some(Entry::Body(body)) => body.clone(),
        Some(Entry::Sequence(bodies)) => bodies[nth(bodies.len())].clone(),
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
