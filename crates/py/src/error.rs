//! 异常：core 的 `Error` 翻译为按处理方式分组的 Python 异常，以及异常里各种原因的类型。
//!
//! 类表示出了什么问题；core 里说明出在哪里的包装（分片、key、失败后收拾）不另成异常类，抛的是包着的原因，位置与
//! 残留作为基类的属性。`str(异常)` 是 core 给出的完整说明，可否重试与要求的等待也按 core 对最外层的判断。

use hs_m3u8_core as core;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::{PyClass, PyClassInitializer};
use pyo3_stub_gen::derive::gen_stub_pyclass;

use crate::hooks::PyHookError;
use crate::path::PyPath;
use crate::record::{record, variants};
use crate::report::{
    HttpErrorKind, HttpFailure, Leftover, RefreshCause, Shape, StallCause, StreamKind,
};

record! {
    /// 出错的分片。
    pub struct SegmentRef {
        /// 第几条轨：0 为所选变体（来源本身是媒体播放列表时即它），1 为独立的音频
        pub track: usize,
        /// 媒体序号
        pub sequence: u64,
        /// 分片地址（播放列表里写的，可能带令牌；异常的说明里只显示到路径）
        pub url: String,
    }
}

variants! {
    /// 按选轨偏好选不出轨的原因。
    pub enum SelectFailure {
        /// 主播放列表里没有变体
        NoVariants {},
        /// `VariantChoice.Index` 的下标超出范围
        VariantIndexOutOfRange {
            /// 给的下标
            index: usize,
            /// 变体个数
            count: usize,
        },
        /// 所选变体的音频组里没有这个语言的音频
        AudioLanguageNotFound {
            /// 音频组
            group: String,
            /// 给的语言
            language: String,
            /// 组里有的语言
            available: Vec<String>,
        },
        /// `AudioChoice.Index` 的下标超出范围
        AudioIndexOutOfRange {
            /// 给的下标
            index: usize,
            /// 音频 rendition 的个数
            count: usize,
        },
        /// 指定的音频不在所选变体引用的音频组里
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
        /// 不是 UTF-8 文本；站点用了别的编码时在 `Hooks.on_playlist` 中转码
        NotUtf8 {},
        /// 同时含有主播放列表（EXT-X-STREAM-INF）与媒体播放列表（EXTINF）的标签
        Mixed {},
        /// 语法错误
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
            url: String,
        },
        /// 直播播放列表既没有 EXT-X-TARGETDURATION 也没有分片，无法确定刷新间隔
        NoTargetDuration {
            /// 播放列表的地址
            url: String,
        },
        /// EXT-X-KEY 用了本库解不了的加密
        Encrypted {
            /// 播放列表的地址
            url: String,
            /// EXT-X-KEY 所在的行，从 1 起
            line: usize,
            /// 加密方式
            encryption: Encryption,
        },
        /// 同一不连续段内 EXT-X-MAP 发生变化
        InitChangesWithinGroup {
            /// 第几条轨
            track: usize,
            /// 不连续段编号
            discontinuity: u64,
        },
        /// 各轨的不连续段不一致
        DiscontinuityMismatch {
            /// 与第 0 条轨不一致的轨
            track: usize,
            /// 第 0 条轨的各不连续段编号
            first: Vec<u64>,
            /// 这条轨的各不连续段编号
            found: Vec<u64>,
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
        /// 编码放不进 MP4（只支持 H.264、HEVC、AAC）
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
        /// 流的种类与第 0 组不同
        LayoutChanged {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
        },
        /// 编码参数与第 0 组不同
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
        /// 解码时间戳不递增：往回跳多为来源在时间戳重新开始处漏标了 EXT-X-DISCONTINUITY，重复多为相邻分片首尾
        /// 重复了一帧
        DtsNotIncreasing {
            /// 第几个不连续段组
            group: usize,
            /// 第几条轨
            track: usize,
            /// 流的种类
            kind: StreamKind,
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

/// 本库所有异常的基类。`str(异常)` 是完整说明（地址只显示到路径，不含查询串与凭据）。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = PyException, subclass, frozen, get_all)]
pub struct HsM3u8Error {
    /// 外部的临时故障，稍后重试可能成功
    pub retryable: bool,
    /// 服务器在 429/503 中要求（Retry-After）的最短等待，秒；没有要求时为 None
    pub retry_after: Option<u64>,
    /// 出错的分片；不是分片的失败时为 None
    pub segment: Option<SegmentRef>,
    /// 出错的 key 的地址（播放列表里写的）；不是取 key 的失败时为 None
    pub key_url: Option<String>,
    /// 失败后收拾也没做完而留下的东西；都做完时为空
    pub leftovers: Vec<Leftover>,
}

/// 改请求再试：参数不对、输出路径不能用、选轨偏好与来源不符、没开启直播录制、内容放不进所选的输出。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = HsM3u8Error, subclass, frozen)]
pub struct RequestError;

/// 参数不对。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = RequestError, frozen)]
pub struct InvalidInputError;

/// 输出已存在；要求覆盖（`OutputOptions.overwrite`）即可替换。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = RequestError, frozen, get_all)]
pub struct OutputExistsError {
    /// 输出路径
    pub path: PyPath,
}

/// 输出路径用不了，覆盖也不会替换（以免路径给错时删掉别人的文件），换一个路径：要输出 MP4 而那里是目录，要输出
/// HLS 而那里不是目录或目录里有不是本库写出的文件；或那里什么也没有，而最近的已存在的上级不是目录，建不出来。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = RequestError, frozen, get_all)]
pub struct OutputOccupiedError {
    /// 输出路径
    pub path: PyPath,
    /// 建不出来时，挡住它的上级（文件，或悬空的符号链接）；路径本身被占用时为 None
    pub ancestor: Option<PyPath>,
}

/// 选轨偏好与来源不符。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = RequestError, frozen, get_all)]
pub struct SelectError {
    /// 选不出的原因
    pub reason: SelectFailure,
}

/// 来源是直播，而请求没开启直播录制（`JobRequest.live` 为 None）。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = RequestError, frozen)]
pub struct LiveNotEnabledError;

/// 内容放不进 MP4；可只输出本地 HLS（`Target.Hls`）。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = RequestError, frozen, get_all)]
pub struct Mp4UnsupportedError {
    /// 放不进的原因
    pub reason: Mp4Unsupported,
}

/// 本地 HLS 表示不了这些内容；可只输出 MP4（`Target.Mp4`）。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = RequestError, frozen, get_all)]
pub struct HlsUnsupportedError {
    /// 表示不了的原因
    pub reason: HlsUnsupported,
}

/// 来源本身不行，同样的请求再试也不会成功。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = HsM3u8Error, subclass, frozen)]
pub struct SourceError;

/// 内容不是合法的播放列表；用了不支持的加密是 `SourceUnsupportedError`。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = SourceError, frozen, get_all)]
pub struct PlaylistError {
    /// 播放列表的地址（重定向之后的）
    pub url: String,
    /// 不合法的原因
    pub reason: Malformed,
}

/// 应为媒体播放列表的地址（变体、音频 rendition）返回了主播放列表。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = SourceError, frozen, get_all)]
pub struct NotMediaPlaylistError {
    /// 这个地址
    pub url: String,
}

/// 来源用到了不支持的特性。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = SourceError, frozen, get_all)]
pub struct SourceUnsupportedError {
    /// 哪个特性
    pub reason: SourceUnsupported,
}

/// 数据校验失败（解密后的内容不对）。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = SourceError, frozen, get_all)]
pub struct IntegrityError {
    /// 出错的分片或 init 段的地址
    pub url: String,
    /// 失败的原因
    pub reason: IntegrityFailure,
}

/// key 不是 16 字节。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = SourceError, frozen, get_all)]
pub struct KeyLengthError {
    /// key 的地址
    pub url: String,
    /// 实际的字节数
    pub length: usize,
}

/// 请求失败；在分片、key 的请求里出错时，`segment`、`key_url` 指出是哪个。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = HsM3u8Error, frozen, get_all)]
pub struct HttpError {
    /// 请求的地址（可能带令牌；异常的说明里只显示到路径）
    pub url: String,
    /// 失败的原因
    pub kind: HttpErrorKind,
}

/// 直播的一条轨长时间没有录到新分片，且不是直播正常结束；任务目录保留，可稍后续录。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = HsM3u8Error, frozen, get_all)]
pub struct LiveStalledError {
    /// 第几条轨
    pub track: usize,
    /// 停滞的原因
    pub reason: StallReason,
}

/// 读写文件失败。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = HsM3u8Error, frozen, get_all)]
pub struct FileError {
    /// 做的是什么（读取、删除……）
    pub action: String,
    /// 操作的路径
    pub path: PyPath,
}

/// 路径的根（Windows 的盘符、网络共享）读不到：盘不在、共享连不上或没有权限；接上后再试。根下的东西看不到不当作
/// 没有，不据此删东西、丢记录。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = HsM3u8Error, frozen, get_all)]
pub struct VolumeUnavailableError {
    /// 读不到的根
    pub root: PyPath,
}

/// 合并 MP4 失败：FFmpeg 初始化、读分片、写出或回读核对出错；内容放不进 MP4 是 `Mp4UnsupportedError`。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = HsM3u8Error, frozen)]
pub struct RemuxError;

/// 任务目录不能使用。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = HsM3u8Error, frozen, get_all)]
pub struct WorkDirError {
    /// 任务目录
    pub path: PyPath,
    /// 不能使用的原因
    pub problem: WorkDirProblem,
}

/// 直播没有录到可合并的分片。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = HsM3u8Error, frozen)]
pub struct NothingRecordedError;

/// 回调抛出了异常，任务失败、不重试；`__cause__` 为回调抛出的异常。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = HsM3u8Error, frozen, get_all)]
pub struct HookError {
    /// 哪个回调
    pub hook: HookKind,
}

/// 任务已取消。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = HsM3u8Error, frozen)]
pub struct JobCancelledError;

/// 把 core 的错误翻译为 Python 异常；翻译本身出错时（如内存不足）抛那个错误。
pub(crate) fn to_pyerr(py: Python<'_>, error: core::Error) -> PyErr {
    let base = HsM3u8Error {
        retryable: error.retryable(),
        retry_after: error.retry_after().map(|wait| wait.as_secs()),
        segment: None,
        key_url: None,
        leftovers: Vec::new(),
    };
    let message = error.to_string();
    exception(py, error, base, message).unwrap_or_else(|failure| failure)
}

/// 参数在 Python 侧就能判定不对时抛的 `InvalidInputError`。
pub(crate) fn invalid_input(py: Python<'_>, message: String) -> PyErr {
    let base = HsM3u8Error {
        retryable: false,
        retry_after: None,
        segment: None,
        key_url: None,
        leftovers: Vec::new(),
    };
    request_error(py, base, InvalidInputError)
        .and_then(|object| raised(object, message))
        .unwrap_or_else(|failure| failure)
}

/// `error` 对应的异常：剥掉说明位置的包装，记进 `base`，按里面的原因选类。
fn exception(
    py: Python<'_>,
    error: core::Error,
    mut base: HsM3u8Error,
    message: String,
) -> PyResult<PyErr> {
    use core::Error as E;
    let object = match error {
        E::Segment {
            track,
            sequence,
            url,
            cause,
        } => {
            base.segment = Some(SegmentRef {
                track,
                sequence,
                url: url.to_string(),
            });
            return exception(py, *cause, base, message);
        }
        E::Key { url, cause } => {
            base.key_url = Some(url.to_string());
            return exception(py, *cause, base, message);
        }
        E::Cleanup { failure, leftovers } => {
            base.leftovers = leftovers.into_iter().map(Leftover::from).collect();
            return exception(py, *failure, base, message);
        }
        E::Hook { hook, cause } => {
            let object = direct(py, base, HookError { hook: hook.into() })?;
            let error = raised(object, message)?;
            if let Some(PyHookError(original)) = cause.downcast_ref::<PyHookError>() {
                error.set_cause(py, Some(original.clone_ref(py)));
            }
            return Ok(error);
        }
        E::InvalidInput(_) => request_error(py, base, InvalidInputError)?,
        E::OutputExists(path) => request_error(py, base, OutputExistsError { path: path.into() })?,
        E::OutputOccupied { path, ancestor } => request_error(
            py,
            base,
            OutputOccupiedError {
                path: path.into(),
                ancestor: ancestor.map(PyPath::from),
            },
        )?,
        E::Select(failure) => request_error(
            py,
            base,
            SelectError {
                reason: failure.into(),
            },
        )?,
        E::Unsupported(unsupported) => {
            use core::Unsupported as U;
            let source = |reason| SourceUnsupportedError { reason };
            match unsupported {
                U::Live => request_error(py, base, LiveNotEnabledError)?,
                U::Mp4(reason) => request_error(
                    py,
                    base,
                    Mp4UnsupportedError {
                        reason: reason.into(),
                    },
                )?,
                U::Hls(reason) => request_error(
                    py,
                    base,
                    HlsUnsupportedError {
                        reason: reason.into(),
                    },
                )?,
                U::EmptyPlaylist(url) => {
                    let url = url.to_string();
                    source_error(py, base, source(SourceUnsupported::EmptyPlaylist { url }))?
                }
                U::NoTargetDuration(url) => {
                    let url = url.to_string();
                    let reason = SourceUnsupported::NoTargetDuration { url };
                    source_error(py, base, source(reason))?
                }
                U::Encryption { url, line, what } => {
                    let reason = SourceUnsupported::Encrypted {
                        url: url.to_string(),
                        line,
                        encryption: what.into(),
                    };
                    source_error(py, base, source(reason))?
                }
                U::InitChangesWithinGroup {
                    track,
                    discontinuity,
                } => {
                    let reason = SourceUnsupported::InitChangesWithinGroup {
                        track,
                        discontinuity,
                    };
                    source_error(py, base, source(reason))?
                }
                U::DiscontinuityMismatch {
                    track,
                    first,
                    found,
                } => {
                    let reason = SourceUnsupported::DiscontinuityMismatch {
                        track,
                        first,
                        found,
                    };
                    source_error(py, base, source(reason))?
                }
            }
        }
        E::Playlist { url, cause } => source_error(
            py,
            base,
            PlaylistError {
                url: url.to_string(),
                reason: (*cause).into(),
            },
        )?,
        E::NotMediaPlaylist { url } => source_error(
            py,
            base,
            NotMediaPlaylistError {
                url: url.to_string(),
            },
        )?,
        E::Integrity { url, kind } => source_error(
            py,
            base,
            IntegrityError {
                url: url.to_string(),
                reason: kind.into(),
            },
        )?,
        E::KeyLength { url, length } => source_error(
            py,
            base,
            KeyLengthError {
                url: url.to_string(),
                length,
            },
        )?,
        E::Http { url, kind, .. } => direct(
            py,
            base,
            HttpError {
                url: url.to_string(),
                kind: kind.into(),
            },
        )?,
        E::LiveStalled { track, cause } => direct(
            py,
            base,
            LiveStalledError {
                track,
                reason: cause.into(),
            },
        )?,
        E::Io { action, path, .. } => direct(
            py,
            base,
            FileError {
                action: action.to_owned(),
                path: path.into(),
            },
        )?,
        E::VolumeUnavailable { root, .. } => {
            direct(py, base, VolumeUnavailableError { root: root.into() })?
        }
        E::Remux(_) => direct(py, base, RemuxError)?,
        E::WorkDir { path, problem } => direct(
            py,
            base,
            WorkDirError {
                path: path.into(),
                problem: problem.into(),
            },
        )?,
        E::NothingRecorded => direct(py, base, NothingRecordedError)?,
        E::Cancelled => direct(py, base, JobCancelledError)?,
    };
    raised(object, message)
}

/// 直接继承基类的异常。
fn direct<'py, L>(py: Python<'py>, base: HsM3u8Error, leaf: L) -> PyResult<Bound<'py, PyAny>>
where
    L: PyClass<BaseType = HsM3u8Error>,
{
    let initializer = PyClassInitializer::from(base).add_subclass(leaf);
    Ok(Bound::new(py, initializer)?.into_any())
}

/// 改请求再试一组的异常。
fn request_error<'py, L>(py: Python<'py>, base: HsM3u8Error, leaf: L) -> PyResult<Bound<'py, PyAny>>
where
    L: PyClass<BaseType = RequestError>,
{
    let initializer = PyClassInitializer::from(base)
        .add_subclass(RequestError)
        .add_subclass(leaf);
    Ok(Bound::new(py, initializer)?.into_any())
}

/// 来源本身不行一组的异常。
fn source_error<'py, L>(py: Python<'py>, base: HsM3u8Error, leaf: L) -> PyResult<Bound<'py, PyAny>>
where
    L: PyClass<BaseType = SourceError>,
{
    let initializer = PyClassInitializer::from(base)
        .add_subclass(SourceError)
        .add_subclass(leaf);
    Ok(Bound::new(py, initializer)?.into_any())
}

