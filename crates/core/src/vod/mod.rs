//! 点播：拉取 init 段并检查分组，下载尚未完成的分片，再按计划组织合并输入。

mod plan;

use std::cmp::Reverse;
use std::ops::Range;

use hs_m3u8_hls::Segment;
use hs_m3u8_remux::Streams;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(crate) use self::plan::Plan;
use crate::fetch::{Direct, Fetcher, Item, ItemInfo, count_done};
use crate::http::Permit;
use crate::ident::Fingerprint;
use crate::output::{self, GroupTrack};
use crate::recheck::{Candidate, OnTransient, Recheck, recheck};
use crate::workdir::{self, Layout, SegmentName, SessionStart, WorkDir};
use crate::{Error, Progress, Stage, WorkDirProblem, blocking};

/// 下载计划中的 init 段与尚未完成的分片到任务目录 `dir`，返回合并输入；返回 `Ok` 即全部已在任务目录中。合并 MP4 时
/// `mp4` 为各轨的取流方式：先下各组各轨的第一个分片，核对能否放进 MP4（见 [`output::check_mp4`]），放不进的不必下完
/// 才报。
///
/// 任务目录里有已完成的分片、这次还有要下载的时，先核对这次取到的是同一份内容（见 [`to_verify`]）：要核对的各组里
/// 已完成的分片重新下载第一个取得到的，与已存的比较（见 [`recheck`]，重试后仍是临时故障即如实上抛）。有组不同或都已
/// 取不到报 [`WorkDirProblem::SourceUnverified`]。分片名与时长相同的两个视频（只靠查询串区分的、同一视频的另一档、
/// 同一地址换了内容的）计划摘要相同，只能这样分开；按组核对，各视频共用的片头片尾（单独的组）不会让别的组蒙混过关。
///
/// init 段每次都重新拉取（地址常带每次会话不同的签名，内容决定文件名）；任一项失败时取消其余项并返回该错误，
/// 任务被取消时返回 [`Error::Cancelled`]。
pub(crate) async fn download(
    fetcher: &mut Fetcher,
    plan: &Plan,
    dir: &WorkDir,
    mp4: Option<&[Streams]>,
    progress: &watch::Sender<Progress>,
    cancel: &CancellationToken,
) -> Result<Vec<Vec<GroupTrack>>, Error> {
    progress.send_modify(|p| {
        p.stage = Stage::Downloading;
        p.segments_total = plan.segment_count();
    });
    let layout = dir.layout();
    let inits = store_inits(&fetcher.direct(), plan, layout, progress, cancel).await?;
    plan.check_inits(&inits)?;
    let names = names(plan, &inits);
    verify_completed(fetcher, plan, &names, layout, cancel).await?;

    let mut items: Vec<Vec<Option<Item>>> = plan
        .tracks
        .iter()
        .zip(&names)
        .enumerate()
        .map(|(track, (t, names))| {
            t.segments
                .iter()
                .zip(names)
                .map(|(segment, name)| {
                    Some(Item {
                        track,
                        segment: Box::new(segment.clone()),
                        name: *name,
                        path: layout.segment(track, name),
                        init: layout.segment_init(track, name),
                    })
                })
                .collect()
        })
        .collect();
    if let Some(streams) = mp4 {
        let heads = plan
            .groups
            .iter()
            .flat_map(|ranges| ranges.iter().enumerate())
            .filter_map(|(track, range)| items[track][range.start].take())
            .collect();
        fetch(fetcher, heads, progress).await?;
        let groups: Vec<(usize, Vec<GroupTrack>)> =
            group_input(plan, &names, layout, |range| range.start..range.start + 1)
                .into_iter()
                .enumerate()
                .collect();
        let streams = streams.to_vec();
        let root = layout.root().to_path_buf();
        blocking(move || output::check_mp4(&streams, &groups, &root)).await??;
    }
    let rest = items.into_iter().flatten().flatten().collect();
    fetch(fetcher, rest, progress).await?;
    Ok(group_input(plan, &names, layout, Range::clone))
}

/// 核对已完成的分片时每条轨最多核对几组：取已完成的字节数最多的，主体内容在其中，各视频共用的片头片尾这类小组排在
/// 后面；每片一组的播放列表不必把已完成的都重下一遍。
const RECHECK_GROUPS: usize = 3;

/// 每组最多试几个已完成的分片：取不到（404/410）的才换下一个，一次很快的请求；滚动删除旧分片的回看源可能要多试几个。
const RECHECK_TRIES: usize = 16;

/// 见 [`download`]：要核对的组里已完成的分片重新下载，与已存的比较。
async fn verify_completed(
    fetcher: &Fetcher,
    plan: &Plan,
    names: &[Vec<SegmentName>],
    layout: &Layout,
    cancel: &CancellationToken,
) -> Result<(), Error> {
    let groups: Vec<Group> = plan
        .groups
        .iter()
        .flat_map(|ranges| ranges.iter().enumerate())
        .map(|(track, range)| Group {
            track,
            candidates: range
                .clone()
                .map(|i| Candidate {
                    segment: plan.tracks[track].segments[i].clone(),
                    stored: layout.segment(track, &names[track][i]),
                    init: layout.segment_init(track, &names[track][i]),
                })
                .collect(),
        })
        .collect();
    let tracks = plan.tracks.len();
    let Some(picked) = blocking(move || to_verify(groups, tracks)).await?? else {
        return Ok(());
    };
    let direct = fetcher.direct();
    for Group { track, candidates } in picked {
        match recheck(&direct, track, candidates, OnTransient::Fail, cancel).await? {
            Recheck::Same => {}
            Recheck::Different | Recheck::Gone => {
                return Err(Error::WorkDir {
                    path: layout.root().to_path_buf(),
                    problem: WorkDirProblem::SourceUnverified,
                });
            }
        }
    }
    Ok(())
}

