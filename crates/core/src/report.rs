//! 任务的进度与结果。

use std::fmt;
use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use crate::info::Selected;
use crate::remux::MediaReport;
use crate::{
    FetchErrorKind, FetchFailure, HttpFailure, MergeUnsupported, Purpose, StallReason, hls,
};

/// 任务目录里已完成的内容（[`crate::Engine::inspect`]），按目录里记录的任务种类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saved {
    Hls(HlsSaved),
    File(FileSaved),
}

/// HLS 任务目录里已完成的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsSaved {
    /// 目录里是直播的录制
    pub live: bool,
    /// 已完成的分片数，视频与音频轨合计（同 [`Progress::segments`] 的口径）
    pub segments: usize,
    /// 已完成的分片与 init 段的字节数（解密后），视频与音频轨合计
    pub bytes: u64,
    /// 各轨的，按轨号（见 [`crate::Selected`]），含字幕轨
    pub tracks: Vec<TrackSaved>,
    /// 第 0 条轨已完成分片的声明时长之和；直播即已录时长
    pub duration: Duration,
}

/// 任务目录里一条轨已完成的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSaved {
    pub kind: TrackKind,
    /// 已完成的分片数
    pub segments: usize,
    /// 已完成的分片与 init 段的字节数（解密后）
    pub bytes: u64,
}

/// 文件任务目录里已完成的内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSaved {
    /// 已落盘、续传时沿用的字节数，比任务运行时的 [`FileProgress::bytes`]（已写入、不一定落盘）可能少最近一秒内写的；
    /// 不知道总长度或不支持范围请求时恒为 0（续传从头下）
    pub bytes: u64,
    /// 总字节数；第一个响应没给出时为 None
    pub total: Option<u64>,
}

/// 文件任务的进度快照（[`crate::Engine::start_file`]）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileProgress {
    /// 只经过 [`Stage::Preparing`]、[`Stage::Downloading`]、[`Stage::Writing`]（装上输出）、[`Stage::Done`]
    pub stage: Stage,
    /// 总字节数；收到第一个响应之前，或服务器没给出时为 None
    pub total: Option<u64>,
    /// 已写入数据文件的字节数，含续传前的；等于 `completed` 各段之和
    pub bytes: u64,
    /// 已写入数据文件的范围（字节偏移，不含终点），升序、不重叠、相接的已合并；含续传前的
    pub completed: Vec<Range<u64>>,
    /// 本次运行从网络读到的响应体字节数，读到即计：含重试前没读完的。按它的变化算下载速度
    pub received: u64,
    /// 分到了段的连接数，含等着引擎名额、还没发出请求的；收到第一个响应之前为 0
    pub connections: usize,
    /// 当前的连接上限：收到第一个响应之前为 0，之后为 [`crate::FileRequest::connections`]，服务器限制连接数时降低（见
    /// `throttled`）；不支持范围请求或不知道总长度时为 1
    pub limit: usize,
    /// 正在退避、等着重试的请求，同 [`Progress::retrying`]
    pub retrying: Vec<Retrying>,
    /// 最近一次因服务器限制连接数而降低上限；没有降过时为 None
    pub throttled: Option<Throttled>,
}

/// 一次降低连接上限（[`FileProgress::throttled`]）：同时有多个连接时，新的请求在收到响应之前失败或回了 429、503、403，
/// 视为服务器只允许当时在途的其余连接，上限降到在途的连接数减一（不超过原来的上限减一，至少 1），这一段放回去等别的
/// 连接接着下，不算失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Throttled {
    /// 降到的上限
    pub limit: usize,
    /// 引起这次降低的失败
    pub failure: HttpFailure,
}

/// 文件任务的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutput {
    /// 下载的文件（绝对路径，相对路径按开始任务时的当前目录补全）
    pub path: PathBuf,
    /// 文件的字节数
    pub bytes: u64,
    /// 收尾没删掉的东西，同 [`Output::leftovers`]
    pub leftovers: Vec<Leftover>,
}

