//! HLS 下载引擎：选轨、并发拉取分片、AES-128 解密与校验、可续传的任务目录、直播录制，输出为 MP4 或 Matroska、可直接播放的本地 HLS
//! 目录，或两者都要。另有单个文件的分段下载（[`Engine::start_file`]），与 HLS 共用客户端、引擎的请求名额、重试与任务目
//! 录；先认出地址是播放列表、网页还是文件用 [`Engine::identify`]，按同样的请求设置取回一个小资源（如 tracker 列表）用
//! [`Engine::fetch`]，调用接口（如网盘的 JSON 接口，任何状态都拿回整个响应）用 [`Engine::call`]。
//!
//! 一个任务把一个播放列表地址变成它的输出（[`Target`]）。任务目录（默认与输出同级、名为输出的主名加 `.hsdl`）保存
//! `job.json` 与已完成的分片；分片先写 `.part`、落盘后原子改名，因此「最终文件存在」即「该分片完整」。中断后用同样的请
//! 求再次运行：点播续传缺的分片（播放列表变了则拒绝），直播续录（见 [`LiveOptions`]）；不联网、只合并已录到的直播用
//! [`Engine::merge_recorded`]。
//!
//! 参数错误由 [`Engine::start`]、[`Engine::merge_recorded`] 立即返回，其余失败经 [`Job::wait`] 的 `Err` 返回；任务失败
//! 或取消时不生成输出，任务目录保留以便续传。
//!
//! 结果与错误里的轨道编号（`track`）：第 0 条为所选变体（来源本身是媒体播放列表时即它），其后依次是所选的各独立音频
//! rendition、各字幕 rendition（见 [`Selected`]）。

mod blocking;

/// README 里的 Rust 示例作为文档测试编译（标了 `no_run`：它要联网），接口改了而示例没跟上时编译失败。
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
struct ReadmeExamples;

mod busy;
mod call;
mod cookies;
mod crypto;
mod entries;
mod error;
mod fetch;
mod fetched;
mod file;
mod hooks;
/// fork 前后要调用的：fork 出的子进程关掉继承来的任务目录锁，否则父进程的任务结束后锁仍被子进程占着。只给会在
/// 任务运行期间 fork 的嵌入方用。
pub mod fork {
    pub use crate::workdir::{
        ForkGuard, LockFiles, close_inherited_locks, prepare_fork as prepare,
    };
}
mod http;
mod ident;
mod info;
mod job;
mod live;
mod output;
mod probe;
mod proxy;
mod recheck;
mod report;
mod request;
mod resolve;
#[cfg(test)]
mod scratch;
mod selection;
mod verify;
mod vod;
mod volume;
mod workdir;

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::DropGuard;

pub use call::Reply;
pub use cookies::{CookieHeader, CookieStore};
pub use error::{
    Error, FetchErrorKind, FetchFailure, HlsUnsupported, HttpErrorKind, HttpFailure,
    IntegrityFailure, JobType, Sent, StallReason, Unsupported, WorkDirProblem,
};
pub use fetched::Fetched;
pub use file::{Expected, FileInfo, FileRequest, Resource};
pub use hooks::{HookError, HookKind, Hooks, NoHooks, Purpose, RequestParts};
/// 内容放不进合并文件的原因（[`Unsupported::Merge`]）；本地 HLS 表示不了的见 [`HlsUnsupported`]。
pub use hs_m3u8_remux::Unsupported as MergeUnsupported;
pub use ident::bare_url;
pub use info::{
    AudioInfo, MasterInfo, Selected, SubtitleFormat, SubtitleInfo, SubtitleProblem, TrackInfo,
    VariantInfo,
};
pub use output::{
    FileOutputOptions, MergedFile, OutputOptions, SubtitleFileFormat, SubtitleOutput, Target,
};
pub use probe::Probe;
pub use proxy::{Bypass, ProxySource, SystemProxy, system_proxy};
pub use report::{
    FileCount, FileOutput, FileProgress, FileSaved, HlsSaved, Leftover, LeftoverKind, LiveEnd,
    LiveReport, MergedOutput, MissReason, Missed, NewSession, Output, Progress, RefreshReason,
    Retrying, Saved, SegmentCounts, SourceBreak, Stage, StallCause, Throttled, TrackKind,
    TrackProgress, Writing,
};
pub use request::{
    Body, Call, HttpOptions, JobRequest, KeyOverride, LiveOptions, Method, Proxy, RetryPolicy,
    Source, Timeouts, check_option_header,
};
pub use tokio_util::sync::CancellationToken;
pub use url::Url;

