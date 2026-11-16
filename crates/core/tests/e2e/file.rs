//! 文件任务：认出地址是什么；分段与劈段、按块请求、续传（含来源变了、地址变了核对不了）、不支持范围请求、不知道长度、
//! 服务器限制连接数、可用空间不够、第一次运行按调用方看到的核对；文件任务的目录与 HLS 任务互不沿用，可查看、可放弃。

use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use hs_m3u8_core::{
    CancellationToken, Error, Expected, FileOutput, FileOutputOptions, FileProgress, FileRequest,
    FileSaved, JobType, Resource, RetryPolicy, Saved, Source, Stage, Unsettled, Url,
    WorkDirProblem,
};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::{engine, read_request_head, request, run, test_dir};

const MIB: usize = 1 << 20;

/// 测试服务上的一个文件。
#[derive(Clone)]
pub(crate) struct Served {
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub disposition: Option<String>,
    /// 支持范围请求；为 false 时对 Range 也回整个文件
    pub ranges: bool,
    /// 同时在处理的请求超过它时回 503
    pub max_concurrent: Option<usize>,
    /// 声称的总长度（Content-Range 里的），不给时为内容的长度；只用于在下载之前就失败的测试
    pub claimed: Option<u64>,
    /// 不看 If-Range，照常回范围
    pub ignore_if_range: bool,
    /// 每个 206 至多给这么多字节（Content-Range 照实写）
    pub cap: Option<u64>,
    /// Content-Range 的总长写成 `*`
    pub hide_total: bool,
}

impl Served {
    pub(crate) fn new(body: Vec<u8>) -> Self {
        Served {
            body,
            etag: None,
            last_modified: None,
            content_type: None,
            disposition: None,
            ranges: true,
            max_concurrent: None,
            claimed: None,
            ignore_if_range: false,
            cap: None,
            hide_total: false,
        }
    }
}

/// 收到的一个请求。
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub path: String,
    pub range: Option<String>,
    pub if_range: Option<String>,
}

#[derive(Default)]
struct FileState {
    files: Mutex<HashMap<String, Served>>,
    seen: Mutex<Vec<Seen>>,
    in_flight: AtomicUsize,
    /// 起点不小于它的范围请求等它变回 None 才回应
    held: watch::Sender<Option<u64>>,
}

/// 按 RFC 9110 回应范围请求（`bytes=首-`、`bytes=首-尾`，越界回 416，If-Range 对不上回整个文件）的测试服务。
pub(crate) struct FileServer {
    base: Url,
    state: Arc<FileState>,
}

impl FileServer {
    pub(crate) async fn start() -> FileServer {
        let state = Arc::new(FileState::default());
        let app = axum::Router::new()
            .fallback(serve_file)
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        FileServer { base, state }
    }

    pub(crate) fn url(&self, path: &str) -> Url {
        self.base.join(path).unwrap()
    }

    pub(crate) fn put(&self, path: &str, served: Served) {
        self.state.files.lock().unwrap().insert(path.into(), served);
    }

    /// 改 `path` 上的文件。
    fn change(&self, path: &str, change: impl FnOnce(&mut Served)) {
        change(self.state.files.lock().unwrap().get_mut(path).unwrap());
    }

    /// 之后起点不小于 `offset` 的范围请求挂住，直到 [`FileServer::release`]。
    pub(crate) fn hold_from(&self, offset: u64) {
        self.state.held.send_replace(Some(offset));
    }

    pub(crate) fn release(&self) {
        self.state.held.send_replace(None);
    }

    /// 收到的请求，按先后。
    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.state.seen.lock().unwrap().clone()
    }

    fn clear_seen(&self) {
        self.state.seen.lock().unwrap().clear();
    }
}

