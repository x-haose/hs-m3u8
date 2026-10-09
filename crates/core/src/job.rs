//! 任务编排：点播下载、直播录制、直播只合并三条流程，共用合并与收尾。

use std::sync::Arc;

use hs_m3u8_remux::{DiscontinuityGroup, Streams};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::fetch::Fetcher;
use crate::http::Http;
use crate::live::{self, Context, Recording};
use crate::plan::{Plan, request_digest};
use crate::request::{JobRequest, Resume, check_output};
use crate::resolve::{self, ResolvedTrack};
use crate::workdir::{JobKind, WorkDir, read_kind};
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
    // 任务目录是同一来源的直播录制时按直播继续，即使播放列表已出现 ENDLIST（中断期间直播结束了）
    let continuing_live = matches!(
        read_kind(request.resolved_work_dir()).await?,
        Some(JobKind::Live { request_digest: recorded, .. }) if recorded == request_digest(&request)
    );
    let http = Arc::new(http);
    let tracks = resolve::resolve(&http, &request, &cancel).await?;
    let mut fetcher = Fetcher::new(
        http.clone(),
        request.hooks.clone(),
        request.concurrency,
        &cancel,
    );
    if continuing_live || resolve::is_live(&tracks) {
        record_live(
            request,
            http,
            tracks,
            &mut fetcher,
            &cancel,
            &stop,
            &progress,
        )
        .await
    } else {
        download_vod(request, tracks, &mut fetcher, &cancel, &progress).await
    }
}

async fn download_vod(
    request: JobRequest,
    tracks: Vec<ResolvedTrack>,
    fetcher: &mut Fetcher,
    cancel: &CancellationToken,
    progress: &watch::Sender<Progress>,
) -> Result<Output, Error> {
    let plan = Arc::new(Plan::new(tracks)?);
    let kind = JobKind::Vod {
        plan_digest: plan.digest(),
    };
    let dir = WorkDir::open(request.resolved_work_dir(), kind).await?;
    vod::download(fetcher, plan.clone(), dir.layout(), progress).await?;
    let input = MergeInput {
        streams: plan.tracks.iter().map(|t| t.streams).collect(),
        groups: vod::merge_input(&plan, dir.layout()),
        segments: plan.segment_count(),
        live: None,
    };
    finish(request, dir, input, cancel, progress).await
}

async fn record_live(
    request: JobRequest,
    http: Arc<Http>,
    tracks: Vec<ResolvedTrack>,
    fetcher: &mut Fetcher,
    cancel: &CancellationToken,
    stop: &CancellationToken,
    progress: &watch::Sender<Progress>,
) -> Result<Output, Error> {
    let options = request.live.ok_or(Error::Unsupported(Unsupported::Live))?;
    let streams: Vec<Streams> = tracks.iter().map(|t| t.streams).collect();
    let kind = JobKind::Live {
        request_digest: request_digest(&request),
        streams: streams.clone(),
    };
    let dir = WorkDir::open(request.resolved_work_dir(), kind).await?;
    let layout = dir.layout();
    let earlier = layout.completed_segments(streams.len()).await?;
    let inits = layout.completed_inits(streams.len()).await?;
    count_existing(&earlier, &inits, progress);
    let session = earlier
        .iter()
        .flatten()
        .map(|f| f.name.session + 1)
        .max()
        .unwrap_or(0);
    let ctx = Context {
        http,
        hooks: request.hooks.clone(),
        layout,
        fetcher,
        progress,
        cancel,
        stop,
    };
    let recording = live::record(ctx, tracks, options, session, inits).await?;
    merge_live(request, dir, streams, Some(recording), cancel, progress).await
}

/// 不联网，只合并任务目录中已录到的直播分片。
async fn merge_only(
    request: JobRequest,
    cancel: &CancellationToken,
    progress: &watch::Sender<Progress>,
) -> Result<Output, Error> {
    let root = request.resolved_work_dir();
    let Some(kind @ JobKind::Live { .. }) = read_kind(root.clone()).await? else {
        return Err(Error::NothingRecorded);
    };
    let JobKind::Live {
        request_digest: recorded,
        streams,
    } = &kind
    else {
        unreachable!("上面已匹配为直播")
    };
    if *recorded != request_digest(&request) {
        return Err(Error::WorkDir {
            path: root,
            problem: WorkDirProblem::SourceMismatch,
        });
    }
    let streams = streams.clone();
    let dir = WorkDir::open(root, kind).await?;
    let layout = dir.layout();
    let files = layout.completed_segments(streams.len()).await?;
    let inits = layout.completed_inits(streams.len()).await?;
    count_existing(&files, &inits, progress);
    merge_live(request, dir, streams, None, cancel, progress).await
}

/// 续传前已完成的分片与 init 段计入进度。
fn count_existing(
    segments: &[Vec<crate::workdir::SegmentFile>],
    inits: &[Vec<crate::workdir::InitFile>],
    progress: &watch::Sender<Progress>,
) {
    let done = segments.iter().map(Vec::len).sum();
    let bytes = segments.iter().flatten().map(|f| f.len).sum::<u64>()
        + inits.iter().flatten().map(|f| f.len).sum::<u64>();
    progress.send_modify(|p| {
        p.segments_done = done;
        p.segments_total = done;
        p.bytes = bytes;
    });
}

/// 合并直播录到的分片；`recording` 为本次运行的录制，只合并时为 None。
async fn merge_live(
    request: JobRequest,
    dir: WorkDir,
    streams: Vec<Streams>,
    recording: Option<Recording>,
    cancel: &CancellationToken,
    progress: &watch::Sender<Progress>,
) -> Result<Output, Error> {
    let files = dir.layout().completed_segments(streams.len()).await?;
    let current = recording.as_ref().map(|r| r.session);
    let (groups, mut missed) = live::merge_input(&files, dir.layout(), current)?;
    if groups.is_empty() {
        return Err(Error::NothingRecorded);
    }
    let (end, recorded_missed) = match recording {
        Some(r) => (r.end, r.missed),
        None => (LiveEnd::MergeOnly, Vec::new()),
    };
    missed.extend(recorded_missed);
    let sessions: std::collections::BTreeSet<u32> =
        files.iter().flatten().map(|f| f.name.session).collect();
    let report = LiveReport {
        end,
        sessions: u32::try_from(sessions.len()).expect("录制次数不超过 u32"),
        missed: live::merge_ranges(missed),
    };
    let input = MergeInput {
        segments: groups
            .iter()
            .flat_map(|g| &g.tracks)
            .map(|t| t.segments.len())
            .sum(),
        streams,
        groups,
        live: Some(report),
    };
    finish(request, dir, input, cancel, progress).await
}

/// 交给合并的内容。
struct MergeInput {
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
    input: MergeInput,
    cancel: &CancellationToken,
    progress: &watch::Sender<Progress>,
) -> Result<Output, Error> {
    let MergeInput {
        streams,
        groups,
        segments,
        live,
    } = input;
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