/// 公开接口用到的合并报告与合并错误的类型，及合并完整文件（[`crate::merge_files`]）用到的类型。
pub mod remux {
    pub use hs_m3u8_remux::fmp4::Problem as Fmp4Problem;
    pub use hs_m3u8_remux::{
        Codec, Container, Error, FfmpegError, FileInput, MediaReport, Origin, Shape, StreamKind,
        StreamReport, Streams, Unsupported, WriteProgress,
    };
}

/// 把几个完整文件（如分开下载的视频与音频）的流原样复制进 `output`（`container` 的格式），在阻塞线程池中执行；格式、
/// 编码、时间线、临时文件与各种失败见 [`hs_m3u8_remux::merge_files`]：失败或取消时 `output` 不变。`cancel` 取消、或
/// 返回的 future 被丢弃时合并在下一处检查停下，以 [`Error::Cancelled`] 结束；其余失败为 [`Error::Remux`]。`progress`
/// 以写完的字节数调用，合计为各输入文件的字节数（见 [`remux::WriteProgress`]）。
pub async fn merge_files(
    inputs: Vec<remux::FileInput>,
    output: std::path::PathBuf,
    container: remux::Container,
    cancel: CancellationToken,
    progress: remux::WriteProgress,
) -> Result<remux::MediaReport, Error> {
    let _cancel_on_drop = cancel.clone().drop_guard();
    let control = hs_m3u8_remux::Control {
        cancel: hs_m3u8_remux::Cancel::new(move || cancel.is_cancelled()),
        progress,
    };
    blocking(move || hs_m3u8_remux::merge_files(&inputs, &output, container, &control))
        .await?
        .map_err(|error| match error {
            hs_m3u8_remux::Error::Cancelled => Error::Cancelled,
            other => Error::Remux(Box::new(other)),
        })
}

/// 公开接口用到的播放列表解析与选轨类型。
pub mod hls {
    pub use hs_m3u8_hls::{
        Encryption, Malformed, Preference, RenditionChoice, RenditionKey, RenditionKind,
        Renditions, Resolution, SelectError, SyntaxError, VariantChoice, VariantKey,
    };
}

pub(crate) use blocking::blocking;

use crate::http::{Deadline, Http, Observer, Permits, Redirects, RetryWaiting};

/// 下载引擎；持有跨任务共享的在途请求上限。
#[derive(Clone)]
pub struct Engine {
    requests: Arc<Permits>,
}

impl Engine {
    /// `max_requests` 的默认值：默认并发（[`JobRequest::DEFAULT_CONCURRENCY`]）的四倍，四个任务同时跑都不受这个
    /// 上限限制。
    pub const DEFAULT_MAX_REQUESTS: NonZeroUsize =
        NonZeroUsize::new(JobRequest::DEFAULT_CONCURRENCY.get() * 4).unwrap();

