//! 任务的结果：输出、合并出的流、直播的录制结果与缺失的分片。

use std::time::Duration;

use hs_m3u8_core as core;
use pyo3::prelude::*;

use super::failure::{FetchErrorKind, Leftover, StallCause};
use crate::convert::{PyPath, Seq};
use crate::record::{record, variants};

variants! {
    /// 流的种类。
    pub enum StreamKind {
        /// 视频
        Video {},
        /// 音频
        Audio {},
    }
}

variants! {
    /// 一路流的编码参数。
    pub enum Shape {
        /// 视频
        #[pyo3(constructor = (*, codec, width, height, pixel_format))]
        Video {
            /// FFmpeg 的编码名，如 `h264`、`hevc`
            codec: String,
            /// 宽，像素
            width: u32,
            /// 高，像素
            height: u32,
            /// FFmpeg 的像素格式名（色度采样与位深），如 `yuv420p`、`yuv420p10le`；探测不出时为 None
            pixel_format: Option<String>,
        },
        /// 音频
        #[pyo3(constructor = (*, codec, sample_rate, channels))]
        Audio {
            /// FFmpeg 的编码名，如 `aac`
            codec: String,
            /// 采样率，Hz
            sample_rate: u32,
            /// 声道数
            channels: u32,
        },
    }
}

record! {
    /// 合并出的一路流。
    pub struct StreamReport {
        /// 编码参数
        pub shape: Shape,
        /// 写入、且回读核对一致的包数
        pub packets: u64,
        /// 输入中没有解码时间戳、因而未写入的包数
        pub skipped_without_dts: u64,
        /// 各组开头解不出、因而未写入的视频包数：第一个关键帧（随机访问点）之前的包，与参考上一个 GOP 的前导帧（HEVC 为
        /// RASL；其余编码为解码在关键帧之后、呈现在它之前的帧）。音频为 0
        pub skipped_before_keyframe: u64,
        /// 损坏而没有写入的音频字节数：TS 里读不出帧头或帧长与包长对不上的包（来源丢了 TS 包，或缺片处 PES 不完整，
        /// 拼出的包），打包音频里切帧时跳过的不完整的帧或标签、认不出的字节，以及片内损坏之后定不了时刻（这一片是最后
        /// 一片，或下一片之前缺了分片）的帧。视频包照常写入，由解码器做错误隐藏
        pub discarded_corrupt_bytes: u64,
        /// 输出时间线上从最早的呈现时刻到最晚的结束时刻；含组内时间戳间断（如缺失的分片）留下的空档
        pub duration: Duration,
    }
}

record! {
    /// 合并 MP4 的结果。
    pub struct MediaReport {
        /// 各路输出流，按流的下标排列
        pub streams: Seq<StreamReport>,
    }
}

record! {
    /// 生成的 MP4。
    pub struct Mp4Output {
        /// 绝对路径，相对路径按开始任务时的当前目录补全
        pub path: PyPath,
        /// 各路输出流的编码参数、包数与时长
        pub report: MediaReport,
    }
}

variants! {
    /// 本次运行的录制如何结束。
    pub enum LiveEnd {
        /// 所有轨出现 EXT-X-ENDLIST
        EndList {},
        /// 调用了 `Job.stop`
        Stopped {},
        /// 各轨都录满了 `LiveOptions.max_duration`
        DurationReached {},
        /// 一条轨停滞，且看起来是直播已结束：它与其他轨都不再出新分片；故障导致的停滞是 `LiveStalledError`
        #[pyo3(constructor = (*, track, cause))]
        Stalled {
            /// 第几条轨
            track: usize,
            /// 看起来已结束的原因
            cause: StallCause,
        },
        /// 一条轨的播放列表与之前刷新得到的接不上，且录的不止一条轨：其他轨从哪里起与它对得上不知道，不另起会话，
        /// 之后的分片未录。只录一条轨时另起会话接着录（见 `NewSession`）
        #[pyo3(constructor = (*, track, cause))]
        Broken {
            /// 第几条轨
            track: usize,
            /// 接不上的方式
            cause: SourceBreak,
        },
    }
}

variants! {
    /// 媒体播放列表与之前刷新得到的接不上的方式。
    pub enum SourceBreak {
        /// 媒体序号回退、且与上次的窗口没有重叠，连续两次刷新都如此（多为编码器重启；只一次多为 CDN 的旧缓存，不算）
        Restarted {},
        /// 在某个序号处矛盾：同一序号换了分片，或不连续段编号对不上（服务器错误）
        Inconsistent {
            /// 矛盾处的媒体序号
            sequence: u64,
        },
    }
}

