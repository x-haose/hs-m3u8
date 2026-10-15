//! 续录时的会话判定：由各轨的结论定下会话编号、各轨的起点，以及本次的记录有须核对的差别时能否改记（纯计算；
//! 等各轨的候选、核对内容见 [`super::deciding`]）。
//!
//! 一条轨的窗口与它最近一次有分片的会话重叠（身份与编号一致），且重叠的分片重新下载后与已存的逐字节相同，
//! 才算接得上；编码器重启后序号与文件名都可能从头再来，只看文件名分不出是不是同一段内容。已录满、这次不录的轨同样
//! 核对（用任务目录里的 init 段），只作对齐的证据。至少一条轨接得上、没有一条轨内容不同时，接着目录里最近的会话录：
//! 最近的会话就是它的轨补录没录完的分片，其余要录的轨跳过录过的序号并入（之前没录到的从窗口起点并入）；否则另起一个
//! 会话，接得上的轨跳过录过的序号。没有一条轨核对一致时，之前没录到的轨不知道与哪段时间对得上，不并入旧会话。另起的
//! 会话里有轨不录（已录满或这次没有内容）时，它在这个会话里没有分片，录到的合并不进去，判定时即报错。
//!
//! 本次的记录与目录里的有须核对的差别（见 [`SourceChange`]）时：所选的轨身份变了的轨，目录里已有它的分片的，须它自己
//! 核对一致，别的轨接得上不能替它作证；地址变了的，须接着最近的会话录。做不到即无法确认是同一内容，判定时报错；这次没
//! 有要录的轨而地址变了时不改记，照常合并。

use std::collections::BTreeMap;

use crate::workdir::{SegmentFile, SegmentName, SessionStart, SourceChange, Stored};

/// 一条轨在会话开始时的状况。
pub(super) enum Start {
    /// 在这个会话里从这里开始录，起点要记进任务目录
    New(SessionStart),
    /// 接着这条轨在这个会话里已录的部分：沿用其编号与已记下的起点，补录窗口内尚未录完的分片
    Continue(LatestSession),
}

/// 一条轨最近一次有分片的会话中已录完的分片。
pub(super) struct LatestSession {
    session: u32,
    /// 序号 → 文件名记录的信息
    segments: BTreeMap<u64, SegmentName>,
    /// 这条轨在这个会话的起点
    start: SessionStart,
}

impl LatestSession {
    /// 第 `track` 条轨的分片（按会话、序号排列）中最近一个会话的。一个分片都没有时为 Ok(None)；这个会话的分片
    /// 记的起点不一致时目录内容矛盾，返回原因。
    fn latest(track: usize, files: &[SegmentFile]) -> Result<Option<Self>, String> {
        let Some(session) = files.iter().map(|f| f.name.session).max() else {
            return Ok(None);
        };
        let segments: BTreeMap<u64, SegmentName> = files
            .iter()
            .filter(|f| f.name.session == session)
            .map(|f| (f.name.sequence, f.name))
            .collect();
        let start = segments
            .values()
            .next()
            .expect("最近的会话至少有一个分片")
            .start;
        if segments.values().any(|name| name.start != start) {
            return Err(format!(
                "第 {track} 条轨会话 {session} 的分片记的起点不一致"
            ));
        }
        Ok(Some(LatestSession {
            session,
            segments,
            start,
        }))
    }

    pub(super) fn segments(&self) -> &BTreeMap<u64, SegmentName> {
        &self.segments
    }

    pub(super) fn last(&self) -> u64 {
        *self
            .segments
            .keys()
            .next_back()
            .expect("最近的会话至少有一个分片")
    }

    pub(super) fn start(&self) -> SessionStart {
        self.start
    }

    /// 这条轨在这个会话的起点跳过到的序号；补录不越过它。从窗口起点录的为 None。
    pub(super) fn skipped_through(&self) -> Option<u64> {
        match self.start {
            SessionStart::Fresh => None,
            SessionStart::After(through) => Some(through),
        }
    }
}

