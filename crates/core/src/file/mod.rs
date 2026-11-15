//! 单个文件的下载：认出地址是什么（[`identify()`]），与可续传的分段下载（[`run`]）。
//!
//! 与 HLS 共用客户端、代理、Cookie 存储、重试与失败分类、引擎的请求名额，以及任务目录的锁、换上输出与放弃。任务目录里
//! 放数据文件（按偏移写入）与已落盘范围的记录（见 [`crate::workdir`]）。

mod download;
mod headers;
mod identify;
mod range_set;
mod schedule;

use std::num::{NonZeroU64, NonZeroUsize};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderValue};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use url::Url;

use self::download::{Download, Start};
pub(crate) use self::identify::identify;
use self::range_set::RangeSet;
use crate::hooks::Purpose;
use crate::http::{Http, Permit, RangeRequest, Streaming, http_error};
use crate::output::{self, FileOutputOptions, ResolvedOutput};
use crate::workdir::{self, FileDir, FileRecord, FileResume, url_digest};
use crate::{
    Error, FileOutput, FileProgress, HttpErrorKind, Probe, Source, Stage, WorkDirProblem, blocking,
    job,
};

/// 认出的地址（[`crate::Engine::identify`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// 内容开头是 `#EXTM3U`：照 [`crate::Engine::probe`] 解析选轨的结果
    Playlist(Box<Probe>),
    /// Content-Type 是 text/html 而不是播放列表：多为网页（播放页、登录页、错误页）；`content_type` 为响应头原样
    Page { content_type: String },
    /// 其余的：当作文件下载
    File(FileInfo),
}

/// 地址上的文件（[`Resource::File`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// 跟随重定向之后的最终地址
    pub url: Url,
    /// 总字节数：支持范围请求时取自 Content-Range，否则取自 Content-Length；都没有时为 None
    pub length: Option<u64>,
    /// 支持范围请求：对 `Range: bytes=0-` 回了 206，且 Content-Range 从 0 起、给出了总长度（服务器可能只给了开头一部
    /// 分）
    pub accepts_ranges: bool,
    /// ETag，原样（弱 ETag 带 `W/`）
    pub etag: Option<String>,
    /// Last-Modified，原样
    pub last_modified: Option<String>,
    /// Content-Type，原样
    pub content_type: Option<String>,
    /// Content-Disposition 给的文件名（RFC 6266，`filename*` 优先），原样：可能含路径分隔符与 `..`，用作文件名前由调用
    /// 方清理。没有时为 None，不从地址推
    pub file_name: Option<String>,
}

/// 调用方之前看到的文件（通常即 [`crate::Engine::identify`] 得到的 [`FileInfo`]，见 `From`）。任务目录里没有可续的内容
/// 时，第一个响应按它核对，对不上报 [`Error::NotExpected`]：签名地址过期、会话失效后服务器常回一个 HTML 错误页或登录
/// 页，不核对就会把它当成文件下完。
///
/// - 支持范围请求且有验证器（强 ETag，没有时 Last-Modified）时请求带 If-Range，回了整个文件、或响应带的验证器与它不同
///   即对不上；
/// - `length` 有值而总长度不同即对不上；
/// - 两者都没有时，`content_type` 不是 text/html 而响应是即对不上。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expected {
    pub length: Option<u64>,
    /// 同 [`FileInfo::accepts_ranges`]：为 false 时不带 If-Range（不支持范围请求的服务器一律回整个文件）
    pub accepts_ranges: bool,
    /// 弱 ETag 不用
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
}

impl From<&FileInfo> for Expected {
    fn from(info: &FileInfo) -> Self {
        Expected {
            length: info.length,
            accepts_ranges: info.accepts_ranges,
            etag: info.etag.clone(),
            last_modified: info.last_modified.clone(),
            content_type: info.content_type.clone(),
        }
    }
}

impl Expected {
    /// 带 If-Range 用的验证器；不支持范围请求时为 None。
    fn validator(&self) -> Option<String> {
        self.accepts_ranges
            .then(|| headers::validator(self.etag.as_deref(), self.last_modified.as_deref()))
            .flatten()
    }
}

