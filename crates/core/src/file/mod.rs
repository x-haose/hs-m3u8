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
use std::sync::Arc;

use reqwest::StatusCode;
use reqwest::header::{CONTENT_LENGTH, HeaderValue};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use url::Url;

use self::download::{Download, Mode};
pub(crate) use self::identify::identify;
pub(crate) use self::range_set::RangeSet;
use crate::hooks::Purpose;
use crate::http::{Conditions, Http, Permit, Streaming};
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
    /// 支持范围请求：对 `Range: bytes=0-` 回了 206，且 Content-Range 从 0 起、给出了总长度
    pub accepts_ranges: bool,
    /// ETag，原样（弱 ETag 带 `W/`）
    pub etag: Option<String>,
    /// Last-Modified，原样
    pub last_modified: Option<String>,
    /// Content-Type，原样
    pub content_type: Option<String>,
    /// Content-Disposition 给的文件名（RFC 6266，`filename*` 优先），原样：可能含路径分隔符与 `..`，用作文件名前由调用方
    /// 清理。没有时为 None，不从地址推
    pub file_name: Option<String>,
}

/// 调用方之前看到的文件（通常即 [`crate::Engine::identify`] 得到的 [`FileInfo`]，见 `From`）。任务目录里没有可续的内容
/// 时，第一个响应按它核对，对不上报 [`WorkDirProblem::FileChanged`]：签名地址过期、会话失效后服务器常回一个 HTML 错误页
/// 或登录页，不核对就会把它当成文件下完。
///
/// - 支持范围请求且有验证器（强 ETag，没有时 Last-Modified）时请求带 If-Range，回了整个文件即对不上；
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
/// 支持范围请求且知道总长度时分段下载：开始时一个连接覆盖要下的全部，有连接空出来就把剩得最多的一段劈成两半、后半交给
/// 它（剩下不到 2 MiB 的不劈），被劈的连接读到新的终点即断开；每个请求都写明终点。同时有多个连接时，新的请求在收到响应
/// 之前失败或回了 429、503、403，视为服务器限制了连接数：上限降一个（至少 1），这一段放回去，不算失败（见
/// [`crate::FileProgress::throttled`]）。数据先落盘、再记已完成的范围，约每秒或每 8 MiB 一次；再次运行时只补没完成的，
/// 请求带 If-Range（强 ETag，没有时 Last-Modified），回了整个文件或总长度变了报 [`WorkDirProblem::FileChanged`]；地址与
/// 记录的不同而记录里没有验证器时报 [`WorkDirProblem::SourceUnverified`]。
///
/// 不支持范围请求或不知道总长度时只用一个连接，从头读到尾，中断后从头下。不解压（Accept-Encoding 由本库控制），下完核对
/// 字节数与总长度。开始下载前按还要写的字节数查任务目录所在卷的可用空间，不够时报 [`Error::NoSpace`]。下完后装到输出：
/// 与任务目录在同一文件系统时用硬链接，不然复制。
#[derive(Clone)]
pub struct FileRequest {
    /// 来源；[`Source::preference`] 不用，[`crate::Hooks::on_request`] 照常调用（用途为 [`Purpose::File`]），其余回调
    /// 不调用
    pub source: Source,
    pub output: FileOutputOptions,
    /// 本任务同时的连接数上限，每个连接占引擎的一个点播名额（见 [`crate::Engine::new`]）
    pub connections: NonZeroUsize,
    /// 每个请求至多取这么多字节；None 时一个请求取到它那一段的终点。用于限制单个请求大小的站点
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