/// 任务进度快照。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Progress {
    pub stage: Stage,
    /// 所选的变体、音频与字幕 rendition（怎样选见 [`crate::Source::preference`]）；解析完成前、或来源本身是媒体播放列表时为
    /// None。重新选轨而选出的与记录的不是同一条时为这次选出的，核对上才沿用；核对不上时输出仍照任务目录里记录的
    pub selected: Option<Selected>,
    /// 视频与音频轨的分片数合计；字幕轨的分片很小，不计入，见 `tracks`
    pub segments: SegmentCounts,
    /// 各轨的分片数与字节数，按轨号；解析完成前为空
    pub tracks: Vec<TrackProgress>,
    /// 第 0 条轨已完成分片的声明时长之和；含续传前已完成的。直播即已录时长
    pub duration: Duration,
    /// 任务目录中视频与音频轨已完成的分片与 init 段的字节数（解密后）；含续传前已完成的。字幕轨的见 `tracks`
    pub bytes: u64,
    /// 本次运行从网络读到的响应体字节数，读到即计：含播放列表与 key，含没读完、校验失败与重试前的请求。
    /// 按它的变化算下载速度
    pub received: u64,
    /// 正在退避、等着重试的请求（分片、key、init 段，与开始任务时拉的播放列表；直播的刷新不重试，见
    /// `refresh_errors`），按开始退避的先后；退避结束（接着重试或被取消）即去掉
    pub retrying: Vec<Retrying>,
    /// 写出的进度；[`Stage::Writing`] 之前为 None
    pub writing: Option<Writing>,
    /// 直播：各轨（下标为轨道编号）最近一次刷新失败的原因。这条轨之后刷新成功、不再刷新或录制结束即清空；
    /// 一直失败到停滞时任务以 [`crate::Error::LiveStalled`] 结束。点播与录制开始前为空
    pub refresh_errors: Vec<Option<RefreshReason>>,
    /// 直播：录制中的会话编号（从 0 开始，会话的含义见 [`LiveReport::session_count`]）。会话定下后才有，录制中途另起
    /// 会话（见 [`NewSession`]）时随之变；点播与会话定下之前为 None
    pub session: Option<u32>,
    /// 直播且输出含合并文件：录到的内容放不进合并文件的原因。续录时任务目录里已录齐的组在开始后核对一次，之后每个（会话,
    /// 不连续段）组在各轨都有分片时核对（录制循环结束、收尾时才下完的不核对）；第 0 组开头的分片判断不了时，各组只
    /// 核对自身、不与它比较。组号为核对时的，之后才录齐、排在前面的组会让合并时的组号变大。录制照常进行，停不停由调用
    /// 方定（直播错过的内容录不回来）；合并时照样以 [`crate::Unsupported::Merge`] 失败，可改存 MKV 或只输出本地 HLS 再
    /// 合并。点播（下载各组的第一个分片后核对，放不进时任务直接失败）与不输出合并文件时为 None
    pub merge_unsupported: Option<MergeUnsupported>,
}

/// 分片数：[`Progress::segments`] 为视频与音频轨合计，[`TrackProgress::segments`] 为一条轨的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SegmentCounts {
    /// 已完成的；含续传前已完成的
    pub done: usize,
    /// 要下载的：已完成的（含续传前的）加上排入下载的；直播另含列出了但 init 段取不到的，随录制增长；点播不含所在组合并
    /// 不了的（见 [`Output::missed`]）。全部处理完时等于 `done` 加 `failed`
    pub total: usize,
    /// 直播本次运行中列出了、但分片或其 init 段取不到的，计入 `total`；点播恒为 0
    pub failed: usize,
    /// 直播本次运行中两次刷新之间已滑出窗口、没有列出过的，不在 `total` 里；点播恒为 0
    pub expired: usize,
    /// 来源标了 EXT-X-GAP（没有内容）、不取的，不在 `total` 里；直播为本次运行中列出的
    pub gap: usize,
}

/// 一条轨的进度（[`Progress::tracks`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackProgress {
    pub kind: TrackKind,
    /// 口径同 [`Progress::segments`]，只算这条轨
    pub segments: SegmentCounts,
    /// 口径同 [`Progress::bytes`]，只算这条轨
    pub bytes: u64,
    /// 直播的字幕轨：在录制中的会话里记为尾部没录到（停滞满了时限，或前后接不上而停了；见 [`UnrecordedTail`]），已写进
    /// 任务目录；录到新分片即恢复为 false。其余为 false
    pub unrecorded_tail: bool,
}

