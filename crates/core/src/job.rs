//! 任务编排：点播下载、直播录制、直播只合并三条流程，共用合并与收尾。

use std::sync::Arc;

use hs_m3u8_remux::{DiscontinuityGroup, Streams};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::fetch::Fetcher;
use crate::http::Http;
use crate::ident::{source_digest, url_digest};
use crate::live::{self, Context, Recording};
use crate::plan::Plan;
use crate::request::{JobRequest, Resume, check_output};
use crate::resolve::{self, Resolved};
use crate::workdir::{JobRecord, RecordKind, Stored, WorkDir, read_record};
use crate::{
    Error, LiveEnd, LiveReport, Output, Progress, Stage, Unsupported, WorkDirProblem, blocking, vod,
};

pub(crate) async fn run(
    request: JobRequest,
    http: Http,
    cancel: CancellationToken,
    stop: CancellationToken,
    progress: watch::Sender<Progress>,
) -> Result<Output, Error> {
    if request
        .live
        .is_some_and(|live| live.resume == Resume::MergeOnly)
    {
        return merge_only(request, &cancel, &progress).await;
    }
    let source = source_digest(&request.url, &request.preference);
    // 任务目录里有同一来源的记录时，按记录的选轨找回同一条轨；记录的是直播时按直播继续，
    // 即使播放列表已出现 ENDLIST（中断期间直播结束了）
    let recorded = read_record(request.resolved_work_dir())
        .await?
        .filter(|r| r.source == source);
    let continuing_live = recorded
        .as_ref()
        .is_some_and(|r| matches!(r.kind, RecordKind::Live { .. }));
    let http = Arc::new(http);
    let selection = recorded.as_ref().and_then(|r| r.selection.as_ref());
    let resolved = resolve::resolve(&http, &request, selection, &cancel).await?;
    let mut fetcher = Fetcher::new(
        http.clone(),
        request.hooks.clone(),
        request.concurrency,
        &cancel,
    );
    if continuing_live || resolved.is_live() {
        let options = request.live.ok_or(Error::Unsupported(Unsupported::Live))?;
        let record = job_record(
            source,
            &resolved,
            RecordKind::Live {
                url: url_digest(&request.url),
            },
        );
        let dir = WorkDir::open(request.resolved_work_dir(), record.clone()).await?;
        let ctx = Context {
            http,
            hooks: request.hooks.clone(),
            dir: &dir,
            fetcher: &mut fetcher,
            progress: &progress,
            cancel: &cancel,
            stop: &stop,
        };
        let recording = live::record(ctx, resolved.tracks, options, &record).await?;
        let stored = dir.scan(record.streams.len()).await?;
        let merge = merge_live(&dir, &stored, record.streams, Some(recording))?;
        finish(request, dir, merge, &cancel, &progress).await
    } else {
        let streams = resolved.tracks.iter().map(|t| t.streams).collect();
        let selection = resolved.selection.clone();
        let plan = Plan::new(resolved.tracks)?;
        let record = JobRecord {
            source,
            selection,
            streams,
            kind: RecordKind::Vod {
                plan: plan.digest(),
            },
        };
        let dir = WorkDir::open(request.resolved_work_dir(), record.clone()).await?;
        let groups =
            vod::download(&http, &mut fetcher, &plan, dir.layout(), &progress, &cancel).await?;
        let merge = Merge {
            streams: record.streams,
            groups,
            segments: plan.segment_count(),
            live: None,
        };
        finish(request, dir, merge, &cancel, &progress).await
    }
}

fn job_record(source: String, resolved: &Resolved, kind: RecordKind) -> JobRecord {
    JobRecord {
        source,
        selection: resolved.selection.clone(),
        streams: resolved.tracks.iter().map(|t| t.streams).collect(),
        kind,
    }
}

/// 不联网，只合并任务目录中已录到的直播分片。
async fn merge_only(
    request: JobRequest,
    cancel: &CancellationToken,
    progress: &watch::Sender<Progress>,
) -> Result<Output, Error> {
    let root = request.resolved_work_dir();
    let Some(record) = read_record(root.clone())
        .await?
        .filter(|r| matches!(r.kind, RecordKind::Live { .. }))
    else {
        return Err(Error::NothingRecorded);
    };
    if record.source != source_digest(&request.url, &request.preference) {
        return Err(Error::WorkDir {
            path: root,
            problem: WorkDirProblem::SourceMismatch,
        });
    }
    let dir = WorkDir::open(root, record.clone()).await?;
    let stored = dir.scan(record.streams.len()).await?;
    live::count_stored(&stored, progress);
    let merge = merge_live(&dir, &stored, record.streams, None)?;
    finish(request, dir, merge, cancel, progress).await
}

/// 合并直播录到的分片；`recording` 为本次运行的录制，只合并时为 None。
fn merge_live(
    dir: &WorkDir,
    stored: &Stored,
    streams: Vec<Streams>,
    recording: Option<Recording>,
) -> Result<Merge, Error> {
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
    Ok(Merge {
        streams,
        segments: plan.segments,
        groups: plan.groups,
        live: Some(report),
    })
}

/// 交给合并的内容。
struct Merge {
    streams: Vec<Streams>,
    groups: Vec<DiscontinuityGroup>,
    /// 合并进输出的分片数，各轨合计
    segments: usize,
    live: Option<LiveReport>,
}

/// 合并为输出文件，按选项删除任务目录。
async fn finish(
    request: JobRequest,
    dir: WorkDir,
    merge: Merge,
    cancel: &CancellationToken,
    progress: &watch::Sender<Progress>,
) -> Result<Output, Error> {
    let Merge {
        streams,
        groups,
        segments,
        live,
    } = merge;
    // 简化：合并阶段不响应取消（remux 不可中断），合并耗时成为问题时给 remux 加 FFmpeg 中断回调。
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    // 下载或录制期间输出路径可能已被别人占用；开始时检查过，这里再查一次
    check_output(&request.output, request.overwrite)?;
    create_parent(&request.output).await?;
    progress.send_modify(|p| p.stage = Stage::Merging);
    let output = request.output.clone();
    let report = blocking(move || hs_m3u8_remux::remux(&streams, &groups, &output)).await??;

    let cleanup_error = if request.keep_work_dir {
        None
    } else {
        dir.remove().await.err().map(|e| e.to_string())
    };
    progress.send_modify(|p| p.stage = Stage::Done);
    let bytes = progress.borrow().bytes;
    Ok(Output {
        path: request.output,
        report,
        segments,
        bytes,
        cleanup_error,
        live,
    })
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
