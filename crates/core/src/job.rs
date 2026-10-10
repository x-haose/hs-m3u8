//! 任务编排：点播下载、直播录制、直播只合并三条流程，共用合并与收尾。

use std::sync::Arc;

use hs_m3u8_remux::Streams;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::fetch::Fetcher;
use crate::http::Http;
use crate::ident::{source_digest, url_digest};
use crate::live::{self, Context, Outcome};
use crate::output::OutputOptions;
use crate::output::{self, Content};
use crate::request::JobRequest;
use crate::resolve::{self, Resolved};
use crate::vod::{self, Plan};
use crate::workdir::{JobRecord, RecordKind, Stored, WorkDir, read_resumable};
use crate::{Error, LiveReport, Output, Progress, Stage, Unsupported, WorkDirProblem};

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
    let root = request.output.resolved_work_dir();
    let source = source_digest(&request.source.url, &request.source.preference);
    // 任务目录里有同一来源、可续的记录时，按记录的选轨找回同一条轨；记录的是直播且请求开启了直播时按直播继续，
    // 即使播放列表已出现 ENDLIST（中断期间直播结束了）。没开启直播时按点播运行，由 WorkDir::open 报类型不符
    let recorded = read_resumable(root.clone())
        .await?
        .filter(|r| r.source_digest == source);
    let continuing_live = recorded
        .as_ref()
        .is_some_and(|r| matches!(r.kind, RecordKind::Live { .. }));
    let selection = recorded.as_ref().and_then(|r| r.selection.as_ref());
    let resolved = resolve::resolve(&task.http, &request.source, selection, &task.cancel)
        .await?
        .ok_or(Error::WorkDir {
            path: root,
            problem: WorkDirProblem::SelectionGone,
        })?;
    let selection = resolved
        .master
        .as_ref()
        .map(|m| Arc::new(m.selection.clone()));
    task.progress.send_modify(|p| p.selection = selection);
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
        selection: resolved.selection_key(),
        kind: RecordKind::Live {
            url_digest: url_digest(&task.request.source.url),
        },
    };
    let dir = WorkDir::open(task.request.output.resolved_work_dir(), record).await?;
    let mut fetcher = task.fetcher();
    let ctx = Context {
        http: task.http.clone(),
        hooks: task.request.source.hooks.clone(),
        dir: &dir,
        fetcher: &mut fetcher,
        progress: &task.progress,
        cancel: &task.cancel,
        stop: &task.stop,
    };
    let recording = live::record(ctx, resolved.tracks, options).await?;
    let streams = dir.record().streams();
    let stored = dir.scan(streams.len()).await?;
    let input = merge_live(&dir, &stored, streams, Some(recording))?;
    task.finish(dir, input).await
}

/// 点播：下载计划中尚未完成的部分，再合并。
async fn run_vod(task: Task, source: String, resolved: Resolved) -> Result<Output, Error> {
    let streams = resolved.streams();
    let selection = resolved.selection_key();
    let plan = Plan::new(resolved.tracks)?;
    let record = JobRecord {
        source_digest: source,
        selection,
        kind: RecordKind::Vod {
            plan_digest: plan.digest(),
        },
    };
    let dir = WorkDir::open(task.request.output.resolved_work_dir(), record).await?;
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
    let input = Content {
        streams,
        groups,
        segments: plan.segment_count(),
        selection: dir.record().selection.clone(),
        live: None,
    };
    task.finish(dir, input).await
}

/// 不联网，只合并任务目录中已录到的直播分片。
pub(crate) async fn merge_recorded(output: OutputOptions) -> Result<Output, Error> {
    output.validate()?;
    let root = output.resolved_work_dir();
    let Some(record) = read_resumable(root.clone()).await? else {
        return Err(Error::NothingRecorded);
    };
    if let RecordKind::Vod { .. } = record.kind {
        return Err(Error::WorkDir {
            path: root,
            problem: WorkDirProblem::NotLiveRecording,
        });
    }
    let dir = WorkDir::open(root, record).await?;
    let streams = dir.record().streams();
    let stored = dir.scan(streams.len()).await?;
    let input = merge_live(&dir, &stored, streams, None)?;
    output::write(dir, input, &output, stored.bytes()).await
}

/// 合并直播录到的分片；`recording` 为本次运行的录制，只合并时为 None。
fn merge_live(
    dir: &WorkDir,
    stored: &Stored,
    streams: Vec<Streams>,
    recording: Option<Outcome>,
) -> Result<Content, Error> {
    let plan = live::merge_plan(&stored.segments, dir.layout())?;
    if plan.groups.is_empty() {
        return Err(Error::NothingRecorded);
    }
    let (end, known) = match recording {
        Some(r) => (Some(r.end), r.missed),
        None => (None, Vec::new()),
    };
    let report = LiveReport {
        end,
        session_count: plan.sessions,
        missed: live::report_missed(&plan, known),
    };
    Ok(Content {
        streams,
        segments: plan.segments,
        groups: plan.groups,
        selection: dir.record().selection.clone(),
        live: Some(report),
    })
}

impl Task {
    fn fetcher(&self) -> Fetcher {
        Fetcher::new(
            self.http.clone(),
            self.request.source.hooks.clone(),
            self.request.key,
            self.request.concurrency,
            &self.cancel,
        )
    }

    /// 合并为输出文件，按选项删除任务目录。
    async fn finish(self, dir: WorkDir, input: Content) -> Result<Output, Error> {
        // 简化：合并阶段不响应取消（remux 不可中断），合并耗时成为问题时给 remux 加 FFmpeg 中断回调。
        if self.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.progress.send_modify(|p| p.stage = Stage::Merging);
        let bytes = self.progress.borrow().bytes;
        let output = output::write(dir, input, &self.request.output, bytes).await?;
        self.progress.send_modify(|p| p.stage = Stage::Done);
        Ok(output)
    }
}