record! {
    /// 录制中途另起的会话：只录一条轨时，它的播放列表与之前刷新得到的接不上，不结束录制，另起会话接着录。序号回退时
    /// 从新窗口的起点录；矛盾时跳过已处理到的序号，只录更新的分片。
    pub struct NewSession {
        /// 另起的会话编号
        pub session: u32,
        /// 接不上的方式
        pub cause: SourceBreak,
    }
}

variants! {
    /// 分片不在输出中的原因。
    pub enum MissReason {
        /// 两次刷新之间已滑出播放列表窗口，没有被列出过
        Expired {},
        /// 列出了，但取不到：404/410，可重试的错误重试后仍失败，或重试后内容仍没写完
        Failed {
            /// 取不到的原因
            cause: FetchErrorKind,
        },
        /// 列出了，但它的 init 段取不到（同上）
        InitFailed {
            /// 取不到的原因
            cause: FetchErrorKind,
        },
        /// 所在不连续段不是每条轨都有内容（直播为有轨没录到，点播为有轨只有标了 EXT-X-GAP 的分片），无法合并
        Unmergeable {},
        /// 来源标了 EXT-X-GAP：这一片没有内容，不取
        Gap {},
        /// 之前的运行中缺失，原因没有记录
        Unknown {},
    }
}

record! {
    /// 一段连续的缺失分片：第 `session` 个会话中第 `track` 条轨序号 `first` 到 `last`（含）的分片不在输出中。
    pub struct Missed {
        /// 会话编号，从 0 开始
        pub session: u32,
        /// 第几条轨
        pub track: usize,
        /// 第一个缺失的媒体序号
        pub first: u64,
        /// 最后一个缺失的媒体序号
        pub last: u64,
        /// 区间里的分片数，即 `last - first + 1`；超出 64 位整数时为 2**64 - 1
        pub count: usize,
        /// 缺失的原因
        pub reason: MissReason,
    }
}

record! {
    /// 直播的录制结果。
    pub struct LiveReport {
        /// 本次运行的录制如何结束；只合并（`Engine.merge_recorded`）时为 None
        pub end: Option<LiveEnd>,
        /// 合并进输出的录制会话数。会话是一段时间线连续的录制：中断后续录时接得上（见 `LiveOptions`）仍是同一个会话，
        /// 否则另起一个，与之前的首尾相接。录制中途另起的（见 `new_sessions`）也算
        pub session_count: usize,
        /// 本次运行录制中途另起的会话，按先后；只合并时为空
        pub new_sessions: Seq<NewSession>,
    }
}

record! {
    /// 任务结果：下载、录制或只合并。
    pub struct Output {
        /// 生成的 MP4；没要 MP4（`Target.Hls`）时为 None
        pub mp4: Option<Mp4Output>,
        /// 生成的本地 HLS 目录（绝对路径）；没要 HLS 时为 None。入口为其中的 `index.m3u8`：单轨时是媒体播放列表，视频与
        /// 独立音频分离时是主播放列表；各轨的分片、init 段（与分离时这条轨的 `index.m3u8`）在 `<轨道编号>/` 下。内容与
        /// MP4 相同：只放各轨都有的不连续段组，组内缺失的分片不标出、保留原时间戳
        pub hls: Option<PyPath>,
        /// 输出的分片数，各轨合计
        pub segments: usize,
        /// 同 `Progress.bytes`
        pub bytes: u64,
        /// 收尾没删掉的东西：被替换的旧输出、没删干净的任务目录等；输出不受影响。都删干净时为空
        pub leftovers: Seq<Leftover>,
        /// 不在输出中的分片，按会话、轨道、序号排列，相接且原因相同的合成一个区间：点播有来源标了 EXT-X-GAP 的及因此合
        /// 并不了的；直播另有窗口滑过、取不到等（见 `MissReason`），不含之前的运行里、某个会话第一个已完成分片之前或最
        /// 后一个之后缺的（原因与范围随那次运行一起丢了）。缺的分片落在所在组各轨已有内容的时间范围内时，那条轨在那段留
        /// 空、保留原时间戳；各轨都缺的组首尾与整组不在输出中的不占时间（各组首尾相接）
        pub missed: Seq<Missed>,
        /// 直播的录制结果；点播为 None
        pub live: Option<LiveReport>,
    }
}

