//! 合并输入与缺失报告：任务目录中各会话的分片按（会话, 不连续段）分组，只合并各轨都有的组；
//! 不在输出中的分片汇总成区间。

use std::collections::{BTreeMap, BTreeSet};

use hs_m3u8_remux::{DiscontinuityGroup, TrackSegments};

use crate::workdir::{Layout, SegmentFile};
use crate::{Error, MissReason, Missed, WorkDirProblem};

/// 合并计划。
pub(crate) struct MergePlan {
    pub groups: Vec<DiscontinuityGroup>,
    /// 合并进输出的分片数，各轨合计
    pub segments: usize,
    /// 合并进输出的会话数
    pub sessions: usize,
    /// 所在组不是每条轨都有、无法合并的分片
    pub unmergeable: Vec<Missed>,
    /// 同一会话、同一轨相邻两个已完成分片之间缺的序号，原因待定
    pub holes: Vec<Missed>,
}

/// 分组的键：（会话, 不连续段）。
type Key = (u32, u64);

/// 一条轨的分片按组归类。
type ByGroup<'a> = BTreeMap<Key, Vec<&'a SegmentFile>>;

/// 由任务目录中的分片（各轨按（会话, 序号）排列）得出合并计划。
pub(crate) fn merge_plan(files: &[Vec<SegmentFile>], layout: &Layout) -> Result<MergePlan, Error> {
    let by_group: Vec<ByGroup<'_>> = files.iter().map(|track| group_files(track)).collect();
    let common: BTreeSet<Key> = by_group
        .first()
        .map(|first| {
            first
                .keys()
                .filter(|k| by_group.iter().all(|g| g.contains_key(k)))
                .copied()
                .collect()
        })
        .unwrap_or_default();
    let unmergeable = unmergeable(&by_group, &common);
    let holes = files
        .iter()
        .enumerate()
        .flat_map(|(track, files)| holes(track, files))
        .collect();
    let groups = common
        .iter()
        .map(|key| merge_group(*key, &by_group, layout))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MergePlan {
        segments: groups
            .iter()
            .flat_map(|g| &g.tracks)
            .map(|t| t.segments.len())
            .sum(),
        sessions: common
            .iter()
            .map(|(session, _)| session)
            .collect::<BTreeSet<_>>()
            .len(),
        groups,
        unmergeable,
        holes,
    })
}

/// 所在组不是每条轨都有（不在 `common` 里）的分片。
fn unmergeable(by_group: &[ByGroup<'_>], common: &BTreeSet<Key>) -> Vec<Missed> {
    let mut missed = Vec::new();
    for (track, groups) in by_group.iter().enumerate() {
        for (key, files) in groups {
            if !common.contains(key) {
                missed.extend(
                    files
                        .iter()
                        .map(|f| single(f, track, MissReason::Unmergeable)),
                );
            }
        }
    }
    missed
}

/// 一条轨的分片按（会话, 不连续段）归类，组内保持序号顺序。
fn group_files(files: &[SegmentFile]) -> ByGroup<'_> {
    let mut groups = ByGroup::new();
    for file in files {
        groups
            .entry((file.name.session, file.name.discontinuity))
            .or_default()
            .push(file);
    }
    groups
}

