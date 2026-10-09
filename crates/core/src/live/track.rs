//! 录制中的一条轨：刷新的节奏、停滞的判定，以及它的窗口。

use std::ops::ControlFlow;
use std::time::Duration;

use hs_m3u8_hls::{InitSection, MediaPlaylist};
use tokio::time::Instant;
use url::Url;

use super::session::Start;
use super::window::{NewInits, Processed, Scope, Update, Window};
use crate::{Error, HttpError, LiveEnd, MissReason, StallCause, StallError, Unsupported};

/// 两次刷新之间的最短间隔，防止 TARGETDURATION 为 0 或极小时空转。
const MIN_REFRESH: Duration = Duration::from_millis(100);

/// 这么多个目标时长内列出过新分片，即认为播放列表仍在更新：分片时长不超过目标时长（RFC 8216 4.3.3.1），
/// 正常的直播每个目标时长至少出一个新分片，再留一个目标时长给刷新的间隔。
const LISTING_TARGETS: u32 = 2;

/// 停滞判定至少等这么多个目标时长：比「仍在更新」的判定多一个，正常出分片的直播不会因为 stall_timeout
/// 设得比目标时长还短而被判为停滞。
const STALL_TARGETS: u32 = 3;

enum Refresh {
    Due(Instant),
    InFlight {
        started: Instant,
    },
    /// 续录判定会话期间已拿到候选，等其他轨；`live` 为候选的播放列表还没有结束
    Held {
        live: bool,
    },
    /// 服务器要求的等待长到无法表示：不再刷新，由停滞判定或停止结束录制
    Suspended,
    Ended,
}

/// 发起一次刷新需要的信息。
pub(super) struct RefreshRequest {
    pub url: Url,
    /// 上一份播放列表引用、内容已知的 init 段
    pub known: Vec<InitSection>,
    /// 已处理到哪里；会话还没定下时为 None，这时不知道哪些分片要录，不拉 init 段
    pub processed: Option<Processed>,
}

/// 录制中的一条轨。
pub(super) struct LiveTrack {
    url: Url,
    /// 刷新间隔的基准：TARGETDURATION，为 0 或缺失时取播放列表中最长的分片时长
    target: Duration,
    refresh: Refresh,
    window: Window,
    /// 会话已定下，窗口按会话的起点建好
    begun: bool,
    /// 最近一次有分片下载成功的时刻（或会话定下的时刻）
    last_recorded: Instant,
    /// 最近一次列出要录的新分片的时刻（或录制开始）
    last_listed: Instant,
    /// 排入下载、还没有结果的分片数
    outstanding: usize,
    /// 最近一次取不到分片或 init 段的原因
    last_download_failure: Option<HttpError>,
    /// 最近一次刷新失败的原因；刷新成功即清空
    last_refresh_error: Option<Error>,
}

impl LiveTrack {
    /// `playlist` 为首次拉到的播放列表，`recorded_us` 为之前各会话已录到的时长。
    pub(super) fn new(
        url: Url,
        playlist: &MediaPlaylist,
        recorded_us: u64,
        now: Instant,
    ) -> Result<Self, Error> {
        let target = target(playlist).ok_or_else(|| {
            Error::Unsupported(Unsupported::NoTargetDuration(Box::new(url.clone())))
        })?;
        Ok(LiveTrack {
            url,
            target,
            refresh: Refresh::Due(now),
            window: Window::new(Start::Fresh, recorded_us),
            begun: false,
            last_recorded: now,
            last_listed: now,
            outstanding: 0,
            last_download_failure: None,
            last_refresh_error: None,
        })
    }

    /// 会话定下：按 `start` 重建窗口，停滞计时从 `now` 重新起算（判定会话花的时间不算）。
    pub(super) fn begin(&mut self, start: Start, now: Instant) {
        self.window = Window::new(start, self.window.recorded_us());
        self.begun = true;
        self.last_recorded = now;
    }

    pub(super) fn window(&self) -> &Window {
        &self.window
    }

    /// 已录满 `max_us`。
    pub(super) fn is_full(&self, max_us: Option<u64>) -> bool {
        max_us.is_some_and(|max| self.window.recorded_us() >= max)
    }

    /// 出现了 EXT-X-ENDLIST。
    pub(super) fn is_ended(&self) -> bool {
        matches!(self.refresh, Refresh::Ended)
    }

    /// 还要刷新：没有出现 ENDLIST，也没有录满 `max_us`。
    pub(super) fn needs_refresh(&self, max_us: Option<u64>) -> bool {
        !self.is_ended() && !self.is_full(max_us)
    }

    /// 下次刷新的时刻；在途、暂停或已结束时为 None。
    pub(super) fn due_at(&self) -> Option<Instant> {
        match self.refresh {
            Refresh::Due(at) => Some(at),
            Refresh::InFlight { .. }
            | Refresh::Held { .. }
            | Refresh::Suspended
            | Refresh::Ended => None,
        }
    }

    /// 发起刷新。
    pub(super) fn start_refresh(&mut self, now: Instant) -> RefreshRequest {
        self.refresh = Refresh::InFlight { started: now };
        RefreshRequest {
            url: self.url.clone(),
            known: self.window.known_inits(),
            processed: self.begun.then(|| self.window.processed().clone()),
        }
    }

    /// 刷新拿到了播放列表。
    pub(super) fn refreshed(&mut self) {
        self.last_refresh_error = None;
    }

