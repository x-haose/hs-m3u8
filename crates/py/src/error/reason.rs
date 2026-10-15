//! 异常里的原因与位置：出错的分片、选不出轨、播放列表不合法等的类型，及由 core 的对应类型转换。

use hs_m3u8_core as core;
use pyo3::prelude::*;

use crate::convert::{Address, Seq};
use crate::record::{record, variants};
use crate::report::{FetchFailure, RefreshCause, StallCause};

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
        /// 主播放列表里没有与 `VariantChoice.Matching` 身份相同的变体
        VariantNotFound {},
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
        /// 所选变体的音频组里没有与 `AudioChoice.Matching` 身份相同的音频（变体没有引用音频组时也是）
        AudioNotFound {},
        /// 按下标指定（`AudioChoice.Index`）的音频不在所选变体引用的音频组里
        #[pyo3(constructor = (*, index, group))]
        NotVariantAudio {
            /// 音频的下标（见 `AudioInfo.index`）
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
        /// EXT-X-I-FRAMES-ONLY：只有关键帧的播放列表，供快进时预览，不是正常播放的内容；来源应为主播放列表或正常的
        /// 媒体播放列表
        #[pyo3(constructor = (*, url, line))]
        IFramesOnly {
            /// 播放列表的地址
            url: Address,
            /// EXT-X-I-FRAMES-ONLY 所在的行，从 1 起
            line: usize,
        },
        /// EXT-X-SKIP：增量更新略去了前面的分片。服务器只在地址里请求了 `_HLS_skip` 时才这样给，去掉这个参数即可
        #[pyo3(constructor = (*, url, line))]
        DeltaUpdate {
            /// 播放列表的地址
            url: Address,
            /// EXT-X-SKIP 所在的行，从 1 起
            line: usize,
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
        /// 点播没有一个不连续段合并得了：每段都有轨只有来源标了 EXT-X-GAP 的分片（合并要各轨都有内容），如某条轨全部
        /// 标了 GAP
        NoMergeableGroup {
            /// 第一段里只有 GAP 分片的一条轨的播放列表的地址
            url: Address,
        },
        /// fMP4 的 moof 用了绝对位置（tfhd 的 base_data_offset）：RFC 8216 3.3 要求按 moof 相对寻址，分片单独存放、
        /// 接起来合并时绝对位置都对不上
        #[pyo3(constructor = (*, url, at))]
        Fmp4BaseDataOffset {
            /// 分片或 init 段的地址
            url: Address,
            /// 这个 moof 的起点，字节
            at: u64,
        },
        /// 要取的分片或 init 段加密而所用的 EXT-X-KEY 没写 URI（违反 RFC 8216），取不到 key；已知 key 时用
        /// `JobRequest.key`。下载与录制收到播放列表即核对，不等取分片：续传只差合并时也要给
        KeyWithoutUri {
            /// 播放列表的地址
            url: Address,
        },
        /// init 段（EXT-X-MAP）是 TS 的节目表（PAT/PMT），本库只支持 fMP4 的 init 段
        TsInit {
            /// init 段的地址
            url: Address,
        },
    }
}

