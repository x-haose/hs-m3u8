//! HLS 下载引擎：选轨、并发拉取分片、AES-128 解密与校验、可续传的任务目录、直播录制，输出为 MP4、可直接播放的
//! 本地 HLS 目录，或两者都要。
//!
//! 一个任务把一个播放列表地址变成它的输出（[`Target`]）。任务目录（默认与输出同级、名为输出的主名加 `.hsdl`）保存
//! `job.json` 与已完成的分片；分片先写 `.part`、落盘后原子改名，因此「最终文件存在」即「该分片完整」。中断后用同样的请
//! 求再次运行：点播续传缺的分片（播放列表变了则拒绝），直播续录（见 [`LiveOptions`]）；不联网、只合并已录到的直播用
//! [`Engine::merge_recorded`]。
//!
//! 参数错误由 [`Engine::start`]、[`Engine::merge_recorded`] 立即返回，其余失败经 [`Job::wait`] 的 `Err` 返回；任务失败
//! 或取消时不生成输出，任务目录保留以便续传。
//!
//! 结果与错误里的轨道编号（`track`）：第 0 条为所选变体（来源本身是媒体播放列表时即它），第 1 条（若有）为
//! 独立的音频 rendition。

mod blocking;
mod busy;
mod cookies;
mod crypto;
mod entries;
mod error;
mod fetch;
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

pub use cookies::{CookieHeader, CookieStore};
pub use error::{
    Error, FetchError, FetchFailure, HlsUnsupported, HttpError, HttpFailure, Integrity, JobType,
    StallError, Unsupported, WorkDirProblem,
};
pub use hooks::{HookError, HookKind, Hooks, NoHooks, Purpose, RequestParts};
/// 内容放不进 MP4 的原因（[`Unsupported::Mp4`]）；本地 HLS 表示不了的见 [`HlsUnsupported`]。
pub use hs_m3u8_remux::Unsupported as Mp4Unsupported;
pub use ident::bare_url;
pub use info::{AudioInfo, MasterInfo, Selected, TrackInfo, VariantInfo};
pub use output::{OutputOptions, Target};
pub use probe::Probe;
pub use proxy::{Bypass, ProxySource, SystemProxy, system_proxy};
pub use report::{
    FileCount, Leftover, LeftoverKind, LiveEnd, LiveReport, MissReason, Missed, Mp4Output,
    NewSession, Output, Progress, RefreshCause, Retrying, SegmentCounts, SourceBreak, Stage,
    StallCause, Writing,
};
pub use request::{
    HttpOptions, JobRequest, KeyOverride, LiveOptions, Proxy, RetryPolicy, Source, Timeouts,
};
pub use tokio_util::sync::CancellationToken;
pub use url::Url;

/// 公开接口用到的合并报告与合并错误的类型。
pub mod remux {
    pub use hs_m3u8_remux::{Error, FfmpegError, Report, Shape, StreamKind, StreamReport};
}

/// 公开接口用到的播放列表解析与选轨类型。
pub mod hls {
    pub use hs_m3u8_hls::{
        AudioChoice, AudioKey, Encryption, Malformed, Preference, Resolution, SelectError,
        SyntaxError, VariantChoice, VariantKey,
    };
}

pub(crate) use blocking::blocking;

