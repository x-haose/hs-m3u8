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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::{engine, request, run, test_dir};

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
    /// 回响应之前等多久：让同时发出的请求在服务端重叠
    pub delay: Duration,
    /// 声称的总长度（Content-Range 里的），不给时为内容的长度；只用于在下载之前就失败的测试
    pub claimed: Option<u64>,
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
            delay: Duration::ZERO,
            claimed: None,
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
    tokio::time::sleep(served.delay).await;
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
    let validator_matches = if_range.as_ref().is_none_or(|v| {
        Some(v) == served.etag.as_ref() || Some(v) == served.last_modified.as_ref()
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
        format!("bytes {first}-{last}/{total}").parse().unwrap(),
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

fn assert_downloaded(output: &FileOutput, body: &[u8]) {
    assert_eq!(std::fs::read(&output.path).unwrap(), body);
    assert_eq!(output.bytes, body.len() as u64);
    assert_eq!(output.leftovers, []);
    assert!(!output.path.with_extension("hsdl").exists());
}

/// 范围请求的起点与长度；不带 Range 时为 None。
fn span(seen: &Seen) -> Option<(u64, Option<u64>)> {
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
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.ends_with(b"\r\n\r\n") {
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    head.extend_from_slice(&buf[..n]);
                }
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

/// 服务器只允许两个连接、多的回 503：上限降下来，报出降低，照常下完。
#[tokio::test(flavor = "multi_thread")]
async fn connection_limits_lower_the_connection_count() {
    let dir = test_dir("file_throttled");
    let server = FileServer::start().await;
    let body = content(16 * MIB);
    let mut served = Served::new(body.clone());
    served.max_concurrent = Some(2);
    served.delay = Duration::from_millis(50);
    server.put("a.bin", served);
    let mut request = file_request(server.url("a.bin"), &dir);
    request.connections = NonZeroUsize::new(6).unwrap();

    let (output, progress) = run_file(request).await;

    assert_downloaded(&output.unwrap(), &body);
    let throttled = progress.throttled.clone().expect("应降过上限");
    assert_eq!(
        throttled.failure.kind,
        hs_m3u8_core::HttpErrorKind::Status(503)
    );
    assert!(progress.limit < 6, "{progress:?}");
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

/// 第一次运行按调用方看到的核对：ETag 变了（If-Range 换来整个文件）、长度不同、没有验证器与长度时换成了网页，都报来源
/// 的文件变了；对得上时照常下完。
#[tokio::test(flavor = "multi_thread")]
async fn the_first_run_checks_what_the_caller_saw() {
    let dir = test_dir("file_expected");
    let server = FileServer::start().await;
    let body = content(1000);
    let mut served = Served::new(body.clone());
    served.etag = Some("\"v2\"".into());
    server.put("a.bin", served);
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
            matches!(
                result,
                Err(Error::WorkDir {
                    problem: WorkDirProblem::FileChanged,
                    ..
                })
            ),
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