/// 一条轨的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// 窗口与已录的分片重叠一致，内容核对相同
    Matches,
    /// 有矛盾或内容不同；要录的轨没有可核对的重叠（重叠的分片都标了 EXT-X-GAP 也算）、或重叠的分片都已取不到时也算
    /// （接不上，只能另起会话）
    Differs,
    /// 已录满、这次不录的轨核对不了：没有可核对的重叠、重叠的分片都已取不到，或播放列表里没有分片。不影响判定
    Unverifiable,
    /// 这条轨之前一个分片都没录到：没有可核对的，在定下的会话里从窗口起点录；另有轨核对一致时不阻挡接续
    Unrecorded,
    /// 没拿到有分片的播放列表就结束了：播放列表已结束而没有分片，或停滞且其他轨也不再出新分片。这次不录，
    /// 不影响判定
    Ended,
}

/// 作不出判定的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Undecided {
    /// 本次的记录有须核对的差别而核对不上（见模块说明）
    Unverified,
    /// 要另起会话而会话编号已达上限
    SessionsExhausted,
    /// 要另起会话，而第几条轨在这个会话里不录（已录满或这次没有内容）：会话里各轨都有的组才合并得了，录到的合并不进去
    Unaligned(usize),
}

/// 判定结果。
pub(super) struct Decision {
    pub session: u32,
    /// 本次的记录有须核对的差别时，已核对上、改记（见模块说明）；没有这种差别时不起作用
    pub adopt: bool,
    /// 各轨的起点；None 为这次不录（已录满，或见 [`Verdict::Ended`]）
    pub tracks: Vec<Option<Start>>,
}

impl Decision {
    /// 目录里没有录过的分片：第 0 个会话，`tracks` 条轨都从窗口起点录。
    pub(super) fn first(tracks: usize) -> Self {
        Decision {
            session: 0,
            adopt: true,
            tracks: (0..tracks)
                .map(|_| Some(Start::New(SessionStart::Fresh)))
                .collect(),
        }
    }
}

/// 正在判定的会话：各轨最近的会话与收集到的结论。
pub(super) struct Verdicts {
    /// 目录里最近的会话编号
    previous_session: u32,
    /// 各轨最近一次有分片的会话
    recorded: Vec<Option<LatestSession>>,
    /// 各轨已录满 max_duration、这次不录
    full: Vec<bool>,
    verdicts: Vec<Option<Verdict>>,
    /// 本次的记录与目录里的须核对的差别
    change: Option<SourceChange>,
}

impl Verdicts {
    /// 由目录里已录的内容开始判定，`change` 为本次的记录与目录里的须核对的差别；一个分片都没有时为 Ok(None)（直接录
    /// 第 0 个会话）。某轨最近的会话里分片记的起点不一致时目录内容矛盾，返回原因。
    pub(super) fn new(
        stored: &Stored,
        change: Option<&SourceChange>,
    ) -> Result<Option<Self>, String> {
        let Some(previous_session) = stored
            .segments
            .iter()
            .flatten()
            .map(|f| f.name.session)
            .max()
        else {
            return Ok(None);
        };
        let recorded = stored
            .segments
            .iter()
            .enumerate()
            .map(|(track, files)| LatestSession::latest(track, files))
            .collect::<Result<_, _>>()?;
        Ok(Some(Verdicts {
            previous_session,
            recorded,
            full: stored.segments.iter().map(|_| false).collect(),
            verdicts: stored.segments.iter().map(|_| None).collect(),
            change: change.cloned(),
        }))
    }

    pub(super) fn recorded(&self, track: usize) -> Option<&LatestSession> {
        self.recorded[track].as_ref()
    }

    /// 第 `track` 条轨已录满、这次不录，只核对作证据。
    pub(super) fn mark_full(&mut self, track: usize) {
        self.full[track] = true;
    }

