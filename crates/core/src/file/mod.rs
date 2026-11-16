//! 单个文件的下载：认出地址是什么（[`identify()`]），与可续传的分段下载（[`run`]）。
//!
//! 与 HLS 共用客户端、代理、Cookie 存储、重试与失败分类、引擎的请求名额，以及任务目录的锁、换上输出与放弃。任务目录里
//! 放数据文件（按偏移写入）与已落盘范围的记录（见 [`crate::workdir`]）。

mod download;
mod headers;
mod identify;
mod open;
mod range_set;
mod schedule;

use std::num::{NonZeroU64, NonZeroUsize};
use std::path::Path;
use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderValue};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use url::Url;

use self::download::{Download, Mode};
pub(crate) use self::identify::identify;
use self::open::{FromZero, open_from_zero, open_range, request_end};
use self::range_set::RangeSet;
use crate::hooks::Purpose;
use crate::http::{Http, Permit};
use crate::output::{self, FileOutputOptions, ResolvedOutput};
use crate::workdir::{self, FileDir, FileRecord, FileResume, identity_digest, url_digest};
use crate::{Error, FileOutput, FileProgress, Probe, Source, Stage, WorkDirProblem, blocking, job};

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
/// [`WorkDirProblem::FileChanged`]；地址与记录的不同而记录里没有验证器时报 [`WorkDirProblem::SourceUnverified`]（调用方
/// 担保了内容身份时另有规则，见 `identity`）。已全部下完、装上输出之前中断的，地址没变时不再请求；变了时照样核对（发一
/// 个字节的范围请求）。
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
    /// 调用方确知的内容身份（如网盘的文件编号加内容哈希），不能为空串。给了即由调用方担保身份相同的内容逐字节相同，内容
    /// 对不对由调用方在下完后核对；任务目录只记它的摘要。续传时这次与记录的都有：不同报
    /// [`WorkDirProblem::FileChanged`]；相同则地址变了也沿用已下的范围、不报 [`WorkDirProblem::SourceUnverified`]，新地址
    /// 的请求不带记录的验证器（ETag、Last-Modified 只对给出它的地址有意义，签名地址换了主机时可能不同），改带新地址这次
    /// 给的；总长度照常核对。一方没有时同两方都没有，按地址与验证器核对
    pub identity: Option<String>,
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
            identity: None,
        }
    }

    /// 校验请求，返回补全为绝对路径的输出配置：来源同 [`Source::validate`]，输出见 [`FileOutputOptions`]，`expected` 里
    /// 的验证器要能放进请求头，`identity` 不能为空串。
    pub(crate) fn validate(&self) -> Result<ResolvedOutput, Error> {
        self.source.validate()?;
        if self.identity.as_deref() == Some("") {
            return Err(Error::InvalidInput("identity 不能为空串".into()));
        }
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
    let done = match &resume {
        Some(resume) => RangeSet::from_sorted(resume.ranges.clone()),
        None => RangeSet::default(),
    };
    let http = Arc::new(http);
    let url = request.source.url.clone();
    let (mode, record) = http
        .retrying(Purpose::File, &url, &cancel, || {
            let resumed = resume.as_ref().map(|r| (r, &done));
            first_response(&http, &request, resumed, &root, &cancel)
        })
        .await?;
    let validator = headers::validator(record.etag.as_deref(), record.last_modified.as_deref());
    let dir = FileDir::open(root, lock, record, resume.is_some()).await?;
    if let Some(length) = mode.length() {
        check_space(dir.root(), length - done.len()).await?;
    }
    progress.send_modify(|p| {
        p.stage = Stage::Downloading;
        p.total = mode.length();
        p.bytes = done.len();
        p.completed = done.ranges().to_vec();
        p.limit = match mode {
            Mode::Ranged { .. } => request.connections.get(),
            Mode::Whole { .. } => 1,
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
    let bytes = download.run(&dir, mode, done).await?;
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

/// 发第一个请求并核对（规则见 [`FileRequest`]），返回下载方式与写进 job.json 的记录。续传（`resume` 为可续的内容与它已
/// 落盘的范围）时见 [`resumed`]；否则从 0 起请求（见 [`open_from_zero`]），按 `expected` 核对。`root` 为任务目录，用于
/// 报错。
async fn first_response(
    http: &Http,
    request: &FileRequest,
    resume: Option<(&FileResume, &RangeSet)>,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<(Mode, FileRecord), Error> {
    if let Some((resume, done)) = resume {
        return resumed(http, request, resume, done, root, cancel).await;
    }
    let url = &request.source.url;
    let expected = request.expected.as_ref();
    let if_range = expected.and_then(Expected::validator);
    let first_end = request.chunk_size.map(NonZeroU64::get);
    let not_expected = || Error::NotExpected {
        url: Box::new(url.clone()),
    };
    let opened = open_from_zero(
        http,
        url,
        first_end,
        if_range.as_deref(),
        Permit::Vod,
        cancel,
    )
    .await?;
    let (mode, headers) = match opened {
        FromZero::Ranged { length, first } => {
            let headers = first.stream.headers().clone();
            let mode = Mode::Ranged {
                length,
                first: Some(first),
            };
            (mode, headers)
        }
        // 带了 If-Range 而回了整个文件：来源变了；没带时即不支持范围请求
        FromZero::Ignored(_) if if_range.is_some() => return Err(not_expected()),
        FromZero::Ignored(stream) | FromZero::Plain(stream) => {
            let headers = stream.headers().clone();
            let length = headers::content_length(&headers);
            (Mode::Whole { length, stream }, headers)
        }
    };
    // 服务器忽略 If-Range 照常回了 206，或认不出范围、不带 Range 重新取的：响应带的验证器与调用方看到的不同即换了文件
    if if_range
        .as_deref()
        .is_some_and(|sent| headers::contradicts(&headers, sent))
    {
        return Err(not_expected());
    }
    if let Some(expected) = expected {
        let length_differs = expected.length.is_some_and(|l| mode.length() != Some(l));
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
    let identity = request.identity.as_deref().map(identity_digest);
    let record = file_record(url, mode.length(), &headers, None, identity);
    Ok((mode, record))
}

/// 续传：这次与记录的内容身份（[`FileRequest::identity`]）都有而不同时报 [`WorkDirProblem::FileChanged`]；地址与记录
/// 的不同时，身份相同即沿用，否则要有记录的验证器，没有报 [`WorkDirProblem::SourceUnverified`]。请求第一段没下的（带
/// 记录的验证器，地址变了而身份相同时不带，见 [`open_range`]）。都已落盘时，地址没变就不请求，变了发一个字节的范围请
/// 求核对。
async fn resumed(
    http: &Http,
    request: &FileRequest,
    resume: &FileResume,
    done: &RangeSet,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<(Mode, FileRecord), Error> {
    let url = &request.source.url;
    let recorded = &resume.record;
    let problem = |problem| Error::WorkDir {
        path: root.to_path_buf(),
        problem,
    };
    let identity = request.identity.as_deref().map(identity_digest);
    let vouched = match (&recorded.identity_digest, &identity) {
        (Some(recorded), Some(current)) if recorded != current => {
            return Err(problem(WorkDirProblem::FileChanged));
        }
        (Some(_), Some(_)) => true,
        _ => false,
    };
    let moved = recorded.url_digest != url_digest(url);
    // 记录的验证器是旧地址给的：身份相同时不拿它去问新地址，这次的记录也不沿用它
    let previous = (!(moved && vouched)).then_some(recorded);
    let validator =
        previous.and_then(|r| headers::validator(r.etag.as_deref(), r.last_modified.as_deref()));
    if moved && !vouched && validator.is_none() {
        return Err(problem(WorkDirProblem::SourceUnverified));
    }
    let length = resume.length;
    let gap = done.missing(length).first().cloned();
    let span = match &gap {
        Some(gap) => gap.start..request_end(gap, request.chunk_size.map(NonZeroU64::get)),
        None if moved => 0..1,
        None => {
            let mode = Mode::Ranged {
                length,
                first: None,
            };
            let record = FileRecord {
                identity_digest: identity,
                ..recorded.clone()
            };
            return Ok((mode, record));
        }
    };
    let opened = open_range(http, url, span, validator.as_deref(), length, root, cancel).await?;
    let headers = opened.stream.headers();
    let record = file_record(url, Some(length), headers, previous, identity);
    let mode = Mode::Ranged {
        length,
        first: gap.map(|_| opened),
    };
    Ok((mode, record))
}

/// 写进 job.json 的记录：地址 `url` 的摘要、总长度 `length`，响应头 `headers` 里的强 ETag、Last-Modified（响应没带的
/// 沿用之前的记录 `previous`；弱 ETag 不记），与这次给的内容身份的摘要 `identity_digest`。
fn file_record(
    url: &Url,
    length: Option<u64>,
    headers: &HeaderMap,
    previous: Option<&FileRecord>,
    identity_digest: Option<String>,
) -> FileRecord {
    FileRecord {
        url_digest: url_digest(url),
        length,
        etag: headers::etag(headers)
            .filter(|e| headers::is_strong(e))
            .or_else(|| previous.and_then(|p| p.etag.clone())),
        last_modified: headers::last_modified(headers)
            .or_else(|| previous.and_then(|p| p.last_modified.clone())),
        identity_digest,
    }
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
