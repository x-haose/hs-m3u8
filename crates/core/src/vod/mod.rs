//! 点播：拉取 init 段并检查分组，下载尚未完成的分片，再按计划组织合并输入。

mod plan;

use hs_m3u8_hls::Segment;
use hs_m3u8_remux::{DiscontinuityGroup, TrackSegments};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(crate) use self::plan::Plan;
use crate::fetch::{Fetcher, Item, count_done, fetch_init};
use crate::http::{Http, Permit};
use crate::ident::Fingerprint;
use crate::workdir::{self, Layout, SegmentName};
use crate::{Error, Progress, Stage, blocking};

/// 下载计划中的 init 段与尚未完成的分片，返回合并输入；返回 `Ok` 即全部已在任务目录中。
///
/// init 段每次都重新拉取（地址常带每次会话不同的签名，内容决定文件名）；任一项失败时取消其余项并返回该错误，
/// 任务被取消时返回 [`Error::Cancelled`]。
pub(crate) async fn download(
    http: &Http,
    fetcher: &mut Fetcher,
    plan: &Plan,
    layout: &Layout,
    progress: &watch::Sender<Progress>,
    cancel: &CancellationToken,
) -> Result<Vec<DiscontinuityGroup>, Error> {
    progress.send_modify(|p| {
        p.stage = Stage::Downloading;
        p.segments_total = plan.segment_count();
    });
    let inits = store_inits(http, plan, layout, progress, cancel).await?;
    plan.check_inits(&inits)?;
    let names = names(plan, &inits);

    let mut items = Vec::new();
    for (track, (t, names)) in plan.tracks.iter().zip(&names).enumerate() {
        for (segment, name) in t.segments.iter().zip(names) {
            items.push(Item {
                track,
                segment: Box::new(segment.clone()),
                path: layout.segment(track, name),
            });
        }
    }
    let (pending, done, bytes) = blocking(move || pending(items)).await??;
    progress.send_modify(|p| {
        p.segments_done = done;
        p.bytes += bytes;
    });
    for item in pending {
        fetcher.push(item);
    }
    fetcher
        .drain(|_, result| {
            count_done(progress, result?);
            Ok(())
        })
        .await?;
    Ok(merge_input(plan, &names, layout))
}

/// 拉取并存入各轨的 init 段，返回 `inits[t][i]`：第 t 条轨 `inits[i]` 的内容指纹。
async fn store_inits(
    http: &Http,
    plan: &Plan,
    layout: &Layout,
    progress: &watch::Sender<Progress>,
    cancel: &CancellationToken,
) -> Result<Vec<Vec<Fingerprint>>, Error> {
    let mut all = Vec::with_capacity(plan.tracks.len());
    for (track, t) in plan.tracks.iter().enumerate() {
        let mut fingerprints: Vec<Fingerprint> = Vec::with_capacity(t.inits.len());
        for init in &t.inits {
            let data = fetch_init(http, init, Permit::Required, cancel).await?;
            let len = data.len() as u64;
            let fingerprint = Fingerprint::of_content(&data);
            workdir::store_init(layout, track, fingerprint, data).await?;
            // 内容相同的 init 段共用一个文件，字节数只计一次
            if !fingerprints.contains(&fingerprint) {
                progress.send_modify(|p| p.bytes += len);
            }
            fingerprints.push(fingerprint);
        }
        all.push(fingerprints);
    }
    Ok(all)
}

/// 各轨各分片在任务目录中的名字；会话恒为 0。
fn names(plan: &Plan, inits: &[Vec<Fingerprint>]) -> Vec<Vec<SegmentName>> {
    plan.tracks
        .iter()
        .zip(inits)
        .map(|(track, inits)| {
            track
                .segments
                .iter()
                .map(|s| segment_name(s, track.init_index(s).map(|i| inits[i])))
                .collect()
        })
        .collect()
}

fn segment_name(segment: &Segment, init: Option<Fingerprint>) -> SegmentName {
    SegmentName {
        session: 0,
        sequence: segment.sequence,
        discontinuity: segment.discontinuity,
        init,
        duration_us: segment.duration_us,
        id: Fingerprint::of_segment(&segment.uri, segment.byte_range),
    }
}

/// 尚未完成的项，以及已完成的分片数与字节数。
fn pending(items: Vec<Item>) -> Result<(Vec<Item>, usize, u64), Error> {
    let mut pending = Vec::new();
    let (mut done, mut bytes) = (0, 0);
    for item in items {
        match workdir::completed_len(&item.path)? {
            Some(len) => {
                done += 1;
                bytes += len;
            }
            None => pending.push(item),
        }
    }
    Ok((pending, done, bytes))
}

/// 计划中的不连续段组换成任务目录中的文件路径。
fn merge_input(
    plan: &Plan,
    names: &[Vec<SegmentName>],
    layout: &Layout,
) -> Vec<DiscontinuityGroup> {
    plan.groups
        .iter()
        .map(|ranges| DiscontinuityGroup {
            tracks: ranges
                .iter()
                .enumerate()
                .map(|(t, range)| {
                    let names = &names[t][range.clone()];
                    TrackSegments {
                        init: names[0].init.map(|f| layout.init(t, f)),
                        segments: names.iter().map(|n| layout.segment(t, n)).collect(),
                    }
                })
                .collect(),
        })
        .collect()
}