/// 各轨都有的一组换成合并输入；组内的分片引用了不同的 init 段时目录内容矛盾。
fn merge_group(
    key: Key,
    by_group: &[ByGroup<'_>],
    layout: &Layout,
) -> Result<DiscontinuityGroup, Error> {
    let mut tracks = Vec::with_capacity(by_group.len());
    for (track, groups) in by_group.iter().enumerate() {
        let files = &groups[&key];
        let init = files[0].name.init;
        if files.iter().any(|f| f.name.init != init) {
            return Err(Error::WorkDir {
                path: layout.root().to_path_buf(),
                problem: WorkDirProblem::Corrupt(format!(
                    "第 {track} 条轨会话 {} 不连续段 {} 内的分片引用了不同的 init 段",
                    key.0, key.1
                )),
            });
        }
        tracks.push(TrackSegments {
            init: init.map(|f| layout.init(track, f)),
            segments: files.iter().map(|f| f.path.clone()).collect(),
        });
    }
    Ok(DiscontinuityGroup { tracks })
}

/// 报告中的缺失分片：`known` 为本次运行记下原因的，加上无法合并的，以及空洞中其余的（原因不明），
/// 按会话、轨道、序号排列，相接且原因相同的合成一个区间。
///
/// 简化：缺失不落盘，之前的运行里某个会话第一个已完成分片之前或最后一个之后缺的不在报告里；需要完整报告时
/// 把缺失记进任务目录。
pub(crate) fn report_missed(plan: &MergePlan, known: Vec<Missed>) -> Vec<Missed> {
    let known = merge_ranges(known);
    let mut all: Vec<Missed> = plan
        .holes
        .iter()
        .flat_map(|hole| uncovered(hole, &known))
        .collect();
    all.extend(known);
    all.extend(plan.unmergeable.iter().cloned());
    merge_ranges(all)
}

/// 缺失按会话、轨道、序号排序，序号相接且原因相同的合成一个区间。
fn merge_ranges(mut missed: Vec<Missed>) -> Vec<Missed> {
    missed.sort_by_key(|m| (m.session, m.track, m.first));
    let mut merged: Vec<Missed> = Vec::new();
    for m in missed {
        match merged.last_mut() {
            Some(prev)
                if (prev.session, prev.track) == (m.session, m.track)
                    && prev.reason == m.reason
                    && prev.last.checked_add(1) == Some(m.first) =>
            {
                prev.last = m.last;
            }
            _ => merged.push(m),
        }
    }
    merged
}

/// 一个分片单独构成的缺失区间。
fn single(file: &SegmentFile, track: usize, reason: MissReason) -> Missed {
    Missed {
        session: file.name.session,
        track,
        first: file.name.sequence,
        last: file.name.sequence,
        reason,
    }
}

/// 一条轨中同一会话相邻两个已完成分片之间缺的序号，原因记为不明。
fn holes(track: usize, files: &[SegmentFile]) -> Vec<Missed> {
    files
        .windows(2)
        .filter_map(|pair| {
            let (a, b) = (&pair[0].name, &pair[1].name);
            let gap_start = a.sequence.checked_add(1)?;
            (a.session == b.session && b.sequence > gap_start).then(|| Missed {
                session: a.session,
                track,
                first: gap_start,
                last: b.sequence - 1,
                reason: MissReason::Unknown,
            })
        })
        .collect()
}

/// `hole` 中不被 `known`（已按会话、轨道、序号排序）覆盖的部分。
fn uncovered(hole: &Missed, known: &[Missed]) -> Vec<Missed> {
    let part = |first, last| Missed {
        first,
        last,
        ..hole.clone()
    };
    let mut rest = Vec::new();
    // 尚未判定的最小序号
    let mut next = hole.first;
    let same = |k: &&Missed| (k.session, k.track) == (hole.session, hole.track);
    for k in known.iter().filter(same).filter(|k| k.first <= hole.last) {
        if k.last < next {
            continue;
        }
        if k.first > next {
            rest.push(part(next, k.first - 1));
        }
        match k.last.checked_add(1) {
            Some(after) if after <= hole.last => next = after,
            _ => return rest,
        }
    }
    rest.push(part(next, hole.last));
    rest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HttpError;

    fn missed(session: u32, track: usize, range: (u64, u64), reason: MissReason) -> Missed {
        Missed {
            session,
            track,
            first: range.0,
            last: range.1,
            reason,
        }
    }

    #[test]
    fn adjacent_ranges_with_the_same_reason_merge() {
        let not_found = MissReason::Failed(HttpError::Status(404));
        let merged = merge_ranges(vec![
            missed(0, 0, (5, 5), not_found.clone()),
            missed(0, 0, (3, 4), not_found.clone()),
            missed(0, 0, (6, 6), MissReason::Expired),
            missed(0, 1, (7, 7), not_found.clone()),
            missed(1, 0, (7, 7), not_found.clone()),
            missed(0, 0, (u64::MAX, u64::MAX), not_found.clone()),
        ]);
        assert_eq!(
            merged,
            vec![
                missed(0, 0, (3, 5), not_found.clone()),
                missed(0, 0, (6, 6), MissReason::Expired),
                missed(0, 0, (u64::MAX, u64::MAX), not_found.clone()),
                missed(0, 1, (7, 7), not_found.clone()),
                missed(1, 0, (7, 7), not_found),
            ]
        );
    }

    #[test]
    fn holes_minus_known_reasons_are_unknown() {
        let not_found = MissReason::Failed(HttpError::Status(404));
        let hole = missed(0, 0, (10, 20), MissReason::Unknown);
        let known = merge_ranges(vec![
            missed(0, 0, (12, 13), not_found.clone()),
            missed(0, 0, (13, 15), MissReason::Expired),
            missed(0, 0, (18, 30), not_found.clone()),
            // 其他会话或轨道的不算
            missed(1, 0, (10, 20), not_found.clone()),
            missed(0, 1, (10, 20), not_found.clone()),
        ]);
        assert_eq!(
            uncovered(&hole, &known),
            vec![
                missed(0, 0, (10, 11), MissReason::Unknown),
                missed(0, 0, (16, 17), MissReason::Unknown),
            ]
        );
        assert_eq!(uncovered(&hole, &[]), vec![hole.clone()]);
        let all = vec![missed(0, 0, (0, u64::MAX), not_found)];
        assert_eq!(uncovered(&hole, &all), vec![]);
    }
}
