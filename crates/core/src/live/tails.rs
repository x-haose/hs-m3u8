//! 直播字幕轨尾部没录到（见 [`crate::UnrecordedTail`]）的记录：每件事处理完都由各轨的状态推出该不该记（见
//! [`Recorder::wants_tail`]），有变化即写进任务目录，所以取消、失败或进程退出之后只合并照样报得出；录制结束时另给出本次
//! 判定的原因。字幕轨不结束录制，它的故障在这里报出。

use tokio::time::Instant;

use super::{Phase, Recorder};
use crate::{Error, TailCause};

impl Recorder<'_> {
    /// 会话定下后，第 `track` 条字幕轨此刻（`now`）该不该记为尾部没录到：前后接不上而停了的记；出现了 ENDLIST 或录满的
    /// 不记；否则已停滞（见 [`super::LiveTrack::stall_since`]）、或之前的运行记下而本次还没录到它的新分片的记。音视频轨
    /// 不记。
    fn wants_tail(&self, track: usize, now: Instant) -> bool {
        if self.media[track] {
            return false;
        }
        let t = &self.tracks[track];
        if t.is_broken() {
            return true;
        }
        if t.is_ended() || t.is_full(self.max_us()) {
            return false;
        }
        self.carried[track]
            || t.stall_since(self.options.stall_timeout)
                .is_some_and(|since| since <= now)
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
        for track in 0..self.tracks.len() {
            let tailed = self.wants_tail(track, now);
            if tailed != self.tails_written[track] {
                self.dir.set_tail(session, track, tailed).await?;
                self.tails_written[track] = tailed;
                self.progress
                    .send_modify(|p| p.tracks[track].unrecorded_tail = tailed);
            }
        }
        Ok(())
    }

    /// 录制结束（会话已定下、在途的下载已收尾）时改记各字幕轨（见 [`Recorder::update_tails`]），返回本次判定得出原因的
    /// （见 [`super::LiveTrack::unrecorded_tail`]），按轨号。之前的运行记下、本次没满停滞时限的照旧记着，没有原因。
    pub(super) async fn settle_tails(
        &mut self,
        now: Instant,
    ) -> Result<Vec<(usize, TailCause)>, Error> {
        self.update_tails(now).await?;
        let (timeout, max_us) = (self.options.stall_timeout, self.max_us());
        let mut tails = Vec::new();
        for track in (0..self.tracks.len()).filter(|&t| !self.media[t]) {
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
                tails.push((track, cause));
            }
        }
        Ok(tails)
    }
}