/// 处理中的请求数减一。
struct InFlight<'a>(&'a AtomicUsize);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn serve_file(State(state): State<Arc<FileState>>, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/').to_owned();
    let text = |name| {
        headers
            .get(name)
            .map(|v: &HeaderValue| v.to_str().unwrap().to_owned())
    };
    let (range, if_range) = (text(header::RANGE), text(header::IF_RANGE));
    state.seen.lock().unwrap().push(Seen {
        path: path.clone(),
        range: range.clone(),
        if_range: if_range.clone(),
    });
    let in_flight = state.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
    let _counted = InFlight(&state.in_flight);
    let Some(served) = state.files.lock().unwrap().get(&path).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if served.max_concurrent.is_some_and(|max| in_flight > max) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let total = served.claimed.unwrap_or(served.body.len() as u64);
    let mut response_headers = HeaderMap::new();
    for (name, value) in [
        (header::ETAG, &served.etag),
        (header::LAST_MODIFIED, &served.last_modified),
        (header::CONTENT_TYPE, &served.content_type),
        (header::CONTENT_DISPOSITION, &served.disposition),
    ] {
        if let Some(value) = value {
            response_headers.insert(name, value.parse().unwrap());
        }
    }
    // RFC 9110 13.1.5：值是 ETag 时与强 ETag 比，是日期时与 Last-Modified 比
    let validator_matches = served.ignore_if_range
        || if_range.as_ref().is_none_or(|v| {
            if v.starts_with('"') {
                Some(v) == served.etag.as_ref()
            } else {
                Some(v) == served.last_modified.as_ref()
            }
        });
    let span = range
        .filter(|_| served.ranges && validator_matches)
        .map(|range| {
            let (first, last) = range
                .strip_prefix("bytes=")
                .unwrap()
                .split_once('-')
                .unwrap();
            let first: u64 = first.parse().unwrap();
            let last = match last {
                "" => total.saturating_sub(1),
                last => last.parse::<u64>().unwrap().min(total.saturating_sub(1)),
            };
            let last = served.cap.map_or(last, |cap| last.min(first + cap - 1));
            (first, last)
        });
    let Some((first, last)) = span else {
        return (StatusCode::OK, response_headers, served.body).into_response();
    };
    if first >= total {
        response_headers.insert(
            header::CONTENT_RANGE,
            format!("bytes */{total}").parse().unwrap(),
        );
        return (StatusCode::RANGE_NOT_SATISFIABLE, response_headers).into_response();
    }
    let mut held = state.held.subscribe();
    if held.borrow().is_some_and(|at| first >= at) {
        held.wait_for(Option::is_none).await.unwrap();
    }
    response_headers.insert(
        header::CONTENT_RANGE,
        match served.hide_total {
            true => format!("bytes {first}-{last}/*"),
            false => format!("bytes {first}-{last}/{total}"),
        }
        .parse()
        .unwrap(),
    );
    let body_end = (last as usize + 1).min(served.body.len());
    let body = served.body[(first as usize).min(body_end)..body_end].to_vec();
    (StatusCode::PARTIAL_CONTENT, response_headers, body).into_response()
}