/// 一个文件任务的配置（[`crate::Engine::start_file`]）。用 [`FileRequest::new`] 取默认值后按需修改字段。
///
/// 支持范围请求且知道总长度时分段下载：开始时一个连接覆盖要下的全部（第一个请求为 `bytes=0-`，给了块大小时取第一块），
/// 有连接空出来就把剩得最多的一段劈成两半、后半交给它（剩下不到 2 MiB 的不劈），被劈的连接读到新的终点即断开；之后的
/// 请求都写明终点，服务器给得比请求的少时从它给到的地方接着请求。同时有多个连接时，新的请求在收到响应之前失败或回了
/// 429、503、403，视为服务器限制了连接数：上限降到在途的连接数减一（至少 1），这一段放回去，不算失败（见
/// [`crate::Throttled`]）。数据先落盘、再记已完成的范围，约每秒或每 8 MiB 一次；再次运行时只补没完成的，请求带 If-Range
/// （强 ETag，没有时 Last-Modified），来源的文件变了（回了整个文件、响应带的验证器不同、总长度不同）报
/// [`WorkDirProblem::FileChanged`]；地址与记录的不同而记录里没有验证器时报 [`WorkDirProblem::SourceUnverified`]。已全部
/// 下完、装上输出之前中断的，地址没变时不再请求；变了时照样核对（发一个字节的范围请求）。
///
/// 不支持范围请求或不知道总长度时只用一个连接，从头读到尾，中断后从头下。请求带 `Accept-Encoding: identity`，服务器仍
/// 回了编码过的内容时原样保存；下完核对字节数与总长度。请求不受 [`crate::Timeouts::request`] 限制（文件可能很大），卡住
/// 由读空闲超时判断。开始下载前按还要写的字节数查任务目录所在卷的可用空间，不够时报 [`Error::NoSpace`]。下完后装到
/// 输出：与任务目录在同一文件系统时用硬链接，不然复制。
#[derive(Clone)]
pub struct FileRequest {
    /// 来源；[`Source::preference`] 不用，[`crate::Hooks::on_request`] 照常调用（用途为 [`Purpose::File`]），其余回调
    /// 不调用
    pub source: Source,
    pub output: FileOutputOptions,
    /// 本任务同时的连接数上限，每个连接占引擎的一个点播名额（见 [`crate::Engine::new`]）；不支持范围请求或不知道总长度
    /// 时只用一个
    pub connections: NonZeroUsize,
    /// 每个请求至多取这么多字节；None 时一个请求取到它那一段的终点。用于限制单个请求大小的站点；不支持范围请求或不知道
    /// 总长度时不起作用
    pub chunk_size: Option<NonZeroU64>,
    /// 见 [`Expected`]；None 时不核对
    pub expected: Option<Expected>,
}

impl FileRequest {
    /// `connections` 的默认值。
    pub const DEFAULT_CONNECTIONS: NonZeroUsize = NonZeroUsize::new(8).unwrap();

    pub fn new(source: Source, output: FileOutputOptions) -> Self {
        FileRequest {
            source,
            output,
            connections: Self::DEFAULT_CONNECTIONS,
            chunk_size: None,
            expected: None,
        }
    }

    /// 校验请求，返回补全为绝对路径的输出配置：来源同 [`Source::validate`]，输出见 [`FileOutputOptions`]，`expected` 里
    /// 的验证器要能放进请求头。
    pub(crate) fn validate(&self) -> Result<ResolvedOutput, Error> {
        self.source.validate()?;
        if let Some(validator) = self.expected.as_ref().and_then(Expected::validator) {
            HeaderValue::from_str(&validator).map_err(|_| {
                Error::InvalidInput(format!(
                    "expected 的 ETag 或 Last-Modified 不合法：{validator:?}"
                ))
            })?;
        }
        self.output.resolve()
    }
}

