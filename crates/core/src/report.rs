//! 任务的进度与结果。

use std::path::PathBuf;

use crate::{HttpError, Report};

/// 任务进度快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub stage: Stage,
    /// 已完成的分片，各轨合计；含续传前已完成的
    pub segments_done: usize,
    /// 分片总数；直播时为目前已发现的分片数，随录制增长
    pub segments_total: usize,
    /// 直播的漏段数（窗口已滑过或取不到的分片）；点播恒为 0
    pub segments_missed: usize,
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
    /// 本次运行的录制如何结束
    pub end: LiveEnd,
    /// 合并进输出的录制次数（中断后继续录制会多出一次）
    pub sessions: u32,
    /// 不在输出中的分片，按录制次数、轨道、序号排列，相接且原因相同的合成一个区间
    pub missed: Vec<Missed>,
}

/// 本次运行的录制如何结束。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEnd {
    /// 所有轨出现 EXT-X-ENDLIST
    EndList,
    /// 调用了 [`crate::Job::stop`]
    Stopped,
    /// 各轨都录满了 [`crate::LiveOptions::max_duration`]
    MaxDuration,
    /// 第 `track` 条轨连续 [`crate::LiveOptions::stall_timeout`] 没有新分片
    Stalled { track: usize, cause: StallCause },
    /// 第 `track` 条轨的媒体序号回退、且与上次的窗口没有重叠（多为编码器重启）；之后的分片未录
    Restarted { track: usize },
    /// 第 `track` 条轨序号 `sequence` 的分片与上次刷新时不同（服务器错误，RFC 8216 6.3.4）；之后的分片未录
    SegmentChanged { track: usize, sequence: u64 },
    /// 按 [`crate::Resume::MergeOnly`] 只合并了已录到的分片
    MergeOnly,
}

/// 判定停滞时该轨的状况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StallCause {
    /// 刷新成功，但没有新分片
    NoNewSegments,
    /// 最近一次刷新失败
    RefreshFailed(String),
    /// 刷新请求一直没有返回
    RefreshPending,
}

/// 一段连续的漏段：第 `session` 次录制中第 `track` 条轨序号 `first..=last` 的分片不在输出中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missed {
    /// 第几次录制，从 0 开始
    pub session: u32,
    pub track: usize,
    pub first: u64,
    pub last: u64,
    pub reason: MissReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissReason {
    /// 两次刷新之间已滑出播放列表窗口，没有被列出过
    Expired,
    /// 列出了，但重试后仍未取到
    Failed(HttpError),
    /// 所在不连续段不是每条轨都录到，无法合并
    Unmergeable,
    /// 之前某次录制中缺失，原因没有记录
    Unknown,
}