use crate::http::{Http, Observer, Permits, RetryWaiting};

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

    /// `max_requests`：所有点播任务合计同时在途的 key、分片与 init 段请求数；所有直播任务的 key 与分片另有同样大小的一
    /// 份，不与点播共用：直播的分片排在点播的大批下载（或挂住的请求）之后会滑出窗口、永久缺失。播放列表与刷新时拉的
    /// init 段（连同它的 key）不占名额：每条轨同时最多一个，刷新不排在本任务的分片下载之后；同一个 key 正被分片的请求拉
    /// 取时，等那次请求（它占着名额）。
    pub fn new(max_requests: NonZeroUsize) -> Self {
        Engine {
            requests: Permits::new(max_requests.get()),
        }
    }

    /// 拉取来源并按偏好选轨，不下载分片、不碰任务目录；请求与回调同下载。用于开始下载前列出可选的清晰度与
    /// 音轨、查看时长与是否直播。参数错误时立即返回错误；不在 tokio 运行时内调用会 panic。
    pub async fn probe(&self, source: &Source) -> Result<Probe, Error> {
        source.validate()?;
        let http = Http::new(source, self.requests.clone(), None)?;
        probe::probe(&http, source).await
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
            Some(Arc::new(ProgressObserver(progress_tx.clone()))),
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
        Ok(Job::new(task, cancel, stop, progress))
    }

    /// 不联网，只把任务目录中已录到的直播分片写成输出；用于中断后不再续录、或来源已取不到的录制。与 [`Engine::start`]
    /// 一样返回任务句柄：进度给出已录的分片数、字节数与时长，阶段先为 [`Stage::Preparing`]、写出时为
    /// [`Stage::Writing`]；开始写出前可取消，停止不起作用。参数错误时立即返回错误；不在 tokio 运行时内调用会 panic。
    ///
    /// 任务目录见 [`OutputOptions::resolved_work_dir`]。目录里是点播的下载时报
    /// [`WorkDirProblem::NotLiveRecording`]（点播用同样的请求再次运行即可续传）；目录不存在或没有已完成的分片时报
    /// [`Error::NothingRecorded`]。结果的 [`LiveReport::end`] 为 None。
    pub fn merge_recorded(&self, output: OutputOptions) -> Result<Job, Error> {
        let output = output.resolve()?;
        let (progress_tx, progress) = watch::channel(Progress::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(job::merge_recorded(output, cancel.clone(), progress_tx));
        Ok(Job::new(task, cancel, CancellationToken::new(), progress))
    }

    /// 放弃任务：删除任务目录 `work_dir`（相对路径按当前目录补全），只删本库写的文件。先按其中的记录收拾写到一半
    /// 的输出：删掉临时输出，挪开的旧输出按开始任务时的规则放回或删掉。返回留下的东西，记录已随任务目录删掉，要用户
    /// 处理：原处是别的东西而没放回的旧输出各一项；收拾不了而按 `unsettled` 记下的；任务目录没删干净（里面有不是本库
    /// 写的文件，或删除出错）时一项 [`LeftoverKind::WorkDir`]。目录不存在时什么也不做。
    ///
    /// 路径为空或不合要求（见 [`Target`]）时报参数错误。任务目录正被任务使用时报 [`WorkDirProblem::Locked`]，不是
    /// 本库的任务目录（没有 `job.json` 且不为空）时报 [`WorkDirProblem::NotEmpty`]。记录（`outputs.json`）是另一个
    /// 格式版本写的时报 [`WorkDirProblem::UnsupportedVersion`]、无法解析时报 [`WorkDirProblem::Corrupt`]，什么也不动：
    /// 不知道它记着什么就不收拾，可用写下它的版本放弃，或由用户删掉记录（写了一半的临时输出与没能放回的旧输出以
    /// `hsdl-` 开头，留在输出旁）后再放弃。`job.json` 不读：它是另一个版本写的也照常删除。不在 tokio 运行时内调用会
    /// panic。
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

/// 请求的进展计入任务的进度：读到的字节计入 [`Progress::received`]，退避计入 [`Progress::retrying`]。
struct ProgressObserver(watch::Sender<Progress>);

impl Observer for ProgressObserver {
    fn received(&self, len: u64) {
        self.0.send_modify(|p| p.received += len);
    }

    fn retry_waiting(&self, wait: &RetryWaiting) {
        self.0.send_modify(|p| p.retrying.push(retrying(wait)));
    }

    /// 去掉与 `wait` 相同的一项：相同的几项分不出是哪个请求的，去掉哪个结果都一样。
    fn retry_waited(&self, wait: &RetryWaiting) {
        let ended = retrying(wait);
        self.0.send_modify(|p| {
            let at = p
                .retrying
                .iter()
                .position(|r| *r == ended)
                .expect("退避结束之前报过开始");
            p.retrying.remove(at);
        });
    }
}

/// 运行中的任务：等结果用 [`Job::wait`]，取消、停止与读进度用 [`Job::control`]。丢弃句柄即取消任务。
pub struct Job {
    control: JobControl,
    task: JoinHandle<Result<Output, Error>>,
    _guard: DropGuard,
}

impl Job {
    fn new(
        task: JoinHandle<Result<Output, Error>>,
        cancel: CancellationToken,
        stop: CancellationToken,
        progress: watch::Receiver<Progress>,
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
    pub fn control(&self) -> JobControl {
        self.control.clone()
    }

    /// 等待任务结束。任务内部 panic（不变量被违反）原样传播；运行时关闭导致任务被取消时返回 [`Error::Cancelled`]。
    pub async fn wait(self) -> Result<Output, Error> {
        match self.task.await {
            Ok(result) => result,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => Err(Error::Cancelled),
        }
    }
}

/// 任务的控制句柄，可克隆。只持有它不会让任务继续运行（见 [`Job`]）；任务结束后调用不起作用。
#[derive(Clone)]
pub struct JobControl {
    cancel: CancellationToken,
    stop: CancellationToken,
    progress: watch::Receiver<Progress>,
}

impl JobControl {
    /// 进度快照的接收端。每读到一块数据就更新一次，按时间显示的消费者应定时采样，不要长时间持有
    /// `borrow()` 的结果（会挡住下载）。任务结束（成功、失败或取消）后发送端关闭，最后一份快照仍读得到；
    /// [`Stage`] 没有失败状态，失败时停在原来的阶段，结果只能从 [`Job::wait`] 取得。
    pub fn progress(&self) -> watch::Receiver<Progress> {
        self.progress.clone()
    }

    /// 请求取消；[`Job::wait`] 随后返回 [`Error::Cancelled`]，已完成的分片保留在任务目录中。写出（[`Stage::Writing`]）
    /// 中取消时删掉写了一半的输出，已有的输出不受影响；换上输出的几步改名不中断，取消晚于它们时任务照常成功。
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// 直播：停止刷新，把已拉到的播放列表处理完、已列出的分片下完后写出，[`Job::wait`] 返回录到的部分；续录时
    /// 会话还没定下就立即结束，不录新的分片。写出完成之前进程退出时，任务目录里不记停止：用同样的请求再次开始会接着
    /// 录，只写出已录到的用 [`Engine::merge_recorded`]。点播与只合并不受影响。
    pub fn stop(&self) {
        self.stop.cancel();
    }
}