/// 轨的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    /// 所选变体（或来源本身），可能混着音频
    Video,
    /// 独立的音频 rendition
    Audio,
    /// 字幕 rendition
    Subtitles,
}

impl TrackKind {
    /// 计入 [`Progress::segments`] 与 [`Progress::bytes`]。
    fn counted(self) -> bool {
        self != TrackKind::Subtitles
    }
}

impl Progress {
    /// 按各轨的种类 `kinds`（按轨号）建出 `tracks`，各项清零；合计不动。
    pub(crate) fn set_tracks(&mut self, kinds: &[TrackKind]) {
        self.tracks = kinds
            .iter()
            .map(|&kind| TrackProgress {
                kind,
                segments: SegmentCounts::default(),
                bytes: 0,
                unrecorded_tail: false,
            })
            .collect();
    }

    /// 改第 `track` 条轨的分片数，视频与音频轨同样改合计。计数之前已建出 `tracks`（见 [`Progress::set_tracks`]）。
    pub(crate) fn segments_of(&mut self, track: usize, change: impl Fn(&mut SegmentCounts)) {
        let t = self
            .tracks
            .get_mut(track)
            .expect("计数之前已按选轨建出各轨的进度");
        change(&mut t.segments);
        if t.kind.counted() {
            change(&mut self.segments);
        }
    }

    /// 第 `track` 条轨又有 `len` 字节落盘，视频与音频轨同样计入合计。计数之前已建出 `tracks`。
    pub(crate) fn add_bytes(&mut self, track: usize, len: u64) {
        let t = self
            .tracks
            .get_mut(track)
            .expect("计数之前已按选轨建出各轨的进度");
        t.bytes += len;
        if t.kind.counted() {
            self.bytes += len;
        }
    }

    /// 计入一个已完成的分片：第 `track` 条轨、声明时长 `duration_us`、`len` 字节。
    pub(crate) fn count_segment(&mut self, track: usize, duration_us: u64, len: u64) {
        self.segments_of(track, |s| s.done += 1);
        self.add_bytes(track, len);
        if track == 0 {
            self.duration = self
                .duration
                .saturating_add(Duration::from_micros(duration_us));
        }
    }
}

/// 一个正在退避、等着重试的请求（[`Progress::retrying`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retrying {
    pub purpose: Purpose,
    /// 上一次尝试的失败
    pub failure: FetchFailure,
    /// 接着是第几次重试，从 1 起（总尝试次数见 [`crate::RetryPolicy::attempts`]）
    pub retry: u32,
    /// 这次退避多久（随机抖动之后的，服务器要求等待更久时按它的）
    pub delay: Duration,
}

/// 写出的进度（[`Progress::writing`]），以文件计：各组各轨的 init 段与分片各算一个（打包音频按分片）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Writing {
    /// 合并：写完的输入文件，写到下一个分片的包时上一个才算完，合并完回读核对时不再增长；不要合并文件时为 None
    pub merged: Option<FileCount>,
    /// 备齐本地 HLS：放好的文件，已放过的 init 段照样算；与任务目录同盘时用硬链接，几乎不花时间。不要本地 HLS 时为 None
    pub hls: Option<FileCount>,
}

/// 完成了的文件数与合计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileCount {
    pub done: usize,
    pub total: usize,
}

/// 直播一次刷新失败的原因；这类失败等下次刷新，不使任务立即失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefreshReason {
    /// 取不到：404/410 或临时故障（刷新只发一次请求，失败了等下次刷新）；字幕轨另含其余 HTTP 错误（如 401、403）
    #[error(transparent)]
    Http(HttpFailure),
    /// 内容为空，多为服务器还没写完
    #[error("播放列表为空")]
    Empty,
    /// 结尾截在多字节字符中间，多为服务器还没写完
    #[error("播放列表在结尾的字符中间截断")]
    Truncated,
    /// 语法错误，多为服务器还没写完
    #[error("第 {line} 行：{kind}")]
    Syntax { line: usize, kind: hls::SyntaxError },
    /// 内容不是媒体播放列表（不是 UTF-8、不是播放列表、是主播放列表、混着主播放列表的标签，或大到超出播放列表的上限），
    /// 多为服务器返回了错误页或回源错配；只出在字幕轨上，其余轨的使任务失败
    #[error("内容不是媒体播放列表")]
    NotAPlaylist,
}

