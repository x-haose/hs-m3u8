//! 录制中的停滞判定：哪条轨停滞了、什么时候能下结论、结论是什么。

use std::time::Duration;

use tokio::time::Instant;

use super::Recorder;
use crate::{Error, LiveEnd};

/// 计时器醒来比定的时刻晚这么多，即循环停住过：远大于正常的调度延迟（毫秒级），又远小于常见的目标时长。
const FROZEN: Duration = Duration::from_millis(500);

/// 当前这次停滞：停滞的轨、停滞开始的时刻（见 [`LiveTrack::stall_since`](super::LiveTrack::stall_since)），与循环察觉它
/// 的时刻（进程被挂起时为醒来之后；察觉之后循环停住过的，改为醒来的时刻，见 [`Recorder::after_wake`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stall {
    pub track: usize,
    pub since: Instant,
    pub noticed: Instant,
}

impl Recorder<'_> {
    /// 最早开始停滞的轨及其停滞开始的时刻（见 [`LiveTrack::stall_since`](super::LiveTrack::stall_since)）。
    fn next_stall(&self) -> Option<(Instant, usize)> {
        self.refreshing()
            .filter_map(|(i, t)| Some((t.stall_since(self.options.stall_timeout)?, i)))
            .min()
    }

    /// 到 `now` 已开始的停滞；第一次看到它时以 `now` 为察觉的时刻记下，停滞的轨或开始的时刻变了即是另一次。
    pub(super) fn observe_stall(&mut self, now: Instant) -> Option<Stall> {
        let current = self
            .next_stall()
            .filter(|&(since, _)| since <= now)
            .map(|(since, track)| match self.stall {
                Some(seen) if (seen.track, seen.since) == (track, since) => seen,
                _ => Stall {
                    track,
                    since,
                    noticed: now,
                },
            });
        self.stall = current;
        current
    }

    /// 下次检查停滞的时刻：还没开始停滞时为最早的停滞开始时刻；已开始的停滞此刻能下结论时为 `now`，否则为让它能下结论的
    /// 最早的将来时刻（有轨在途的请求满期限，见 [`LiveTrack::deadline`](super::LiveTrack::deadline)），没有时为 None
    /// （等刷新返回或准备完成的事件）。只取将来的时刻：已过去的不会再让结论可下，取了会让循环空转。
    pub(super) fn stall_check_at(&self, now: Instant, stall: Option<Stall>) -> Option<Instant> {
        let Some(stall) = stall else {
            return self.next_stall().map(|(since, _)| since);
        };
        if self.judgeable(stall, now) {
            return Some(now);
        }
        self.refreshing()
            .filter_map(|(_, t)| t.deadline(stall.noticed))
            .filter(|&at| at > now)
            .min()
    }

    /// 此刻能对停滞 `stall` 下结论：还要刷新的各轨（含停滞的轨）在停滞开始之后的状况都有据可依（见
    /// [`LiveTrack::settled`](super::LiveTrack::settled)）。结论只依据停滞开始之后的刷新：进程被挂起醒来时停滞已到，而
    /// 期间一次刷新也没发。
    pub(super) fn judgeable(&self, stall: Stall, now: Instant) -> bool {
        self.refreshing()
            .all(|(_, t)| t.settled(stall.since, stall.noticed, now))
    }

    /// 对停滞 `stall` 下结论：其他还要刷新的轨有故障（见 [`LiveTrack::fault`](super::LiveTrack::fault)）时以它的原因使
    /// 任务失败；否则看起来直播已结束时定下结束原因，不然任务失败。会话定下之前停滞的是还没拿到候选的轨，直播看起来已结
    /// 束时，还没拿到候选的轨都没有内容，这次都不录。
    pub(super) async fn on_stall(&mut self, stall: Stall, now: Instant) -> Result<(), Error> {
        let Stall {
            track,
            since,
            noticed,
        } = stall;
        let others = || self.refreshing().filter(move |&(i, _)| i != track);
        if let Some((other, cause)) =
            others().find_map(|(i, t)| Some((i, t.fault(since, noticed, now)?)))
        {
            return Err(Error::LiveStalled {
                track: other,
                cause,
            });
        }
        let others_live = others().any(|(_, t)| t.listed_since(since));
        let cause = self.tracks[track]
            .stall(now, since, noticed, others_live)
            .map_err(|cause| Error::LiveStalled { track, cause })?;
        self.end(LiveEnd::Stalled { track, cause });
        if self.tracks[track].is_undecided() {
            self.drop_waiting().await?;
        }
        Ok(())
    }

    /// 循环醒来（`woke`），`check_at` 为醒来之前定的停滞检查时刻：比它晚了 [`FROZEN`]（或各轨中最短的目标时长，取小的）
    /// 以上，即循环停住过（进程被挂起、调试器暂停）：在途的请求可能其间已返回而还没处理到，当前停滞的期限从醒来起重新
    /// 算。
    pub(super) fn after_wake(&mut self, check_at: Option<Instant>, woke: Instant) {
        let frozen = self.refreshing().map(|(_, t)| t.target()).min();
        if let (Some(at), Some(frozen)) = (check_at, frozen.map(|target| target.min(FROZEN)))
            && woke.saturating_duration_since(at) >= frozen
        {
            self.stall = self.stall.map(|s| Stall { noticed: woke, ..s });
        }
    }
}