/// 不重复、各处不同的内容：分段写错位置时比较得出来。
pub(crate) fn content(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// 下到 `dir/out.bin`、任务目录为 `dir/out.hsdl` 的请求；失败只重试一次，退避 1 毫秒。
pub(crate) fn file_request(url: Url, dir: &Path) -> FileRequest {
    let mut request = FileRequest::new(
        Source::new(url),
        FileOutputOptions::new(dir.join("out.bin")),
    );
    request.source.http.retry = RetryPolicy {
        attempts: NonZeroU32::new(2).unwrap(),
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    request
}

/// 运行到底，返回结果与最后的进度。
async fn run_file(request: FileRequest) -> (Result<FileOutput, Error>, FileProgress) {
    let job = engine().start_file(request).unwrap();
    let progress = job.control().progress();
    let result = job.wait().await;
    let last = progress.borrow().clone();
    (result, last)
}

/// 运行到写完 `bytes` 字节（之后的请求被 [`FileServer::hold_from`] 挂住）后取消，任务目录保留。
pub(crate) async fn interrupt_at(request: FileRequest, bytes: u64) {
    let job = engine().start_file(request).unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.bytes >= bytes).await.unwrap();
    job.control().cancel();
    assert!(matches!(job.wait().await, Err(Error::Cancelled)));
}

/// 下完的文件与 `body` 相同，没有残留，任务目录已删。
fn assert_downloaded(output: &FileOutput, body: &[u8]) {
    assert_downloaded_kept(output, body);
    assert!(!output.path.with_extension("hsdl").exists());
}

/// 同 [`assert_downloaded`]，任务目录保留着。
fn assert_downloaded_kept(output: &FileOutput, body: &[u8]) {
    let written = std::fs::read(&output.path).unwrap();
    assert!(
        written == body,
        "内容不同：{} 字节，应为 {} 字节",
        written.len(),
        body.len()
    );
    assert_eq!(output.bytes, body.len() as u64);
    assert_eq!(output.leftovers, []);
}

/// 范围请求的起点与长度；不带 Range 时为 None。
pub(crate) fn span(seen: &Seen) -> Option<(u64, Option<u64>)> {
    let (first, last) = seen
        .range
        .as_ref()?
        .strip_prefix("bytes=")?
        .split_once('-')?;
    let first: u64 = first.parse().ok()?;
    Some((first, last.parse::<u64>().ok().map(|last| last + 1 - first)))
}

#[tokio::test(flavor = "multi_thread")]
async fn identify_tells_playlists_pages_and_files() {
    let server = FileServer::start().await;
    server.put(
        "index.m3u8",
        Served::new(
            b"#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n".to_vec(),
        ),
    );
    let mut page = Served::new(b"<html></html>".to_vec());
    page.content_type = Some("text/html; charset=utf-8".into());
    server.put("watch", page);
    let mut file = Served::new(content(1000));
    file.etag = Some("\"v1\"".into());
    file.last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".into());
    file.content_type = Some("application/zip".into());
    file.disposition = Some("attachment; filename*=UTF-8''%E6%96%87%E4%BB%B6.zip".into());
    server.put("a.zip", file.clone());
    server.put(
        "plain.bin",
        Served {
            ranges: false,
            ..file
        },
    );
    server.put("empty.bin", Served::new(Vec::new()));
    let identify = |path: &str| {
        let url = server.url(path);
        async move { engine().identify(&Source::new(url)).await.unwrap() }
    };

    match identify("index.m3u8").await {
        Resource::Playlist(probe) => assert_eq!(probe.tracks[0].segments, 1),
        other => panic!("应为播放列表：{other:?}"),
    }
    assert_eq!(
        identify("watch").await,
        Resource::Page {
            content_type: "text/html; charset=utf-8".into()
        }
    );
    let Resource::File(info) = identify("a.zip").await else {
        panic!("应为文件");
    };
    assert_eq!(
        (
            info.url,
            info.length,
            info.accepts_ranges,
            info.etag.as_deref(),
            info.file_name.as_deref()
        ),
        (
            server.url("a.zip"),
            Some(1000),
            true,
            Some("\"v1\""),
            Some("文件.zip")
        )
    );
    let Resource::File(plain) = identify("plain.bin").await else {
        panic!("应为文件");
    };
    assert_eq!((plain.length, plain.accepts_ranges), (Some(1000), false));
    // 空文件对 bytes=0- 回 416：不带 Range 再请求一次
    let Resource::File(empty) = identify("empty.bin").await else {
        panic!("应为文件");
    };
    assert_eq!((empty.length, empty.accepts_ranges), (Some(0), false));
    let empty_requests: Vec<_> = server
        .seen()
        .into_iter()
        .filter(|s| s.path == "empty.bin")
        .collect();
    assert_eq!(empty_requests.len(), 2);
}

/// 多个连接分段下载：劈出了好几段，各段都写对了位置；第一个请求从 0 起不写终点，其余都写明终点。
#[tokio::test(flavor = "multi_thread")]
async fn files_are_downloaded_in_split_pieces() {
    let dir = test_dir("file_split");
    let server = FileServer::start().await;
    let body = content(8 * MIB + 123);
    server.put("a.bin", Served::new(body.clone()));
    let mut request = file_request(server.url("a.bin"), &dir);
    request.connections = NonZeroUsize::new(4).unwrap();

    let (output, progress) = run_file(request).await;

    assert_downloaded(&output.unwrap(), &body);
    let seen = server.seen();
    assert_eq!(seen[0].range.as_deref(), Some("bytes=0-"));
    let starts: Vec<u64> = seen
        .iter()
        .filter_map(span)
        .map(|(start, _)| start)
        .collect();
    assert!(starts.len() >= 4, "{seen:?}");
    assert!(
        seen[1..].iter().all(|s| span(s).unwrap().1.is_some()),
        "{seen:?}"
    );
    assert_eq!(progress.stage, Stage::Done);
    let whole = 0..body.len() as u64;
    assert_eq!(progress.completed, [whole]);
    assert_eq!(
        (progress.total, progress.limit),
        (Some(body.len() as u64), 4)
    );
}

/// 给了块大小：每个请求都不超过它。
#[tokio::test(flavor = "multi_thread")]
async fn chunked_requests_stay_within_the_chunk_size() {
    let dir = test_dir("file_chunks");
    let server = FileServer::start().await;
    let body = content(3 * MIB + 5);
    server.put("a.bin", Served::new(body.clone()));
    let mut request = file_request(server.url("a.bin"), &dir);
    request.connections = NonZeroUsize::new(2).unwrap();
    request.chunk_size = NonZeroU64::new(MIB as u64);

    let (output, _) = run_file(request).await;

    assert_downloaded(&output.unwrap(), &body);
    let seen = server.seen();
    assert!(seen.len() >= 4, "{seen:?}");
    for s in &seen {
        let (_, len) = span(s).unwrap();
        assert!(len.unwrap() <= MIB as u64, "{s:?}");
    }
}

/// 中断后续传：查看得到已落盘的字节数；再次运行只请求没完成的部分、都带 If-Range，结果与原文件相同。
#[tokio::test(flavor = "multi_thread")]
async fn interrupted_downloads_resume_where_they_stopped() {
    let dir = test_dir("file_resume");
    let server = FileServer::start().await;
    let body = content(4 * MIB);
    let mut served = Served::new(body.clone());
    served.etag = Some("\"v1\"".into());
    server.put("a.bin", served);
    let mut request = file_request(server.url("a.bin"), &dir);
    request.connections = NonZeroUsize::new(1).unwrap();
    request.chunk_size = NonZeroU64::new(MIB as u64);
    server.hold_from(2 * MIB as u64);

    interrupt_at(request.clone(), 2 * MIB as u64).await;

    let work = dir.join("out.hsdl");
    assert_eq!(
        engine().inspect(&work).await.unwrap(),
        Some(Saved::File(FileSaved {
            bytes: 2 * MIB as u64,
            total: Some(4 * MIB as u64)
        }))
    );
    server.release();
    server.clear_seen();
    let (output, _) = run_file(request).await;

    assert_downloaded(&output.unwrap(), &body);
    let seen = server.seen();
    assert!(!seen.is_empty());
    for s in &seen {
        assert!(span(s).unwrap().0 >= 2 * MIB as u64, "{s:?}");
        assert_eq!(s.if_range.as_deref(), Some("\"v1\""), "{s:?}");
    }
}

/// 续传时来源变了（ETag 不同，If-Range 换来整个文件）：报来源的文件变了，已下的留着。地址变了而记录里没有验证器：
/// 核对不了，报核对不上。
#[tokio::test(flavor = "multi_thread")]
async fn changed_sources_are_not_resumed() {
    let dir = test_dir("file_changed");
    let server = FileServer::start().await;
    let mut served = Served::new(content(2 * MIB));
    served.etag = Some("\"v1\"".into());
    server.put("a.bin", served.clone());
    server.put(
        "same.bin",
        Served {
            etag: None,
            ..served
        },
    );
    let mut request = file_request(server.url("a.bin"), &dir.join("etag"));
    request.chunk_size = NonZeroU64::new(MIB as u64);
    server.hold_from(MIB as u64);
    interrupt_at(request.clone(), MIB as u64).await;
    server.release();
    server.change("a.bin", |s| s.etag = Some("\"v2\"".into()));

    let (result, _) = run_file(request).await;

    assert!(
        matches!(
            result,
            Err(Error::WorkDir {
                problem: WorkDirProblem::FileChanged,
                ..
            })
        ),
        "{result:?}"
    );
    let saved = engine().inspect(&dir.join("etag/out.hsdl")).await.unwrap();
    assert!(matches!(saved, Some(Saved::File(FileSaved { bytes, .. })) if bytes == MIB as u64));

    let mut request = file_request(server.url("same.bin"), &dir.join("moved"));
    request.chunk_size = NonZeroU64::new(MIB as u64);
    server.hold_from(MIB as u64);
    interrupt_at(request.clone(), MIB as u64).await;
    server.release();
    request.source.url = server.url("a.bin?moved");

    let (result, _) = run_file(request).await;

    assert!(
        matches!(
            result,
            Err(Error::WorkDir {
                problem: WorkDirProblem::SourceUnverified,
                ..
            })
        ),
        "{result:?}"
    );
}

/// 不支持范围请求：一个连接从头读到尾。
#[tokio::test(flavor = "multi_thread")]
async fn servers_without_ranges_get_one_connection() {
    let dir = test_dir("file_no_ranges");
    let server = FileServer::start().await;
    let body = content(3 * MIB);
    server.put(
        "a.bin",
        Served {
            ranges: false,
            ..Served::new(body.clone())
        },
    );

    let (output, progress) = run_file(file_request(server.url("a.bin"), &dir)).await;

    assert_downloaded(&output.unwrap(), &body);
    assert_eq!(server.seen().len(), 1);
    assert_eq!(progress.limit, 1);
}

/// 不知道长度（响应没有 Content-Length、发完即关连接，也不支持范围请求）：读到结束。
#[tokio::test(flavor = "multi_thread")]
async fn files_of_unknown_length_are_read_to_the_end() {
    let dir = test_dir("file_unknown_length");
    let body = content(MIB + 7);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/a.bin", listener.local_addr().unwrap())).unwrap();
    let served = body.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let body = served.clone();
            tokio::spawn(async move {
                read_request_head(&mut stream)
                    .await
                    .expect("请求头没读完连接就关了");
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                stream.write_all(&body).await.unwrap();
                stream.shutdown().await.unwrap();
            });
        }
    });

    let (output, progress) = run_file(file_request(url, &dir)).await;

    assert_downloaded(&output.unwrap(), &body);
    assert_eq!(progress.total, None);
}