    /// 校验请求，返回补全为绝对路径的输出配置：来源同 [`Source::validate`]，输出见 [`FileOutputOptions`]，`expected` 里的
    /// 验证器要能放进请求头。
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
    let complete = resume.as_ref().filter(|r| {
        RangeSet::from_sorted(r.ranges.clone())
            .missing(r.length)
            .is_empty()
    });
    let first = match complete {
        // 上次下完、装上输出之前中断了：不用再请求
        Some(resume) => First {
            mode: Mode::Ranged {
                length: resume.length,
                validator: None,
            },
            stream: None,
            request_end: None,
            record: resume.record.clone(),
        },
        None => {
            http.retrying(Purpose::File, &url, &cancel, || {
                first_response(&http, &request, resume.as_ref(), &root, &cancel)
            })
            .await?
        }
    };
    let dir = FileDir::open(root, lock, first.record.clone(), resume.is_some()).await?;
    let done = match (&first.mode, &resume) {
        (Mode::Ranged { .. }, Some(resume)) => RangeSet::from_sorted(resume.ranges.clone()),
        _ => RangeSet::default(),
    };
    let needed = match first.mode {
        Mode::Ranged { length, .. } => Some(length - done.len()),
        Mode::Whole { length } => length,
    };
    if let Some(needed) = needed {
        check_space(dir.root(), needed).await?;
    }
    progress.send_modify(|p| {
        p.stage = Stage::Downloading;
        p.total = first.mode.length();
        p.bytes = done.len();
        p.completed = done.ranges().to_vec();
        p.limit = match first.mode {
            Mode::Ranged { .. } => request.connections.get(),
            Mode::Whole { .. } => 1,
        };
    });
    let download = Download {
        http: http.clone(),
        url,
        chunk: request.chunk_size.map(NonZeroU64::get),
        connections: request.connections.get(),
        progress: progress.clone(),
        cancel: cancel.clone(),
    };
    let bytes = download
        .run(&dir, first.mode, first.stream, first.request_end, done)
        .await?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    progress.send_modify(|p| p.stage = Stage::Writing);
    let leftovers = output::install_file(dir, &output, &cancel).await?;
    progress.send_modify(|p| p.stage = Stage::Done);
    Ok(FileOutput {
        path: output.outputs[0].target().to_path_buf(),
        bytes,
        leftovers,
    })
}

/// 第一个响应与据它定下的下载方式。
struct First {
    mode: Mode,
    /// 都已落盘时为 None
    stream: Option<Streaming>,
    /// 这个请求取到哪里（不含）；不带 Range 的请求为总长度，不知道时为 None
    request_end: Option<u64>,
    /// 写进 job.json 的记录
    record: FileRecord,
}

/// 发第一个请求并核对（规则见 [`FileRequest`]）。续传（`resume`）时取第一段没完成的，带记录里的验证器；否则取
/// `bytes=0-`（给了块大小时取第一块），带 `expected` 的验证器，回 416 或 Content-Range 不对时不带 Range 再请求一次。
/// `root` 为任务目录，用于报错。
async fn first_response(
    http: &Http,
    request: &FileRequest,
    resume: Option<&FileResume>,
    root: &std::path::Path,
    cancel: &CancellationToken,
) -> Result<First, Error> {
    let url = &request.source.url;
    let chunk = request.chunk_size.map(NonZeroU64::get);
    let changed = || Error::WorkDir {
        path: root.to_path_buf(),
        problem: WorkDirProblem::FileChanged,
    };
    let open = |conditions: Conditions| async move {
        http.open(Purpose::File, url, &conditions, Permit::Vod, cancel)
            .await
    };
    if let Some(resume) = resume {
        let recorded = &resume.record;
        let validator =
            headers::validator(recorded.etag.as_deref(), recorded.last_modified.as_deref());
        if recorded.url_digest != url_digest(url) && validator.is_none() {
            return Err(Error::WorkDir {
                path: root.to_path_buf(),
                problem: WorkDirProblem::SourceUnverified,
            });
        }
        let gaps = RangeSet::from_sorted(resume.ranges.clone()).missing(resume.length);
        let gap = gaps.first().expect("都已落盘的不发请求（见 run）");
        let end = chunk.map_or(gap.end, |c| gap.end.min(gap.start.saturating_add(c)));
        let stream = match open(Conditions {
            range: Some(format!("bytes={}-{}", gap.start, end - 1)),
            if_range: validator.clone(),
        })
        .await
        {
            Err(Error::Http {
                kind: HttpErrorKind::Status(416),
                ..
            }) => return Err(changed()),
            other => other?,
        };
        check_partial(&stream, gap.start, end, resume.length, root)?;
        let record = FileRecord {
            url_digest: url_digest(url),
            length: Some(resume.length),
            etag: headers::etag(stream.headers()).or_else(|| recorded.etag.clone()),
            last_modified: headers::last_modified(stream.headers())
                .or_else(|| recorded.last_modified.clone()),
        };
        return Ok(First {
            mode: Mode::Ranged {
                length: resume.length,
                validator: headers::validator(
                    record.etag.as_deref(),
                    record.last_modified.as_deref(),
                ),
            },
            stream: Some(stream),
            request_end: Some(end),
            record,
        });
    }
    let expected = request.expected.as_ref();
    let if_range = expected.and_then(Expected::validator);
    let range = match chunk {
        Some(c) => format!("bytes=0-{}", c - 1),
        None => "bytes=0-".to_owned(),
    };
    let ranged = open(Conditions {
        range: Some(range),
        if_range: if_range.clone(),
    })
    .await;
    let stream = match ranged {
        Err(Error::Http {
            kind: HttpErrorKind::Status(416),
            ..
        }) => None,
        Err(e) => return Err(e),
        Ok(stream) => Some(stream),
    };
    let (mode, stream, request_end) = match stream.map(|s| (ranged_length(&s), s)) {
        Some((Some(length), stream)) if stream.status() == StatusCode::PARTIAL_CONTENT => {
            let end = chunk.map_or(length, |c| length.min(c));
            (
                Mode::Ranged {
                    length,
                    validator: headers::validator(
                        headers::etag(stream.headers()).as_deref(),
                        headers::last_modified(stream.headers()).as_deref(),
                    ),
                },
                stream,
                Some(end),
            )
        }
        Some((_, stream)) if stream.status() != StatusCode::PARTIAL_CONTENT => {
            // 带了 If-Range 而回了整个文件：来源变了；没带时即不支持范围请求
            if if_range.is_some() {
                return Err(changed());
            }
            let length = content_length(&stream);
            (Mode::Whole { length }, stream, length)
        }
        _ => {
            // 416，或 206 而 Content-Range 不对（总长为 *、不从 0 起）：不带 Range 整个取
            let stream = open(Conditions::default()).await?;
            let length = content_length(&stream);
            (Mode::Whole { length }, stream, length)
        }
    };
    if let Some(expected) = expected {
        let length_differs = expected.length.is_some_and(|l| mode.length() != Some(l));
        let turned_html = if_range.is_none()
            && expected.length.is_none()
            && !expected
                .content_type
                .as_deref()
                .is_some_and(headers::is_html)
            && headers::content_type(stream.headers()).is_some_and(|t| headers::is_html(&t));
        if length_differs || turned_html {
            return Err(changed());
        }
    }
    let record = FileRecord {
        url_digest: url_digest(url),
        length: mode.length(),
        etag: headers::etag(stream.headers()).filter(|e| headers::is_strong(e)),
        last_modified: headers::last_modified(stream.headers()),
    };
    Ok(First {
        mode,
        stream: Some(stream),
        request_end,
        record,
    })
}