impl From<core::remux::StreamKind> for StreamKind {
    fn from(kind: core::remux::StreamKind) -> Self {
        match kind {
            core::remux::StreamKind::Video => StreamKind::Video {},
            core::remux::StreamKind::Audio => StreamKind::Audio {},
        }
    }
}

impl From<core::remux::Shape> for Shape {
    fn from(shape: core::remux::Shape) -> Self {
        match shape {
            core::remux::Shape::Video {
                codec,
                width,
                height,
                pixel_format,
            } => Shape::Video {
                codec: codec.to_owned(),
                width,
                height,
                pixel_format: pixel_format.map(str::to_owned),
            },
            core::remux::Shape::Audio {
                codec,
                sample_rate,
                channels,
            } => Shape::Audio {
                codec: codec.to_owned(),
                sample_rate,
                channels,
            },
        }
    }
}

impl From<core::remux::StreamReport> for StreamReport {
    fn from(s: core::remux::StreamReport) -> Self {
        StreamReport {
            shape: s.shape.into(),
            packets: s.packets,
            skipped_without_dts: s.skipped_without_dts,
            skipped_before_keyframe: s.skipped_before_keyframe,
            discarded_corrupt_bytes: s.discarded_corrupt_bytes,
            duration: s.duration,
        }
    }
}

impl From<core::Mp4Output> for Mp4Output {
    fn from(m: core::Mp4Output) -> Self {
        Mp4Output {
            path: m.path.into(),
            report: MediaReport {
                streams: m
                    .report
                    .streams
                    .into_iter()
                    .map(StreamReport::from)
                    .collect(),
            },
        }
    }
}

impl From<core::LiveEnd> for LiveEnd {
    fn from(end: core::LiveEnd) -> Self {
        match end {
            core::LiveEnd::EndList => LiveEnd::EndList {},
            core::LiveEnd::Stopped => LiveEnd::Stopped {},
            core::LiveEnd::DurationReached => LiveEnd::DurationReached {},
            core::LiveEnd::Stalled { track, cause } => LiveEnd::Stalled {
                track,
                cause: cause.into(),
            },
            core::LiveEnd::Broken { track, cause } => LiveEnd::Broken {
                track,
                cause: cause.into(),
            },
        }
    }
}

impl From<core::SourceBreak> for SourceBreak {
    fn from(cause: core::SourceBreak) -> Self {
        match cause {
            core::SourceBreak::Restarted => SourceBreak::Restarted {},
            core::SourceBreak::Inconsistent { sequence } => SourceBreak::Inconsistent { sequence },
        }
    }
}

impl From<core::NewSession> for NewSession {
    fn from(s: core::NewSession) -> Self {
        NewSession {
            session: s.session,
            cause: s.cause.into(),
        }
    }
}

impl From<core::MissReason> for MissReason {
    fn from(reason: core::MissReason) -> Self {
        match reason {
            core::MissReason::Expired => MissReason::Expired {},
            core::MissReason::Failed(cause) => MissReason::Failed {
                cause: cause.into(),
            },
            core::MissReason::InitFailed(cause) => MissReason::InitFailed {
                cause: cause.into(),
            },
            core::MissReason::Unmergeable => MissReason::Unmergeable {},
            core::MissReason::Gap => MissReason::Gap {},
            core::MissReason::Unknown => MissReason::Unknown {},
        }
    }
}

impl From<core::Missed> for Missed {
    fn from(m: core::Missed) -> Self {
        Missed {
            count: m.count(),
            session: m.session,
            track: m.track,
            first: m.first,
            last: m.last,
            reason: m.reason.into(),
        }
    }
}

impl From<core::LiveReport> for LiveReport {
    fn from(l: core::LiveReport) -> Self {
        LiveReport {
            end: l.end.map(LiveEnd::from),
            session_count: l.session_count,
            new_sessions: l.new_sessions.into_iter().map(NewSession::from).collect(),
        }
    }
}

impl From<core::Output> for Output {
    fn from(o: core::Output) -> Self {
        Output {
            mp4: o.mp4.map(Mp4Output::from),
            hls: o.hls.map(PyPath::from),
            segments: o.segments,
            bytes: o.bytes,
            leftovers: o.leftovers.into_iter().map(Leftover::from).collect(),
            missed: o.missed.into_iter().map(Missed::from).collect(),
            live: o.live.map(LiveReport::from),
        }
    }
}