    /// 刷新失败、可以再试：半个目标时长后再刷新，服务器要求等更久时按它的。
    pub(super) fn refresh_failed(&mut self, error: Error, now: Instant) {
        let wait = self
            .half_target()
            .max(error.retry_after().unwrap_or_default());
        self.refresh = now
            .checked_add(wait)
            .map_or(Refresh::Suspended, Refresh::Due);
        self.last_refresh_error = Some(error);
    }

    /// 播放列表没有变化（如续录判定期间拿到空的）：半个目标时长后再刷新（RFC 8216 6.3.4）。
    pub(super) fn wait_unchanged(&mut self, now: Instant) {
        self.refresh = Refresh::Due(now + self.half_target());
    }

    /// 续录判定会话期间暂停刷新；`playlist` 为拿到的候选。
    pub(super) fn hold(&mut self, playlist: &MediaPlaylist) {
        self.refresh = Refresh::Held {
            live: !playlist.ended,
        };
    }

    /// 处理一份播放列表并安排下次刷新；`started` 为这次加载开始的时刻。见 [`Window::update`]。
    pub(super) fn apply(
        &mut self,
        scope: &Scope<'_>,
        playlist: &MediaPlaylist,
        fetched: NewInits,
        started: Instant,
        now: Instant,
    ) -> Result<ControlFlow<LiveEnd, Update>, Error> {
        let update = match self.window.update(scope, playlist, fetched)? {
            ControlFlow::Break(end) => return Ok(ControlFlow::Break(end)),
            ControlFlow::Continue(update) => update,
        };
        if let Some(target) = target(playlist) {
            self.target = target;
        }
        if update.any_new {
            self.last_listed = now;
        }
        let failure = update.missed.iter().rev().find_map(|m| match &m.reason {
            MissReason::Failed(kind) | MissReason::InitFailed(kind) => Some(kind),
            _ => None,
        });
        if let Some(kind) = failure {
            self.last_download_failure = Some(kind.clone());
        }
        self.outstanding += update.items.len();
        self.refresh = if update.ended {
            Refresh::Ended
        } else if update.changed {
            // RFC 8216 6.3.4：有变化后从开始加载起至少等一个目标时长，没变化时等半个
            Refresh::Due((started + self.target).max(now + MIN_REFRESH))
        } else {
            Refresh::Due(now + self.half_target())
        };
        Ok(ControlFlow::Continue(update))
    }

    /// 本轨排入下载的一个分片下载成功。
    pub(super) fn segment_recorded(&mut self, now: Instant) {
        self.outstanding -= 1;
        self.last_recorded = now;
    }

    /// 本轨排入下载的一个分片取不到，记为缺失。
    pub(super) fn segment_missed(&mut self, kind: HttpError) {
        self.outstanding -= 1;
        self.last_download_failure = Some(kind);
    }

    fn half_target(&self) -> Duration {
        (self.target / 2).max(MIN_REFRESH)
    }

    /// 播放列表仍在列出要录的新分片。
    fn is_listing(&self, now: Instant) -> bool {
        now.duration_since(self.last_listed) < self.target.saturating_mul(LISTING_TARGETS)
    }

    /// 播放列表还在出：仍在列出新分片，或续录判定期间暂停时候选还没结束（暂停是在等本任务，不是它停了）。
    pub(super) fn is_live(&self, now: Instant) -> bool {
        match self.refresh {
            Refresh::Held { live } => live,
            _ => self.is_listing(now),
        }
    }

    /// 停滞的时刻：最近一次录到分片（或会话定下）之后，持续 `stall_timeout`（至少 [`STALL_TARGETS`] 个
    /// 目标时长）。续录判定期间暂停，或有分片排着队、在下载时，是在等本任务，不算停滞，为 None；
    /// 时长大到无法表示时也为 None。
    pub(super) fn stall_at(&self, stall_timeout: Duration) -> Option<Instant> {
        if matches!(self.refresh, Refresh::Held { .. }) || self.outstanding > 0 {
            return None;
        }
        self.last_recorded
            .checked_add(stall_timeout.max(self.target.saturating_mul(STALL_TARGETS)))
    }

    /// 停滞的结论，取走最近一次刷新失败的原因；只在据此结束录制时调用。`Ok` 为看起来直播已结束，
    /// `Err` 为故障。`others_live` 为其他轨的播放列表还在出：这时本轨不再出新分片是本轨的故障。
    pub(super) fn stall(
        &mut self,
        now: Instant,
        others_live: bool,
    ) -> Result<StallCause, StallError> {
        if let Refresh::InFlight { started } = self.refresh
            && now.duration_since(started) >= self.target
        {
            return Err(StallError::RefreshPending);
        }
        let ended = match self.last_refresh_error.take() {
            Some(Error::Http {
                kind: HttpError::Status(status @ (404 | 410)),
                ..
            }) => StallCause::PlaylistGone(status),
            Some(error) => return Err(StallError::RefreshFailed(Box::new(error))),
            None if self.is_listing(now) => {
                let kind = self.last_download_failure.clone().expect(
                    "仍在列出新分片、没有在途的下载又没有录到：新分片都记了缺失，有失败原因",
                );
                return Err(StallError::Unrecordable(kind));
            }
            None => StallCause::NoNewSegments,
        };
        if others_live {
            return Err(StallError::TrackStopped(ended));
        }
        Ok(ended)
    }
}

/// 刷新间隔的基准；播放列表既没有正的 TARGETDURATION 也没有分片时为 None。
fn target(playlist: &MediaPlaylist) -> Option<Duration> {
    let longest = playlist.segments.iter().map(|s| s.duration_us).max();
    let us = playlist
        .target_duration_us
        .filter(|&us| us > 0)
        .or(longest)?;
    Some(Duration::from_micros(us))
}
