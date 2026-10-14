//! 录制中的一条轨：刷新的节奏、拉到还没处理的播放列表、停滞的判定，以及会话定下后的窗口。

use std::collections::{BTreeMap, VecDeque};
use std::ops::ControlFlow;
use std::time::Duration;

use hs_m3u8_hls::{MediaPlaylist, Segment};
use tokio::time::Instant;
use url::Url;

use super::session::Start;
use super::window::{InitsToFetch, NewInits, Scope, Update, Window};
use crate::ident::Fingerprint;
use crate::workdir::SessionStart;
use crate::{
    Error, FetchFailure, HttpError, HttpFailure, RefreshCause, SourceBreak, StallCause, StallError,
    Unsupported,
};

/// 两次刷新之间的最短间隔，防止 TARGETDURATION 为 0 或极小时空转。
const MIN_REFRESH: Duration = Duration::from_millis(100);

/// 停滞判定至少等这么多个目标时长：分片时长不超过目标时长（RFC 8216 4.3.3.1），正常的直播每个目标时长至少出一个
/// 新分片，再留出刷新的间隔与下载的时间，正常出分片的直播不会因为 stall_timeout 设得比目标时长还短而被判为停滞。
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
    /// 会话还没定下。`candidate` 为已拿到候选（第一份有分片的播放列表），在等核对或等其他轨；
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
    /// 最近一次有分片下载成功、或一份播放列表列出的新分片全是 EXT-X-GAP（直播还在，只是这段没有内容）的时刻，或录制
    /// 开始、会话定下的时刻
    last_progress: Instant,
    /// 最近一次发起的刷新的发起时刻
    last_started: Option<Instant>,
    /// 最近一次返回的刷新（成功或失败）的发起时刻
    last_returned: Option<Instant>,
    /// 最近一次列出新分片的时刻（或录制开始、会话定下的时刻）
    last_listed: Instant,
    /// 排入下载、还没有结果的分片数
    outstanding: usize,
    /// 最近一次取不到分片或 init 段的原因
    last_download_failure: Option<FetchFailure>,
    /// 最近一次刷新失败的原因；刷新成功即清空
    last_refresh_error: Option<RefreshCause>,
    /// 服务器在最近一次失败的刷新里要求等到的时刻（Retry-After）；刷新成功即清空
    server_wait: Option<Instant>,
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
            last_progress: now,
            last_started: None,
            last_returned: None,
            last_listed: now,
            outstanding: 0,
            last_download_failure: None,
            last_refresh_error: None,
            server_wait: None,
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
        self.last_progress = now;
        self.last_listed = now;
    }

    /// 判定期间看起来已结束（还没拿到候选）：不再刷新，这次不录。
    pub(super) fn end_undecided(&mut self) {
        assert!(self.pending.is_empty(), "还没拿到候选的轨没有暂存");
        self.refresh = Refresh::Ended;
        self.role = Role::Idle;
    }

    /// 会话定下之前拿到了候选（第一份有分片的播放列表）。
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

    /// 最近一次刷新失败的原因；之后刷新成功即清空。
    pub(super) fn last_refresh_error(&self) -> Option<&RefreshCause> {
        self.last_refresh_error.as_ref()
    }

    /// 出现了 EXT-X-ENDLIST，或这条轨不再刷新、不再处理（判定期间看起来已结束、接不上而结束录制）。
    pub(super) fn is_ended(&self) -> bool {
        matches!(self.refresh, Refresh::Ended)
    }

    /// 还要刷新：没有结束，也没有录满 `max_us`。
    pub(super) fn needs_refresh(&self, max_us: Option<u64>) -> bool {
        !self.is_ended() && !self.is_full(max_us)
    }

    /// 下次刷新的时刻。会话定下后，暂存的处理完才再刷新：刷新时按已处理到的位置决定拉哪些新 init 段。停滞开始于
    /// `stall_since`（见 [`super::Recorder`] 的停滞判定）而那之后还没发起过刷新时，最迟在那时发起；服务器要求的等待
    /// 照样遵守。在途、等处理、暂停或已结束时为 None。
    pub(super) fn due_at(&self, stall_since: Option<Instant>) -> Option<Instant> {
        let Refresh::Due(at) = self.refresh else {
            return None;
        };
        if self.window().is_some() && self.has_held() {
            return None;
        }
        let at = match stall_since {
            Some(since) if !self.last_started.is_some_and(|s| s >= since) => at.min(since),
            _ => at,
        };
        Some(self.server_wait.map_or(at, |until| at.max(until)))
    }

    /// 发起刷新。
    pub(super) fn start_refresh(&mut self, now: Instant) -> RefreshRequest {
        self.refresh = Refresh::InFlight { started: now };
        self.last_started = Some(now);
        RefreshRequest {
            url: self.url.clone(),
            inits: self.window().map(Window::inits_to_fetch),
        }
    }

    /// 刷新返回了（成功、失败或在结束中被丢弃）：不再在途。
    pub(super) fn refresh_returned(&mut self) {
        if let Refresh::InFlight { started } = self.refresh {
            self.refresh = Refresh::Waiting;
            self.last_returned = Some(started);
        }
    }

    /// 在途的网络请求（刷新，或准备暂存的播放列表时拉 init 段）发起的时刻；没有在途的时为 None。
    fn in_flight_since(&self) -> Option<Instant> {
        match self.refresh {
            Refresh::InFlight { started } => Some(started),
            _ => self.preparing,
        }
    }

    /// 在途的请求算作没有返回的时刻：从发起与 `noticed`（察觉停滞的时刻）中晚的那个起，再过一个目标时长。进程被
    /// 挂起醒来时，按醒来之后实际运行的时间算。没有在途的请求时为 None。
    pub(super) fn deadline(&self, noticed: Instant) -> Option<Instant> {
        let from = self.in_flight_since()?.max(noticed);
        from.checked_add(self.target)
    }

    /// 停滞开始于 `since`、在 `noticed` 被察觉时，这条轨在 `now` 的状况有据可依：有在途的请求时它满了期限（见
    /// [`LiveTrack::deadline`]）；没有时已有结果（见 [`LiveTrack::observed`]）。
    pub(super) fn settled(&self, since: Instant, noticed: Instant, now: Instant) -> bool {
        match self.deadline(noticed) {
            Some(deadline) => deadline <= now,
            None => self.observed(since, noticed),
        }
    }

    /// 停滞开始之后的刷新结果已有：`since` 之后发起的刷新已经返回；服务器在最近一次失败里要求了等待的，要等待结束之后
    /// 发起的那次，等待太长的（见 [`LiveTrack::barred`]）以要求等待的那次失败为据。
    fn observed(&self, since: Instant, noticed: Instant) -> bool {
        let from = self.server_wait.map_or(since, |until| until.max(since));
        self.last_returned.is_some_and(|r| r >= from) || self.barred(noticed)
    }

    /// 服务器在最近一次失败的刷新里要求的等待，到察觉停滞（`noticed`）之后一个目标时长还没结束，或长到无法表示（刷新
    /// 已暂停）：等不到停滞开始之后的刷新结果，以要求等待的那次失败为据。
    fn barred(&self, noticed: Instant) -> bool {
        matches!(self.refresh, Refresh::Suspended)
            || self.server_wait.is_some_and(|until| {
                noticed
                    .checked_add(self.target)
                    .is_none_or(|limit| until > limit)
            })
    }

    /// 目标时长。
    pub(super) fn target(&self) -> Duration {
        self.target
    }

    /// 在 `since` 之后列出过新分片。
    pub(super) fn listed_since(&self, since: Instant) -> bool {
        self.last_listed >= since
    }

    /// 刷新拿到了播放列表。
    pub(super) fn refreshed(&mut self) {
        self.last_refresh_error = None;
        self.server_wait = None;
    }

    /// 刷新失败、可以再试：半个目标时长后再刷新，服务器要求等更久时按它的。
    pub(super) fn refresh_failed(&mut self, cause: RefreshCause, now: Instant) {
        let asked = cause.retry_after();
        let wait = self.half_target().max(asked.unwrap_or_default());
        self.refresh = now
            .checked_add(wait)
            .map_or(Refresh::Suspended, Refresh::Due);
        self.server_wait = asked.and_then(|asked| now.checked_add(asked));
        self.last_refresh_error = Some(cause);
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

    /// 暂存一份拉到的播放列表。与上一份暂存的接得上（见 [`joined`]）时合成一份：按顺序处理两份与处理合成的这份
    /// 结果相同，暂存的量只随新列出的分片增长，什么也不丢；接不上的分开暂存，交给窗口比对。
    pub(super) fn hold(&mut self, fetched: Fetched) {
        if let Some(last) = self.pending.back_mut()
            && let Some(playlist) = joined(&last.playlist, &fetched.playlist)
        {
            *last = Fetched {
                playlist,
                started: fetched.started,
            };
            return;
        }
        self.pending.push_back(fetched);
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

    /// 之后不再刷新、不再处理这条轨（接不上而结束录制）。
    pub(super) fn stop_processing(&mut self) {
        self.refresh = Refresh::Ended;
        self.pending.clear();
    }

    /// 播放列表 `fetched` 与之前刷新得到的接不上（`cause`）后另起会话：序号回退时从新窗口的起点录，矛盾时跳过已
    /// 处理到的序号；已录到的时长沿用。`fetched` 排在暂存的最前面，按新窗口重新准备、处理。停滞的计时不重新起算：
    /// 来源反复接不上而一直录不到分片时照样判为停滞。
    pub(super) fn restart(&mut self, cause: SourceBreak, fetched: Fetched) {
        let Role::Decided(window) = &self.role else {
            panic!("只有按定下的会话录的轨才会接不上");
        };
        let start = match (cause, window.processed_through()) {
            (SourceBreak::Inconsistent { .. }, Some(through)) => SessionStart::After(through),
            _ => SessionStart::Fresh,
        };
        let window = Window::new(Start::New(start), window.recorded_us());
        self.role = Role::Decided(Box::new(window));
        self.pending.push_front(fetched);
    }

    /// 按定下的会话处理一份播放列表并安排下次刷新；`started` 为这次拉取开始的时刻。见 [`Window::update`]。
    pub(super) fn apply(
        &mut self,
        scope: &Scope<'_>,
        playlist: &MediaPlaylist,
        fetched: NewInits,
        started: Instant,
        now: Instant,
    ) -> Result<ControlFlow<SourceBreak, Update>, Error> {
        let Role::Decided(window) = &mut self.role else {
            panic!("只有按定下的会话录的轨才处理播放列表");
        };
        let update = match window.update(scope, playlist, fetched)? {
            ControlFlow::Break(cause) => return Ok(ControlFlow::Break(cause)),
            ControlFlow::Continue(update) => update,
        };
        if let Some(target) = target(playlist) {
            self.target = target;
        }
        if update.any_new {
            self.last_listed = now;
        }
        if !update.gaps.is_empty() && update.items.is_empty() && update.init_failed.is_empty() {
            self.last_progress = now;
        }
        if let Some(failure) = &update.init_failure {
            self.last_download_failure = Some(failure.clone());
        }
        self.outstanding += update.items.len();
        self.schedule(update.changed, update.ended, started, now);
        Ok(ControlFlow::Continue(update))
    }

    /// 本轨排入下载的一个分片下载成功。
    pub(super) fn segment_recorded(&mut self, now: Instant) {
        self.outstanding -= 1;
        self.last_progress = now;
    }

    /// 本轨排入下载的一个分片取不到，记为缺失。
    pub(super) fn segment_missed(&mut self, failure: FetchFailure) {
        self.outstanding -= 1;
        self.last_download_failure = Some(failure);
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
            .and_then(RefreshCause::retry_after)
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

    /// 停滞开始的时刻：最近一次录到分片（或录制开始、会话定下）之后，持续 `stall_timeout`（至少 [`STALL_TARGETS`]
    /// 个目标时长）。在等本任务的时为 None：会话定下之前已有候选（在等核对或其他轨），或有分片排着队、在下载；时长大到
    /// 无法表示时也为 None。拉到的播放列表还在准备（拉 init 段）时照样算，那次准备当作在途的请求（见
    /// [`LiveTrack::deadline`]）。停滞的结论另要这之后的刷新结果（见 [`LiveTrack::settled`]）。
    pub(super) fn stall_since(&self, stall_timeout: Duration) -> Option<Instant> {
        let waiting = match self.role {
            Role::Undecided { candidate, .. } => candidate,
            Role::Decided(_) => self.outstanding > 0,
            Role::Idle => true,
        };
        if waiting {
            return None;
        }
        self.last_progress
            .checked_add(stall_timeout.max(self.target.saturating_mul(STALL_TARGETS)))
    }

    /// 作为其他轨（不是停滞的那条）看，在 `now` 有故障：在途的请求满了期限（见 [`LiveTrack::deadline`]），或停滞开始
    /// 之后的刷新结果（见 [`LiveTrack::observed`]）是失败（404/410 除外：播放列表已删除，是直播结束的迹象）。没有故障
    /// 时为 None：在出新分片，或有结束的迹象。
    pub(super) fn fault(
        &self,
        since: Instant,
        noticed: Instant,
        now: Instant,
    ) -> Option<StallError> {
        if self
            .deadline(noticed)
            .is_some_and(|deadline| deadline <= now)
        {
            return Some(StallError::RefreshPending);
        }
        match &self.last_refresh_error {
            Some(cause) if self.observed(since, noticed) && gone(cause).is_none() => {
                Some(StallError::RefreshFailed(cause.clone()))
            }
            _ => None,
        }
    }

    /// 停滞的结论，取走最近一次刷新失败的原因；只在据此结束录制时调用，此时这条轨的状况有据可依（见
    /// [`LiveTrack::settled`]）。停滞开始于 `since`、在 `noticed` 被察觉。`Ok` 为看起来直播已结束，`Err` 为故障。
    /// `others_live` 为其他轨在 `since` 之后列出过新分片：这时本轨不再出新分片是本轨的故障。
    ///
    /// 依次看：在途的请求满了期限；播放列表没被删除（最近一次刷新不是 404/410）而 `since` 之后列出过新分片（刷新是
    /// 通的，新分片都取不到）；播放列表已删除；刷新失败；都没有时为不再出新分片。
    pub(super) fn stall(
        &mut self,
        now: Instant,
        since: Instant,
        noticed: Instant,
        others_live: bool,
    ) -> Result<StallCause, StallError> {
        if self
            .deadline(noticed)
            .is_some_and(|deadline| deadline <= now)
        {
            return Err(StallError::RefreshPending);
        }
        let refresh_error = self.last_refresh_error.take();
        let gone = refresh_error.as_ref().and_then(gone);
        if gone.is_none() && self.listed_since(since) {
            let failure = self.last_download_failure.clone().expect(
                "停滞开始后列出过新分片、没有在途的下载又没有录到：新分片都记了缺失，有失败原因",
            );
            return Err(StallError::Unrecordable(failure));
        }
        let ended = match (gone, refresh_error) {
            (Some(status), _) => StallCause::PlaylistGone(status),
            (None, Some(cause)) => return Err(StallError::RefreshFailed(cause)),
            (None, None) => StallCause::NoNewSegments,
        };
        if others_live {
            return Err(StallError::TrackStopped(ended));
        }
        Ok(ended)
    }
}

/// 刷新失败说明播放列表已被删除（404/410）时为那个状态码。
fn gone(cause: &RefreshCause) -> Option<u16> {
    match cause {
        RefreshCause::Http(HttpFailure {
            kind: HttpError::Status(status @ (404 | 410)),
            ..
        }) => Some(*status),
        _ => None,
    }
}

/// `older` 与之后拉到的 `newer` 合成的一份：两者的分片按序号取并集，同一序号取 `newer` 的（地址里的令牌最新），
/// 是否结束等取 `newer` 的。两者都有分片、`newer` 的第一个序号落在 `older` 的序号范围里（至少重叠一个）、重叠的序号
/// 身份与不连续段序号都一致时才接得上；否则为 None，要窗口逐份比对：序号回退（编码器重启或 CDN 的旧缓存）、同一序号
/// 换了分片、不连续段重新编号，以及没有重叠（首尾相接时核对不了 `newer` 的不连续段序号与 `older` 的是否同一基准，
/// 服务器没写 EXT-X-DISCONTINUITY-SEQUENCE 而有不连续标记恰好在这时滑出窗口时，合成会把后面的分片编进错的组）。
fn joined(older: &MediaPlaylist, newer: &MediaPlaylist) -> Option<MediaPlaylist> {
    let older_first = older.segments.first()?.sequence;
    let older_last = older.segments.last()?.sequence;
    let newer_first = newer.segments.first()?.sequence;
    if !(older_first..=older_last).contains(&newer_first) {
        return None;
    }
    let mut segments: BTreeMap<u64, &Segment> =
        older.segments.iter().map(|s| (s.sequence, s)).collect();
    for new in &newer.segments {
        if let Some(old) = segments.get(&new.sequence)
            && (identity(old) != identity(new) || old.discontinuity != new.discontinuity)
        {
            return None;
        }
        segments.insert(new.sequence, new);
    }
    Some(MediaPlaylist {
        media_sequence: older_first,
        segments: segments.into_values().cloned().collect(),
        ..newer.clone()
    })
}

fn identity(segment: &Segment) -> Fingerprint {
    Fingerprint::of_segment(&segment.uri, segment.byte_range)
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

#[cfg(test)]
mod tests {
    use hs_m3u8_hls::{Playlist, parse};

    use super::*;

    /// 序号从 `first` 起、分片依次为 `names`、每个 1 秒的直播播放列表。
    fn playlist(first: u64, names: &[&str]) -> MediaPlaylist {
        let mut text = format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
        for name in names {
            text += &format!("#EXTINF:1,\n{name}\n");
        }
        let base = Url::parse("https://cdn.example/live.m3u8").unwrap();
        match parse(&text, &base).unwrap() {
            Playlist::Media(media) => media,
            Playlist::Master(_) => unreachable!("文本里没有 EXT-X-STREAM-INF"),
        }
    }

    /// 暂存几份播放列表后，各份暂存的分片名。
    fn held(playlists: &[MediaPlaylist]) -> Vec<Vec<String>> {
        let url = Url::parse("https://cdn.example/live.m3u8").unwrap();
        let now = Instant::now();
        let mut track = LiveTrack::new(url, &playlists[0], 0, now).unwrap();
        for playlist in playlists {
            track.hold(Fetched {
                playlist: playlist.clone(),
                started: now,
            });
        }
        track
            .pending
            .iter()
            .map(|f| {
                f.playlist
                    .segments
                    .iter()
                    .map(|s| {
                        let name = s.uri.as_str().trim_start_matches("https://cdn.example/");
                        format!("{}:{name}", s.sequence)
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn held_playlists_that_join_are_merged_without_losing_segments() {
        let names = |list: &[&str]| -> Vec<String> { list.iter().map(|s| s.to_string()).collect() };
        // 窗口增长、滑动、CDN 返回的较旧子集：都接得上，合成一份，滑走的分片仍在
        assert_eq!(
            held(&[
                playlist(0, &["s0.ts", "s1.ts"]),
                playlist(1, &["s1.ts", "s2.ts"]),
                playlist(2, &["s2.ts", "s3.ts", "s4.ts"]),
                playlist(2, &["s2.ts", "s3.ts"]),
            ]),
            vec![names(&[
                "0:s0.ts", "1:s1.ts", "2:s2.ts", "3:s3.ts", "4:s4.ts"
            ])]
        );
        // 同一序号取后拉到的：地址里的令牌最新
        let merged = held(&[
            playlist(0, &["s0.ts?t=1"]),
            playlist(0, &["s0.ts?t=2", "s1.ts"]),
        ]);
        assert_eq!(merged, vec![names(&["0:s0.ts?t=2", "1:s1.ts"])]);

        // 中间有没列出过的序号、首尾相接没有重叠、序号回退、同一序号换了分片：接不上，分开暂存
        let apart = |a: MediaPlaylist, b: MediaPlaylist| held(&[a, b]).len();
        assert_eq!(apart(playlist(0, &["s0.ts"]), playlist(2, &["s2.ts"])), 2);
        assert_eq!(apart(playlist(0, &["s0.ts"]), playlist(1, &["s1.ts"])), 2);
        assert_eq!(
            apart(playlist(100, &["s100.ts"]), playlist(0, &["s0.ts"])),
            2
        );
        assert_eq!(
            apart(
                playlist(0, &["s0.ts", "s1.ts"]),
                playlist(0, &["s0.ts", "x1.ts"])
            ),
            2
        );
    }

    #[test]
    fn held_playlists_with_renumbered_discontinuities_stay_apart() {
        let base = Url::parse("https://cdn.example/live.m3u8").unwrap();
        let parse_media = |text: &str| match parse(text, &base).unwrap() {
            Playlist::Media(media) => media,
            Playlist::Master(_) => unreachable!("文本里没有 EXT-X-STREAM-INF"),
        };
        let first = parse_media("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\ns0.ts\n");
        let renumbered = parse_media(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-DISCONTINUITY-SEQUENCE:1\n#EXTINF:1,\ns0.ts\n",
        );
        assert_eq!(held(&[first, renumbered]).len(), 2);
    }
}
