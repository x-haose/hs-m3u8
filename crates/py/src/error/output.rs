//! 内容放不进输出的原因：MP4 与本地 HLS 各自表示不了的内容，及由 core 的对应类型转换。

use std::time::Duration;

use hs_m3u8_core as core;
use pyo3::prelude::*;

use crate::record::variants;
use crate::report::{Shape, StreamKind};

variants! {
    /// 内容放不进 MP4 的原因；可只输出本地 HLS（`Target.Hls`），它原样保留分片，不受这些限制。各取值的 `group` 为合并
    /// 时的组号：点播即播放列表里第几个不连续段组，直播为各轨都录到的组按会话、不连续段排列的下标。
    pub enum Mp4Unsupported {
        /// 编码放不进 MP4；异常的说明文字里列出支持的编码
        #[pyo3(constructor = (*, group, track, kind, codec))]
        Codec {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
            /// 流的种类
            kind: StreamKind,
            /// FFmpeg 的编码名
            codec: String,
        },
        /// MP3 的采样率低于 16 kHz（MPEG-2.5 的 8、11.025、12 kHz）：MP4 封装器不收
        #[pyo3(constructor = (*, group, track, sample_rate))]
        Mp3SampleRate {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
            /// Hz
            sample_rate: u32,
        },
        /// 选中的流探测不出编码或编码参数（视频的宽高、音频的采样率与声道数），或与第 0 组比较时没有确定的不同、而
        /// 有项比不了（视频的像素格式只有一边探测出，fMP4 里配置不同的 AAC 有一边没解出帧）：这一组的第一个分片里没有
        /// 这路流能解的数据
        #[pyo3(constructor = (*, group, track, kind))]
        ParamsUnknown {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
            /// 流的种类
            kind: StreamKind,
        },
        /// 流的种类与第 0 组不同
        #[pyo3(constructor = (*, group, track))]
        LayoutChanged {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
        },
        /// 编码参数与第 0 组不同
        #[pyo3(constructor = (*, group, track, kind, first, found))]
        ParamsChanged {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
            /// 流的种类
            kind: StreamKind,
            /// 第 0 组的参数
            first: Shape,
            /// 这一组的参数
            found: Shape,
        },
        /// 编码参数相同而解码配置与第 0 组不同：如 AAC 的 profile（LC 与 HE-AAC 的采样率都可以是 48 kHz）、H.264 的
        /// 参数集，或一组是 fMP4 而另一组是 TS 或打包音频
        #[pyo3(constructor = (*, group, track, kind))]
        ConfigChanged {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
            /// 流的种类
            kind: StreamKind,
        },
        /// TS 的一条轨在这一组里换了要取的那一路的 PID（PMT 更新后原来那路不再列出）
        #[pyo3(constructor = (*, group, track, kind))]
        StreamChangedInGroup {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
            /// 流的种类
            kind: StreamKind,
        },
        /// 这一组里中途换了编码或编码参数（采样率、声道、画面宽高等），只出在参数随帧携带的 TS 与打包音频里
        #[pyo3(constructor = (*, group, track, kind))]
        ParamsChangedInGroup {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
            /// 流的种类
            kind: StreamKind,
        },
        /// 没有 init 段的一条轨在这一组里换了分片格式（TS 与打包音频，或打包音频的格式之间）
        #[pyo3(constructor = (*, group, track))]
        FormatChangedInGroup {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
        },
        /// E-AC-3 每帧不足 6 块（只出在 TS 与打包音频里）：MP4 封装器把几帧并成一个样本，末尾凑不满的帧不写出
        #[pyo3(constructor = (*, group, track))]
        Eac3ShortFrames {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
        },
        /// E-AC-3 有多个独立子流（只出在 TS 与打包音频里）：MP4 封装器不支持
        #[pyo3(constructor = (*, group, track))]
        Eac3Substreams {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
        },
        /// 解码时间戳不递增：往回跳多为来源在时间戳重新开始处漏标了 EXT-X-DISCONTINUITY，重复多为相邻分片首尾
        /// 重复了一帧
        #[pyo3(constructor = (*, group, track, kind))]
        DtsNotIncreasing {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
            /// 流的种类
            kind: StreamKind,
        },
        /// 组内时间戳往前跳，输出里会留下整组都没有内容的空档，扣掉这条轨上一个包所在的片（跨片时）之后仍比一个最长
        /// 的声明时长还长：多为来源在时间戳重新开始处漏标了 EXT-X-DISCONTINUITY。一路流单独中断不算；其间有缺片时不
        /// 核对
        #[pyo3(constructor = (*, group, track, kind, segment, jump))]
        TimestampJump {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
            /// 流的种类
            kind: StreamKind,
            /// 跳变后的第一个包所在的分片在这一组这条轨里是第几个
            segment: usize,
            /// 空档至少多长：与全组已写出的最大时间戳之差，减去这条轨上一个包所在的片与其间各片声明的时长
            jump: Duration,
        },
        /// 这一组里这条轨的视频没有关键帧（随机访问点；不带 recovery point SEI 的 H.264 开放 GOP 里非 IDR 的 I 帧不算：
        /// 之后的帧可能参考它之前的帧）：写进去解不出画面
        #[pyo3(constructor = (*, group, track))]
        NoKeyframe {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
        },
        /// 这条轨是打包音频，这一组有分片没有 ID3 时间戳，而这一组还有别的轨：不知道怎样与它们对齐
        #[pyo3(constructor = (*, group, track))]
        UntimedPackedAudio {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
        },
        /// 各轨都没有要取的视频或音频流
        NoStreams {},
    }
}