    /// `max_requests`：所有点播任务合计同时在途的 key、分片与 init 段请求数，文件任务（[`Engine::start_file`]）的每个连
    /// 接也占一个；所有直播任务的 key 与分片另有同样大小的一份，不与点播共用：直播的分片排在点播的大批下载（或挂住的请
    /// 求）之后会滑出窗口、永久缺失。播放列表、认出地址（[`Engine::identify`]）、取回小资源（[`Engine::fetch`]）与调用
    /// 接口（[`Engine::call`]）的请求与刷新时拉的 init 段（连同它的 key）不占名额：每条轨同时最多一个，刷新不排在本任务
    /// 的分片下载之后；同一个 key 正被分片的请求拉取时，等那次请求（它占着名额）。
    pub fn new(max_requests: NonZeroUsize) -> Self {
        Engine {
            requests: Permits::new(max_requests.get()),
        }
    }

    /// 拉取来源并按偏好选轨，不下载分片、不碰任务目录；请求与回调同下载。用于开始下载前列出可选的清晰度与
    /// 音轨、查看时长与是否直播。参数错误时立即返回错误；不在 tokio 运行时内调用会 panic。
    pub async fn probe(&self, source: &Source) -> Result<Probe, Error> {
        source.validate()?;
        let http = Http::new(
            source,
            self.requests.clone(),
            None,
            Deadline::Request,
            Redirects::Follow,
        )?;
        probe::probe(&http, source).await
    }

    /// 认出地址 `source.url` 是什么（见 [`Resource`]），不碰任务目录：发一次带 `Range: bytes=0-` 的请求（回 416 或
    /// Content-Range 不对时不带 Range 再发一次），看响应头与内容开头。是播放列表时接着照 [`Engine::probe`] 解析选轨（主
    /// 播放列表还要拉所选轨的媒体播放列表；206 只给了开头一部分时不带 Range 再取一次完整的）；是文件时读到开头几个字节
    /// 即断开，不下载其余内容。请求的用途为 [`Purpose::File`]，失败按 [`HttpOptions::retry`] 重试。服务器回 406（按请求的
    /// Accept 给不出内容，多为只给浏览器看网页的网站）时按在地址栏打开（网页导航的 Accept 等，见 [`HttpOptions::headers`]）
    /// 从头再认一次，结果以这一次为准。参数错误时立即返回错误；不在 tokio 运行时内调用会 panic。
    pub async fn identify(&self, source: &Source) -> Result<Resource, Error> {
        source.validate()?;
        let http = || {
            Http::new(
                source,
                self.requests.clone(),
                None,
                Deadline::Request,
                Redirects::Follow,
            )
        };
        match file::identify(&http()?, source).await {
            Err(Error::Http {
                kind: HttpErrorKind::Status(406),
                ..
            }) => file::identify(&http()?.navigating(), source).await,
            identified => identified,
        }
    }

    /// 取回地址 `source.url` 上的一个小资源（如 BitTorrent 的公共 tracker 列表），不碰任务目录：一次 GET，请求头、
    /// Cookie、代理、证书与超时按 `source.http`，跟随重定向，可重试的失败按 [`HttpOptions::retry`] 重试；请求的用途为
    /// [`Purpose::File`]，[`Hooks::on_request`] 照常调用；不占引擎的请求名额（同 [`Engine::identify`]）。响应体原样给出
    /// （带 `Accept-Encoding: identity`，不解压、不解码），超过 `limit` 字节时读到即停，报 [`Error::ResponseTooLarge`]
    /// （不可重试）；非 2xx 报 [`Error::Http`]。参数错误时立即返回错误；丢弃返回的 future 即取消；不在 tokio 运行时内调
    /// 用会 panic。
    pub async fn fetch(&self, source: &Source, limit: u64) -> Result<Fetched, Error> {
        Source::check_url(&source.url)?;
        source.http.validate()?;
        let http = Http::new(
            source,
            self.requests.clone(),
            None,
            Deadline::Request,
            Redirects::Follow,
        )?;
        fetched::fetch(&http, source, limit).await
    }