/// 服务器同时只处理两个请求、多的回 503：上限降下来，报出降低，照常下完。不从 0 起的范围请求先挂住，在服务端占着那两个
/// 名额，等降过上限再放行。
#[tokio::test(flavor = "multi_thread")]
async fn connection_limits_lower_the_connection_count() {
    let dir = test_dir("file_throttled");
    let server = FileServer::start().await;
    let body = content(16 * MIB);
    let mut served = Served::new(body.clone());
    served.max_concurrent = Some(2);
    server.put("a.bin", served);
    let mut request = file_request(server.url("a.bin"), &dir);
    request.connections = NonZeroUsize::new(6).unwrap();
    server.hold_from(1);

    let job = engine().start_file(request).unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.throttled.is_some()).await.unwrap();
    server.release();
    let output = job.wait().await;

    assert_downloaded(&output.unwrap(), &body);
    let last = progress.borrow().clone();
    let throttled = last.throttled.clone().unwrap();
    assert_eq!(
        throttled.failure.kind,
        hs_m3u8_core::HttpErrorKind::Status(503)
    );
    assert!(last.limit < 6, "{last:?}");
}

/// 卷上放不下：下载之前报空间不够，不写数据文件。
#[tokio::test(flavor = "multi_thread")]
async fn insufficient_space_is_reported_before_downloading() {
    let dir = test_dir("file_no_space");
    let server = FileServer::start().await;
    let claimed = u64::MAX / 2;
    server.put(
        "a.bin",
        Served {
            claimed: Some(claimed),
            ..Served::new(vec![0])
        },
    );

    let (result, _) = run_file(file_request(server.url("a.bin"), &dir)).await;

    match result {
        Err(Error::NoSpace { needed, .. }) => assert_eq!(needed, claimed),
        other => panic!("应报空间不够：{other:?}"),
    }
    assert!(!dir.join("out.hsdl/data").exists());
}

