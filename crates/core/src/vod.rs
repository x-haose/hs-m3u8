//! 点播：下载计划中尚未完成的 init 段与分片，再按计划组织合并输入。

use std::sync::Arc;

use hs_m3u8_hls::Segment;
use hs_m3u8_remux::{DiscontinuityGroup, TrackSegments};
use tokio::sync::watch;

use crate::fetch::{Fetcher, Item, record_done};
use crate::plan::{Plan, Track};
use crate::workdir::{self, Layout, SegmentName};
use crate::{Error, Progress, Stage, blocking};

/// 下载计划中尚未完成的 init 段与分片。返回 `Ok` 即全部已在任务目录中。
///
/// 任一项失败时取消其余项并返回该错误；任务被取消时返回 [`Error::Cancelled`]。
pub(crate) async fn download(
    fetcher: &mut Fetcher,
    plan: Arc<Plan>,
    layout: &Layout,
    progress: &watch::Sender<Progress>,
) -> Result<(), Error> {
    let (scan_plan, scan_layout) = (plan.clone(), layout.clone());
    let (items, done, bytes) = blocking(move || pending(&scan_plan, &scan_layout)).await??;
    progress.send_modify(|p| {
        p.stage = Stage::Downloading;
        p.segments_total = plan.segment_count();
        p.segments_done = done;
        p.bytes = bytes;
    });
    for item in items {
        fetcher.push(item);
    }
    fetcher
        .drain(|id, result| {
            record_done(progress, id, result?);
            Ok(())
        })
        .await
}

/// 计划中的不连续段组换成任务目录中的文件路径。
pub(crate) fn merge_input(plan: &Plan, layout: &Layout) -> Vec<DiscontinuityGroup> {
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
                        init: track.init_index(&segments[0]).map(|i| layout.init(t, i)),
                        segments: segments
                            .iter()
                            .map(|s| layout.segment(t, &segment_name(track, s)))
                            .collect(),
                    }
                })
                .collect(),
        })
        .collect()
}

/// 点播分片在任务目录中的名字；录制次数恒为 0。
fn segment_name(track: &Track, segment: &Segment) -> SegmentName {
    SegmentName {
        session: 0,
        sequence: segment.sequence,
        discontinuity: segment.discontinuity,
        init: track.init_index(segment),
        duration_us: segment.duration_us,
    }
}

/// 尚未完成的项（init 段在前），以及已完成的分片数与字节数（含 init 段）。
fn pending(plan: &Plan, layout: &Layout) -> Result<(Vec<Item>, usize, u64), Error> {
    let mut items = Vec::new();
    let (mut done, mut bytes) = (0, 0);
    for (track, t) in plan.tracks.iter().enumerate() {
        for (index, init) in t.inits.iter().enumerate() {
            let path = layout.init(track, index);
            match workdir::completed_len(&path)? {
                Some(len) => bytes += len,
                None => items.push(Item::Init {
                    track,
                    init: init.clone(),
                    path,
                }),
            }
        }
    }
    for (track, t) in plan.tracks.iter().enumerate() {
        for segment in &t.segments {
            let path = layout.segment(track, &segment_name(t, segment));
            match workdir::completed_len(&path)? {
                Some(len) => {
                    done += 1;
                    bytes += len;
                }
                None => items.push(Item::Segment {
                    track,
                    segment: Box::new(segment.clone()),
                    path,
                }),
            }
        }
    }
    Ok((items, done, bytes))
}