    /// 调用地址 `source.url` 上的接口（如网盘的 JSON 接口），不碰任务目录：发出 `request`，任何 HTTP 状态都作为结果返回
    /// （见 [`Reply`]），由调用方看状态与响应体里的错误码决定怎么办。请求头、Cookie、代理、证书与超时按 `source.http`，
    /// 请求的用途为 [`Purpose::Call`]，[`Hooks::on_request`] 照常调用；不占引擎的请求名额（同 [`Engine::identify`]）。
    /// 带 `Accept-Encoding: identity`，响应体原样给出。没给 Cookie 存储（[`HttpOptions::cookies`]）时，响应下发的
    /// Cookie 只在这次调用（含重定向与重试）里有效，不留到下一次调用。
    ///
    /// 只有 GET 跟随重定向；POST、PATCH、DELETE 不跟随，3xx 作为结果返回（不把 POST 改成 GET 重发）。状态一律不重试。
    /// 可重试的传输失败（[`Error::retryable`]）按 [`HttpOptions::retry`] 重试：GET 都重试；其余方法只在确定请求没有发出
    /// （连接、TLS 握手或经代理的隧道没建成）时重试，发出之后的失败不重发（DELETE 重发会拿到 404 之类，盖住第一次的结
    /// 果）。
    ///
    /// 参数错误（见 [`Source::check_url`]、[`HttpOptions::validate`]；GET 带请求体，或带了请求体而请求头里另有
    /// Content-Type）、开始时系统代理用不了与建不出 HTTP 客户端时立即返回那个错误；之后的失败（没拿到完整的响应、回调出
    /// 错、重试时重读系统代理失败、响应体超过 `request.limit`）报 [`Error::Call`]，其中的 [`Sent`] 说明服务器是否
    /// 可能已处理。丢弃返回的 future 即取消；不在 tokio 运行时内调用会 panic。
    pub async fn call(&self, source: &Source, request: &Call) -> Result<Reply, Error> {
        Source::check_url(&source.url)?;
        source.http.validate()?;
        request.validate(&source.http)?;
        let redirects = if request.method.safe() {
            Redirects::Follow
        } else {
            Redirects::Return
        };
        // 简化：每次调用新建客户端，不复用连接；调用频繁到握手成为瓶颈时改为按来源建一次、可反复调用的对象
        let http = Http::new(
            source,
            self.requests.clone(),
            None,
            Deadline::Request,
            redirects,
        )?;
        call::call(&http, source, request).await
    }

    /// 校验文件任务的请求并在当前 tokio 运行时上启动，规则见 [`FileRequest`]；不在 tokio 运行时内调用会 panic。参数错误
    /// 时立即返回错误，不访问文件系统；其余失败经 [`Job::wait`] 返回。
    pub fn start_file(&self, request: FileRequest) -> Result<Job<FileOutput, FileProgress>, Error> {
        let output = request.validate()?;
        let (progress_tx, progress) = watch::channel(FileProgress::default());
        let http = Http::new(
            &request.source,
            self.requests.clone(),
            Some(Arc::new(ProgressObserver::new(progress_tx.clone()))),
            Deadline::Idle,
            Redirects::Follow,
        )?;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(file::run(
            request,
            output,
            http,
            cancel.clone(),
            progress_tx,
        ));
        Ok(Job::new(task, cancel, None, progress))
    }

    /// 校验请求并在当前 tokio 运行时上启动任务；不在 tokio 运行时内调用会 panic。参数错误时立即返回错误，
    /// 不访问文件系统；输出已存在等要读文件系统的检查、建出输出所在的目录在任务开头、任何下载之前进行，结果经
    /// [`Job::wait`] 返回。
    pub fn start(&self, request: JobRequest) -> Result<Job, Error> {
        let output = request.validate()?;
        let (progress_tx, progress) = watch::channel(Progress::default());
        let http = Http::new(
            &request.source,
            self.requests.clone(),
            Some(Arc::new(ProgressObserver::new(progress_tx.clone()))),
            Deadline::Request,
            Redirects::Follow,
        )?;
        let cancel = CancellationToken::new();
        let stop = CancellationToken::new();
        let task = tokio::spawn(job::run(
            request,
            output,
            http,
            cancel.clone(),
            stop.clone(),
            progress_tx,
        ));
        Ok(Job::new(task, cancel, Some(stop), progress))
    }