/// 对从 0 起的范围请求的 206 响应：Content-Range 从 0 起、给出了总长度时为总长度，否则为 None。
fn ranged_length(stream: &Streaming) -> Option<u64> {
    if stream.status() != StatusCode::PARTIAL_CONTENT {
        return None;
    }
    let value = stream
        .headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?;
    match headers::content_range(value)? {
        (0, _, Some(total)) => Some(total),
        _ => None,
    }
}

/// Content-Length；没有或写法不对时为 None。
fn content_length(stream: &Streaming) -> Option<u64> {
    stream
        .headers()
        .get(CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// 核对范围请求 `start..end` 的响应：须是 206、Content-Range 恰为这一段，总长度为 `length`；回了整个文件（带 If-Range
/// 时即来源变了，不带时服务器不再支持范围请求，都续不了）或总长度不同报 [`WorkDirProblem::FileChanged`]（`root` 为任务
/// 目录），只是段不对报 [`HttpErrorKind::RangeMismatch`]。
pub(super) fn check_partial(
    stream: &Streaming,
    start: u64,
    end: u64,
    length: u64,
    root: &std::path::Path,
) -> Result<(), Error> {
    let changed = || Error::WorkDir {
        path: root.to_path_buf(),
        problem: WorkDirProblem::FileChanged,
    };
    if stream.status() != StatusCode::PARTIAL_CONTENT {
        return Err(changed());
    }
    let value = stream
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    match value.as_deref().and_then(headers::content_range) {
        Some((_, _, Some(total))) if total != length => Err(changed()),
        Some((first, last, Some(_))) if first == start && last + 1 == end => Ok(()),
        _ => Err(crate::http::http_error(
            stream.url(),
            HttpErrorKind::RangeMismatch(value),
        )),
    }
}

/// 任务目录 `root` 所在卷的可用空间放得下 `needed` 字节，否则报 [`Error::NoSpace`]。
async fn check_space(root: &std::path::Path, needed: u64) -> Result<(), Error> {
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
