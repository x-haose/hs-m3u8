//! 异常里的原因与位置：出错的分片、选不出轨、播放列表不合法等的类型，及由 core 的对应类型转换。

use hs_m3u8_core as core;
use pyo3::prelude::*;

use crate::convert::{Address, Seq};
use crate::record::{record, variants};
use crate::report::{HttpFailure, RefreshCause, Shape, StallCause, StreamKind};

record! {
    /// 出错的分片。
    pub struct SegmentRef {
        /// 第几条轨：0 为所选变体（来源本身是媒体播放列表时即它），1 为独立的音频
        pub track: usize,
        /// 媒体序号
        pub sequence: u64,
        /// 分片地址（播放列表里写的，可能带令牌；异常的说明里只显示到路径）
        pub url: Address,
    }
}

variants! {
    /// 按选轨偏好选不出轨的原因。
    pub enum SelectFailure {
        /// 主播放列表里没有变体
        NoVariants {},
        /// `VariantChoice.Index` 的下标超出范围
        #[pyo3(constructor = (*, index, count))]
        VariantIndexOutOfRange {
            /// 给的下标
            index: usize,
            /// 变体个数
            count: usize,
        },
        /// 所选变体的音频组里没有这个语言的音频
        #[pyo3(constructor = (*, group, language, available))]
        AudioLanguageNotFound {
            /// 音频组
            group: String,
            /// 给的语言
            language: String,
            /// 组里有的语言
            available: Seq<String>,
        },
        /// `AudioChoice.Index` 的下标超出范围
        #[pyo3(constructor = (*, index, count))]
        AudioIndexOutOfRange {
            /// 给的下标
            index: usize,
            /// 音频 rendition 的个数
            count: usize,
        },
        /// 指定的音频不在所选变体引用的音频组里
        #[pyo3(constructor = (*, index, group))]
        NotVariantAudio {
            /// 给的下标
            index: usize,
            /// 所选变体引用的音频组；变体没有引用音频组时为 None
            group: Option<String>,
        },
    }
}

variants! {
    /// 内容不是合法的播放列表的原因。
    pub enum Malformed {
        /// 内容为空或只有空白
        Empty {},
        /// 第一行不是 #EXTM3U
        NotAPlaylist {},
        /// 首行是 #EXTM3U，但不是 UTF-8 文本；站点用了别的编码时在 `Hooks.on_playlist` 中转码
        NotUtf8 {},
        /// 首行是 #EXTM3U，结尾截在多字节字符中间（多为服务器还没写完）；可以重试。别的编码的完整内容，唯一的非 ASCII
        /// 字符恰在结尾时也可能报这一种
        Truncated {},
        /// 同时含有主播放列表（EXT-X-STREAM-INF）与媒体播放列表（EXTINF）的标签
        Mixed {},
        /// 语法错误
        #[pyo3(constructor = (*, line, detail))]
        Syntax {
            /// 出错的行，从 1 起
            line: usize,
            /// 错在哪里
            detail: String,
        },
    }
}

variants! {
    /// 本库解不了的加密。
    pub enum Encryption {
        /// SAMPLE-AES
        SampleAes {},
        /// DRM
        Drm {
            /// KEYFORMAT 属性
            keyformat: String,
        },
        /// 未知的加密方式
        Method {
            /// METHOD 属性
            name: String,
        },
    }
}