    /// 不联网，只把任务目录中已录到的直播分片写成输出；用于中断后不再续录、或来源已取不到的录制。与 [`Engine::start`]
    /// 一样返回任务句柄：进度给出已录的分片数、字节数与时长，阶段先为 [`Stage::Preparing`]、写出时为
    /// [`Stage::Writing`]；开始写出前可取消，停止不起作用。参数错误时立即返回错误；不在 tokio 运行时内调用会 panic。
    ///
    /// 任务目录见 [`OutputOptions::resolved_work_dir`]。目录里是点播的下载时报
    /// [`WorkDirProblem::NotLiveRecording`]（点播用同样的请求再次运行即可续传）；没有可合并的分片时报
    /// [`Error::NothingRecorded`]。结果的 [`LiveReport::end`] 为 None。
    pub fn merge_recorded(&self, output: OutputOptions) -> Result<Job, Error> {
        let output = output.resolve()?;
        let (progress_tx, progress) = watch::channel(Progress::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(job::merge_recorded(output, cancel.clone(), progress_tx));
        Ok(Job::new(task, cancel, None, progress))
    }

    /// 查看任务目录 `work_dir`（相对路径按当前目录补全）里已完成的内容，按目录里记录的任务种类给出（HLS 任务或文件任
    /// 务）；不联网、不加锁、不改动任何东西，任务正在运行时为读到的那一刻的。目录或其中的 `job.json` 不存在时为 None。
    /// 用于任务没在运行时显示已下载了多少；点播的总片数要拉播放列表才知道，不在其中。
    ///
    /// 路径为空或不合要求（见 [`Target`]）时报参数错误；目录所在的盘读不到时报 [`Error::VolumeUnavailable`]，但 macOS
    /// 上卷卸下后与目录被删分不出来，这时为 None；`job.json`（或文件任务的 `ranges.json`）是本库读不了的格式版本写的时
    /// 报 [`WorkDirProblem::UnsupportedVersion`]、无法解析时报 [`WorkDirProblem::Corrupt`]。不在 tokio 运行时内调用会
    /// panic。
    pub async fn inspect(&self, work_dir: &Path) -> Result<Option<Saved>, Error> {
        if work_dir.as_os_str().is_empty() {
            return Err(Error::InvalidInput("任务目录路径为空".to_owned()));
        }
        workdir::inspect(output::absolute_path(work_dir)?).await
    }

    /// 放弃任务：删除任务目录 `work_dir`（相对路径按当前目录补全），只删本库写的文件。先按其中的记录收拾写到一半
    /// 的输出：删掉临时输出，挪开的旧输出按开始任务时的规则放回或删掉。返回留下的东西，记录已随任务目录删掉，要用户
    /// 处理：原处是别的东西而没放回的旧输出各一项；收拾不了而按 `unsettled` 记下的；任务目录没删干净（里面有不是本库
    /// 写的文件，或删除出错）时一项 [`LeftoverKind::WorkDir`]。目录不存在时什么也不做。
    ///
    /// 路径为空或不合要求（见 [`Target`]）时报参数错误。任务目录正被任务使用时报 [`WorkDirProblem::Locked`]，不是本库的
    /// 任务目录（没有 `job.json` 且不为空）时报 [`WorkDirProblem::NotEmpty`]。记录（`outputs.json`）是本库读不了的格式
    /// 版本写的时报 [`WorkDirProblem::UnsupportedVersion`]、无法解析时报 [`WorkDirProblem::Corrupt`]，什么也不动：不知
    /// 道它记着什么就不收拾，可用写下它的版本放弃，或由用户删掉记录（写了一半的临时输出与没能放回的旧输出以 `hsdl-` 开
    /// 头，留在输出旁）后再放弃。`job.json` 只用来认出文件任务的数据文件与范围记录（`data`、`ranges.json`）：它是本库读
    /// 不懂的版本写的或无法解析时照常删除其余的，这两个当作不是本库写的留下（[`LeftoverKind::WorkDir`]）。不在 tokio 运
    /// 行时内调用会 panic。
    ///
    /// 分几步做：收拾输出、删各轨的分片、删记录与目录本身。`cancel` 取消时做完进行中的一步就停下、放开锁，报
    /// [`Error::Cancelled`]：记录还在，再放弃即可接着做、照样报出留下的东西；取消落在最后一步进行中时照常做完、返回结
    /// 果。丢弃这个 future 同样停在步与步之间，但进行中的一步在后台做完才放开锁。
    pub async fn discard(
        &self,
        work_dir: &Path,
        unsettled: Unsettled,
        cancel: &CancellationToken,
    ) -> Result<Vec<Leftover>, Error> {
        if work_dir.as_os_str().is_empty() {
            return Err(Error::InvalidInput("任务目录路径为空".to_owned()));
        }
        job::discard(output::absolute_path(work_dir)?, unsettled, cancel).await
    }
}

/// 放弃任务（[`Engine::discard`]）时，任务目录或记录里的输出所在的盘读不到，或收拾输出出错，怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsettled {
    /// 报错（盘读不到时为 [`Error::VolumeUnavailable`]），任务目录与记录都留着，接上或排除后重试
    Fail,
    /// 记为残留，照常放弃：收拾不了的输出旁的两个保留名各一项（盘读不到时核实不了，不一定还在）——临时输出可删；挪开
    /// 的旧输出在全部装上之后可删，之前是没能放回原处的旧输出；任务目录本身读不到时一项 [`LeftoverKind::WorkDir`]。
    /// 用于盘不再接上、用户确认仍要放弃时
    Report,
}