/// 第一次运行按调用方看到的核对：ETag 变了（If-Range 换来整个文件；服务器忽略 If-Range、或认不出范围而不带 Range 重新
/// 取时看响应带的 ETag）、长度不同、没有验证器与长度时换成了网页，都报已不是之前看到的文件；对得上时照常下完。
#[tokio::test(flavor = "multi_thread")]
async fn the_first_run_checks_what_the_caller_saw() {
    let dir = test_dir("file_expected");
    let server = FileServer::start().await;
    let body = content(1000);
    let mut served = Served::new(body.clone());
    served.etag = Some("\"v2\"".into());
    server.put("a.bin", served.clone());
    server.put(
        "ignoring.bin",
        Served {
            ignore_if_range: true,
            ..served.clone()
        },
    );
    // 忽略 If-Range 照常回范围、Content-Range 的总长写成 *：客户端退回不带 Range 重新取
    server.put(
        "hidden.bin",
        Served {
            hide_total: true,
            ignore_if_range: true,
            ..served
        },
    );
    let mut page = Served::new(b"<html>login</html>".to_vec());
    page.content_type = Some("text/html".into());
    server.put("login", page);
    let seen = Expected {
        length: Some(1000),
        accepts_ranges: true,
        etag: Some("\"v2\"".into()),
        last_modified: None,
        content_type: Some("application/octet-stream".into()),
    };
    let stale = [
        (
            "a.bin",
            Expected {
                etag: Some("\"v1\"".into()),
                ..seen.clone()
            },
        ),
        (
            "ignoring.bin",
            Expected {
                etag: Some("\"v1\"".into()),
                ..seen.clone()
            },
        ),
        (
            "hidden.bin",
            Expected {
                etag: Some("\"v1\"".into()),
                ..seen.clone()
            },
        ),
        (
            "a.bin",
            Expected {
                length: Some(999),
                etag: None,
                ..seen.clone()
            },
        ),
        (
            "login",
            Expected {
                length: None,
                etag: None,
                ..seen.clone()
            },
        ),
    ];
    for (i, (path, expected)) in stale.into_iter().enumerate() {
        let mut request = file_request(server.url(path), &dir.join(i.to_string()));
        request.expected = Some(expected);
        let (result, _) = run_file(request).await;
        assert!(
            matches!(result, Err(Error::NotExpected { .. })),
            "{path}：{result:?}"
        );
    }

    let mut request = file_request(server.url("a.bin"), &dir.join("ok"));
    request.expected = Some(seen);
    let (output, _) = run_file(request).await;
    assert_downloaded(&output.unwrap(), &body);
}

