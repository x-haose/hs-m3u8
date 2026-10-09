//! 录制中的一条轨：刷新的节奏、拉到还没处理的播放列表、停滞的判定，以及会话定下后的窗口。

use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::time::Duration;

use hs_m3u8_hls::{InitSection, MediaPlaylist};
use tokio::time::Instant;
use url::Url;

use super::session::Start;
use super::tasks::Fetched;
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
    /// 在为暂存的播放列表拉 init 段；处理完暂存的才再刷新
    Loading {
        started: Instant,
    },
    /// 服务器要求的等待长到无法表示：不再刷新，由停滞判定或停止结束录制
    Suspended,
    /// 出现了 EXT-X-ENDLIST，或这条轨不再刷新、不再处理
    Ended,
}

/// 这条轨处在录制的哪个阶段。
enum Stage {
    /// 会话还没定下；`candidate` 为已拿到第一份有分片（或已结束）的播放列表，在等核对或等其他轨
    Undecided { candidate: bool },
    /// 按定下的会话录
    Recording(Box<Window>),
    /// 这次不录：已录满 max_duration，或判定期间看起来已结束
    Idle,
}

/// 发起一次刷新需要的信息。
pub(super) struct RefreshRequest {
    pub url: Url,
    /// 会话还没定下时为 None：不知道哪些分片要录，不拉 init 段
    pub inits: Option<InitsToFetch>,
}

/// 拉哪些新 init 段：要录的分片（见 [`Processed::is_new`]）引用、又不在 `known` 中的。
pub(super) struct InitsToFetch {
    /// 上一份播放列表引用、内容已知的 init 段
    pub known: Vec<InitSection>,
    pub processed: Processed,
}

