//! 直播字幕轨尾部没录到（见 [`crate::UnrecordedTail`]）的记录：每件事处理完都由各轨的状态推出该不该记（见
//! [`Recorder::wants_tail`]），有变化即写进任务目录，所以取消、失败或进程退出之后只合并照样报得出；录制结束时另给出本次
//! 判定的原因。字幕轨不结束录制，它的故障在这里报出。

use tokio::time::Instant;

use super::{Phase, Recorder};
use crate::Error;

impl Recorder<'_> {
    /// 会话定下后，第 `track` 条字幕轨该不该记为尾部没录到：前后接不上而停了的记；出现了 ENDLIST 或录满的不记；否则
    /// 停滞过而之后还没录到新分片的（见 `marked`）记。音视频轨不记。
    fn wants_tail(&self, track: usize) -> bool {
        if self.media[track] {
            return false;
        }
        let t = &self.tracks[track];
        if t.is_broken() {
            return true;
        }
        !t.is_ended() && !t.is_full(self.max_us()) && self.marked[track]
    }

    /// 此刻（`now`）已停滞（见 [`super::LiveTrack::stall_since`]）的字幕轨置上 `marked`。
    fn mark_stalled(&mut self, now: Instant) {
        for track in 0..self.tracks.len() {
            let stalled = !self.media[track]
                && self.tracks[track]
                    .stall_since(self.options.stall_timeout)
                    .is_some_and(|since| since <= now);
            if stalled {
                self.marked[track] = true;
            }
        }
    }

    /// 还没记为尾部没录到的字幕轨里，最早变得该记的时刻（停滞开始的时刻，见 [`Recorder::wants_tail`]）；会话定下之前或
    /// 没有时为 None。
    pub(super) fn tail_due(&self) -> Option<Instant> {
        if !matches!(self.phase, Phase::Decided { .. }) {
            return None;
        }
        let max_us = self.max_us();
        (0..self.tracks.len())
            .filter(|&t| !self.media[t] && !self.tails_written[t])
            .filter_map(|t| {
                let track = &self.tracks[t];
                if track.is_broken() || track.is_ended() || track.is_full(max_us) {
                    return None;
                }
                track.stall_since(self.options.stall_timeout)
            })
            .min()
    }

    /// 按 [`Recorder::wants_tail`] 改记各字幕轨，有变化的写进任务目录并更新进度。会话定下之前什么也不做。
    pub(super) async fn update_tails(&mut self, now: Instant) -> Result<(), Error> {
        let Phase::Decided { session } = self.phase else {
            return Ok(());
        };
        self.mark_stalled(now);
        for track in 0..self.tracks.len() {
            let tailed = self.wants_tail(track);
            if tailed != self.tails_written[track] {
                self.dir.set_tail(session, track, tailed).await?;
                self.tails_written[track] = tailed;
                self.progress
                    .send_modify(|p| p.tracks[track].unrecorded_tail = tailed);
            }
        }
        Ok(())
    }

    /// 录制结束（会话已定下、在途的下载已收尾）时改记各字幕轨（见 [`Recorder::update_tails`]），并判出这个会话里记为尾部
    /// 没录到的原因（见 [`Recorder::note_tail_causes`]）。
    pub(super) async fn settle_tails(&mut self, now: Instant) -> Result<(), Error> {
        self.update_tails(now).await?;
        self.note_tail_causes(now);
        Ok(())
    }

    /// 录制中的会话里记为尾部没录到的字幕轨，判出本次的原因（见 [`super::LiveTrack::unrecorded_tail`]）记进
    /// `tail_causes`；之前的运行记下、本次没满停滞时限的没有原因。录制结束时与另起会话之前各调一次。
    pub(super) fn note_tail_causes(&mut self, now: Instant) {
        let session = self.session();
        let (timeout, max_us) = (self.options.stall_timeout, self.max_us());
        for track in (0..self.tracks.len()).filter(|&t| self.tails_written[t]) {
            let media = &self.media;
            let (before, rest) = self.tracks.split_at_mut(track);
            let (this, after) = rest.split_first_mut().expect("track 在范围内");
            // 音视频轨在 since 之后还列出过新分片
            let others_live = |since| {
                let after = after.iter().enumerate().map(|(i, t)| (track + 1 + i, t));
                before
                    .iter()
                    .enumerate()
                    .chain(after)
                    .any(|(i, t)| media[i] && t.listed_since(since))
            };
            if let Some(cause) = this.unrecorded_tail(timeout, max_us, now, others_live) {
                self.tail_causes.push((session, track, cause));
            }
        }
    }
}