impl RefreshReason {
    /// 服务器要求的最短等待，下次刷新不早于它。
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            RefreshReason::Http(failure) => failure.retry_after,
            RefreshReason::Empty
            | RefreshReason::Truncated
            | RefreshReason::Syntax { .. }
            | RefreshReason::NotAPlaylist => None,
        }
    }

    /// 一直这样失败到停滞（[`crate::StallReason::RefreshFailed`]）时，稍后续录可能成功：取不到的看原因；内容不完整
    /// 的录制时已当作暂时故障等下次刷新，同样可以。
    pub fn retryable(&self) -> bool {
        match self {
            RefreshReason::Http(failure) => failure.kind.retryable(),
            RefreshReason::Empty
            | RefreshReason::Truncated
            | RefreshReason::Syntax { .. }
            | RefreshReason::NotAPlaylist => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stage {
    /// 准备：检查输出、读取任务目录，下载时还要拉取播放列表、选轨
    #[default]
    Preparing,
    /// 点播：下载分片
    Downloading,
    /// 直播：刷新播放列表并下载新分片
    Recording,
    /// 写出输出（合并、写本地 HLS）
    Writing,
    Done,
}

/// 任务结果：下载、录制或只合并。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// 生成的合并文件；没要（[`crate::Target::Hls`]）时为 None
    pub merged: Option<MergedOutput>,
    /// 生成的本地 HLS 目录（绝对路径，相对路径按开始任务时的当前目录补全）；没要 HLS 时为 None。入口为其中的
    /// `index.m3u8`：单轨时是媒体播放列表，有独立音频或字幕时是主播放列表（音频组 `audio`、字幕组 `subs`）；各轨的分片、
    /// init 段与这条轨的 `index.m3u8` 在 `<轨道编号>/` 下，字幕为原 WebVTT 分片（`.vtt`）。内容与合并文件相同：只放各轨
    /// 都有的不连续段组，组内缺失的分片不标出、保留原时间戳
    pub hls: Option<PathBuf>,
    /// 另存的字幕文件，按所选字幕的顺序（见 [`crate::SubtitleOutput::files`]）；不另存或没选字幕时为空
    pub subtitle_files: Vec<SubtitleFile>,
    /// 输出的分片数，视频与音频轨合计（同 [`Progress::segments`] 的口径；字幕见 [`Output::subtitle_files`]）
    pub segments: usize,
    /// 同 [`Progress::bytes`]：视频与音频轨的
    pub bytes: u64,
    /// 收尾没删掉的东西：被替换的旧输出、没删干净的任务目录等；输出不受影响。都删干净时为空
    pub leftovers: Vec<Leftover>,
    /// 不在输出中的分片，按会话、轨道、序号排列，相接且原因相同的合成一个区间：点播有来源标了 EXT-X-GAP 的及因此合并不
    /// 了的；直播另有窗口滑过、取不到等（见 [`MissReason`]），不含之前的运行里、某个会话第一个已完成分片之前或最后一个
    /// 之后缺的（原因与范围随那次运行一起丢了）。缺的分片落在所在组各轨已有内容的时间范围内时，那条轨在那段留空、保留原
    /// 时间戳；各轨都缺的组首尾与整组不在输出中的不占时间（各组首尾相接）
    pub missed: Vec<Missed>,
    /// 直播的录制结果；点播为 None
    pub live: Option<LiveReport>,
}

/// 收尾时没能删掉或放回的一样东西；见 [`Output::leftovers`] 与 [`crate::Error::Cleanup`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub path: PathBuf,
    pub kind: LeftoverKind,
    /// 没能删掉或放回的原因
    pub cause: String,
}

/// 残留是什么，决定调用方怎么处理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeftoverKind {
    /// 本库写的、可以直接删除的东西：写了一半的临时输出、已被新输出替换的旧输出、任务目录里的记录
    Removable,
    /// 没能放回原处的旧输出，原来的路径为 `target`。[`crate::Error::Cleanup`] 里的多数仍记在任务目录里：下次用同一任务
    /// 目录运行或放弃任务时，原处空着或是空目录就放回，原处是输出（更新的输出已装上）就删掉，原处是别的东西就留着；
    /// 放弃任务（[`crate::Engine::discard`]）返回的已没有记录，之后用同一任务目录运行时不动它，在任务开头报出来。留着
    /// 的要用户处理
    Displaced { target: PathBuf },
    /// 写输出失败后没能撤下的新输出：完整可用，留在原处
    Installed,
    /// 没删干净的任务目录，`cause` 说明原因：里面有不是本库写的文件（例如经符号链接写进去的输出本身），要用户处理；
    /// 或删除时出错，可用 [`crate::Engine::discard`] 重试。不要整个删除
    WorkDir,
}

