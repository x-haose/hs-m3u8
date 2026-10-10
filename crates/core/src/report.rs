//! 任务的进度与结果。

use std::path::PathBuf;

use crate::{HttpError, Report};

/// 任务进度快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub stage: Stage,
    /// 已完成的分片，各轨合计；含续传前已完成的
    pub segments_done: usize,
    /// 要下载的分片，各轨合计：已完成的（含续传前的）加上排入下载的；直播另含列出了但 init 段取不到的，
    /// 随录制增长。全部处理完时等于 `segments_done` 加 `segments_failed`
    pub segments_total: usize,
    /// 直播本次运行中列出了、但分片或其 init 段取不到的分片，计入 `segments_total`；点播恒为 0
    pub segments_failed: usize,
    /// 直播本次运行中两次刷新之间已滑出窗口、没有列出过的分片，不在 `segments_total` 里；点播恒为 0
    pub segments_expired: usize,
    /// 任务目录中已完成的分片与 init 段的字节数（解密后）；含续传前已完成的
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stage {
    /// 拉取播放列表、选轨
    #[default]
    Resolving,
    /// 点播：下载分片
    Downloading,
    /// 直播：刷新播放列表并下载新分片
    Recording,
    /// 合并为输出文件；不响应取消
    Merging,
    Done,
}

/// 下载结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub path: PathBuf,
    pub report: Report,
    /// 合并进输出的分片数，各轨合计
    pub segments: usize,
    /// 同 [`Progress::bytes`]
    pub bytes: u64,
    /// 删除任务目录失败的原因（含路径）；输出文件不受影响，残留目录由调用方处理。
    /// 未删除（`keep_work_dir`）或删除成功时为 None
    pub cleanup_error: Option<String>,
    /// 直播的录制结果；点播为 None
    pub live: Option<LiveReport>,
}

/// 直播的录制结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveReport {
    /// 本次运行的录制如何结束；只合并（[`crate::merge_recorded`]）时为 None
    pub end: Option<LiveEnd>,
    /// 合并进输出的录制会话数。会话是一段时间线连续的录制：中断后续录时，若各轨都与之前录到的内容接得上，
    /// 仍是同一个会话；否则另起一个，与之前的首尾相接
    pub session_count: usize,
    /// 不在输出中的分片，按会话、轨道、序号排列，相接且原因相同的合成一个区间。不含之前的运行里、
    /// 某个会话第一个已完成分片之前或最后一个之后缺的（原因与范围随那次运行一起丢了）
    pub missed: Vec<Missed>,
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
    /// 第 `track` 条轨的媒体序号回退、且与上次的窗口没有重叠（多为编码器重启）；之后的分片未录
    Restarted { track: usize },
    /// 第 `track` 条轨在序号 `sequence` 处与之前刷新得到的播放列表矛盾：同一序号换了分片，或不连续段编号
    /// 对不上（服务器错误，RFC 8216 6.2.2）；之后的分片未录
    Inconsistent { track: usize, sequence: u64 },
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
    /// 列出了，但取不到：404/410，或可重试的错误重试后仍失败
    Failed(HttpError),
    /// 列出了，但它的 init 段取不到（同上）
    InitFailed(HttpError),
    /// 所在不连续段不是每条轨都录到，无法合并
    Unmergeable,
    /// 之前的运行中缺失，原因没有记录
    Unknown,
}
