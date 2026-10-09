//! 任务编排：解析 → 下载（点播）或录制（直播）→ 合并 → 收尾。

use std::sync::Arc;

use hs_m3u8_remux::{DiscontinuityGroup, Streams, TrackSegments};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::fetch::{self, Fetcher};
use crate::http::Http;
use crate::live::{self, Session};
use crate::plan::{self, Plan};
use crate::workdir::{self, DirLock, JobKind, SegmentFile, WorkDir};
use crate::{
    Error, JobRequest, LiveEnd, LiveReport, Missed, Output, Progress, Stage, Unsupported, blocking,
    check_output,
};

pub(crate) async fn run(
    request: JobRequest,
    http: Http,
    cancel: CancellationToken,
    stop: CancellationToken,
    progress: watch::Sender<Progress>,
) -> Result<Output, Error> {
    create_output_dir(&request).await?;
    let root = request.work_dir();
    let source_digest = plan::source_digest(&request);

    // 上次直播录制中断留下的任务目录：不联网，直接合并已录到的分片；一个也没录到时重新录制
    if let Some(JobKind::Live {
        source_digest: recorded,
        tracks,
    }) = workdir::recorded(root.clone()).await?
        && recorded == source_digest
    {
        let kind = JobKind::Live {
            source_digest: recorded,
            tracks: tracks.clone(),
        };
        let (dir, lock) = WorkDir::open(root.clone(), kind).await?;
        let files = dir.completed_segments(tracks.len()).await?;
        if files.iter().any(|f| !f.is_empty()) {
            progress.send_modify(|p| {
                p.segments_done = files.iter().map(Vec::len).sum();
                p.segments_total = p.segments_done;
                p.bytes = files.iter().flatten().map(|f| f.len).sum();
            });
            let recorded = Recorded {
                end: LiveEnd::Interrupted,
                missed: Vec::new(),
            };
            return merge_live(request, (dir, lock), tracks, files, recorded, &progress).await;
        }
    }

    let http = Arc::new(http);
    let sources = plan::resolve(&http, &request, &cancel).await?;
    let mut fetcher = Fetcher::new(
        http.clone(),
        request.hooks.clone(),
        request.concurrency,
        &cancel,
    );
    if plan::is_live(&sources) {
        let options = request.live.ok_or(Error::Unsupported(Unsupported::Live))?;
        let tracks: Vec<Streams> = sources.iter().map(|s| s.streams).collect();
        let kind = JobKind::Live {
            source_digest,
            tracks: tracks.clone(),
        };
        let (dir, lock) = WorkDir::open(root, kind).await?;
        let session = Session {
            http,
            hooks: request.hooks.clone(),
            dir: &dir,
            fetcher: &mut fetcher,
            progress: &progress,
            cancel: &cancel,
            stop: &stop,
        };
        let recording = live::record(session, sources, options).await?;
        let files = dir.completed_segments(tracks.len()).await?;
        let recorded = Recorded {
            end: recording.end,
            missed: recording.missed,
        };
        merge_live(request, (dir, lock), tracks, files, recorded, &progress).await
    } else {
        let plan = Arc::new(Plan::new(sources)?);
        let kind = JobKind::Vod {
            plan_digest: plan.digest(),
        };
        let (dir, lock) = WorkDir::open(root, kind).await?;
        fetch::download(&mut fetcher, plan.clone(), &dir, &progress).await?;
        // 简化：合并阶段不响应取消（remux 不可中断），合并耗时成为问题时给 remux 加 FFmpeg 中断回调。
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let streams = plan.tracks.iter().map(|t| t.streams).collect();
        let groups = vod_merge_input(&plan, &dir);
        finish(request, (dir, lock), streams, groups, &progress, None).await
    }
}

/// 直播录制的结束原因与录制中记下的漏段。
struct Recorded {
    end: LiveEnd,
    missed: Vec<Missed>,
}

async fn create_output_dir(request: &JobRequest) -> Result<(), Error> {
    let Some(parent) = request
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    else {
        return Ok(());
    };
    let parent = parent.to_path_buf();
    blocking(move || {
        std::fs::create_dir_all(&parent).map_err(|source| Error::Io {
            action: "创建",
            path: parent,
            source,
        })
    })
    .await?
}

/// 合并直播录到的分片：只合并各轨都有的不连续段，其余并入漏段。
async fn merge_live(
    request: JobRequest,
    dir: (WorkDir, DirLock),
    tracks: Vec<Streams>,
    files: Vec<Vec<SegmentFile>>,
    recorded: Recorded,
    progress: &watch::Sender<Progress>,
) -> Result<Output, Error> {
    let (groups, unmergeable) = live::merge_input(&files, &dir.0)?;
    if groups.is_empty() {
        return Err(Error::NothingRecorded);
    }
    let mut missed = recorded.missed;
    missed.extend(unmergeable);
    let report = LiveReport {
        end: recorded.end,
        missed: live::merge_ranges(missed),
    };
    finish(request, dir, tracks, groups, progress, Some(report)).await
}

/// 合并为输出文件，按选项删除任务目录。
async fn finish(
    request: JobRequest,
    (dir, lock): (WorkDir, DirLock),
    streams: Vec<Streams>,
    groups: Vec<DiscontinuityGroup>,
    progress: &watch::Sender<Progress>,
    live: Option<LiveReport>,
) -> Result<Output, Error> {
    // 下载或录制期间输出路径可能已被别人占用；开始时检查过，这里再查一次
    check_output(&request.output, request.overwrite)?;
    progress.send_modify(|p| p.stage = Stage::Merging);
    let segments = groups
        .iter()
        .flat_map(|g| &g.tracks)
        .map(|t| t.segments.len())
        .sum();
    let output = request.output.clone();
    let report = blocking(move || hs_m3u8_remux::remux(&streams, &groups, &output)).await??;

    let cleanup_error = if request.keep_work_dir {
        None
    } else {
        workdir::remove(dir, lock)
            .await
            .err()
            .map(|e| e.to_string())
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

/// 点播计划中的不连续段组换成任务目录中的文件路径。
fn vod_merge_input(plan: &Plan, dir: &WorkDir) -> Vec<DiscontinuityGroup> {
    plan.groups
        .iter()
        .map(|ranges| DiscontinuityGroup {
            tracks: ranges
                .iter()
                .enumerate()
                .map(|(t, range)| {
                    let track = &plan.tracks[t];
                    let segments = &track.segments[range.clone()];
                    TrackSegments {
                        init: track.init_index(&segments[0]).map(|i| dir.init(t, i)),
                        segments: segments
                            .iter()
                            .map(|s| {
                                dir.segment(t, s.sequence, s.discontinuity, track.init_index(s))
                            })
                            .collect(),
                    }
                })
                .collect(),
        })
        .collect()
}