impl fmt::Display for Leftover {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (path, cause) = (self.path.display(), &self.cause);
        match &self.kind {
            LeftoverKind::Removable => write!(f, "删除 {path} 失败：{cause}"),
            LeftoverKind::Displaced { target } => {
                write!(
                    f,
                    "旧输出没能放回 {}，现在在 {path}：{cause}",
                    target.display()
                )
            }
            LeftoverKind::Installed => write!(f, "撤下新输出 {path} 失败：{cause}"),
            LeftoverKind::WorkDir => write!(f, "任务目录 {path} 没删干净：{cause}"),
        }
    }
}

/// 另存的一个字幕文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleFile {
    /// 绝对路径
    pub path: PathBuf,
    /// 写进去的 cue 数
    pub cues: usize,
    /// 丢掉的 cue 数，口径同 [`hs_m3u8_remux::StreamReport::discarded_cues`]
    pub discarded_cues: u64,
}

/// 生成的合并文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedOutput {
    /// 绝对路径，相对路径按开始任务时的当前目录补全
    pub path: PathBuf,
    pub container: hs_m3u8_remux::Container,
    /// 各路输出流的编码参数、包数与时长；字幕流在音视频之后
    pub report: MediaReport,
}

/// 直播的录制结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveReport {
    /// 本次运行的录制如何结束；只合并（[`crate::Engine::merge_recorded`]）时为 None
    pub end: Option<LiveEnd>,
    /// 合并进输出的录制会话数。会话是一段时间线连续的录制：中断后续录时接得上（见 [`crate::LiveOptions`]）仍是同一个
    /// 会话，否则另起一个，与之前的首尾相接。录制中途另起的（见 `new_sessions`）也算
    pub session_count: usize,
    /// 本次运行录制中途另起的会话，按先后；只合并时为空
    pub new_sessions: Vec<NewSession>,
    /// 尾部没录到的字幕轨，按会话、轨号排列；含之前各次运行记下的（见 [`UnrecordedTail`]）
    pub unrecorded_tails: Vec<UnrecordedTail>,
}

/// 直播里一条字幕轨在一个会话的尾部没录到：`after` 之后的字幕不在输出中。字幕轨的故障不结束录制、不使任务失败（录制的
/// 结束只看音视频轨），在这里报出。
///
/// 录制中它停滞（持续 [`crate::LiveOptions::stall_timeout`] 与 3 个目标时长中较长的那么久没录到新分片）或前后接不上而停了
/// 时即记进任务目录（[`TrackProgress::unrecorded_tail`] 随之变为 true），录到新分片即去掉；所以取消、失败或进程退出后
/// 只合并照样报出。录制结束时出现了 ENDLIST 或录满的去掉。简化：录制结束前停滞还不满时限的那一段不报。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnrecordedTail {
    pub session: u32,
    pub track: usize,
    /// 这个会话里录到的最后一个分片的媒体序号；一个也没录到时为 None
    pub after: Option<u64>,
    /// 这条轨合并进输出的最后一个分片在输出时间轴上的结束时刻（从输出开头算），供显示「某时之后没录到」。是推算：之前
    /// 各组取音视频轨声明时长（EXTINF）之和中最长的首尾相接，加上这一组里这条轨合并进输出的分片的声明时长。偏早的
    /// 来源：缺的分片（见 [`Output::missed`]）在输出里仍占时间却没算进去，这条轨在这一组里起步晚于画面的部分也没算；
    /// 另差出各组声明与实际时长之差。这个会话里没有分片合并进输出时为 None
    pub at: Option<Duration>,
    /// 本次运行判定的原因；之前的运行记下的、或只合并时为 None（原因不落盘）
    pub cause: Option<TailCause>,
}

