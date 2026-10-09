//! 任务编排：点播下载、直播录制、直播只合并三条流程，共用合并与收尾。

use std::sync::Arc;

use hs_m3u8_remux::{DiscontinuityGroup, Streams};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::fetch::Fetcher;
use crate::http::Http;
use crate::ident::{source_digest, url_digest};
use crate::live::{self, Context, Recording};
use crate::request::{JobRequest, Resume, check_output};
use crate::resolve::{self, Resolved};
use crate::vod::{self, Plan};
use crate::workdir::{JobRecord, RecordKind, Stored, WorkDir, read_resumable};
use crate::{
    Error, LiveEnd, LiveReport, Output, Progress, Stage, Unsupported, WorkDirProblem, blocking,
};

/// 一次运行用到的共享对象。
struct Task {
    request: JobRequest,
    http: Arc<Http>,
    cancel: CancellationToken,
    stop: CancellationToken,
    progress: watch::Sender<Progress>,
}

pub(crate) async fn run(
    request: JobRequest,
    http: Http,
    cancel: CancellationToken,
    stop: CancellationToken,
    progress: watch::Sender<Progress>,
) -> Result<Output, Error> {
    let task = Task {
        request,
        http: Arc::new(http),
        cancel,
        stop,
        progress,
    };
    let request = &task.request;
    if request
        .live
        .is_some_and(|live| live.resume == Resume::MergeOnly)
    {
        return merge_only(task).await;
    }
    let root = request.resolved_work_dir();
    let source = source_digest(&request.url, &request.preference);
    // 任务目录里有同一来源、可续的记录时，按记录的选轨找回同一条轨；记录的是直播且请求开启了直播时按直播继续，
    // 即使播放列表已出现 ENDLIST（中断期间直播结束了）。没开启直播时按点播运行，由 WorkDir::open 报类型不符
    let recorded = read_resumable(root.clone())
        .await?
        .filter(|r| r.source_digest == source);
    let continuing_live = recorded
        .as_ref()
        .is_some_and(|r| matches!(r.kind, RecordKind::Live { .. }));
    let selection = recorded.as_ref().and_then(|r| r.selection.as_ref());
    let resolved = resolve::resolve(&task.http, request, selection, &task.cancel)
        .await?
        .ok_or(Error::WorkDir {
            path: root,
            problem: WorkDirProblem::SelectionGone,
        })?;
    if resolved.is_live() || continuing_live && request.live.is_some() {
        run_live(task, source, resolved).await
    } else {
        run_vod(task, source, resolved).await
    }
}

/// 直播：录制，再合并任务目录中录到的全部分片。
async fn run_live(task: Task, source: String, resolved: Resolved) -> Result<Output, Error> {
    let options = task
        .request
        .live
        .ok_or(Error::Unsupported(Unsupported::Live))?;
    let record = JobRecord {
        source_digest: source,
        selection: resolved.selection.clone(),
        kind: RecordKind::Live {
            url_digest: url_digest(&task.request.url),
        },
    };
    let dir = WorkDir::open(task.request.resolved_work_dir(), record.clone()).await?;
    let mut fetcher = task.fetcher();
    let ctx = Context {
        http: task.http.clone(),
        hooks: task.request.hooks.clone(),
        dir: &dir,
        fetcher: &mut fetcher,
        progress: &task.progress,
        cancel: &task.cancel,
        stop: &task.stop,
    };
    let recording = live::record(ctx, resolved.tracks, options, &record).await?;
    let streams = record.streams();
    let stored = dir.scan(streams.len()).await?;
    let input = merge_live(&dir, &stored, streams, Some(recording))?;
    task.finish(dir, input).await
}

/// 点播：下载计划中尚未完成的部分，再合并。
async fn run_vod(task: Task, source: String, resolved: Resolved) -> Result<Output, Error> {
    let streams = resolved.streams();
    let selection = resolved.selection.clone();
    let plan = Plan::new(resolved.tracks)?;
    let record = JobRecord {
        source_digest: source,
        selection,
        kind: RecordKind::Vod {
            plan_digest: plan.digest(),
        },
    };
    let dir = WorkDir::open(task.request.resolved_work_dir(), record.clone()).await?;
    let mut fetcher = task.fetcher();
    let groups = vod::download(
        &task.http,
        &mut fetcher,
        &plan,
        dir.layout(),
        &task.progress,
        &task.cancel,
    )
    .await?;
    let input = MergeInput {
        streams,
        groups,
        segments: plan.segment_count(),
        live: None,
    };
    task.finish(dir, input).await
}