/// 进度里的一个等着重试的请求。
fn retrying(wait: &RetryWaiting) -> Retrying {
    Retrying {
        purpose: wait.purpose,
        failure: wait.failure.clone(),
        retry: wait.retry,
        delay: wait.delay,
    }
}

/// 记着请求进展的进度（[`Progress`]、[`FileProgress`]）。
trait RequestProgress: Send + Sync + 'static {
    /// 本次运行从网络读到的响应体字节数
    fn received(&mut self) -> &mut u64;
    /// 正在退避、等着重试的请求
    fn retrying(&mut self) -> &mut Vec<Retrying>;
}

impl RequestProgress for Progress {
    fn received(&mut self) -> &mut u64 {
        &mut self.received
    }

    fn retrying(&mut self) -> &mut Vec<Retrying> {
        &mut self.retrying
    }
}

impl RequestProgress for FileProgress {
    fn received(&mut self) -> &mut u64 {
        &mut self.received
    }

    fn retrying(&mut self) -> &mut Vec<Retrying> {
        &mut self.retrying
    }
}

/// 请求的进展计入任务的进度：读到的字节计入 `received`，退避计入 `retrying`（见 [`RequestProgress`]）。
struct ProgressObserver<P> {
    progress: watch::Sender<P>,
    /// 进度里 `retrying` 各项的 [`RetryWaiting::id`]，一一对应；在改进度时一并改
    waiting: std::sync::Mutex<Vec<u64>>,
}

impl<P> ProgressObserver<P> {
    fn new(progress: watch::Sender<P>) -> Self {
        ProgressObserver {
            progress,
            waiting: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn waiting(&self) -> std::sync::MutexGuard<'_, Vec<u64>> {
        self.waiting.lock().expect("持锁期间只增删 Vec，不会 panic")
    }
}

impl<P: RequestProgress> Observer for ProgressObserver<P> {
    fn received(&self, len: u64) {
        self.progress.send_modify(|p| *p.received() += len);
    }