/// 一个不连续段组里第 `track` 条轨的分片。
struct Group {
    track: usize,
    candidates: Vec<Candidate>,
}

/// `groups`（各组各轨的分片，`tracks` 为轨数）里要核对的：计划里的分片都已完成时为 None（这次不下载，拼不进别的
/// 内容）；否则每条轨已完成的字节数最多的至多 [`RECHECK_GROUPS`] 组，各组已完成的分片按字节数从大到小、至多
/// [`RECHECK_TRIES`] 个（大的不像黑场、静音那样容易彼此相同）。
fn to_verify(groups: Vec<Group>, tracks: usize) -> Result<Option<Vec<Group>>, Error> {
    let mut all_done = true;
    // 各轨各组：(已完成的字节数, 按字节数从大到小的已完成分片)
    let mut by_track: Vec<Vec<(u64, Vec<Candidate>)>> = (0..tracks).map(|_| Vec::new()).collect();
    for Group { track, candidates } in groups {
        let mut sized = Vec::new();
        for candidate in candidates {
            match workdir::completed_len(&candidate.stored)? {
                Some(len) => sized.push((len, candidate)),
                None => all_done = false,
            }
        }
        if sized.is_empty() {
            continue;
        }
        let total = sized.iter().map(|(len, _)| len).sum();
        sized.sort_by_key(|(len, _)| Reverse(*len));
        let largest = sized.into_iter().take(RECHECK_TRIES).map(|(_, c)| c);
        by_track[track].push((total, largest.collect()));
    }
    if all_done {
        return Ok(None);
    }
    let mut picked = Vec::new();
    for (track, mut groups) in by_track.into_iter().enumerate() {
        groups.sort_by_key(|(total, _)| Reverse(*total));
        picked.extend(
            groups
                .into_iter()
                .take(RECHECK_GROUPS)
                .map(|(_, candidates)| Group { track, candidates }),
        );
    }
    Ok(Some(picked))
}

/// 下载 `items` 中尚未在任务目录里的，已在的计入进度；任一项失败时取消其余项并返回该错误。
async fn fetch(
    fetcher: &mut Fetcher,
    items: Vec<Item>,
    progress: &watch::Sender<Progress>,
) -> Result<(), Error> {
    let Split { pending, done } = blocking(move || split(items)).await??;
    progress.send_modify(|p| {
        for (id, len) in done {
            p.count_segment(id.track, id.name.duration_us, len);
        }
    });
    for item in pending {
        fetcher.push(item);
    }
    fetcher
        .drain(|id, result| {
            count_done(progress, id, result?);
            Ok(())
        })
        .await
}

/// 拉取并存入各轨的 init 段，返回 `inits[t][i]`：第 t 条轨 `inits[i]` 的内容指纹。
async fn store_inits(
    direct: &Direct,
    plan: &Plan,
    layout: &Layout,
    progress: &watch::Sender<Progress>,
    cancel: &CancellationToken,
) -> Result<Vec<Vec<Fingerprint>>, Error> {
    let mut all = Vec::with_capacity(plan.tracks.len());
    for (track, t) in plan.tracks.iter().enumerate() {
        let mut fingerprints: Vec<Fingerprint> = Vec::with_capacity(t.inits.len());
        for init in &t.inits {
            let data = direct.fetch_init(init, Permit::Vod, cancel).await?;
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
        start: SessionStart::Fresh,
        sequence: segment.sequence,
        discontinuity: segment.discontinuity,
        init,
        duration_us: segment.duration_us,
        id: Fingerprint::of_segment(&segment.uri, segment.byte_range),
    }
}

/// 计划中的项按是否已在任务目录中分成两份。
struct Split {
    pending: Vec<Item>,
    /// 已完成的项与其字节数
    done: Vec<(ItemInfo, u64)>,
}

fn split(items: Vec<Item>) -> Result<Split, Error> {
    let mut split = Split {
        pending: Vec::new(),
        done: Vec::new(),
    };
    for item in items {
        match workdir::completed_len(&item.path)? {
            Some(len) => split.done.push((ItemInfo::of(&item), len)),
            None => split.pending.push(item),
        }
    }
    Ok(split)
}

/// 计划中的不连续段组换成任务目录中的文件；`pick` 从各组各轨分片的下标范围里取要的部分（不为空）。
fn group_input(
    plan: &Plan,
    names: &[Vec<SegmentName>],
    layout: &Layout,
    pick: impl Fn(&Range<usize>) -> Range<usize>,
) -> Vec<Vec<GroupTrack>> {
    plan.groups
        .iter()
        .map(|ranges| {
            ranges
                .iter()
                .enumerate()
                .map(|(t, range)| GroupTrack::of(layout, t, &names[t][pick(range)]))
                .collect()
        })
        .collect()
}