/// 不联网，只合并任务目录中已录到的直播分片。
async fn merge_only(task: Task) -> Result<Output, Error> {
    let request = &task.request;
    let root = request.resolved_work_dir();
    let Some(record) = read_resumable(root.clone()).await? else {
        return Err(Error::NothingRecorded);
    };
    if let RecordKind::Vod { .. } = record.kind {
        return Err(Error::WorkDir {
            path: root,
            problem: WorkDirProblem::NotLiveRecording,
        });
    }
    if record.source_digest != source_digest(&request.url, &request.preference) {
        return Err(Error::WorkDir {
            path: root,
            problem: WorkDirProblem::SourceMismatch,
        });
    }
    let dir = WorkDir::open(root, record.clone()).await?;
    let streams = record.streams();
    let stored = dir.scan(streams.len()).await?;
    live::count_stored(&stored, &task.progress);
    let input = merge_live(&dir, &stored, streams, None)?;
    task.finish(dir, input).await
}

/// 合并直播录到的分片；`recording` 为本次运行的录制，只合并时为 None。
fn merge_live(
    dir: &WorkDir,
    stored: &Stored,
    streams: Vec<Streams>,
    recording: Option<Recording>,
) -> Result<MergeInput, Error> {
    let plan = live::merge_plan(&stored.segments, dir.layout())?;
    if plan.groups.is_empty() {
        return Err(Error::NothingRecorded);
    }
    let (end, known) = match recording {
        Some(r) => (r.end, r.missed),
        None => (LiveEnd::MergeOnly, Vec::new()),
    };
    let report = LiveReport {
        end,
        session_count: plan.sessions,
        missed: live::report_missed(&plan, known),
    };
    Ok(MergeInput {
        streams,
        segments: plan.segments,
        groups: plan.groups,
        live: Some(report),
    })
}

/// 交给合并的内容。
struct MergeInput {
    streams: Vec<Streams>,
    groups: Vec<DiscontinuityGroup>,
    /// 合并进输出的分片数，各轨合计
    segments: usize,
    live: Option<LiveReport>,
}

impl Task {
    fn fetcher(&self) -> Fetcher {
        Fetcher::new(
            self.http.clone(),
            self.request.hooks.clone(),
            self.request.concurrency,
            &self.cancel,
        )
    }

    /// 合并为输出文件，按选项删除任务目录。
    async fn finish(self, dir: WorkDir, input: MergeInput) -> Result<Output, Error> {
        let MergeInput {
            streams,
            groups,
            segments,
            live,
        } = input;
        let request = self.request;
        // 简化：合并阶段不响应取消（remux 不可中断），合并耗时成为问题时给 remux 加 FFmpeg 中断回调。
        if self.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        // 下载或录制期间输出路径可能已被别人占用；开始时检查过，这里再查一次
        check_output(&request.output, request.overwrite)?;
        create_parent(&request.output).await?;
        self.progress.send_modify(|p| p.stage = Stage::Merging);
        let output = request.output.clone();
        let report = blocking(move || hs_m3u8_remux::remux(&streams, &groups, &output)).await??;

        let cleanup_error = if request.keep_work_dir {
            None
        } else {
            dir.remove().await.err().map(|e| e.to_string())
        };
        self.progress.send_modify(|p| p.stage = Stage::Done);
        let bytes = self.progress.borrow().bytes;
        Ok(Output {
            path: request.output,
            report,
            segments,
            bytes,
            cleanup_error,
            live,
        })
    }
}

async fn create_parent(output: &std::path::Path) -> Result<(), Error> {
    let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    let parent = parent.to_path_buf();
    blocking(move || {
        std::fs::create_dir_all(&parent).map_err(|cause| Error::Io {
            action: "创建",
            path: parent,
            cause,
        })
    })
    .await?
}
