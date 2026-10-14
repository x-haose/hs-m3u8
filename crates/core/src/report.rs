//! 任务的进度与结果。

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::info::Selected;
use crate::remux::Report;
use crate::{FetchError, FetchFailure, HttpFailure, Mp4Unsupported, Purpose, hls};

/// 任务进度快照。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Progress {
    pub stage: Stage,
    /// 所选的变体与音频 rendition；解析完成前、或来源本身是媒体播放列表时为 None。有已完成分片的任务目录按
    /// 记录找回原来的轨，不一定是偏好会选的那条
    pub selected: Option<Selected>,
    /// 已完成的分片，各轨合计；含续传前已完成的
    pub segments_done: usize,
    /// 要下载的分片，各轨合计：已完成的（含续传前的）加上排入下载的；直播另含列出了但 init 段取不到的，随录制增长；点播
    /// 不含所在组合并不了的分片（见 [`Output::missed`]）。全部处理完时等于 `segments_done` 加 `segments_failed`
    pub segments_total: usize,
    /// 直播本次运行中列出了、但分片或其 init 段取不到的分片，计入 `segments_total`；点播恒为 0
    pub segments_failed: usize,
    /// 直播本次运行中两次刷新之间已滑出窗口、没有列出过的分片，不在 `segments_total` 里；点播恒为 0
    pub segments_expired: usize,
    /// 来源标了 EXT-X-GAP（没有内容）、不取的分片，不在 `segments_total` 里；直播为本次运行中列出的
    pub segments_gap: usize,
    /// 第 0 条轨已完成分片的声明时长之和，微秒；含续传前已完成的。直播即已录时长
    pub duration_us: u64,
    /// 任务目录中已完成的分片与 init 段的字节数（解密后）；含续传前已完成的
    pub bytes: u64,
    /// 本次运行从网络读到的响应体字节数，读到即计：含播放列表与 key，含没读完、校验失败与重试前的请求。
    /// 按它的变化算下载速度
    pub received: u64,
    /// 正在退避、等着重试的请求（分片、key、init 段，与开始任务时拉的播放列表；直播的刷新不重试，见
    /// `refresh_errors`）；没有在等的为 None
    pub retrying: Option<Retrying>,
    /// 写出的进度；[`Stage::Writing`] 之前为 None
    pub writing: Option<Writing>,
    /// 直播：各轨（下标为轨道编号）最近一次刷新失败的原因。这条轨之后刷新成功、不再刷新或录制结束即清空；
    /// 一直失败到停滞时任务以 [`crate::Error::LiveStalled`] 结束。点播与录制开始前为空
    pub refresh_errors: Vec<Option<RefreshCause>>,
    /// 直播且输出含 MP4：录到的内容放不进 MP4 的原因。续录时任务目录里已录齐的组在开始后核对一次，之后每个（会话,
    /// 不连续段）组在各轨都有分片时核对（录制循环结束、收尾时才下完的不核对）；第 0 组开头的分片判断不了时，各组只
    /// 核对自身、不与它比较。组号为核对时的，之后才录齐、排在前面的组会让合并时的组号变大。录制照常进行，停不停由调用
    /// 方定（直播错过的内容录不回来）；合并 MP4 时照样以 [`crate::Unsupported::Mp4`] 失败，可改为只输出本地 HLS 再
    /// 合并。点播（下载各组的第一个分片后核对，放不进时任务直接失败）与不输出 MP4 时为 None
    pub mp4_unsupported: Option<Mp4Unsupported>,
}

impl Progress {
    /// 计入一个已完成的分片：第 `track` 条轨、声明时长 `duration_us`、`len` 字节。
    pub(crate) fn count_segment(&mut self, track: usize, duration_us: u64, len: u64) {
        self.segments_done += 1;
        self.bytes += len;
        if track == 0 {
            self.duration_us = self.duration_us.saturating_add(duration_us);
        }
    }
}

/// 正在退避、等着重试的请求（[`Progress::retrying`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retrying {
    /// 在等的请求数，至少 1
    pub waiting: usize,
    /// 最近一次要重试的请求的用途
    pub purpose: Purpose,
    /// 最近一次要重试的失败
    pub failure: FetchFailure,
}

/// 写出的进度（[`Progress::writing`]），以文件计：各组各轨的 init 段与分片各算一个（打包音频按分片）。合并 MP4 时每
/// 读完一个计一，备齐本地 HLS 时每放好一个计一，两者都要时各算一遍；合并完回读核对 MP4 时不再增长。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Writing {
    pub done: usize,
    pub total: usize,
}

/// 直播一次刷新失败的原因；这类失败等下次刷新，不使任务立即失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefreshCause {
    /// 取不到：404/410 或临时故障（刷新只发一次请求，失败了等下次刷新）
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
}

impl RefreshCause {
    /// 服务器要求的最短等待，下次刷新不早于它。
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            RefreshCause::Http(failure) => failure.retry_after,
            RefreshCause::Empty | RefreshCause::Truncated | RefreshCause::Syntax { .. } => None,
        }
    }

    /// 一直这样失败到停滞（[`crate::StallError::RefreshFailed`]）时，稍后续录可能成功：取不到的看原因；内容不完整
    /// 的录制时已当作暂时故障等下次刷新，同样可以。
    pub fn retryable(&self) -> bool {
        match self {
            RefreshCause::Http(failure) => failure.kind.retryable(),
            RefreshCause::Empty | RefreshCause::Truncated | RefreshCause::Syntax { .. } => true,
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
    /// 写出输出（合并 MP4、写本地 HLS）
    Writing,
    Done,
}

/// 任务结果：下载、录制或只合并。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// 生成的 MP4；没要 MP4（[`crate::Target::Hls`]）时为 None
    pub mp4: Option<Mp4Output>,
    /// 生成的本地 HLS 目录（绝对路径，相对路径按开始任务时的当前目录补全）；没要 HLS 时为 None。入口为其中的
    /// `index.m3u8`：单轨时是媒体播放列表，视频与独立音频分离时是主播放列表；各轨的分片、init 段（与分离时这条轨的
    /// `index.m3u8`）在 `<轨道编号>/` 下。内容与 MP4 相同：只放各轨都有的不连续段组，组内缺失的分片不标出、保留原
    /// 时间戳
    pub hls: Option<PathBuf>,
    /// 输出的分片数，各轨合计
    pub segments: usize,
    /// 同 [`Progress::bytes`]
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

/// 生成的 MP4。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mp4Output {
    /// 绝对路径，相对路径按开始任务时的当前目录补全
    pub path: PathBuf,
    /// 各路输出流的编码参数、包数与时长
    pub report: Report,
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
/// [`crate::StallError::TrackStopped`]（只有这条轨结束了）。
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
    Failed(FetchError),
    /// 列出了，但它的 init 段取不到（同上）
    InitFailed(FetchError),
    /// 所在不连续段不是每条轨都有内容（直播为有轨没录到，点播为有轨只有标了 EXT-X-GAP 的分片），无法合并
    Unmergeable,
    /// 来源标了 EXT-X-GAP：这一片没有内容，不取
    Gap,
    /// 之前的运行中缺失，原因没有记录
    Unknown,
}