    pub(super) fn is_full(&self, track: usize) -> bool {
        self.full[track]
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

    /// 各轨都有结论后作出判定，取走各轨最近的会话（规则见模块说明）。没有要录的轨时不另起会话。
    pub(super) fn decide(&mut self) -> Result<Decision, Undecided> {
        let verdicts: Vec<Verdict> = std::mem::take(&mut self.verdicts)
            .into_iter()
            .map(|v| v.expect("判定前各轨都已有结论"))
            .collect();
        let records: Vec<bool> = verdicts
            .iter()
            .zip(&self.full)
            .map(|(&v, &full)| !full && v != Verdict::Ended)
            .collect();
        let previous = self.previous_session;
        let recording = records.contains(&true);
        let continued =
            verdicts.contains(&Verdict::Matches) && !verdicts.contains(&Verdict::Differs);
        let adopt = match &self.change {
            None => false,
            Some(change) => {
                let unproven = change.tracks.iter().zip(&verdicts).zip(&self.recorded).any(
                    |((&changed, &verdict), recorded)| {
                        changed && recorded.is_some() && verdict != Verdict::Matches
                    },
                );
                if unproven || (change.url && recording && !continued) {
                    return Err(Undecided::Unverified);
                }
                !change.url || recording
            }
        };
        let session = if continued || !recording {
            previous
        } else {
            if let Some(idle) = records.iter().position(|&r| !r) {
                return Err(Undecided::Unaligned(idle));
            }
            previous
                .checked_add(1)
                .ok_or(Undecided::SessionsExhausted)?
        };
        let tracks = std::mem::take(&mut self.recorded)
            .into_iter()
            .zip(verdicts)
            .zip(records)
            .map(|((recorded, verdict), records)| match (recorded, verdict) {
                _ if !records => None,
                (Some(r), Verdict::Matches) if continued && r.session == previous => {
                    Some(Start::Continue(r))
                }
                (Some(r), Verdict::Matches) => Some(Start::New(SessionStart::After(r.last()))),
                _ => Some(Start::New(SessionStart::Fresh)),
            })
            .collect();
        Ok(Decision {
            session,
            adopt,
            tracks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::Fingerprint;

    /// 第 `session` 个会话里从窗口起点录的、序号为 `sequences` 的分片。
    fn recorded(session: u32, sequences: &[u64]) -> LatestSession {
        let segments = sequences
            .iter()
            .map(|&sequence| {
                let name = SegmentName {
                    session,
                    start: SessionStart::Fresh,
                    sequence,
                    discontinuity: 0,
                    init: None,
                    duration_us: 1_000_000,
                    id: Fingerprint::of_content(&sequence.to_be_bytes()),
                };
                (sequence, name)
            })
            .collect();
        LatestSession {
            session,
            segments,
            start: SessionStart::Fresh,
        }
    }

    /// 视频（第 0 条轨）在会话 3 录过、音频（第 1 条轨）一个分片都没录到，视频已录满时 `video_full`，判定结论为
    /// `video` 与 `audio` 时的结果。
    fn decide(video_full: bool, video: Verdict, audio: Verdict) -> Result<Decision, Undecided> {
        decide_with(false, [video_full, false], None, [video, audio])
    }

    /// 视频（第 0 条轨）在会话 3 录过，音频（第 1 条轨）也录过时 `audio_recorded`；各轨已录满为 `full`，本次的记录与
    /// 目录里须核对的差别为 `change`，各轨的结论为 `verdicts` 时的结果。
    fn decide_with(
        audio_recorded: bool,
        full: [bool; 2],
        change: Option<SourceChange>,
        verdicts: [Verdict; 2],
    ) -> Result<Decision, Undecided> {
        let mut decision = Verdicts {
            previous_session: 3,
            recorded: vec![
                Some(recorded(3, &[10, 11])),
                audio_recorded.then(|| recorded(3, &[10, 11])),
            ],
            full: full.to_vec(),
            verdicts: vec![None, None],
            change,
        };
        for (track, verdict) in verdicts.into_iter().enumerate() {
            decision.conclude(track, verdict);
        }
        decision.decide()
    }

    fn change(url: bool, tracks: [bool; 2]) -> Option<SourceChange> {
        Some(SourceChange {
            url,
            tracks: tracks.to_vec(),
        })
    }

    #[test]
    fn an_unrecorded_track_joins_the_old_session_only_when_some_track_matches() {
        // 视频已录满，核对一致：作为证据，接着会话 3，视频不录，音频从窗口起点并入
        let full = decide(true, Verdict::Matches, Verdict::Unrecorded).unwrap();
        assert_eq!(full.session, 3);
        assert!(matches!(
            full.tracks[..],
            [None, Some(Start::New(SessionStart::Fresh))]
        ));

        // 视频已录满、核对不了：音频不知道与旧会话哪段时间对得上，另起的会话里视频不录，合并不进去
        assert!(matches!(
            decide(true, Verdict::Unverifiable, Verdict::Unrecorded),
            Err(Undecided::Unaligned(0))
        ));

        // 视频核对一致：接着会话 3，音频从窗口起点并入
        let matched = decide(false, Verdict::Matches, Verdict::Unrecorded).unwrap();
        assert_eq!(matched.session, 3);
        assert!(matches!(
            matched.tracks[..],
            [
                Some(Start::Continue(_)),
                Some(Start::New(SessionStart::Fresh))
            ]
        ));

        // 视频接不上：另起会话，视频从窗口起点录
        let differs = decide(false, Verdict::Differs, Verdict::Unrecorded).unwrap();
        assert_eq!(differs.session, 4);
        assert!(matches!(
            differs.tracks[..],
            [
                Some(Start::New(SessionStart::Fresh)),
                Some(Start::New(SessionStart::Fresh))
            ]
        ));
    }

    /// 地址变了：接着最近的会话录才改记，要另起会话时报错；这次没有要录的轨时不改记，照常合并。
    #[test]
    fn a_new_address_is_adopted_only_when_the_session_continues() {
        let url = || change(true, [false, false]);
        let both = |video, audio| decide_with(true, [false, false], url(), [video, audio]);
        assert!(both(Verdict::Matches, Verdict::Matches).unwrap().adopt);
        assert!(matches!(
            both(Verdict::Differs, Verdict::Matches),
            Err(Undecided::Unverified)
        ));
        let full = decide_with(
            true,
            [true, true],
            url(),
            [Verdict::Unverifiable, Verdict::Unverifiable],
        );
        assert!(!full.unwrap().adopt);
    }

    /// 重新选轨换了身份的轨：目录里有它的分片时，须它自己核对一致，已录满、这次没有内容而核对不了的都不行，别的轨接得上
    /// 不能替它作证；目录里没有它的分片时无须核对。
    #[test]
    fn a_track_whose_identity_changed_must_match_by_itself() {
        let audio = || change(false, [false, true]);
        for (full, unproven) in [
            (true, Verdict::Unverifiable),
            (false, Verdict::Ended),
            (false, Verdict::Differs),
        ] {
            assert!(
                matches!(
                    decide_with(true, [false, full], audio(), [Verdict::Matches, unproven]),
                    Err(Undecided::Unverified)
                ),
                "{unproven:?}"
            );
        }
        let matched = decide_with(
            true,
            [false, true],
            audio(),
            [Verdict::Matches, Verdict::Matches],
        );
        assert!(matched.unwrap().adopt);
        let unrecorded = decide_with(
            false,
            [false, false],
            audio(),
            [Verdict::Matches, Verdict::Unrecorded],
        );
        assert!(unrecorded.unwrap().adopt);
    }
}