/// 填上说明（`args`，`str(异常)` 即它），做成可以抛出的异常。
fn raised(object: Bound<'_, PyAny>, message: String) -> PyResult<PyErr> {
    object.setattr("args", (message,))?;
    Ok(PyErr::from_value(object))
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
                available,
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
            U::DtsNotIncreasing { group, track, kind } => Mp4Unsupported::DtsNotIncreasing {
                group,
                track,
                kind: kind.into(),
            },
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
    module.add_class::<HsM3u8Error>()?;
    module.add_class::<RequestError>()?;
    module.add_class::<InvalidInputError>()?;
    module.add_class::<OutputExistsError>()?;
    module.add_class::<OutputOccupiedError>()?;
    module.add_class::<SelectError>()?;
    module.add_class::<LiveNotEnabledError>()?;
    module.add_class::<Mp4UnsupportedError>()?;
    module.add_class::<HlsUnsupportedError>()?;
    module.add_class::<SourceError>()?;
    module.add_class::<PlaylistError>()?;
    module.add_class::<NotMediaPlaylistError>()?;
    module.add_class::<SourceUnsupportedError>()?;
    module.add_class::<IntegrityError>()?;
    module.add_class::<KeyLengthError>()?;
    module.add_class::<HttpError>()?;
    module.add_class::<LiveStalledError>()?;
    module.add_class::<FileError>()?;
    module.add_class::<VolumeUnavailableError>()?;
    module.add_class::<RemuxError>()?;
    module.add_class::<WorkDirError>()?;
    module.add_class::<NothingRecordedError>()?;
    module.add_class::<HookError>()?;
    module.add_class::<JobCancelledError>()?;
    Ok(())
}