/// 文件任务的目录：HLS 任务不沿用（报类型不符），放弃时整个删掉。
#[tokio::test(flavor = "multi_thread")]
async fn file_task_dirs_are_kept_apart_and_can_be_discarded() {
    let dir = test_dir("file_dir");
    let server = FileServer::start().await;
    server.put("a.bin", Served::new(content(2 * MIB)));
    let mut file = file_request(server.url("a.bin"), &dir);
    file.output.work_dir = Some(dir.join("work"));
    file.chunk_size = NonZeroU64::new(MIB as u64);
    server.hold_from(MIB as u64);
    interrupt_at(file, MIB as u64).await;
    server.release();
    let mut hls = request(server.url("index.m3u8"), &dir);
    hls.output.work_dir = Some(dir.join("work"));
    server.put(
        "index.m3u8",
        Served::new(
            b"#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n".to_vec(),
        ),
    );

    let result = run(hls).await;

    assert!(
        matches!(
            result,
            Err(Error::WorkDir {
                problem: WorkDirProblem::KindMismatch {
                    recorded: JobType::File,
                    current: JobType::Vod
                },
                ..
            })
        ),
        "{result:?}"
    );
    let leftovers = engine()
        .discard(
            &dir.join("work"),
            Unsettled::Fail,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(leftovers, []);
    assert!(!dir.join("work").exists());
}

/// 不知道长度时中途断开、从头重下，这次的内容比上次写下的短：输出只有这次的内容，不留上次多出的字节。第一次是分块传输，
/// 等客户端写下至少 1 MiB 后在块中间断开（响应体不完整）。
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_whole_download_keeps_only_the_new_bytes() {
    let dir = test_dir("file_whole_restart");
    let (first, second) = (content(4 * MIB), content(100));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/a.bin", listener.local_addr().unwrap())).unwrap();
    let (written_tx, mut written_rx) = tokio::sync::oneshot::channel::<()>();
    let served = second.clone();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request_head(&mut stream)
            .await
            .expect("请求头没读完连接就关了");
        let head = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            2 * first.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        tokio::select! {
            sent = stream.write_all(&first) => {
                sent.unwrap();
                (&mut written_rx).await.unwrap();
            }
            written = &mut written_rx => written.unwrap(),
        }
        // 块只发了一半就关连接
        stream.shutdown().await.unwrap();
        drop(stream);
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request_head(&mut stream)
            .await
            .expect("请求头没读完连接就关了");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        stream.write_all(&served).await.unwrap();
        stream.shutdown().await.unwrap();
    });

    let job = engine().start_file(file_request(url, &dir)).unwrap();
    let mut progress = job.control().progress();
    progress.wait_for(|p| p.bytes >= MIB as u64).await.unwrap();
    written_tx.send(()).unwrap();
    let output = job.wait().await;

    assert_downloaded(&output.unwrap(), &second);
}

/// 服务器每个 206 至多给 1 MiB：从它给到的地方接着请求，照常下完；认出地址时这样截短的播放列表不带 Range 重新取完整的。
#[tokio::test(flavor = "multi_thread")]
async fn capped_partial_responses_are_continued() {
    let dir = test_dir("file_capped");
    let server = FileServer::start().await;
    let body = content(3 * MIB + 9);
    server.put(
        "a.bin",
        Served {
            cap: Some(MIB as u64),
            ..Served::new(body.clone())
        },
    );
    let mut playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n".to_owned();
    for i in 0..20 {
        playlist += &format!("#EXTINF:1,\nseg{i}.ts\n");
    }
    playlist += "#EXT-X-ENDLIST\n";
    server.put(
        "index.m3u8",
        Served {
            cap: Some(89),
            ..Served::new(playlist.into_bytes())
        },
    );
    let mut request = file_request(server.url("a.bin"), &dir);
    request.connections = NonZeroUsize::new(2).unwrap();

    let (output, _) = run_file(request).await;

    assert_downloaded(&output.unwrap(), &body);
    let identified = engine()
        .identify(&Source::new(server.url("index.m3u8")))
        .await
        .unwrap();
    let Resource::Playlist(probe) = identified else {
        panic!("应为播放列表：{identified:?}");
    };
    assert_eq!((probe.tracks[0].segments, probe.live), (20, false));
}

/// 206 的 Content-Range 没有总长：放开它再不带 Range 请求，引擎只有一个名额也不卡住。
#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_first_response_is_released_before_asking_again() {
    let dir = test_dir("file_hidden_total");
    let server = FileServer::start().await;
    let body = content(MIB);
    server.put(
        "a.bin",
        Served {
            hide_total: true,
            ..Served::new(body.clone())
        },
    );
    let engine = hs_m3u8_core::Engine::new(NonZeroUsize::MIN);

    let job = engine
        .start_file(file_request(server.url("a.bin"), &dir))
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(10), job.wait())
        .await
        .expect("不应卡住");

    assert_downloaded(&output.unwrap(), &body);
}