    fn retry_waiting(&self, wait: &RetryWaiting) {
        self.progress.send_modify(|p| {
            self.waiting().push(wait.id);
            p.retrying().push(retrying(wait));
        });
    }

    fn retry_waited(&self, wait: &RetryWaiting) {
        self.progress.send_modify(|p| {
            let mut waiting = self.waiting();
            let at = waiting
                .iter()
                .position(|&id| id == wait.id)
                .expect("退避结束之前报过开始");
            waiting.remove(at);
            p.retrying().remove(at);
        });
    }
}

/// 运行中的任务：等结果用 [`Job::wait`]，取消、停止与读进度用 [`Job::control`]。丢弃句柄即取消任务。`O` 为结果，`P` 为
/// 进度：HLS 任务为 [`Output`] 与 [`Progress`]，文件任务为 [`FileOutput`] 与 [`FileProgress`]。
pub struct Job<O = Output, P = Progress> {
    control: JobControl<P>,
    task: JoinHandle<Result<O, Error>>,
    _guard: DropGuard,
}

impl<O, P> Job<O, P> {
    fn new(
        task: JoinHandle<Result<O, Error>>,
        cancel: CancellationToken,
        stop: Option<CancellationToken>,
        progress: watch::Receiver<P>,
    ) -> Self {
        Job {
            control: JobControl {
                cancel: cancel.clone(),
                stop,
                progress,
            },
            task,
            _guard: cancel.drop_guard(),
        }
    }

    /// 控制句柄；与本句柄分开持有，等结果期间也能取消、停止。
    pub fn control(&self) -> JobControl<P> {
        self.control.clone()
    }

    /// 等待任务结束。任务内部 panic（不变量被违反）原样传播；运行时关闭导致任务被取消时返回 [`Error::Cancelled`]。
    pub async fn wait(self) -> Result<O, Error> {
        match self.task.await {
            Ok(result) => result,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => Err(Error::Cancelled),
        }
    }
}

/// 任务的控制句柄，可克隆。只持有它不会让任务继续运行（见 [`Job`]）；任务结束后调用不起作用。
pub struct JobControl<P = Progress> {
    cancel: CancellationToken,
    /// 直播的停止；没有停止的任务（只合并、文件任务）为 None
    stop: Option<CancellationToken>,
    progress: watch::Receiver<P>,
}

impl<P> Clone for JobControl<P> {
    fn clone(&self) -> Self {
        JobControl {
            cancel: self.cancel.clone(),
            stop: self.stop.clone(),
            progress: self.progress.clone(),
        }
    }
}

impl<P> JobControl<P> {
    /// 进度快照的接收端。每读到一块数据就更新一次，按时间显示的消费者应定时采样，不要长时间持有
    /// `borrow()` 的结果（会挡住下载）。任务结束（成功、失败或取消）后发送端关闭，最后一份快照仍读得到；
    /// [`Stage`] 没有失败状态，失败时停在原来的阶段，结果只能从 [`Job::wait`] 取得。
    pub fn progress(&self) -> watch::Receiver<P> {
        self.progress.clone()
    }

    /// 请求取消；[`Job::wait`] 随后返回 [`Error::Cancelled`]，已完成的分片（文件任务为已落盘的范围）保留在任务目录中。
    /// 写出（[`Stage::Writing`]）中取消时删掉写了一半的输出，已有的输出不受影响；换上输出的几步改名不中断，取消晚于它们
    /// 时任务照常成功。
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl JobControl<Progress> {
    /// 直播：停止刷新，把已拉到的播放列表处理完、已列出的分片下完后写出，[`Job::wait`] 返回录到的部分；续录时
    /// 会话还没定下就立即结束，不录新的分片。写出完成之前进程退出时，任务目录里不记停止：用同样的请求再次开始会接着
    /// 录，只写出已录到的用 [`Engine::merge_recorded`]。点播与只合并不受影响。
    pub fn stop(&self) {
        if let Some(stop) = &self.stop {
            stop.cancel();
        }
    }
}
