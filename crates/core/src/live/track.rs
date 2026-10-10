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
    pub(super) fn last_refresh_error(&self) -> Option<&Error> {
        self.last_refresh_error.as_ref()
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

/// `older` 与之后拉到的 `newer` 合成的一份：两者的分片按序号取并集，同一序号取 `newer` 的（地址里的令牌最新），
/// 是否结束等取 `newer` 的。两者都有分片、`newer` 的第一个序号不早于 `older` 的第一个且不晚于它最后一个的下一个
/// （并集没有空档）、重叠的序号身份与不连续段序号都一致时才接得上；否则为 None：序号回退（编码器重启或 CDN 的
/// 旧缓存）、同一序号换了分片、不连续段重新编号、中间有没列出过的序号，都要窗口逐份比对。
fn joined(older: &MediaPlaylist, newer: &MediaPlaylist) -> Option<MediaPlaylist> {
    let older_first = older.segments.first()?.sequence;
    let older_last = older.segments.last()?.sequence;
    let newer_first = newer.segments.first()?.sequence;
    if newer_first < older_first || newer_first > older_last.checked_add(1)? {
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

        // 中间有没列出过的序号、序号回退、同一序号换了分片：接不上，分开暂存
        let apart = |a: MediaPlaylist, b: MediaPlaylist| held(&[a, b]).len();
        assert_eq!(apart(playlist(0, &["s0.ts"]), playlist(2, &["s2.ts"])), 2);
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