/// 下完后保留任务目录，再下到同一处：地址没变时不再请求；地址变了时按 ETag 核对（一个字节的范围请求），同一个文件照常
/// 装上，换了文件（ETag 不同）报来源的文件变了，不把旧内容装上去。
#[tokio::test(flavor = "multi_thread")]
async fn completed_dirs_are_checked_when_the_address_changes() {
    let dir = test_dir("file_completed");
    let server = FileServer::start().await;
    let body = content(2 * MIB);
    let mut served = Served::new(body.clone());
    served.etag = Some("\"v1\"".into());
    server.put("a.bin", served.clone());
    server.put("mirror.bin", served.clone());
    server.put(
        "other.bin",
        Served {
            etag: Some("\"v2\"".into()),
            ..Served::new(content(2 * MIB + 1)[1..].to_vec())
        },
    );
    let request = |path: &str| {
        let mut request = file_request(server.url(path), &dir);
        request.output.keep_work_dir = true;
        request.output.overwrite = true;
        request
    };
    let (first, _) = run_file(request("a.bin")).await;
    assert_downloaded_kept(&first.unwrap(), &body);
    server.clear_seen();

    let (same, _) = run_file(request("a.bin")).await;
    assert_downloaded_kept(&same.unwrap(), &body);
    assert_eq!(server.seen().len(), 0);

    let (mirror, _) = run_file(request("mirror.bin")).await;
    assert_downloaded_kept(&mirror.unwrap(), &body);
    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].range.as_deref(), seen[0].if_range.as_deref()),
        (Some("bytes=0-0"), Some("\"v1\""))
    );

    let (other, _) = run_file(request("other.bin")).await;
    assert!(
        matches!(
            other,
            Err(Error::WorkDir {
                problem: WorkDirProblem::FileChanged,
                ..
            })
        ),
        "{other:?}"
    );
    assert_eq!(std::fs::read(dir.join("out.bin")).unwrap(), body);
}

/// 续传时服务器忽略 If-Range、照常回 206，而文件换了（ETag 不同、长度相同）：按响应带的 ETag 认出来，报来源的文件变了，
/// 不把新旧内容拼在一起。
#[tokio::test(flavor = "multi_thread")]
async fn ignored_if_range_is_caught_by_the_returned_etag() {
    let dir = test_dir("file_ignored_if_range");
    let server = FileServer::start().await;
    let mut served = Served::new(content(2 * MIB));
    served.etag = Some("\"v1\"".into());
    served.ignore_if_range = true;
    server.put("a.bin", served);
    let mut request = file_request(server.url("a.bin"), &dir);
    request.chunk_size = NonZeroU64::new(MIB as u64);
    server.hold_from(MIB as u64);
    interrupt_at(request.clone(), MIB as u64).await;
    server.release();
    server.change("a.bin", |s| {
        s.etag = Some("\"v2\"".into());
        s.body = content(2 * MIB + 1)[1..].to_vec();
    });

    let (result, _) = run_file(request).await;

    assert!(
        matches!(
            result,
            Err(Error::WorkDir {
                problem: WorkDirProblem::FileChanged,
                ..
            })
        ),
        "{result:?}"
    );
}

/// 文件任务不沿用有已完成分片的 HLS 任务目录（报类型不符）；放弃那个目录时，里面不是本库写的、名为 data 的文件留下、
/// 报出来。
#[tokio::test(flavor = "multi_thread")]
async fn hls_dirs_are_not_taken_over_by_file_tasks() {
    let dir = test_dir("file_on_hls_dir");
    let hls = crate::server::Server::start().await;
    hls.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n",
    );
    hls.put("seg0.ts", crate::fixture("ts_long/seg0.ts"));
    let mut hls_request = request(hls.url("index.m3u8"), &dir);
    hls_request.output.work_dir = Some(dir.join("work"));
    assert!(run(hls_request).await.is_err(), "seg1 取不到，任务失败");
    std::fs::write(dir.join("work/data"), b"user file").unwrap();
    let server = FileServer::start().await;
    server.put("a.bin", Served::new(content(1000)));
    let mut file = file_request(server.url("a.bin"), &dir);
    file.output.work_dir = Some(dir.join("work"));

    let (result, _) = run_file(file).await;

    assert!(
        matches!(
            result,
            Err(Error::WorkDir {
                problem: WorkDirProblem::KindMismatch {
                    recorded: JobType::Vod,
                    current: JobType::File
                },
                ..
            })
        ),
        "{result:?}"
    );
    let leftovers = engine()
        .discard(
            &dir.join("work"),
            Unsettled::Report,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    assert!(leftovers[0].cause.contains("data"), "{leftovers:?}");
    assert_eq!(std::fs::read(dir.join("work/data")).unwrap(), b"user file");
}

