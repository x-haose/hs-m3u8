//! 任务进度：阶段、分片数、等着重试的请求与写出的进度。

use std::time::Duration;

use hs_m3u8_core as core;
use pyo3::prelude::*;

use super::failure::{FetchFailure, RefreshCause};
use crate::convert::Seq;
use crate::error::Mp4Unsupported;
use crate::hooks::Purpose;
use crate::info::Selected;
use crate::record::{record, variants};

variants! {
    /// 任务所处的阶段。失败时停在原来的阶段，结果只能从 `Job.wait` 取得。
    pub enum Stage {
        /// 准备：检查输出、读取任务目录，下载时还要拉取播放列表、选轨
        Preparing {},
        /// 点播：下载分片
        Downloading {},
        /// 直播：刷新播放列表并下载新分片
        Recording {},
        /// 写出输出（合并 MP4、写本地 HLS）
        Writing {},
        /// 完成
        Done {},
    }
}

record! {
    /// 任务进度快照（`Job.progress`）。
    pub struct Progress {
        /// 所处的阶段
        pub stage: Stage,
        /// 所选的变体与音频（怎样选见 `Preference`）；解析完成前、或来源本身是媒体播放列表时为 None。重新选轨而选出的与
        /// 记录的不是同一条时为这次选出的，核对上才沿用；核对不上时输出仍照任务目录里记录的
        pub selected: Option<Selected>,
        /// 分片数，各轨合计
        pub segments: SegmentCounts,
        /// 第 0 条轨已完成分片的声明时长之和；含续传前已完成的。直播即已录时长
        pub duration: Duration,
        /// 任务目录中已完成的分片与 init 段的字节数（解密后）；含续传前已完成的
        pub bytes: u64,
        /// 本次运行从网络读到的响应体字节数，读到即计：含播放列表与 key，含没读完、校验失败与重试前的请求。按它的
        /// 变化算下载速度
        pub received: u64,
        /// 正在退避、等着重试的请求（分片、key、init 段，与开始任务时拉的播放列表；直播的刷新不重试，见
        /// `refresh_errors`），按开始退避的先后；退避结束（接着重试或被取消）即去掉
        pub retrying: Seq<Retrying>,
        /// 写出的进度；`Stage.Writing` 之前为 None
        pub writing: Option<Writing>,
        /// 直播：各轨（下标为轨道编号）最近一次刷新失败的原因。这条轨之后刷新成功、不再刷新或录制结束即为 None；
        /// 一直失败到停滞时任务以 `LiveStalledError` 结束。点播与录制开始前为空
        pub refresh_errors: Seq<Option<RefreshCause>>,
        /// 直播且输出含 MP4：录到的内容放不进 MP4 的原因。续录时任务目录里已录齐的组在开始后核对一次，之后每个（会话,
        /// 不连续段）组在各轨都有分片时核对（录制循环结束、收尾时才下完的不核对）；第 0 组开头的分片判断不了时，各组
        /// 只核对自身、不与它比较。组号为核对时的，之后才录齐、排在前面的组会让合并时的组号变大。录制照常进行，停不停
        /// 由调用方定（直播错过的内容录不回来）；合并 MP4 时照样以 `Mp4UnsupportedError` 失败，可改为只输出本地 HLS
        /// 再合并。点播（下载各组的第一个分片后核对，放不进时任务直接失败）与不输出 MP4 时为 None
        pub mp4_unsupported: Option<Mp4Unsupported>,
    }
}

record! {
    /// 分片数，各轨合计（`Progress.segments`）。
    pub struct SegmentCounts {
        /// 已完成的；含续传前已完成的
        pub done: usize,
        /// 要下载的：已完成的（含续传前的）加上排入下载的；直播另含列出了但 init 段取不到的，随录制增长；点播不含所在组
        /// 合并不了的（见 `Output.missed`）。全部处理完时等于 `done` 加 `failed`
        pub total: usize,
        /// 直播本次运行中列出了、但分片或其 init 段取不到的，计入 `total`；点播恒为 0
        pub failed: usize,
        /// 直播本次运行中两次刷新之间已滑出窗口、没有列出过的，不在 `total` 里；点播恒为 0
        pub expired: usize,
        /// 来源标了 EXT-X-GAP（没有内容）、不取的，不在 `total` 里；直播为本次运行中列出的
        pub gap: usize,
    }
}

record! {
    /// 一个正在退避、等着重试的请求（`Progress.retrying`）。
    pub struct Retrying {
        /// 请求的用途
        pub purpose: Purpose,
        /// 上一次尝试的失败
        pub failure: FetchFailure,
        /// 接着是第几次重试，从 1 起（总尝试次数见 `RetryPolicy.attempts`）
        pub retry: u32,
        /// 这次退避多久（随机抖动之后的，服务器要求等待更久时按它的）
        pub delay: Duration,
    }
}

record! {
    /// 写出的进度（`Progress.writing`），以文件计：各组各轨的 init 段与分片各算一个（打包音频按分片）。
    pub struct Writing {
        /// 合并 MP4：写完的输入文件，写到下一个分片的包时上一个才算完，合并完回读核对 MP4 时不再增长；不要 MP4 时为
        /// None
        pub mp4: Option<FileCount>,
        /// 备齐本地 HLS：放好的文件，已放过的 init 段照样算；与任务目录同盘时用硬链接，几乎不花时间。不要本地 HLS 时为
        /// None
        pub hls: Option<FileCount>,
    }
}

record! {
    /// 完成了的文件数与合计。
    pub struct FileCount {
        /// 完成了的
        pub done: usize,
        /// 合计
        pub total: usize,
    }
}

impl From<core::Stage> for Stage {
    fn from(stage: core::Stage) -> Self {
        match stage {
            core::Stage::Preparing => Stage::Preparing {},
            core::Stage::Downloading => Stage::Downloading {},
            core::Stage::Recording => Stage::Recording {},
            core::Stage::Writing => Stage::Writing {},
            core::Stage::Done => Stage::Done {},
        }
    }
}

impl From<core::Progress> for Progress {
    fn from(p: core::Progress) -> Self {
        Progress {
            stage: p.stage.into(),
            selected: p.selected.map(Selected::from),
            segments: SegmentCounts {
                done: p.segments.done,
                total: p.segments.total,
                failed: p.segments.failed,
                expired: p.segments.expired,
                gap: p.segments.gap,
            },
            duration: p.duration,
            bytes: p.bytes,
            received: p.received,
            retrying: p
                .retrying
                .into_iter()
                .map(|r| Retrying {
                    purpose: r.purpose.into(),
                    failure: r.failure.into(),
                    retry: r.retry,
                    delay: r.delay,
                })
                .collect(),
            writing: p.writing.map(|w| {
                let count = |c: core::FileCount| FileCount {
                    done: c.done,
                    total: c.total,
                };
                Writing {
                    mp4: w.mp4.map(count),
                    hls: w.hls.map(count),
                }
            }),
            refresh_errors: p
                .refresh_errors
                .into_iter()
                .map(|error| error.map(RefreshCause::from))
                .collect(),
            mp4_unsupported: p.mp4_unsupported.map(Mp4Unsupported::from),
        }
    }
}