variants! {
    /// 本地 HLS 表示不了的内容；可只输出 MP4（`Target.Mp4`）。
    pub enum HlsUnsupported {
        /// 这条轨里不用 init 段的不连续段组排在用 init 段（fMP4）的组后面：EXT-X-MAP 一直作用到下一个
        /// EXT-X-MAP。直播续录时服务器从 fMP4 换成 TS 会这样
        MixedInit {
            /// 第几条轨
            track: usize,
        },
        /// 音视频分离时主播放列表必须写码率，而某条轨的分片声明的时长都为 0 算不出，来源也没写
        BandwidthUnknown {},
    }
}

impl From<core::Mp4Unsupported> for Mp4Unsupported {
    fn from(unsupported: core::Mp4Unsupported) -> Self {
        use core::Mp4Unsupported as U;
        match unsupported {
            U::Codec {
                group,
                track,
                kind,
                codec,
            } => Mp4Unsupported::Codec {
                group,
                track,
                kind: kind.into(),
                codec: codec.to_owned(),
            },
            U::ParamsUnknown { group, track, kind } => Mp4Unsupported::ParamsUnknown {
                group,
                track,
                kind: kind.into(),
            },
            U::LayoutChanged { group, track } => Mp4Unsupported::LayoutChanged { group, track },
            U::ParamsChanged {
                group,
                track,
                kind,
                first,
                found,
            } => Mp4Unsupported::ParamsChanged {
                group,
                track,
                kind: kind.into(),
                first: first.into(),
                found: found.into(),
            },
            U::ConfigChanged { group, track, kind } => Mp4Unsupported::ConfigChanged {
                group,
                track,
                kind: kind.into(),
            },
            U::StreamChangedInGroup { group, track, kind } => {
                Mp4Unsupported::StreamChangedInGroup {
                    group,
                    track,
                    kind: kind.into(),
                }
            }
            U::ParamsChangedInGroup { group, track, kind } => {
                Mp4Unsupported::ParamsChangedInGroup {
                    group,
                    track,
                    kind: kind.into(),
                }
            }
            U::Mp3SampleRate {
                group,
                track,
                sample_rate,
            } => Mp4Unsupported::Mp3SampleRate {
                group,
                track,
                sample_rate,
            },
            U::FormatChangedInGroup { group, track } => {
                Mp4Unsupported::FormatChangedInGroup { group, track }
            }
            U::Eac3ShortFrames { group, track } => Mp4Unsupported::Eac3ShortFrames { group, track },
            U::Eac3Substreams { group, track } => Mp4Unsupported::Eac3Substreams { group, track },
            U::DtsNotIncreasing { group, track, kind } => Mp4Unsupported::DtsNotIncreasing {
                group,
                track,
                kind: kind.into(),
            },
            U::TimestampJump {
                group,
                track,
                kind,
                segment,
                jump_us,
            } => Mp4Unsupported::TimestampJump {
                group,
                track,
                kind: kind.into(),
                segment,
                jump: Duration::from_micros(jump_us),
            },
            U::NoKeyframe { group, track } => Mp4Unsupported::NoKeyframe { group, track },
            U::UntimedPackedAudio { group, track } => {
                Mp4Unsupported::UntimedPackedAudio { group, track }
            }
            U::NoStreams => Mp4Unsupported::NoStreams {},
        }
    }
}

impl From<core::HlsUnsupported> for HlsUnsupported {
    fn from(unsupported: core::HlsUnsupported) -> Self {
        match unsupported {
            core::HlsUnsupported::MixedInit { track } => HlsUnsupported::MixedInit { track },
            core::HlsUnsupported::BandwidthUnknown => HlsUnsupported::BandwidthUnknown {},
        }
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Mp4Unsupported>()?;
    module.add_class::<HlsUnsupported>()?;
    Ok(())
}
