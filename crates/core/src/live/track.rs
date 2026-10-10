//! 录制中的一条轨：刷新的节奏、拉到还没处理的播放列表、停滞的判定，以及会话定下后的窗口。

use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::time::Duration;

use hs_m3u8_hls::MediaPlaylist;
use tokio::time::Instant;
use url::Url;

use super::session::Start;
use super::window::{InitsToFetch, NewInits, Scope, Update, Window};
use crate::{Error, HttpError, LiveEnd, MissReason, StallCause, StallError, Unsupported};

/// 两次刷新之间的最短间隔，防止 TARGETDURATION 为 0 或极小时空转。
const MIN_REFRESH: Duration = Duration::from_millis(100);

/// 这么多个目标时长内列出过新分片，即认为播放列表仍在更新：分片时长不超过目标时长（RFC 8216 4.3.3.1），
/// 正常的直播每个目标时长至少出一个新分片，再留一个目标时长给刷新的间隔。
const LISTING_TARGETS: u32 = 2;

/// 停滞判定至少等这么多个目标时长：比「仍在更新」的判定多一个，正常出分片的直播不会因为 stall_timeout
/// 设得比目标时长还短而被判为停滞。
const STALL_TARGETS: u32 = 3;

/// 网络刷新的状态。准备暂存的播放列表另记（[`LiveTrack`] 的 `preparing`），两者互不等待。
enum Refresh {
    Due(Instant),
    InFlight {
        started: Instant,
    },
    /// 拉到的播放列表还没处理完（含首次拉到的）；处理到它时再安排下次刷新
    Waiting,
    /// 服务器要求的等待长到无法表示：不再刷新，由停滞判定或停止结束录制
    Suspended,
    /// 出现了 EXT-X-ENDLIST，或这条轨不再刷新、不再处理
    Ended,
}

/// 这条轨在本次运行里的角色。
enum Role {
    /// 会话还没定下。`candidate` 为已拿到第一份有分片（或已结束）的播放列表，在等核对或等其他轨；
    /// `seen` 为见过的分片序号范围（首, 尾），用于判断是否列出了新分片
    Undecided {
        candidate: bool,
        seen: Option<(u64, u64)>,
    },
    /// 按定下的会话录
    Decided(Box<Window>),
    /// 这次不录：已录满 max_duration，或判定期间看起来已结束
    Idle,
}

/// 拉到的一份播放列表。
pub(super) struct Fetched {
    pub playlist: MediaPlaylist,
    /// 这次拉取开始的时刻
    pub started: Instant,
}

/// 发起一次刷新需要的信息。
pub(super) struct RefreshRequest {
    pub url: Url,
    /// 会话还没定下时为 None：不知道哪些分片要录，不拉 init 段
    pub inits: Option<InitsToFetch>,
}