/// 运行文件任务：开头同 HLS 任务（检查输出、给已存在的任务目录加锁），读可续的内容，发第一个请求并核对，打开任务目录，
/// 查可用空间，下载，装上输出。
pub(crate) async fn run(
    request: FileRequest,
    output: ResolvedOutput,
    http: Http,
    cancel: CancellationToken,
    progress: watch::Sender<FileProgress>,
) -> Result<FileOutput, Error> {
    let lock = job::prepare(&output).await?;
    let root = output.work_dir.clone();
    let resume = workdir::read_file_resume(root.clone()).await?;
    let http = Arc::new(http);
    let url = request.source.url.clone();
    let (start, record) = http
        .retrying(Purpose::File, &url, &cancel, || {
            first_response(&http, &request, resume.as_ref(), &root, &cancel)
        })
        .await?;
    let done = match &resume {
        Some(resume) => RangeSet::from_sorted(resume.ranges.clone()),
        None => RangeSet::default(),
    };
    let validator = headers::validator(record.etag.as_deref(), record.last_modified.as_deref());
    let dir = FileDir::open(root, lock, record, !done.ranges().is_empty()).await?;
    if let Some(length) = start.length() {
        check_space(dir.root(), length - done.len()).await?;
    }
    progress.send_modify(|p| {
        p.stage = Stage::Downloading;
        p.total = start.length();
        p.bytes = done.len();
        p.completed = done.ranges().to_vec();
        p.limit = match start {
            Start::Ranged { .. } => request.connections.get(),
            Start::Whole { .. } => 1,
        };
    });
    let download = Download {
        http: http.clone(),
        url,
        chunk: request.chunk_size.map(NonZeroU64::get),
        connections: request.connections.get(),
        validator,
        progress: progress.clone(),
        cancel: cancel.clone(),
    };
    let bytes = download.run(&dir, start, done).await?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    progress.send_modify(|p| p.stage = Stage::Writing);
    let (path, leftovers) = output::install_file(dir, &output, &cancel).await?;
    progress.send_modify(|p| p.stage = Stage::Done);
    Ok(FileOutput {
        path,
        bytes,
        leftovers,
    })
}

