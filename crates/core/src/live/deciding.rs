//! 续录时的会话判定期间：等各轨拿到候选（第一份有分片的播放列表），核对能否接着它最近的会话录，各轨都有
//! 结论后定下会话（规则见 [`super::session`]），再开始录。判定期间各轨照常刷新，拉到的都暂存起来，会话定下后
//! 按顺序处理；已有候选的轨在等本任务，不算停滞。

use hs_m3u8_hls::MediaPlaylist;
use tokio::time::Instant;

use super::session::{Decision, NewUrl, Verdict, Verdicts};
use super::tasks::StoredOverlap;
use super::track::Fetched;
use super::window::overlaps;
use super::{Phase, Recorder};
use crate::workdir::WorkDir;
use crate::{Error, WorkDirProblem};

impl Recorder<'_> {
    /// 开始录制，`first` 为各轨首次拉到的播放列表。目录里没有录过的分片时直接定下第 0 个会话；否则开始判定，
    /// 已录满的轨不参与，其余以 `first` 作为判定期间拉到的第一份。
    pub(super) async fn start(&mut self, first: Vec<Fetched>) -> Result<(), Error> {
        if matches!(self.phase, Phase::Decided { .. }) {
            let tracks = first.len();
            for (track, fetched) in first.into_iter().enumerate() {
                self.tracks[track].hold(fetched);
            }
            return self.enter_session(Decision::first(tracks)).await;
        }
        let max_us = self.max_us();
        for (track, fetched) in first.into_iter().enumerate() {
            if self.tracks[track].is_full(max_us) {
                self.verdicts().conclude(track, Verdict::Full);
            } else {
                self.note(track, fetched);
            }
        }
        self.decide_if_complete().await
    }

    /// 会话定下之前第 `track` 条轨刷新拿到一份播放列表。
    pub(super) async fn undecided(&mut self, track: usize, fetched: Fetched) -> Result<(), Error> {
        self.note(track, fetched);
        self.decide_if_complete().await
    }

    /// 第 `track` 条轨核对完。
    pub(super) async fn checked(&mut self, track: usize, verdict: Verdict) -> Result<(), Error> {
        self.verdicts().conclude(track, verdict);
        self.decide_if_complete().await
    }

    /// 会话定下之前直播看起来已结束：还没拿到候选（也没有已录满）的轨都没有内容，这次不录，不影响判定。
    pub(super) async fn drop_waiting(&mut self) -> Result<(), Error> {
        for track in 0..self.tracks.len() {
            if self.tracks[track].has_candidate() || self.verdicts().has_concluded(track) {
                continue;
            }
            self.tracks[track].end_undecided();
            self.verdicts().conclude(track, Verdict::Ended);
        }
        self.decide_if_complete().await
    }

    fn verdicts(&mut self) -> &mut Verdicts {
        match &mut self.phase {
            Phase::Deciding(verdicts) => verdicts,
            Phase::Decided { .. } => panic!("只有判定期间才收集各轨的结论"),
        }
    }

    /// 记下第 `track` 条轨拉到的播放列表是否列出了新分片、安排下次刷新；有分片时暂存，其中第一份开始核对。
    /// 还没拿到候选就拉到已结束而没有分片的，这条轨这次没有可录的，不影响判定。
    fn note(&mut self, track: usize, fetched: Fetched) {
        let t = &mut self.tracks[track];
        t.note_undecided(&fetched.playlist, fetched.started, Instant::now());
        if fetched.playlist.segments.is_empty() {
            if fetched.playlist.ended && !t.has_candidate() {
                t.end_undecided();
                self.verdicts().conclude(track, Verdict::Ended);
            }
            return;
        }
        if !t.has_candidate() {
            t.mark_candidate();
            self.check(track, &fetched.playlist);
        }
        self.tracks[track].hold(fetched);
    }

    /// 核对第 `track` 条轨的候选 `playlist` 能否接着它最近的会话录；没有录过分片或没有重叠时直接得出接不上。
    fn check(&mut self, track: usize, playlist: &MediaPlaylist) {
        let dir: &WorkDir = self.dir;
        let verdicts = self.verdicts();
        let found: Vec<StoredOverlap> = verdicts
            .recorded(track)
            .map(|recorded| {
                overlaps(recorded, playlist)
                    .into_iter()
                    .map(|segment| StoredOverlap {
                        stored: dir
                            .layout()
                            .segment(track, &recorded.segments()[&segment.sequence]),
                        segment: segment.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if found.is_empty() {
            verdicts.conclude(track, Verdict::Differs);
        } else {
            self.tasks.check(self.direct.clone(), track, found);
        }
    }

    /// 各轨都有结论时定下会话；会话编号已达上限时目录无法再用。
    async fn decide_if_complete(&mut self) -> Result<(), Error> {
        let Phase::Deciding(verdicts) = &mut self.phase else {
            return Ok(());
        };
        if !verdicts.is_complete() {
            return Ok(());
        }
        let decision = verdicts.decide().ok_or_else(|| Error::WorkDir {
            path: self.dir.layout().root().to_path_buf(),
            problem: WorkDirProblem::Corrupt("会话编号已达上限".into()),
        })?;
        self.enter_session(decision).await
    }

    /// 定下会话：完整来源地址变了时按 [`NewUrl`] 处理；记下要录的轨的起点；各轨按起点建窗口（这次不录的不建），
    /// 开始逐份处理暂存的播放列表。
    async fn enter_session(&mut self, decision: Decision) -> Result<(), Error> {
        if self.dir.url_changed() {
            match decision.new_url {
                NewUrl::Adopt => self.dir.adopt_url().await?,
                NewUrl::Keep => {}
                NewUrl::Unverified => {
                    return Err(Error::WorkDir {
                        path: self.dir.layout().root().to_path_buf(),
                        problem: WorkDirProblem::SourceUnverified,
                    });
                }
            }
        }
        self.phase = Phase::Decided {
            session: decision.session,
        };
        let now = Instant::now();
        for (track, start) in decision.tracks.into_iter().enumerate() {
            self.tracks[track].enter_session(start, now);
            self.prepare_next(track);
        }
        Ok(())
    }
}