/// 不支持范围请求时，传得比 `Timeouts::request` 慢的文件照样下完：文件任务的请求只看读空闲超时。
#[tokio::test(flavor = "multi_thread")]
async fn slow_whole_downloads_are_not_cut_by_the_request_timeout() {
    let dir = test_dir("file_slow_whole");
    let body = content(4000);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/a.bin", listener.local_addr().unwrap())).unwrap();
    let served = body.clone();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request_head(&mut stream)
            .await
            .expect("请求头没读完连接就关了");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4000\r\n\r\n")
            .await
            .unwrap();
        // 模拟慢速网络：每 400 毫秒发 1000 字节，合计约 1.6 秒，超过下面的 request 时限
        for chunk in served.chunks(1000) {
            stream.write_all(chunk).await.unwrap();
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    });
    let mut request = file_request(url, &dir);
    request.source.http.timeouts.request = Duration::from_secs(1);

    let (output, _) = run_file(request).await;

    assert_downloaded(&output.unwrap(), &body);
}

/// 文件任务不接手有不是本库写的 data 或 ranges.json 的 HLS 任务目录（没有已完成的分片）：报目录里有不是本库写的文件，
/// job.json 照旧，那个文件不动；再运行一次照样如此。
#[tokio::test(flavor = "multi_thread")]
async fn file_tasks_do_not_take_over_foreign_data_files() {
    for name in ["data", "ranges.json"] {
        foreign_file_is_left_alone(name, |path| std::fs::write(path, b"user file").unwrap()).await;
    }
    // 悬空的符号链接：按目录项判断，不跟随
    #[cfg(unix)]
    foreign_file_is_left_alone("data", |path| {
        std::os::unix::fs::symlink(path.with_file_name("gone"), path).unwrap()
    })
    .await;
}

/// 续传时数据文件被换成了指向别处的符号链接：报目录里有不是本库写的文件，不往链接指向的文件里写。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn symlinked_data_files_are_not_resumed() {
    let dir = test_dir("file_data_symlink");
    let server = FileServer::start().await;
    let mut served = Served::new(content(4 * MIB));
    served.etag = Some("\"v1\"".into());
    server.put("a.bin", served);
    let mut request = file_request(server.url("a.bin"), &dir);
    request.connections = NonZeroUsize::new(1).unwrap();
    request.chunk_size = NonZeroU64::new(MIB as u64);
    server.hold_from(2 * MIB as u64);
    interrupt_at(request.clone(), 2 * MIB as u64).await;
    let data = dir.join("out.hsdl/data");
    let victim = dir.join("victim.bin");
    std::fs::rename(&data, &victim).unwrap();
    std::os::unix::fs::symlink(&victim, &data).unwrap();
    let before = std::fs::read(&victim).unwrap();
    server.release();

    let (result, _) = run_file(request).await;

    assert!(
        matches!(
            result,
            Err(Error::WorkDir {
                problem: WorkDirProblem::NotEmpty,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(std::fs::read(&victim).unwrap(), before);
}

/// 在没有已完成分片的 HLS 任务目录里用 `put` 放下名为 `name` 的东西，文件任务两次都不接手、不动它。
async fn foreign_file_is_left_alone(name: &str, put: impl Fn(&Path)) {
    let dir = test_dir(&format!("file_foreign_{name}"));
    let hls = crate::server::Server::start().await;
    hls.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let mut hls_request = request(hls.url("index.m3u8"), &dir);
    hls_request.output.work_dir = Some(dir.join("work"));
    assert!(run(hls_request).await.is_err(), "seg0 取不到，任务失败");
    let job = std::fs::read(dir.join("work/job.json")).unwrap();
    let foreign = dir.join("work").join(name);
    put(&foreign);
    let before = std::fs::symlink_metadata(&foreign).unwrap();
    let server = FileServer::start().await;
    server.put("a.bin", Served::new(content(1000)));
    let mut file = file_request(server.url("a.bin"), &dir);
    file.output.work_dir = Some(dir.join("work"));

    for _ in 0..2 {
        let (result, _) = run_file(file.clone()).await;
        assert!(
            matches!(
                result,
                Err(Error::WorkDir {
                    problem: WorkDirProblem::NotEmpty,
                    ..
                })
            ),
            "{name}：{result:?}"
        );
        assert_eq!(std::fs::read(dir.join("work/job.json")).unwrap(), job);
        let after = std::fs::symlink_metadata(&foreign).unwrap();
        assert_eq!(
            (after.file_type(), after.len()),
            (before.file_type(), before.len()),
            "{name}"
        );
    }
}