/// 发第一个请求并核对（规则见 [`FileRequest`]），返回下载方式与写进 job.json 的记录。续传（`resume`）时见 [`resumed`]；
/// 否则从 0 起请求（见 [`open_from_start`]），按 `expected` 核对。`root` 为任务目录，用于报错。
async fn first_response(
    http: &Http,
    request: &FileRequest,
    resume: Option<&FileResume>,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<(Start, FileRecord), Error> {
    if let Some(resume) = resume {
        return resumed(http, request, resume, root, cancel).await;
    }
    let url = &request.source.url;
    let expected = request.expected.as_ref();
    let if_range = expected.and_then(Expected::validator);
    let first_end = request.chunk_size.map(NonZeroU64::get);
    let not_expected = || Error::NotExpected {
        url: Box::new(url.clone()),
    };
    let opened = open_from_start(
        http,
        url,
        first_end,
        if_range.as_deref(),
        Permit::Vod,
        cancel,
    )
    .await?;
    let (start, headers) = match opened {
        FromStart::Ranged { length, first } => {
            if if_range
                .as_deref()
                .is_some_and(|sent| headers::contradicts(first.stream.headers(), sent))
            {
                return Err(not_expected());
            }
            let headers = first.stream.headers().clone();
            let start = Start::Ranged {
                length,
                first: Some(first),
            };
            (start, headers)
        }
        // 带了 If-Range 而回了整个文件：来源变了；没带时即不支持范围请求
        FromStart::Ignored(_) if if_range.is_some() => return Err(not_expected()),
        FromStart::Ignored(stream) | FromStart::Plain(stream) => {
            let headers = stream.headers().clone();
            let length = headers::content_length(&headers);
            (Start::Whole { length, stream }, headers)
        }
    };
    if let Some(expected) = expected {
        let length_differs = expected.length.is_some_and(|l| start.length() != Some(l));
        let turned_html = if_range.is_none()
            && expected.length.is_none()
            && !expected
                .content_type
                .as_deref()
                .is_some_and(headers::is_html)
            && headers::content_type(&headers).is_some_and(|t| headers::is_html(&t));
        if length_differs || turned_html {
            return Err(not_expected());
        }
    }
    let record = file_record(url, start.length(), &headers, None);
    Ok((start, record))
}

/// 续传：地址与记录的不同而记录里没有验证器时报 [`WorkDirProblem::SourceUnverified`]；请求第一段没下的（带记录的验证
/// 器，见 [`open_range`]）。都已落盘时，地址没变就不请求，变了发一个字节的范围请求核对。
async fn resumed(
    http: &Http,
    request: &FileRequest,
    resume: &FileResume,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<(Start, FileRecord), Error> {
    let url = &request.source.url;
    let recorded = &resume.record;
    let validator = headers::validator(recorded.etag.as_deref(), recorded.last_modified.as_deref());
    let moved = recorded.url_digest != url_digest(url);
    if moved && validator.is_none() {
        return Err(Error::WorkDir {
            path: root.to_path_buf(),
            problem: WorkDirProblem::SourceUnverified,
        });
    }
    let length = resume.length;
    let gap = RangeSet::from_sorted(resume.ranges.clone())
        .missing(length)
        .first()
        .cloned();
    let span = match &gap {
        Some(gap) => gap.start..request_end(gap, request.chunk_size.map(NonZeroU64::get)),
        None if moved => 0..1,
        None => {
            let start = Start::Ranged {
                length,
                first: None,
            };
            return Ok((start, recorded.clone()));
        }
    };
    let opened = open_range(http, url, span, validator.as_deref(), length, root, cancel).await?;
    let record = file_record(url, Some(length), opened.stream.headers(), Some(recorded));
    let start = Start::Ranged {
        length,
        first: gap.map(|_| opened),
    };
    Ok((start, record))
}

/// 写进 job.json 的记录：地址 `url` 的摘要、总长度 `length`，与响应头 `headers` 里的强 ETag、Last-Modified（响应没带的
/// 沿用之前的记录 `previous`；弱 ETag 不记）。
fn file_record(
    url: &Url,
    length: Option<u64>,
    headers: &HeaderMap,
    previous: Option<&FileRecord>,
) -> FileRecord {
    FileRecord {
        url_digest: url_digest(url),
        length,
        etag: headers::etag(headers)
            .filter(|e| headers::is_strong(e))
            .or_else(|| previous.and_then(|p| p.etag.clone())),
        last_modified: headers::last_modified(headers)
            .or_else(|| previous.and_then(|p| p.last_modified.clone())),
    }
}

/// 一段剩下的 `remaining` 这次请求到哪里（不含）：给了块大小 `chunk` 时至多一块。
fn request_end(remaining: &Range<u64>, chunk: Option<u64>) -> u64 {
    chunk.map_or(remaining.end, |c| {
        remaining.end.min(remaining.start.saturating_add(c))
    })
}

/// 收到了响应头的范围请求：从 `start` 起，到 `end`（不含）为止。服务器可能给得比请求的少，`end` 为它给到的。
pub(super) struct Opened {
    pub stream: Streaming,
    pub start: u64,
    pub end: u64,
}

/// 请求 `span`（带 If-Range `validator`），核对响应：须是 206、从请求的起点起、总长度为 `length`。终点比请求的早（服务
/// 器一次只给这么多）时照收，比请求的晚时只取到请求的终点。来源的文件变了报 [`WorkDirProblem::FileChanged`]（`root` 为
/// 任务目录）：回 416（文件变短了）、总长度不同、响应带的验证器与 `validator` 不同、带了 If-Range 而回了整个文件。没带
/// If-Range 而回了整个文件（服务器这次忽略了范围请求）报 [`HttpErrorKind::RangeIgnored`]，起点不对报
/// [`HttpErrorKind::RangeMismatch`]。
pub(super) async fn open_range(
    http: &Http,
    url: &Url,
    span: Range<u64>,
    validator: Option<&str>,
    length: u64,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<Opened, Error> {
    let changed = || Error::WorkDir {
        path: root.to_path_buf(),
        problem: WorkDirProblem::FileChanged,
    };
    let request = RangeRequest {
        span: Some((span.start, Some(span.end))),
        if_range: validator.map(str::to_owned),
    };
    let stream = match http
        .open(Purpose::File, url, &request, Permit::Vod, cancel)
        .await
    {
        Err(Error::Http {
            kind: HttpErrorKind::Status(416),
            ..
        }) => return Err(changed()),
        other => other?,
    };
    let status = stream.status();
    if status != StatusCode::PARTIAL_CONTENT {
        return match validator {
            Some(_) => Err(changed()),
            None => Err(http_error(
                stream.url(),
                HttpErrorKind::RangeIgnored(status.as_u16()),
            )),
        };
    }
    if validator.is_some_and(|sent| headers::contradicts(stream.headers(), sent)) {
        return Err(changed());
    }
    match stream.content_range() {
        Some((_, _, Some(total))) if total != length => Err(changed()),
        Some((first, last, Some(_))) if first == span.start => {
            let end = (last + 1).min(span.end);
            Ok(Opened {
                stream,
                start: span.start,
                end,
            })
        }
        _ => {
            let value = stream
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
            Err(http_error(
                stream.url(),
                HttpErrorKind::RangeMismatch(value),
            ))
        }
    }
}

/// 从 0 起请求的结果（见 [`open_from_start`]）。
pub(super) enum FromStart {
    /// 206，Content-Range 从 0 起、给出了总长度 `length`
    Ranged { length: u64, first: Opened },
    /// 对范围请求回了整个文件
    Ignored(Streaming),
    /// 回 416（多为空文件），或 206 而 Content-Range 不对（没有总长度、不从 0 起）：不带 Range 重新请求得到的
    Plain(Streaming),
}

/// 请求 `url` 从 0 起到 `end`（不含；None 时到文件末尾），带 If-Range `if_range`；回 416 或 Content-Range 不对时放开这
/// 个响应，不带 Range 再请求一次。
pub(super) async fn open_from_start(
    http: &Http,
    url: &Url,
    end: Option<u64>,
    if_range: Option<&str>,
    permit: Permit,
    cancel: &CancellationToken,
) -> Result<FromStart, Error> {
    let request = RangeRequest {
        span: Some((0, end)),
        if_range: if_range.map(str::to_owned),
    };
    match http
        .open(Purpose::File, url, &request, permit, cancel)
        .await
    {
        Err(Error::Http {
            kind: HttpErrorKind::Status(416),
            ..
        }) => {}
        Err(error) => return Err(error),
        Ok(stream) if stream.status() != StatusCode::PARTIAL_CONTENT => {
            return Ok(FromStart::Ignored(stream));
        }
        Ok(stream) => {
            if let Some((0, last, Some(length))) = stream.content_range() {
                let end = end.map_or(last + 1, |e| e.min(last + 1));
                let first = Opened {
                    stream,
                    start: 0,
                    end,
                };
                return Ok(FromStart::Ranged { length, first });
            }
            // 放开这个响应（与它占的名额、连接）再重新请求
            drop(stream);
        }
    }
    let plain = http
        .open(Purpose::File, url, &RangeRequest::default(), permit, cancel)
        .await?;
    Ok(FromStart::Plain(plain))
}

/// 任务目录 `root` 所在卷的可用空间放得下 `needed` 字节，否则报 [`Error::NoSpace`]。
async fn check_space(root: &Path, needed: u64) -> Result<(), Error> {
    let path = root.to_path_buf();
    let available = blocking({
        let path = path.clone();
        move || fs4::available_space(&path)
    })
    .await?
    .map_err(crate::error::io_error("查可用空间", &path))?;
    if needed > available {
        return Err(Error::NoSpace {
            path,
            needed,
            available,
        });
    }
    Ok(())
}
