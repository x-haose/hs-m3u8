//! 直播字幕轨尾部没录到（见 [`crate::UnrecordedTail`]）的记录：录制中察觉即写进任务目录，录到新分片即去掉，录制结束时
//! 按最终的结局再核一遍。字幕轨不结束录制，它的故障在这里报出。

use tokio::time::Instant;

use super::{Phase, Recorder};
use crate::{Error, TailCause};

impl Recorder<'_> {
    /// 下一个要记为尾部没录到的时刻：会话定下后，还没记的、没出现 ENDLIST 的字幕轨里，最早的停滞开始时刻（见
    /// [`super::LiveTrack::stall_since`]），前后接不上而停了的为现在。没有时为 None。
    pub(super) fn tail_due(&self) -> Option<Instant> {
        if !matches!(self.phase, Phase::Decided { .. }) {
            return None;
        }
        let (timeout, max_us) = (self.options.stall_timeout, self.max_us());
        (0..self.tracks.len())
            .filter(|&t| !self.media[t] && !self.tailed[t])
            .filter_map(|t| {
                let track = &self.tracks[t];
                if track.is_broken() {
                    return Some(Instant::now());
                }
                if track.is_ended() || track.is_full(max_us) {
                    return None;
                }
                track.stall_since(timeout)
            })
            .min()
    }

    /// 到了 `now` 的（见 [`Recorder::tail_due`]）记为尾部没录到。
    pub(super) async fn mark_tails(&mut self, now: Instant) -> Result<(), Error> {
        if !matches!(self.phase, Phase::Decided { .. }) {
            return Ok(());
        }
        let (timeout, max_us) = (self.options.stall_timeout, self.max_us());
        for t in 0..self.tracks.len() {
            if self.media[t] || self.tailed[t] {
                continue;
            }
            let track = &self.tracks[t];
            let due = track.is_broken()
                || (!track.is_ended()
                    && !track.is_full(max_us)
                    && track.stall_since(timeout).is_some_and(|since| since <= now));
            if due {
                self.tailed[t] = true;
            }
        }
        self.write_tails().await
    }

    /// 把应记的尾部（`tailed`）与已写进任务目录的不同之处写出。会话定下之前什么也不做。
    pub(super) async fn write_tails(&mut self) -> Result<(), Error> {
        let Phase::Decided { session } = self.phase else {
            return Ok(());
        };
        for track in 0..self.tracks.len() {
            let tailed = self.tailed[track];
            if tailed != self.tails_written[track] {
                self.dir.set_tail(session, track, tailed).await?;
                self.tails_written[track] = tailed;
                self.progress
                    .send_modify(|p| p.tracks[track].unrecorded_tail = tailed);
            }
        }
        Ok(())
    }

    /// 录制结束（会话已定下、在途的下载已收尾）时各字幕轨的结局（见 [`super::LiveTrack::unrecorded_tail`]）：尾部没录到
    /// 的记下并返回原因，按轨号；出现了 ENDLIST 或录满的去掉；录制中没再录到新分片、也还没满停滞时限的照旧（之前的运行
    /// 记下的留着）。
    pub(super) async fn settle_tails(
        &mut self,
        now: Instant,
    ) -> Result<Vec<(usize, TailCause)>, Error> {
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
            let finished = !this.is_broken() && (this.is_ended() || this.is_full(max_us));
            let cause = this.unrecorded_tail(timeout, max_us, now, others_live);
            self.tailed[track] = match &cause {
                Some(_) => true,
                None if finished => false,
                None => self.tailed[track],
            };
            if let Some(cause) = cause {
                tails.push((track, cause));
            }
        }
        self.write_tails().await?;
        Ok(tails)
    }
}
