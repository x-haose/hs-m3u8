//! 任务编排：解析 → 下载 → 合并 → 收尾。

use std::sync::Arc;

use hs_m3u8_remux::{DiscontinuityGroup, TrackSegments};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::http::Http;
use crate::plan::{self, Plan};
use crate::workdir::{self, WorkDir};
use crate::{Error, JobRequest, Output, Progress, Stage, blocking, check_output, fetch};

pub(crate) async fn run(
    request: JobRequest,
    http: Http,
    cancel: CancellationToken,
    progress: watch::Sender<Progress>,
) -> Result<Output, Error> {
    let http = Arc::new(http);
    let plan = Arc::new(plan::resolve(&http, &request, &cancel).await?);
    if let Some(parent) = request
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        let parent = parent.to_path_buf();
        blocking(move || {
            std::fs::create_dir_all(&parent).map_err(|source| Error::Io {
                action: "创建",
                path: parent,
                source,
            })
        })
        .await??;
    }
    let (dir, lock) = WorkDir::open(request.work_dir(), plan.digest()).await?;
    fetch::download(
        http,
        request.hooks.clone(),
        plan.clone(),
        dir.clone(),
        request.concurrency,
        progress.clone(),
        &cancel,
    )
    .await?;

    // 简化：合并阶段不响应取消（remux 不可中断），合并耗时成为问题时给 remux 加 FFmpeg 中断回调。
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    // 下载期间输出路径可能已被别人占用；开始时检查过，这里再查一次
    check_output(&request.output, request.overwrite)?;
    progress.send_modify(|p| p.stage = Stage::Merging);
    let streams: Vec<_> = plan.tracks.iter().map(|t| t.streams).collect();
    let groups = merge_input(&plan, &dir);
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
    let done = *progress.borrow();
    Ok(Output {
        path: request.output,
        report,
        segments: done.segments_total,
        bytes: done.bytes,
        cleanup_error,
    })
}

/// 计划中的不连续段组换成任务目录中的文件路径。
fn merge_input(plan: &Plan, dir: &WorkDir) -> Vec<DiscontinuityGroup> {
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
                            .map(|s| dir.segment(t, s.sequence))
                            .collect(),
                    }
                })
                .collect(),
        })
        .collect()
}