/// 录制中的一条轨。
pub(super) struct LiveTrack {
    url: Url,
    /// 刷新间隔的基准：TARGETDURATION，为 0 或缺失时取播放列表中最长的分片时长
    target: Duration,
    refresh: Refresh,
    stage: Stage,
    /// 拉到、还没处理的播放列表，按拉到的先后：会话定下之前暂存，定下后逐份拉好 init 段再处理
    pending: VecDeque<Fetched>,
    /// 之前各会话已录到的时长，微秒
    recorded_us: u64,
    /// 会话定下之前见过的分片序号范围（首, 尾）
    seen: Option<(u64, u64)>,
    /// 最近一次有分片下载成功的时刻（或录制开始、会话定下的时刻）
    last_recorded: Instant,
    /// 最近一次列出新分片的时刻（或录制开始、会话定下的时刻）
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
            stage: Stage::Undecided { candidate: false },
            pending: VecDeque::new(),
            recorded_us,
            seen: None,
            last_recorded: now,
            last_listed: now,
            outstanding: 0,
            last_download_failure: None,
            last_refresh_error: None,
        })
    }

    /// 会话定下：`start` 为 Some 时按它建窗口录，停滞与「仍在出」的计时从 `now` 重新起算；None 时这次不录。
    pub(super) fn begin(&mut self, start: Option<Start>, now: Instant) {
        self.stage = match start {
            Some(start) => Stage::Recording(Box::new(Window::new(start, self.recorded_us))),
            None => {
                self.pending.clear();
                Stage::Idle
            }
        };
        self.last_recorded = now;
        self.last_listed = now;
    }

    /// 判定期间看起来已结束：不再刷新，这次不录。
    pub(super) fn end_undecided(&mut self) {
        self.refresh = Refresh::Ended;
        self.pending.clear();
        self.stage = Stage::Idle;
    }

    /// 会话定下之前拿到了第一份有分片（或已结束）的播放列表。
    pub(super) fn has_candidate(&self) -> bool {
        matches!(self.stage, Stage::Undecided { candidate: true })
    }

    pub(super) fn mark_candidate(&mut self) {
        self.stage = Stage::Undecided { candidate: true };
    }

    pub(super) fn is_undecided(&self) -> bool {
        matches!(self.stage, Stage::Undecided { .. })
    }

    /// 按定下的会话录时的窗口。
    pub(super) fn window(&self) -> Option<&Window> {
        match &self.stage {
            Stage::Recording(window) => Some(window),
            Stage::Undecided { .. } | Stage::Idle => None,
        }
    }

    /// 已录满 `max_us`。
    pub(super) fn is_full(&self, max_us: Option<u64>) -> bool {
        let recorded = self.window().map_or(self.recorded_us, Window::recorded_us);
        max_us.is_some_and(|max| recorded >= max)
    }

    /// 出现了 EXT-X-ENDLIST，或这条轨不再刷新、不再处理（判定期间看起来已结束、服务器前后矛盾、编码器重启）。
    pub(super) fn is_ended(&self) -> bool {
        matches!(self.refresh, Refresh::Ended)
    }

    /// 还要刷新：没有结束，也没有录满 `max_us`。
    pub(super) fn needs_refresh(&self, max_us: Option<u64>) -> bool {
        !self.is_ended() && !self.is_full(max_us)
    }

    /// 下次刷新的时刻；在途、在拉 init 段、暂停或已结束时为 None。
    pub(super) fn due_at(&self) -> Option<Instant> {
        match self.refresh {
            Refresh::Due(at) => Some(at),
            Refresh::InFlight { .. }
            | Refresh::Loading { .. }
            | Refresh::Suspended
            | Refresh::Ended => None,
        }
    }

    /// 发起刷新。
    pub(super) fn start_refresh(&mut self, now: Instant) -> RefreshRequest {
        self.refresh = Refresh::InFlight { started: now };
        RefreshRequest {
            url: self.url.clone(),
            inits: self.window().map(|w| InitsToFetch {
                known: w.known_inits(),
                processed: w.processed().clone(),
            }),
        }
    }

    /// 刷新拿到了播放列表；处理它时再安排下次刷新。
    pub(super) fn refreshed(&mut self, now: Instant) {
        self.refresh = Refresh::Due(now);
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

    /// 会话定下之前拿到一份播放列表：记下是否列出了新分片（判断它是否仍在出），安排下次刷新。
    /// `started` 为这次加载开始的时刻。
    pub(super) fn note_undecided(
        &mut self,
        playlist: &MediaPlaylist,
        started: Instant,
        now: Instant,
    ) {
        if let Some(target) = target(playlist) {
            self.target = target;
        }
        let range = playlist
            .segments
            .first()
            .zip(playlist.segments.last())
            .map(|(first, last)| (first.sequence, last.sequence));
        if let Some((_, last)) = range
            && self.seen.is_none_or(|(_, seen)| last > seen)
        {
            self.last_listed = now;
        }
        let changed = range.is_some() && range != self.seen;
        if range.is_some() {
            self.seen = range;
        }
        self.schedule(changed, playlist.ended, started, now);
    }

    /// 暂存一份拉到的播放列表；与上一份暂存的相同时不再存。
    pub(super) fn hold(&mut self, fetched: Fetched) {
        if self
            .pending
            .back()
            .is_none_or(|last| last.playlist != fetched.playlist)
        {
            self.pending.push_back(fetched);
        }
    }

    /// 取出最早暂存的播放列表，开始为它拉 init 段；刷新或拉 init 段在途、或没有暂存的时为 None。
    pub(super) fn start_loading(&mut self, now: Instant) -> Option<(Fetched, InitsToFetch)> {
        if matches!(
            self.refresh,
            Refresh::InFlight { .. } | Refresh::Loading { .. }
        ) {
            return None;
        }
        let window = self.window()?;
        let inits = InitsToFetch {
            known: window.known_inits(),
            processed: window.processed().clone(),
        };
        let fetched = self.pending.pop_front()?;
        self.refresh = Refresh::Loading { started: now };
        Some((fetched, inits))
    }

    /// 有暂存还没处理完的播放列表（含正在拉 init 段的）。
    pub(super) fn is_loading(&self) -> bool {
        !self.pending.is_empty() || matches!(self.refresh, Refresh::Loading { .. })
    }

    /// 之后不再刷新、不再处理这条轨（服务器前后矛盾、编码器重启）。
    pub(super) fn stop_processing(&mut self) {
        self.refresh = Refresh::Ended;
        self.pending.clear();
    }

    /// 按定下的会话处理一份播放列表并安排下次刷新；`started` 为这次加载开始的时刻。见 [`Window::update`]。
    pub(super) fn apply(
        &mut self,
        scope: &Scope<'_>,
        playlist: &MediaPlaylist,
        fetched: NewInits,
        started: Instant,
        now: Instant,
    ) -> Result<ControlFlow<LiveEnd, Update>, Error> {
        let Stage::Recording(window) = &mut self.stage else {
            panic!("只有按定下的会话录的轨才处理播放列表");
        };
        let update = match window.update(scope, playlist, fetched)? {
            ControlFlow::Break(end) => return Ok(ControlFlow::Break(end)),
            ControlFlow::Continue(update) => update,
        };
        if let Some(target) = target(playlist) {
            self.target = target;
        }
        if update.any_new {
            self.last_listed = now;
        }
        let failure = update
            .init_failed
            .iter()
            .rev()
            .find_map(|m| match &m.reason {
                MissReason::InitFailed(kind) => Some(kind),
                _ => None,
            });
        if let Some(kind) = failure {
            self.last_download_failure = Some(kind.clone());
        }
        self.outstanding += update.items.len();
        self.schedule(update.changed, update.ended, started, now);
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

    /// 处理完一份播放列表后安排下次刷新（RFC 8216 6.3.4）：有变化后从开始加载起至少等一个目标时长，没变化时等半个。
    fn schedule(&mut self, changed: bool, ended: bool, started: Instant, now: Instant) {
        self.refresh = if ended {
            Refresh::Ended
        } else if changed {
            Refresh::Due((started + self.target).max(now + MIN_REFRESH))
        } else {
            Refresh::Due(now + self.half_target())
        };
    }

    fn half_target(&self) -> Duration {
        (self.target / 2).max(MIN_REFRESH)
    }

    /// 播放列表仍在出新分片：最近 [`LISTING_TARGETS`] 个目标时长内列出过。
    pub(super) fn is_live(&self, now: Instant) -> bool {
        now.duration_since(self.last_listed) < self.target.saturating_mul(LISTING_TARGETS)
    }

    /// 停滞的时刻：最近一次录到分片（或录制开始、会话定下）之后，持续 `stall_timeout`（至少 [`STALL_TARGETS`]
    /// 个目标时长）。在等本任务的时为 None：会话定下之前已有候选（在等核对或其他轨），或有分片排着队、在下载；
    /// 时长大到无法表示时也为 None。
    pub(super) fn stall_at(&self, stall_timeout: Duration) -> Option<Instant> {
        let waiting = match self.stage {
            Stage::Undecided { candidate } => candidate,
            Stage::Recording(_) => self.outstanding > 0,
            Stage::Idle => true,
        };
        if waiting {
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
        if let Refresh::InFlight { started } | Refresh::Loading { started } = self.refresh
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
            None if self.is_live(now) => {
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