variants! {
    /// 来源用到了不支持的特性。
    pub enum SourceUnsupported {
        /// 播放列表没有分片
        EmptyPlaylist {
            /// 播放列表的地址
            url: Address,
        },
        /// 直播播放列表既没有 EXT-X-TARGETDURATION 也没有分片，无法确定刷新间隔
        NoTargetDuration {
            /// 播放列表的地址
            url: Address,
        },
        /// EXT-X-KEY 用了本库解不了的加密
        #[pyo3(constructor = (*, url, line, encryption))]
        Encrypted {
            /// 播放列表的地址
            url: Address,
            /// EXT-X-KEY 所在的行，从 1 起
            line: usize,
            /// 加密方式
            encryption: Encryption,
        },
        /// 同一不连续段内 EXT-X-MAP 发生变化
        #[pyo3(constructor = (*, track, discontinuity))]
        InitChangesWithinGroup {
            /// 第几条轨
            track: usize,
            /// 不连续段编号
            discontinuity: u64,
        },
        /// 各轨的不连续段不一致
        #[pyo3(constructor = (*, track, first, found))]
        DiscontinuityMismatch {
            /// 与第 0 条轨不一致的轨
            track: usize,
            /// 第 0 条轨的各不连续段编号
            first: Seq<u64>,
            /// 这条轨的各不连续段编号
            found: Seq<u64>,
        },
    }
}

variants! {
    /// 数据校验失败的原因。
    pub enum IntegrityFailure {
        /// 解密后填充不合法：多为 key 不对（IV 不对不会这样），或数据损坏
        Padding {},
        /// 密文长度不是 16 的整数倍
        CipherLength {
            /// 密文字节数
            length: usize,
        },
        /// 字节范围请求返回的长度不对
        #[pyo3(constructor = (*, expected, found))]
        RangeLength {
            /// 应有的字节数
            expected: u64,
            /// 实际的字节数
            found: usize,
        },
        /// 不是 TS 或可识别的打包音频（AAC、MP3、AC-3、E-AC-3）
        UnrecognizedSegment {
            /// 开头几个字节的十六进制
            head: String,
        },
        /// 应为 fMP4 却不是
        NotFmp4 {
            /// 开头几个字节的十六进制
            head: String,
        },
    }
}

