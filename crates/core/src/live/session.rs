//! 续录时的会话判定。
//!
//! 中断后再次运行，先等各轨拿到第一份有分片的播放列表。各轨都与上一个会话已录的分片重叠（身份与编号一致），
//! 且重叠中序号最大的那个分片重新下载后与已存的逐字节相同，才接着该会话录；否则另起一个会话，
//! 核对一致的轨跳过之前录过的序号。编码器重启后序号与文件名都可能从头再来，只看文件名分不出是不是同一段内容，
//! 所以要核对内容。

use std::collections::BTreeMap;

use hs_m3u8_hls::MediaPlaylist;
use tokio::time::Instant;

use super::window::NewInits;
use crate::workdir::{SegmentFile, SegmentName};

/// 一条轨在会话开始时的状况。
pub(super) enum Start {
    /// 新会话，窗口内的分片全部录
    Fresh,
    /// 新会话，序号不超过 `through` 的分片之前已录过，跳过
    After { through: u64 },
    /// 接着之前的会话：沿用其编号，补录窗口内尚未录完的分片
    Continue(Earlier),
}

/// 之前的会话中一条轨已录完的分片，按序号。
pub(super) struct Earlier(BTreeMap<u64, SegmentName>);

impl Earlier {
    /// 第 `session` 个会话的分片；一个都没有时为 None。
    pub(super) fn of(files: &[SegmentFile], session: u32) -> Option<Self> {
        let segments: BTreeMap<u64, SegmentName> = files
            .iter()
            .filter(|f| f.name.session == session)
            .map(|f| (f.name.sequence, f.name))
            .collect();
        (!segments.is_empty()).then_some(Earlier(segments))
    }

    pub(super) fn segments(&self) -> &BTreeMap<u64, SegmentName> {
        &self.0
    }

    pub(super) fn last(&self) -> u64 {
        *self.0.keys().next_back().expect("Earlier 至少有一个分片")
    }
}

/// 判定会话时，一条轨拿到的第一份有分片（或已结束）的播放列表。
pub(super) struct Candidate {
    pub playlist: MediaPlaylist,
    pub fetched: NewInits,
    /// 这次加载开始的时刻
    pub started: Instant,
    /// 能否接着上一个会话录
    pub fits: bool,
}

/// 正在判定的会话。
pub(super) struct Deciding {
    /// 上一个会话的编号
    last: u32,
    /// 各轨在上一个会话中已录的分片
    earlier: Vec<Option<Earlier>>,
    candidates: Vec<Option<Candidate>>,
}

/// 判定结果。
pub(super) struct Decision {
    pub session: u32,
    /// 接着上一个会话录
    pub continued: bool,
    /// 各轨的起点与暂存的播放列表
    pub tracks: Vec<(Start, Candidate)>,
}

impl Deciding {
    pub(super) fn new(last: u32, earlier: Vec<Option<Earlier>>) -> Self {
        let candidates = earlier.iter().map(|_| None).collect();
        Deciding {
            last,
            earlier,
            candidates,
        }
    }

    pub(super) fn earlier(&self, track: usize) -> Option<&Earlier> {
        self.earlier[track].as_ref()
    }

    pub(super) fn offer(&mut self, track: usize, candidate: Candidate) {
        self.candidates[track] = Some(candidate);
    }

    pub(super) fn is_complete(&self) -> bool {
        self.candidates.iter().all(Option::is_some)
    }

    /// 各轨都有候选后作出判定，取走各轨的已录分片与候选；另起会话而会话编号已达上限时为 None。
    pub(super) fn decide(&mut self) -> Option<Decision> {
        let candidates: Vec<Candidate> = std::mem::take(&mut self.candidates)
            .into_iter()
            .map(|c| c.expect("判定前各轨都已有候选"))
            .collect();
        let continued = candidates.iter().all(|c| c.fits);
        let session = if continued {
            self.last
        } else {
            self.last.checked_add(1)?
        };
        let tracks = std::mem::take(&mut self.earlier)
            .into_iter()
            .zip(candidates)
            .map(|(earlier, candidate)| {
                let start = match earlier {
                    Some(earlier) if continued => Start::Continue(earlier),
                    Some(earlier) if candidate.fits => Start::After {
                        through: earlier.last(),
                    },
                    _ => Start::Fresh,
                };
                (start, candidate)
            })
            .collect();
        Some(Decision {
            session,
            continued,
            tracks,
        })
    }
}