/// 录制中的一条轨。
pub(super) struct LiveTrack {
    url: Url,
    /// 刷新间隔的基准：TARGETDURATION，为 0 或缺失时取播放列表中最长的分片时长
    target: Duration,
    refresh: Refresh,
    role: Role,
    /// 拉到、还没处理的播放列表，按拉到的先后：会话定下之前暂存，定下后逐份准备好再处理
    pending: VecDeque<Fetched>,
    /// 正在准备的那份开始准备的时刻；没有在准备的为 None
    preparing: Option<Instant>,
    /// 之前各会话已录到的时长，微秒
    recorded_us: u64,
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
            refresh: Refresh::Waiting,
            role: Role::Undecided {
                candidate: false,
                seen: None,
            },
            pending: VecDeque::new(),
            preparing: None,
            recorded_us,
            last_recorded: now,
            last_listed: now,
            outstanding: 0,
            last_download_failure: None,
            last_refresh_error: None,
        })
    }

    /// 会话定下：`start` 为 Some 时按它建窗口录，停滞与「仍在出」的计时从 `now` 重新起算；None 时这次不录。
    pub(super) fn enter_session(&mut self, start: Option<Start>, now: Instant) {
        self.role = match start {
            Some(start) => Role::Decided(Box::new(Window::new(start, self.recorded_us))),
            None => {
                assert!(
                    self.pending.is_empty(),
                    "这次不录的轨没有暂存：已录满的不参与判定，看起来已结束的没有候选"
                );
                Role::Idle
            }
        };
        self.last_recorded = now;
        self.last_listed = now;
    }

    /// 判定期间看起来已结束（还没拿到候选）：不再刷新，这次不录。
    pub(super) fn end_undecided(&mut self) {
        assert!(self.pending.is_empty(), "还没拿到候选的轨没有暂存");
        self.refresh = Refresh::Ended;
        self.role = Role::Idle;
    }

    /// 会话定下之前拿到了第一份有分片（或已结束）的播放列表。
    pub(super) fn has_candidate(&self) -> bool {
        matches!(
            self.role,
            Role::Undecided {
                candidate: true,
                ..
            }
        )
    }

    pub(super) fn mark_candidate(&mut self) {
        if let Role::Undecided { candidate, .. } = &mut self.role {
            *candidate = true;
        }
    }

    pub(super) fn is_undecided(&self) -> bool {
        matches!(self.role, Role::Undecided { .. })
    }

    /// 按定下的会话录时的窗口。
    pub(super) fn window(&self) -> Option<&Window> {
        match &self.role {
            Role::Decided(window) => Some(window),
            Role::Undecided { .. } | Role::Idle => None,
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

    /// 下次刷新的时刻。会话定下后，暂存的处理完才再刷新：刷新时按已处理到的位置决定拉哪些新 init 段。
    /// 在途、等处理、暂停或已结束时为 None。
    pub(super) fn due_at(&self) -> Option<Instant> {
        let Refresh::Due(at) = self.refresh else {
            return None;
        };
        let held = self.window().is_some() && self.has_held();
        (!held).then_some(at)
    }

    /// 发起刷新。
    pub(super) fn start_refresh(&mut self, now: Instant) -> RefreshRequest {
        self.refresh = Refresh::InFlight { started: now };
        RefreshRequest {
            url: self.url.clone(),
            inits: self.window().map(Window::inits_to_fetch),
        }
    }

    /// 刷新返回了（成功、失败或在结束中被丢弃）：不再在途。
    pub(super) fn refresh_returned(&mut self) {
        if let Refresh::InFlight { .. } = self.refresh {
            self.refresh = Refresh::Waiting;
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

    /// 会话定下之前拿到一份播放列表：记下是否列出了新分片（判断它是否仍在出），安排下次刷新。
    /// `started` 为这次拉取开始的时刻。
    pub(super) fn note_undecided(
        &mut self,
        playlist: &MediaPlaylist,
        started: Instant,
        now: Instant,
    ) {
        let Role::Undecided { seen, .. } = &mut self.role else {
            panic!("只有会话还没定下的轨才记判定期间拉到的播放列表");
        };
        let range = playlist
            .segments
            .first()
            .zip(playlist.segments.last())
            .map(|(first, last)| (first.sequence, last.sequence));
        let listed_new = range.is_some_and(|(_, last)| seen.is_none_or(|(_, s)| last > s));
        let changed = range.is_some() && range != *seen;
        if range.is_some() {
            *seen = range;
        }
        if listed_new {
            self.last_listed = now;
        }
        if let Some(target) = target(playlist) {
            self.target = target;
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

    /// 取出最早暂存的播放列表，开始准备它；会话还没定下、已有一份在准备、或没有暂存的时为 None。
    /// 不等在途的刷新：那是会话定下之前发起的，结果只会排在暂存的后面。
    pub(super) fn start_preparing(&mut self, now: Instant) -> Option<(Fetched, InitsToFetch)> {
        if self.preparing.is_some() {
            return None;
        }
        let inits = self.window()?.inits_to_fetch();
        let fetched = self.pending.pop_front()?;
        self.preparing = Some(now);
        Some((fetched, inits))
    }

    /// 正在准备的那份准备好了，接着处理它。
    pub(super) fn prepared(&mut self) {
        self.preparing = None;
    }

    /// 有暂存还没处理完的播放列表（含正在准备的）。
    pub(super) fn has_held(&self) -> bool {
        !self.pending.is_empty() || self.preparing.is_some()
    }

    /// 之后不再刷新、不再处理这条轨（服务器前后矛盾、编码器重启）。
    pub(super) fn stop_processing(&mut self) {
        self.refresh = Refresh::Ended;
        self.pending.clear();
    }

    /// 按定下的会话处理一份播放列表并安排下次刷新；`started` 为这次拉取开始的时刻。见 [`Window::update`]。
    pub(super) fn apply(
        &mut self,
        scope: &Scope<'_>,
        playlist: &MediaPlaylist,
        fetched: NewInits,
        started: Instant,
        now: Instant,
    ) -> Result<ControlFlow<LiveEnd, Update>, Error> {
        let Role::Decided(window) = &mut self.role else {
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

    /// 处理完一份播放列表后安排下次刷新（RFC 8216 6.3.4）：有变化后从开始拉取起至少等一个目标时长，没变化时等半个；
    /// 上次刷新失败时服务器要求等更久则按它的。刷新在途（结果会再安排）、暂停或已结束时不变。
    fn schedule(&mut self, changed: bool, ended: bool, started: Instant, now: Instant) {
        if !matches!(self.refresh, Refresh::Waiting | Refresh::Due(_)) {
            return;
        }
        let asked = self
            .last_refresh_error
            .as_ref()
            .and_then(Error::retry_after)
            .and_then(|wait| now.checked_add(wait));
        self.refresh = if ended {
            Refresh::Ended
        } else if changed {
            Refresh::Due(
                (started + self.target)
                    .max(now + MIN_REFRESH)
                    .max(asked.unwrap_or(now)),
            )
        } else {
            Refresh::Due((now + self.half_target()).max(asked.unwrap_or(now)))
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
        let waiting = match self.role {
            Role::Undecided { candidate, .. } => candidate,
            Role::Decided(_) => self.outstanding > 0,
            Role::Idle => true,
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
        let pending_since = match self.refresh {
            Refresh::InFlight { started } => Some(started),
            _ => self.preparing,
        };
        if pending_since.is_some_and(|started| now.duration_since(started) >= self.target) {
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
