//! 续录时的会话判定：由各轨的结论定下会话编号、各轨的起点，以及完整来源地址变了时怎么办（纯计算；
//! 等候选与核对内容见 `deciding` 模块）。
//!
//! 一条轨的窗口与它最近一次有分片的会话重叠（身份与编号一致），且重叠的分片重新下载后与已存的逐字节相同，
//! 才算接得上；编码器重启后序号与文件名都可能从头再来，只看文件名分不出是不是同一段内容。要录的各轨都接得上
//! 各自最近的会话时，接着目录里最近的会话录：最近的会话就是它的轨补录没录完的分片，其余的轨跳过录过的序号
//! 并入；否则另起一个会话，接得上的轨跳过录过的序号。

use std::collections::BTreeMap;

use crate::workdir::{SegmentFile, SegmentName, SessionStart, Stored};

/// 一条轨在会话开始时的状况。
pub(super) enum Start {
    /// 窗口内的分片全部录
    Fresh,
    /// 序号不超过 `through` 的分片之前已录过，跳过
    After { through: u64 },
    /// 接着这条轨在这个会话里已录的部分：沿用其编号，补录窗口内尚未录完的分片
    Continue(Recorded),
}

impl Start {
    /// 要记进任务目录的起点；接着录时沿用已记下的，为 None。
    pub(super) fn to_record(&self) -> Option<SessionStart> {
        match self {
            Start::Fresh => Some(SessionStart::Fresh),
            Start::After { through } => Some(SessionStart::After(*through)),
            Start::Continue(_) => None,
        }
    }
}

/// 一条轨最近一次有分片的会话中已录完的分片。
pub(super) struct Recorded {
    session: u32,
    /// 序号 → 文件名记录的信息
    segments: BTreeMap<u64, SegmentName>,
    /// 这条轨在这个会话的起点跳过到的序号；补录不越过它。从窗口起点录的为 None
    skipped_through: Option<u64>,
}

impl Recorded {
    /// 一条轨的分片（按会话、序号排列）中最近一个会话的；`starts` 为该轨各会话的起点。一个分片都没有时为 None。
    pub(super) fn latest(
        files: &[SegmentFile],
        starts: &BTreeMap<u32, SessionStart>,
    ) -> Option<Self> {
        let session = files.iter().map(|f| f.name.session).max()?;
        let segments = files
            .iter()
            .filter(|f| f.name.session == session)
            .map(|f| (f.name.sequence, f.name))
            .collect();
        let start = starts
            .get(&session)
            .expect("有分片的会话都有起点，扫描任务目录时已核对");
        let skipped_through = match *start {
            SessionStart::Fresh => None,
            SessionStart::After(through) => Some(through),
        };
        Some(Recorded {
            session,
            segments,
            skipped_through,
        })
    }

    pub(super) fn segments(&self) -> &BTreeMap<u64, SegmentName> {
        &self.segments
    }

    pub(super) fn last(&self) -> u64 {
        *self
            .segments
            .keys()
            .next_back()
            .expect("Recorded 至少有一个分片")
    }

    pub(super) fn skipped_through(&self) -> Option<u64> {
        self.skipped_through
    }
}

/// 一条轨的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// 窗口与已录的分片重叠一致，内容核对相同
    Matches,
    /// 这条轨没有录过分片，或没有重叠、有矛盾、内容不同，或重叠的分片都已取不到
    Differs,
    /// 已录满 max_duration，这次不录，不影响判定
    Full,
    /// 一直没拿到有分片的播放列表就停滞、看起来已结束（其他轨也不再出新分片）：这次不录，不影响判定
    Ended,
}

impl Verdict {
    /// 这条轨这次要录。
    fn records(self) -> bool {
        matches!(self, Verdict::Matches | Verdict::Differs)
    }
}

/// 完整来源地址与目录里记录的不同时怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NewUrl {
    /// 改记当前地址：目录里还没有录过的分片，或要录的各轨都接得上
    Adopt,
    /// 有轨接不上，无法确认是同一个直播
    Unverified,
    /// 这次没有要录的轨：不改记，照常合并
    Keep,
}

/// 判定结果。
pub(super) struct Decision {
    pub session: u32,
    pub new_url: NewUrl,
    /// 各轨的起点；None 为这次不录（见 [`Verdict::Full`]、[`Verdict::Ended`]）
    pub tracks: Vec<Option<Start>>,
}

impl Decision {
    /// 目录里没有录过的分片：第 0 个会话，`tracks` 条轨都从窗口起点录。
    pub(super) fn first(tracks: usize) -> Self {
        Decision {
            session: 0,
            new_url: NewUrl::Adopt,
            tracks: (0..tracks).map(|_| Some(Start::Fresh)).collect(),
        }
    }
}

/// 正在判定的会话。
pub(super) struct Deciding {
    /// 目录里最近的会话编号
    previous_session: u32,
    /// 各轨最近一次有分片的会话
    recorded: Vec<Option<Recorded>>,
    verdicts: Vec<Option<Verdict>>,
}

impl Deciding {
    /// 由目录里已录的内容开始判定；一个分片都没有时为 None（直接录第 0 个会话）。
    pub(super) fn new(stored: &Stored) -> Option<Self> {
        let previous_session = stored
            .segments
            .iter()
            .flatten()
            .map(|f| f.name.session)
            .max()?;
        let recorded = stored
            .segments
            .iter()
            .zip(&stored.starts)
            .map(|(files, starts)| Recorded::latest(files, starts))
            .collect();
        Some(Deciding {
            previous_session,
            recorded,
            verdicts: stored.segments.iter().map(|_| None).collect(),
        })
    }

    pub(super) fn recorded(&self, track: usize) -> Option<&Recorded> {
        self.recorded[track].as_ref()
    }

    pub(super) fn conclude(&mut self, track: usize, verdict: Verdict) {
        self.verdicts[track] = Some(verdict);
    }

    pub(super) fn has_concluded(&self, track: usize) -> bool {
        self.verdicts[track].is_some()
    }

    pub(super) fn is_complete(&self) -> bool {
        self.verdicts.iter().all(Option::is_some)
    }

    /// 各轨都有结论后作出判定；另起会话而会话编号已达上限时为 None。
    pub(super) fn decide(self) -> Option<Decision> {
        let verdicts: Vec<Verdict> = self
            .verdicts
            .into_iter()
            .map(|v| v.expect("判定前各轨都已有结论"))
            .collect();
        let previous = self.previous_session;
        let continued = verdicts.iter().all(|v| *v != Verdict::Differs);
        let recording = verdicts.iter().any(|v| v.records());
        let session = if continued {
            previous
        } else {
            previous.checked_add(1)?
        };
        let new_url = match (recording, continued) {
            (false, _) => NewUrl::Keep,
            (true, true) => NewUrl::Adopt,
            (true, false) => NewUrl::Unverified,
        };
        let tracks = self
            .recorded
            .into_iter()
            .zip(verdicts)
            .map(|(recorded, verdict)| match (recorded, verdict) {
                (_, Verdict::Full | Verdict::Ended) => None,
                (Some(r), Verdict::Matches) if continued && r.session == previous => {
                    Some(Start::Continue(r))
                }
                (Some(r), Verdict::Matches) => Some(Start::After { through: r.last() }),
                _ => Some(Start::Fresh),
            })
            .collect();
        Some(Decision {
            session,
            new_url,
            tracks,
        })
    }
}