variants! {
    /// 数据校验失败的原因。
    pub enum IntegrityFailure {
        /// 解密后填充不合法：多为 key 不对（IV 不对不会这样），或数据损坏；解出的是截断的媒体数据时报 `IncompleteError`
        Padding {},
        /// 密文长度不是 16 的整数倍，整块部分解出的不是截断的媒体数据（如源站返回的是错误页面、完整的内容后面多了
        /// 字节）；是的报 `IncompleteError`
        CipherLength {
            /// 密文字节数
            length: usize,
        },
        /// 字节范围请求返回的长度不对
        #[pyo3(constructor = (*, expected, found))]
        RangeLength {
            /// 应有的字节数
            expected: u64,
            /// 读到的字节数：超出 `expected` 时读到即停，响应体可能更长
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
        /// fMP4 顶层 box 的长度字段小于 box 头的长度（8 字节，64 位长度时 16 字节）；没写完的报 `IncompleteError`
        #[pyo3(constructor = (*, at, size))]
        Fmp4BoxTooSmall {
            /// 这个 box 的起点，字节
            at: u64,
            /// 长度字段的值，字节
            size: u64,
        },
        /// fMP4 末尾的字节不是完整的 box：不足 8 个凑不成 box 头，或类型认不出而长度写 0 或超出了数据。多为多出的字节
        /// （如 CDN 加了换行或填充、`Hooks.on_segment` 没去干净）；trun 声明的样本数据也超出了数据时报
        /// `IncompleteError`
        #[pyo3(constructor = (*, at, len))]
        Fmp4TrailingBytes {
            /// 多出的字节的起点
            at: u64,
            /// 多出的字节数
            len: u64,
        },
        /// fMP4 顶层 box（moof 或 moov）的内容不合法：子 box 超出、字段不全，或算出的样本位置溢出
        Fmp4Malformed {
            /// 这个 box 的起点，字节
            at: u64,
        },
        /// fMP4 最后一个顶层 box 的长度写为 0（到数据末尾），实际长度超过 4 GiB，存盘前改写不成实际长度
        #[pyo3(constructor = (*, at, size))]
        Fmp4OpenBoxTooLarge {
            /// 这个 box 的起点，字节
            at: u64,
            /// 这个 box 的实际字节数
            size: u64,
        },
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
        /// 刷新播放列表（或为新分片拉 init 段）的请求，在察觉停滞之后又过了一个目标时长仍未返回
        RefreshPending {},
        /// 停滞开始之后播放列表仍列出了新分片，但一个都没有下载成功
        Unrecordable {
            /// 最近一次取不到的分片或 init 段
            cause: FetchFailure,
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
        /// 来源是主播放列表，选轨偏好与记录的不同：换了选轨即是另一个任务，换一个任务目录，或放弃任务
        /// （`Engine.discard`）；选的仍是同一内容（如重新探测后再选）时用 `JobRequest.reselect`
        PreferenceChanged {},
        /// 来源的轨道（所选变体、有无独立音频）与记录的不同；地址的种类与记录的不同（原来是主播放列表、这次是某一档
        /// 的媒体播放列表，或反过来）也是这样，换回同类的地址，或重新选轨（`JobRequest.reselect`，此时只在有无独立音频
        /// 不同时报）
        TracksMismatch {},
        /// 点播：播放列表的分片与记录的不一致（多为换了另一档或来源重新生成了分片），已下载的不能沿用，重新选轨也不行；
        /// 换一个任务目录，或放弃任务（`Engine.discard`）
        PlanChanged {},
        /// 记录的变体或音频已不在主播放列表中：身份相同的已不在，或按稳定 ID 找到的那条分辨率、编码种类与记录的不同。选
        /// 的仍是同一内容时，重新探测后用新得到的 `choice()`（或别的偏好）并重新选轨（`JobRequest.reselect`，核对过内容
        /// 才沿用目录）；已录的直播可用 `Engine.merge_recorded` 合并，或换一个任务目录重新开始
        SelectionGone {},
        /// 核对不上这次取到的与已下载的是同一份内容：点播为核对的组里有已完成的分片重新下载后内容不同，或都已取不到（见
        /// `Source.url`），多为另一个视频、同一视频的另一档或同一地址换了内容，同一地址也核对不上的多为来源每次输出的字
        /// 节不同（边转码边出），只能重新下载；直播为地址或重新选出的轨（`JobRequest.reselect`）与记录的不同，且接不上
        /// 之前录到的（有轨内容不同，或没有一条轨核对一致，或重新选出的轨这次核对不了，见 `LiveOptions`）。记录不变：可
        /// 用原来的地址与选轨续传、续录，直播还可用 `Engine.merge_recorded` 合并已录的部分；或换一个任务目录、放弃任务
        SourceUnverified {},
        /// 直播续录：之前录到的接不上（要另起会话），而这条轨这次不录（已录满 max_duration，或这次没有内容）：另起的
        /// 会话里它没有分片，各轨都有的组才合并得了，录到的合并不进去。可调大 max_duration 后续录、用
        /// `Engine.merge_recorded` 合并已录的部分，或换一个任务目录重新录
        Unaligned {
            /// 第几条轨
            track: usize,
        },
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
        /// `Hooks.on_init`
        Init {},
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
            E::VariantNotFound => SelectFailure::VariantNotFound {},
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
            E::AudioNotFound => SelectFailure::AudioNotFound {},
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

impl From<core::IntegrityFailure> for IntegrityFailure {
    fn from(integrity: core::IntegrityFailure) -> Self {
        use core::IntegrityFailure as I;
        match integrity {
            I::Padding => IntegrityFailure::Padding {},
            I::CipherLength(length) => IntegrityFailure::CipherLength { length },
            I::RangeLength { expected, found } => IntegrityFailure::RangeLength { expected, found },
            I::UnrecognizedSegment(head) => IntegrityFailure::UnrecognizedSegment { head },
            I::NotFmp4(head) => IntegrityFailure::NotFmp4 { head },
            I::Fmp4BoxTooSmall { at, size } => IntegrityFailure::Fmp4BoxTooSmall { at, size },
            I::Fmp4TrailingBytes { at, len } => IntegrityFailure::Fmp4TrailingBytes { at, len },
            I::Fmp4Malformed { at } => IntegrityFailure::Fmp4Malformed { at },
            I::Fmp4OpenBoxTooLarge { at, size } => {
                IntegrityFailure::Fmp4OpenBoxTooLarge { at, size }
            }
        }
    }
}

impl From<core::StallReason> for StallReason {
    fn from(error: core::StallReason) -> Self {
        use core::StallReason as S;
        match error {
            S::RefreshFailed(cause) => StallReason::RefreshFailed {
                cause: cause.into(),
            },
            S::RefreshPending => StallReason::RefreshPending {},
            S::Unrecordable(failure) => StallReason::Unrecordable {
                cause: failure.into(),
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
            W::PreferenceChanged => WorkDirProblem::PreferenceChanged {},
            W::TracksMismatch => WorkDirProblem::TracksMismatch {},
            W::PlanChanged => WorkDirProblem::PlanChanged {},
            W::SelectionGone => WorkDirProblem::SelectionGone {},
            W::SourceUnverified => WorkDirProblem::SourceUnverified {},
            W::Unaligned { track } => WorkDirProblem::Unaligned { track },
        }
    }
}

impl From<core::HookKind> for HookKind {
    fn from(kind: core::HookKind) -> Self {
        match kind {
            core::HookKind::Playlist => HookKind::Playlist {},
            core::HookKind::Request => HookKind::Request {},
            core::HookKind::Key => HookKind::Key {},
            core::HookKind::Init => HookKind::Init {},
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
    module.add_class::<StallReason>()?;
    module.add_class::<JobType>()?;
    module.add_class::<WorkDirProblem>()?;
    module.add_class::<HookKind>()?;
    Ok(())
}
