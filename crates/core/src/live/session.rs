//! 续录时的会话判定。
//!
//! 中断后再次运行，先等各轨拿到第一份有分片（或已结束）的播放列表，对照该轨最近一次有分片的会话：窗口与那些分片
//! 重叠（身份与编号一致），且重叠的分片重新下载后与已存的逐字节相同，才算接得上。各轨（已录满 max_duration 的除外）
//! 都接得上上一个会话时接着它录；否则另起一个会话，接得上的轨跳过之前录过的序号。编码器重启后序号与文件名都可能
//! 从头再来，只看文件名分不出是不是同一段内容，所以要核对内容。

use std::collections::{BTreeMap, BTreeSet};

use hs_m3u8_hls::MediaPlaylist;
use tokio::time::Instant;

use crate::workdir::{SegmentFile, SegmentName};

/// 一条轨在会话开始时的状况。
pub(super) enum Start {
    /// 新会话，窗口内的分片全部录
    Fresh,
    /// 新会话，序号不超过 `through` 的分片之前已录过，跳过
    After { through: u64 },
    /// 接着之前的会话：沿用其编号，补录窗口内尚未录完的分片
    Continue(Recorded),
}

/// 一条轨最近一次有分片的会话中已录完的分片。
#[derive(Clone)]
pub(super) struct Recorded {
    session: u32,
    /// 序号 → 文件名记录的信息
    segments: BTreeMap<u64, SegmentName>,
    /// 补录的下界：更早的会话里录过的最大序号，且这个会话的分片都排在它之后（另起会话时跳过了旧序号，或窗口
    /// 整体前移）；否则（如编码器重启后序号从头再来）为 None，不设下界
    refill_after: Option<u64>,
}

impl Recorded {
    /// 一条轨的全部分片（按会话、序号排列）中最近一个会话的；一个都没有时为 None。
    pub(super) fn latest(files: &[SegmentFile]) -> Option<Self> {
        let session = files.iter().map(|f| f.name.session).max()?;
        let segments: BTreeMap<u64, SegmentName> = files
            .iter()
            .filter(|f| f.name.session == session)
            .map(|f| (f.name.sequence, f.name))
            .collect();
        let first = *segments.keys().next().expect("最近的会话至少有一个分片");
        let refill_after = files
            .iter()
            .filter(|f| f.name.session < session)
            .map(|f| f.name.sequence)
            .max()
            .filter(|&earlier| first > earlier);
        Some(Recorded {
            session,
            segments,
            refill_after,
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

    /// 补录的范围：下界（不含）与已录完的序号。
    pub(super) fn refill(&self) -> (Option<u64>, BTreeSet<u64>) {
        (self.refill_after, self.segments.keys().copied().collect())
    }
}

/// 一条轨能否接着它最近的会话录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// 窗口与已录的分片重叠一致，内容核对相同
    Matches,
    /// 没有重叠、有矛盾、内容不同，或重叠的分片都已取不到
    Differs,
    /// 已录满 max_duration，不再录，不影响判定
    Full,
}

/// 判定会话时，一条轨拿到的第一份有分片（或已结束）的播放列表。
pub(super) struct Candidate {
    pub playlist: MediaPlaylist,
    /// 这次加载开始的时刻
    pub started: Instant,
    pub verdict: Verdict,
}

/// 正在判定的会话。
pub(super) struct Deciding {
    /// 目录里最近的会话编号
    previous_session: u32,
    /// 各轨最近一次有分片的会话
    recorded: Vec<Option<Recorded>>,
    candidates: Vec<Option<Candidate>>,
}

/// 判定结果。
pub(super) struct Decision {
    pub session: u32,
    /// 来源地址（含查询串）变了时可以改记当前地址：接着上一个会话录（内容核对一致），或目录里还没有录过的分片
    pub may_switch_source: bool,
    /// 各轨的起点与暂存的播放列表
    pub tracks: Vec<(Start, Candidate)>,
}

impl Decision {
    /// 目录里没有录过的分片：第 0 个会话，各轨都从首次拉到的播放列表的起点录。
    pub(super) fn first(playlists: Vec<MediaPlaylist>, started: Instant) -> Self {
        let tracks = playlists
            .into_iter()
            .map(|playlist| {
                let candidate = Candidate {
                    playlist,
                    started,
                    verdict: Verdict::Differs,
                };
                (Start::Fresh, candidate)
            })
            .collect();
        Decision {
            session: 0,
            may_switch_source: true,
            tracks,
        }
    }
}

impl Deciding {
    /// 由各轨已录的分片开始判定；目录里一个分片都没有时为 None（直接录第 0 个会话）。
    pub(super) fn new(files: &[Vec<SegmentFile>]) -> Option<Self> {
        let previous_session = files.iter().flatten().map(|f| f.name.session).max()?;
        Some(Deciding {
            previous_session,
            recorded: files.iter().map(|f| Recorded::latest(f)).collect(),
            candidates: files.iter().map(|_| None).collect(),
        })
    }

    pub(super) fn recorded(&self, track: usize) -> Option<&Recorded> {
        self.recorded[track].as_ref()
    }

    pub(super) fn offer(&mut self, track: usize, candidate: Candidate) {
        self.candidates[track] = Some(candidate);
    }

    pub(super) fn is_complete(&self) -> bool {
        self.candidates.iter().all(Option::is_some)
    }

    /// 各轨都有候选后作出判定；另起会话而会话编号已达上限时为 None。
    pub(super) fn decide(self) -> Option<Decision> {
        let candidates: Vec<Candidate> = self
            .candidates
            .into_iter()
            .map(|c| c.expect("判定前各轨都已有候选"))
            .collect();
        let previous = self.previous_session;
        let continued =
            self.recorded
                .iter()
                .zip(&candidates)
                .all(|(recorded, c)| match c.verdict {
                    Verdict::Full => true,
                    Verdict::Matches => recorded.as_ref().is_some_and(|r| r.session == previous),
                    Verdict::Differs => false,
                });
        let session = if continued {
            previous
        } else {
            previous.checked_add(1)?
        };
        let tracks = self
            .recorded
            .into_iter()
            .zip(candidates)
            .map(|(recorded, candidate)| {
                let start = match (recorded, candidate.verdict) {
                    (Some(r), Verdict::Matches) if continued => Start::Continue(r),
                    (Some(r), Verdict::Matches) => Start::After { through: r.last() },
                    _ => Start::Fresh,
                };
                (start, candidate)
            })
            .collect();
        Some(Decision {
            session,
            may_switch_source: continued,
            tracks,
        })
    }
}