variants! {
    /// 内容放不进 MP4 的原因；可只输出本地 HLS（`Target.Hls`），它原样保留分片，不受这些限制。
    pub enum Mp4Unsupported {
        /// 编码放不进 MP4：fMP4 里支持 H.264、HEVC、AV1、VP9 视频与 AAC、AC-3、E-AC-3、MP3、FLAC、Opus、ALAC 音频，TS 与
        /// 打包音频里支持 H.264、HEVC 视频与 AAC、AC-3、E-AC-3、MP3 音频
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
        /// 选中的流探测不出编码或编码参数（视频的宽高、音频的采样率与声道数）：这一组开头一段里没有它的数据
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
        /// 打包音频的一个分片里读不出音频帧：既不是音频帧也不是 ID3 标签、帧或标签不完整，或是自由格式（帧头不给
        /// 码率）的 MP3
        #[pyo3(constructor = (*, group, track, segment, offset))]
        UnreadablePackedAudio {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
            /// 这一组这条轨的第几个分片
            segment: usize,
            /// 读不出的位置，字节
            offset: u64,
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

variants! {
    /// 直播停滞的故障原因。
    pub enum StallReason {
        /// 刷新播放列表一直失败（404/410 之外的取不到，或内容不完整）
        RefreshFailed {
            /// 最近一次刷新失败
            cause: RefreshCause,
        },
        /// 刷新请求超过一个目标时长仍未返回
        RefreshPending {},
        /// 播放列表仍在列出新分片，但一个都没有下载成功
        Unrecordable {
            /// 最近一次取不到分片或 init 段的原因
            failure: HttpFailure,
        },
        /// 这条轨看起来已结束，而其他轨仍在出新分片
        TrackStopped {
            /// 看起来已结束的原因
            cause: StallCause,
        },
    }
}

variants! {
    /// 任务类型。
    pub enum JobType {
        /// 点播
        Vod {},
        /// 直播
        Live {},
    }
}

variants! {
    /// 任务目录不能使用的原因。
    pub enum WorkDirProblem {
        /// 不是本库建立的任务目录（不为空，且没有 job.json），不会去动它
        NotEmpty {},
        /// 正被另一个任务使用
        Locked {},
        /// job.json、outputs.json 或分片文件无法识别
        Corrupt {
            /// 哪里无法识别
            detail: String,
        },
        /// 记录是另一个格式版本写的：比本库新时用更新的版本续传；旧时用写下它的版本做完，或放弃任务
        #[pyo3(constructor = (*, file, found, supported))]
        UnsupportedVersion {
            /// 哪个文件：job.json 或 outputs.json
            file: String,
            /// 记录的版本
            found: u32,
            /// 本库支持的版本
            supported: u32,
        },
        /// 记录的任务类型与本次不同：记录的是直播、本次是点播，多为中断期间直播结束了而请求没开启直播
        /// （`JobRequest.live`），开启即可续录，或用 `Engine.merge_recorded` 只合并；记录的是点播、本次是直播，换一个
        /// 任务目录
        #[pyo3(constructor = (*, recorded, current))]
        KindMismatch {
            /// 记录的
            recorded: JobType,
            /// 本次的
            current: JobType,
        },
        /// 只合并只用于直播录制，目录里是点播的下载；点播用同样的请求正常运行即可续传
        NotLiveRecording {},
        /// 来源（地址去掉用户名、密码、查询串与片段，连同选轨偏好）与记录的不同
        SourceMismatch {},
        /// 来源的轨道（所选变体、有无独立音频）与记录的不同
        TracksMismatch {},
        /// 点播：播放列表的分片与记录的不一致，不能续传
        PlanChanged {},
        /// 记录的变体或音频已不在主播放列表中：已录的直播可用 `Engine.merge_recorded` 合并，或换一个任务目录重新开始
        SelectionGone {},
        /// 直播：地址与记录的不同（如换了令牌），且有轨接不上之前录到的内容，无法确认是同一个直播。可用原来的地址
        /// 续录、用 `Engine.merge_recorded` 合并已录的部分，或换一个任务目录重新录
        SourceUnverified {},
    }
}

variants! {
    /// 出错的是哪个回调。
    pub enum HookKind {
        /// `Hooks.on_playlist`
        Playlist {},
        /// `Hooks.on_request`
        Request {},
        /// `Hooks.on_key`
        Key {},
        /// `Hooks.on_segment`
        Segment {},
    }
}

impl From<core::hls::SelectError> for SelectFailure {
    fn from(error: core::hls::SelectError) -> Self {
        use core::hls::SelectError as E;
        match error {
            E::NoVariants => SelectFailure::NoVariants {},
            E::VariantIndexOutOfRange { index, count } => {
                SelectFailure::VariantIndexOutOfRange { index, count }
            }
            E::AudioLanguageNotFound {
                group,
                language,
                available,
            } => SelectFailure::AudioLanguageNotFound {
                group,
                language,
                available: Seq(available),
            },
            E::AudioIndexOutOfRange { index, count } => {
                SelectFailure::AudioIndexOutOfRange { index, count }
            }
            E::NotVariantAudio { index, group } => SelectFailure::NotVariantAudio { index, group },
        }
    }
}

impl From<core::hls::Malformed> for Malformed {
    fn from(error: core::hls::Malformed) -> Self {
        use core::hls::Malformed as E;
        match error {
            E::Empty => Malformed::Empty {},
            E::NotAPlaylist => Malformed::NotAPlaylist {},
            E::NotUtf8 => Malformed::NotUtf8 {},
            E::Truncated => Malformed::Truncated {},
            E::Mixed => Malformed::Mixed {},
            E::Syntax { line, kind } => Malformed::Syntax {
                line,
                detail: kind.to_string(),
            },
        }
    }
}

impl From<core::hls::Encryption> for Encryption {
    fn from(encryption: core::hls::Encryption) -> Self {
        use core::hls::Encryption as E;
        match encryption {
            E::SampleAes => Encryption::SampleAes {},
            E::Drm { keyformat } => Encryption::Drm { keyformat },
            E::Method(name) => Encryption::Method { name },
        }
    }
}

impl From<core::Integrity> for IntegrityFailure {
    fn from(integrity: core::Integrity) -> Self {
        use core::Integrity as I;
        match integrity {
            I::Padding => IntegrityFailure::Padding {},
            I::CipherLength(length) => IntegrityFailure::CipherLength { length },
            I::RangeLength { expected, found } => IntegrityFailure::RangeLength { expected, found },
            I::UnrecognizedSegment(head) => IntegrityFailure::UnrecognizedSegment { head },
            I::NotFmp4(head) => IntegrityFailure::NotFmp4 { head },
        }
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
            U::UnreadablePackedAudio {
                group,
                track,
                segment,
                offset,
            } => Mp4Unsupported::UnreadablePackedAudio {
                group,
                track,
                segment,
                offset,
            },
            U::Eac3ShortFrames { group, track } => Mp4Unsupported::Eac3ShortFrames { group, track },
            U::Eac3Substreams { group, track } => Mp4Unsupported::Eac3Substreams { group, track },
            U::DtsNotIncreasing { group, track, kind } => Mp4Unsupported::DtsNotIncreasing {
                group,
                track,
                kind: kind.into(),
            },
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

impl From<core::StallError> for StallReason {
    fn from(error: core::StallError) -> Self {
        use core::StallError as S;
        match error {
            S::RefreshFailed(cause) => StallReason::RefreshFailed {
                cause: cause.into(),
            },
            S::RefreshPending => StallReason::RefreshPending {},
            S::Unrecordable(failure) => StallReason::Unrecordable {
                failure: failure.into(),
            },
            S::TrackStopped(cause) => StallReason::TrackStopped {
                cause: cause.into(),
            },
        }
    }
}

impl From<core::JobType> for JobType {
    fn from(kind: core::JobType) -> Self {
        match kind {
            core::JobType::Vod => JobType::Vod {},
            core::JobType::Live => JobType::Live {},
        }
    }
}

impl From<core::WorkDirProblem> for WorkDirProblem {
    fn from(problem: core::WorkDirProblem) -> Self {
        use core::WorkDirProblem as W;
        match problem {
            W::NotEmpty => WorkDirProblem::NotEmpty {},
            W::Locked => WorkDirProblem::Locked {},
            W::Corrupt(detail) => WorkDirProblem::Corrupt { detail },
            W::UnsupportedVersion {
                file,
                found,
                supported,
            } => WorkDirProblem::UnsupportedVersion {
                file: file.to_owned(),
                found,
                supported,
            },
            W::KindMismatch { recorded, current } => WorkDirProblem::KindMismatch {
                recorded: recorded.into(),
                current: current.into(),
            },
            W::NotLiveRecording => WorkDirProblem::NotLiveRecording {},
            W::SourceMismatch => WorkDirProblem::SourceMismatch {},
            W::TracksMismatch => WorkDirProblem::TracksMismatch {},
            W::PlanChanged => WorkDirProblem::PlanChanged {},
            W::SelectionGone => WorkDirProblem::SelectionGone {},
            W::SourceUnverified => WorkDirProblem::SourceUnverified {},
        }
    }
}

impl From<core::HookKind> for HookKind {
    fn from(kind: core::HookKind) -> Self {
        match kind {
            core::HookKind::Playlist => HookKind::Playlist {},
            core::HookKind::Request => HookKind::Request {},
            core::HookKind::Key => HookKind::Key {},
            core::HookKind::Segment => HookKind::Segment {},
        }
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<SegmentRef>()?;
    module.add_class::<SelectFailure>()?;
    module.add_class::<Malformed>()?;
    module.add_class::<Encryption>()?;
    module.add_class::<SourceUnsupported>()?;
    module.add_class::<IntegrityFailure>()?;
    module.add_class::<Mp4Unsupported>()?;
    module.add_class::<HlsUnsupported>()?;
    module.add_class::<StallReason>()?;
    module.add_class::<JobType>()?;
    module.add_class::<WorkDirProblem>()?;
    module.add_class::<HookKind>()?;
    Ok(())
}