/// 字幕轨尾部没录到的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailCause {
    /// 看起来这条字幕轨已结束：不再出新分片，或播放列表已被删除，而停滞开始之后音视频轨也没再出新分片
    Ended(StallCause),
    /// 故障：刷新一直失败（字幕轨另含 401、403 这类不可重试的 HTTP 错误与不是播放列表的内容）、一直不返回，有新分片却
    /// 都取不到，或音视频轨还在出新分片而它不再出（[`StallReason::TrackStopped`]）
    Failed(StallReason),
    /// 播放列表与之前刷新得到的接不上（多为字幕打包器重启）：这次运行不再录它
    Broken(SourceBreak),
}

/// 本次运行的录制如何结束。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEnd {
    /// 所有轨出现 EXT-X-ENDLIST
    EndList,
    /// 调用了 [`crate::JobControl::stop`]
    Stopped,
    /// 各轨都录满了 [`crate::LiveOptions::max_duration`]
    DurationReached,
    /// 第 `track` 条轨停滞（见 [`crate::LiveOptions`]），且看起来是直播已结束：它与其他轨都不再出新分片；
    /// 故障导致的停滞是 [`crate::Error::LiveStalled`]
    Stalled { track: usize, cause: StallCause },
    /// 第 `track` 条轨的播放列表与之前刷新得到的接不上，且录的不止一条轨：其他轨从哪里起与它对得上不知道，不另起
    /// 会话，之后的分片未录。只录一条轨时另起会话接着录（见 [`NewSession`]）
    Broken { track: usize, cause: SourceBreak },
}

/// 媒体播放列表与之前刷新得到的接不上的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceBreak {
    /// 媒体序号回退、且与上次的窗口没有重叠，连续两次刷新都如此（多为编码器重启；只一次多为 CDN 的旧缓存，不算）
    Restarted,
    /// 在序号 `sequence` 处矛盾：同一序号换了分片，或不连续段编号对不上（服务器错误，RFC 8216 6.2.2）
    Inconsistent { sequence: u64 },
}

/// 录制中途另起的会话：只录一条轨时，它的播放列表与之前刷新得到的接不上，不结束录制，另起会话接着录。序号回退时
/// 从新窗口的起点录；矛盾时跳过已处理到的序号，只录更新的分片。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewSession {
    /// 另起的会话编号
    pub session: u32,
    pub cause: SourceBreak,
}

/// 一条轨停滞时看起来已结束的原因：用于 [`LiveEnd::Stalled`]（各轨都已结束），也用于
/// [`crate::StallReason::TrackStopped`]（只有这条轨结束了）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallCause {
    /// 刷新成功，但不再出现新分片
    NoNewSegments,
    /// 播放列表已被删除：最近一次刷新返回此 HTTP 状态（404 或 410）
    PlaylistGone(u16),
}

/// 一段连续的缺失分片：第 `session` 个会话中第 `track` 条轨序号 `first..=last` 的分片不在输出中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missed {
    /// 会话编号，从 0 开始
    pub session: u32,
    pub track: usize,
    pub first: u64,
    pub last: u64,
    pub reason: MissReason,
}

impl Missed {
    /// 区间里的分片数；大到无法表示时为 `usize::MAX`。
    pub fn count(&self) -> usize {
        usize::try_from(self.last - self.first).map_or(usize::MAX, |n| n.saturating_add(1))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissReason {
    /// 两次刷新之间已滑出播放列表窗口，没有被列出过
    Expired,
    /// 列出了，但取不到：404/410，可重试的错误重试后仍失败，或重试后内容仍没写完
    Failed(FetchErrorKind),
    /// 列出了，但它的 init 段取不到（同上）
    InitFailed(FetchErrorKind),
    /// 所在不连续段不是每条音视频轨都有内容（直播为有音视频轨没录到，点播为有音视频轨只有标了 EXT-X-GAP 的分片），无法
    /// 合并
    Unmergeable,
    /// 字幕分片：所在不连续段的序号（直播为同一会话里的）在音视频轨里没有（字幕播放列表的不连续段没跟着画面标，或
    /// DISCONTINUITY-SEQUENCE 的起点不同），不知道该对到哪一段画面上。简化：只按序号对组；要支持这类来源时改为按 cue
    /// 的媒体时刻落进时间相符的组
    NoMatchingDiscontinuity,
    /// 来源标了 EXT-X-GAP：这一片没有内容，不取
    Gap,
    /// 之前的运行中缺失，原因没有记录
    Unknown,
}
