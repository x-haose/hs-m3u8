//! 异常：core 的 `Error` 翻译为按处理方式分组的 Python 异常。异常里的原因与位置的类型见 [`reason`]。
//!
//! 类表示出了什么问题；core 里说明出在哪里的包装（分片、key、失败后收拾）不另成异常类，抛的是包着的原因，位置与
//! 残留作为基类的属性。`str(异常)` 是 core 给出的完整说明，可否重试与要求的等待也按 core 对最外层的判断。

use std::io;
use std::path::Path;

use hs_m3u8_core as core;
use pyo3::exceptions::{PyException, PyOSError};
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use pyo3::{PyClass, PyClassInitializer, PyTraverseError, PyVisit};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};

mod reason;

use self::reason::{
    HlsUnsupported, HookKind, IntegrityFailure, Malformed, Mp4Unsupported, SegmentRef,
    SelectFailure, SourceUnsupported, StallReason, WorkDirProblem,
};
use crate::convert::{Address, PyPath, Seq};
use crate::hooks::PyHookError;
use crate::record::{field_names, partial};
use crate::report::{HttpErrorKind, Leftover};

/// 本库所有异常的基类。`str(异常)` 是完整说明（地址只显示到路径，不含查询串与凭据）。
///
/// 各异常类都能构造（如在测试里模拟失败）：说明按位置给出，其余按关键字，本类的属性可省略；Python 里继承它们时，
/// 子类须保持这个构造签名，构造与 pickle 都按它调用。可以 pickle 与 copy，只保证同一版本之间。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", extends = PyException, subclass, frozen, get_all)]
pub struct HsM3u8Error {
    /// 外部的临时故障，稍后重试可能成功
    pub retryable: bool,
    /// 服务器在 429/503 中要求（Retry-After）的最短等待，秒；没有要求时为 None
    pub retry_after: Option<u64>,
    /// 出错的分片；不是分片的失败时为 None
    pub segment: Option<SegmentRef>,
    /// 取 key 的请求或回调出错时，key 的地址（播放列表里写的）；其余为 None（key 长度不对时地址在
    /// `KeyLengthError.url`）
    pub key_url: Option<Address>,
    /// 失败后收拾也没做完而留下的东西；都做完时为空
    pub leftovers: Seq<Leftover>,
}

#[gen_stub_pymethods]
#[pymethods]
impl HsM3u8Error {
    /// `message` 为说明（`str(异常)`）；其余各参数即同名属性，`leftovers` 为 None 时为空。
    #[new]
    #[pyo3(signature = (
        message,
        *,
        retryable = false,
        retry_after = None,
        segment = None,
        key_url = None,
        leftovers = None,
    ))]
    fn new(
        message: String,
        retryable: bool,
        retry_after: Option<u64>,
        segment: Option<SegmentRef>,
        key_url: Option<Address>,
        leftovers: Option<Seq<Leftover>>,
    ) -> Self {
        // 说明由 __init__ 放进 args
        let _ = message;
        HsM3u8Error {
            retryable,
            retry_after,
            segment,
            key_url,
            leftovers: leftovers.unwrap_or_default(),
        }
    }

    /// 同构造函数：把说明放进 `args`。
    #[pyo3(signature = (
        message,
        *,
        retryable = false,
        retry_after = None,
        segment = None,
        key_url = None,
        leftovers = None,
    ))]
    fn __init__(
        slf: &Bound<'_, Self>,
        message: String,
        retryable: bool,
        retry_after: Option<u64>,
        segment: Option<SegmentRef>,
        key_url: Option<Address>,
        leftovers: Option<Seq<Leftover>>,
    ) -> PyResult<()> {
        // 其余参数已由构造函数用过；收同样的参数，签名才与构造函数一致
        let _ = (retryable, retry_after, segment, key_url, leftovers);
        slf.setattr("args", (message,))
    }

    #[gen_stub(skip)]
    fn __reduce__<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyTuple>> {
        reduce(slf.as_any(), &[])
    }
}

impl HsM3u8Error {
    /// 不在分片与 key 里、没有残留的失败。
    fn plain(retryable: bool, retry_after: Option<u64>) -> Self {
        HsM3u8Error {
            retryable,
            retry_after,
            segment: None,
            key_url: None,
            leftovers: Seq::default(),
        }
    }
}

/// 异常的 pickle：`(partial(类, 各属性), args, __dict__)`，`__dict__` 里是 `add_note` 等加上的东西；`fields` 为
/// 基类之外的属性。
fn reduce<'py>(object: &Bound<'py, PyAny>, fields: &[&str]) -> PyResult<Bound<'py, PyTuple>> {
    let base = field_names!(HsM3u8Error {
        retryable,
        retry_after,
        segment,
        key_url,
        leftovers,
    });
    let fields: Vec<&str> = fields.iter().chain(base).copied().collect();
    let rebuild = partial(object, &fields)?;
    let args = object.getattr("args")?;
    let state = object.getattr("__dict__")?;
    PyTuple::new(object.py(), [rebuild, args, state])
}

/// 异常的直接上级：建立异常时依次初始化到它为止的各层。
trait Parent: PyClass {
    /// 从基类 `base` 起，经本类及以上各层，到它的子类 `leaf` 的初始化。
    fn chain<L: PyClass<BaseType = Self>>(base: HsM3u8Error, leaf: L) -> PyClassInitializer<L>;
}

impl Parent for HsM3u8Error {
    fn chain<L: PyClass<BaseType = Self>>(base: HsM3u8Error, leaf: L) -> PyClassInitializer<L> {
        PyClassInitializer::from(base).add_subclass(leaf)
    }
}

impl Parent for RequestError {
    fn chain<L: PyClass<BaseType = Self>>(base: HsM3u8Error, leaf: L) -> PyClassInitializer<L> {
        HsM3u8Error::chain(base, RequestError {}).add_subclass(leaf)
    }
}

impl Parent for SourceError {
    fn chain<L: PyClass<BaseType = Self>>(base: HsM3u8Error, leaf: L) -> PyClassInitializer<L> {
        HsM3u8Error::chain(base, SourceError {}).add_subclass(leaf)
    }
}

/// `HsM3u8Error` 的子类：属性只读，构造函数为 `(message, *, 自己的属性, 基类的属性可省略)`。`__init__` 与构造函数
/// 签名相同：Python 调用类时两者收同样的参数，类型检查器与 stubtest 都按此核对。
macro_rules! exception {
    (
        $(#[$meta:meta])*
        pub struct $name:ident extends $parent:ident $(, $subclass:ident)? {
            $(
                $(#[$field_meta:meta])*
                pub $field:ident : $ty:ty
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[gen_stub_pyclass]
        #[pyclass(module = "hs_m3u8._native", extends = $parent, $($subclass,)? frozen, get_all)]
        pub struct $name { $( $(#[$field_meta])* pub $field: $ty ),* }

        #[gen_stub_pymethods]
        #[pymethods]
        impl $name {
            /// `message` 为说明（`str(异常)`）；其余各参数即同名属性，基类 `HsM3u8Error` 的可省略，`leftovers` 为
            /// None 时为空。
            #[new]
            #[pyo3(signature = (
                message,
                *,
                $($field,)*
                retryable = false,
                retry_after = None,
                segment = None,
                key_url = None,
                leftovers = None,
            ))]
            fn new(
                message: String,
                $($field: $ty,)*
                retryable: bool,
                retry_after: Option<u64>,
                segment: Option<SegmentRef>,
                key_url: Option<Address>,
                leftovers: Option<Seq<Leftover>>,
            ) -> PyClassInitializer<Self> {
                let base = HsM3u8Error::new(message, retryable, retry_after, segment, key_url, leftovers);
                <$parent as Parent>::chain(base, $name { $($field),* })
            }

            /// 同构造函数：把说明放进 `args`。
            #[pyo3(signature = (
                message,
                *,
                $($field,)*
                retryable = false,
                retry_after = None,
                segment = None,
                key_url = None,
                leftovers = None,
            ))]
            fn __init__(
                slf: &Bound<'_, Self>,
                message: String,
                $($field: $ty,)*
                retryable: bool,
                retry_after: Option<u64>,
                segment: Option<SegmentRef>,
                key_url: Option<Address>,
                leftovers: Option<Seq<Leftover>>,
            ) -> PyResult<()> {
                // 其余参数已由构造函数用过；收同样的参数，签名才与构造函数一致
                let _ = ($($field,)* retryable, retry_after, segment, key_url, leftovers);
                slf.setattr("args", (message,))
            }

            #[gen_stub(skip)]
            fn __reduce__<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyTuple>> {
                reduce(slf.as_any(), &[$(stringify!($field)),*])
            }
        }
    };
}

exception! {
    /// 改请求再试：参数不对、输出路径不能用、选轨偏好与来源不符、没开启直播录制、内容放不进所选的输出。
    pub struct RequestError extends HsM3u8Error, subclass {}
}

exception! {
    /// 参数不对。
    pub struct InvalidInputError extends RequestError {}
}

exception! {
    /// 输出已存在；要求覆盖（`OutputOptions.overwrite`）即可替换。
    pub struct OutputExistsError extends RequestError {
        /// 输出路径
        pub path: PyPath,
    }
}

exception! {
    /// 输出路径用不了，覆盖也不会替换（以免路径给错时删掉别人的文件），换一个路径：要输出 MP4 而那里是目录，要输出
    /// HLS 而那里不是目录或目录里有不是本库写出的文件；或那里什么也没有，而最近的已存在的上级不是目录，建不出来。
    pub struct OutputOccupiedError extends RequestError {
        /// 输出路径
        pub path: PyPath,
        /// 建不出来时，挡住它的上级（文件，或悬空的符号链接）；路径本身被占用时为 None
        pub ancestor: Option<PyPath>,
    }
}

exception! {
    /// 选轨偏好与来源不符。
    pub struct SelectError extends RequestError {
        /// 选不出的原因
        pub reason: SelectFailure,
    }
}

exception! {
    /// 来源是直播，而请求没开启直播录制（`JobRequest.live` 为 None）。
    pub struct LiveNotEnabledError extends RequestError {}
}

exception! {
    /// 内容放不进 MP4；可只输出本地 HLS（`Target.Hls`）。编码、各组的流种类与参数在下到（直播为录到）各组各轨的
    /// 第一个分片时就核对，组内的变化要合并时读到才报。
    pub struct Mp4UnsupportedError extends RequestError {
        /// 放不进的原因
        pub reason: Mp4Unsupported,
    }
}

exception! {
    /// 本地 HLS 表示不了这些内容；可只输出 MP4（`Target.Mp4`）。
    pub struct HlsUnsupportedError extends RequestError {
        /// 表示不了的原因
        pub reason: HlsUnsupported,
    }
}

exception! {
    /// 来源本身不行，同样的请求再试也不会成功。
    pub struct SourceError extends HsM3u8Error, subclass {}
}

exception! {
    /// 内容不是合法的播放列表；用了不支持的加密是 `SourceUnsupportedError`。
    pub struct PlaylistError extends SourceError {
        /// 播放列表的地址（重定向之后的）
        pub url: Address,
        /// 不合法的原因
        pub reason: Malformed,
    }
}

exception! {
    /// 应为媒体播放列表的地址（变体、音频 rendition）返回了主播放列表。
    pub struct NotMediaPlaylistError extends SourceError {
        /// 这个地址
        pub url: Address,
    }
}

exception! {
    /// 来源用到了不支持的特性。
    pub struct SourceUnsupportedError extends SourceError {
        /// 哪个特性
        pub reason: SourceUnsupported,
    }
}

exception! {
    /// 数据校验失败（解密后的内容不对）。
    pub struct IntegrityError extends SourceError {
        /// 出错的分片或 init 段的地址
        pub url: Address,
        /// 失败的原因
        pub reason: IntegrityFailure,
    }
}

exception! {
    /// 拉到的 key 不是 16 字节（`Hooks.on_key` 返回的不对时是 `HookError`）。
    pub struct KeyLengthError extends SourceError {
        /// key 的地址
        pub url: Address,
        /// 实际的字节数
        pub length: usize,
    }
}

exception! {
    /// 请求失败；在分片、key 的请求里出错时，`segment`、`key_url` 指出是哪个。
    pub struct HttpError extends HsM3u8Error {
        /// 请求的地址（可能带令牌；异常的说明里只显示到路径）
        pub url: Address,
        /// 失败的原因
        pub kind: HttpErrorKind,
    }
}

exception! {
    /// 直播的一条轨长时间没有录到新分片，且不是直播正常结束；任务目录保留，可稍后续录。
    pub struct LiveStalledError extends HsM3u8Error {
        /// 第几条轨
        pub track: usize,
        /// 停滞的原因
        pub reason: StallReason,
    }
}

exception! {
    /// 读写文件失败；`__cause__` 为系统报的 `OSError`（如 `PermissionError`）。
    pub struct FileError extends HsM3u8Error {
        /// 做的是什么（读取、删除……）
        pub action: String,
        /// 操作的路径
        pub path: PyPath,
        /// 系统的错误码，同 `__cause__.errno`（磁盘满等按它区分，见 `errno` 模块）；不是系统报的错误时为 None。
        /// pickle 后仍在，`__cause__` 则不随 pickle 保存
        pub errno: Option<i32>,
    }
}

exception! {
    /// 路径的根（Windows 的盘符、网络共享）读不到：盘不在、共享连不上或没有权限；接上后再试。根下的东西看不到不当作
    /// 没有，不据此删东西、丢记录。`__cause__` 为系统报的 `OSError`。
    pub struct VolumeUnavailableError extends HsM3u8Error {
        /// 读不到的根
        pub root: PyPath,
        /// 系统的错误码，同 `FileError.errno`
        pub errno: Option<i32>,
    }
}

exception! {
    /// 读不出操作系统的代理设置（`Proxy.System()` 时）：系统的接口失败。设置的值用不了（写错、用了 PAC 等）是
    /// `InvalidInputError`。可改用 `Proxy.Direct()` 或 `Proxy.Url(...)`。
    pub struct SystemProxyError extends HsM3u8Error {}
}

exception! {
    /// 合并 MP4 或下载中核对能否合并时失败：FFmpeg 初始化、读分片、写出或回读核对出错；内容放不进 MP4 是
    /// `Mp4UnsupportedError`。
    pub struct RemuxError extends HsM3u8Error {}
}

exception! {
    /// 任务目录不能使用。
    pub struct WorkDirError extends HsM3u8Error {
        /// 任务目录
        pub path: PyPath,
        /// 不能使用的原因
        pub problem: WorkDirProblem,
    }
}

exception! {
    /// 直播没有录到可合并的分片。
    pub struct NothingRecordedError extends HsM3u8Error {}
}

exception! {
    /// 回调出错，任务失败、不重试。回调抛出了异常、或返回值的类型不对时，`__cause__` 为那个异常（类型不对时是
    /// `TypeError`）；返回的地址不是 http/https、请求头不合法、或 key 不是 16 字节时为 None，原因在说明里。
    pub struct HookError extends HsM3u8Error {
        /// 哪个回调
        pub hook: HookKind,
    }
}

exception! {
    /// 任务已取消。
    pub struct JobCancelledError extends HsM3u8Error {}
}

/// 向垃圾回收器报出 `error` 里（含包着的原因）回调抛出的异常。
pub(crate) fn traverse(error: &core::Error, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
    match error {
        core::Error::Hook { cause, .. } => match cause.downcast_ref::<PyHookError>() {
            Some(PyHookError(exception)) => visit.call(exception),
            None => Ok(()),
        },
        core::Error::Segment { cause, .. } | core::Error::Key { cause, .. } => {
            traverse(cause, visit)
        }
        core::Error::Cleanup { failure, .. } => traverse(failure, visit),
        _ => Ok(()),
    }
}

/// 把 core 的错误翻译为 Python 异常；翻译本身出错时（如内存不足）抛那个错误。
pub(crate) fn to_pyerr(py: Python<'_>, error: core::Error) -> PyErr {
    let base = HsM3u8Error::plain(
        error.retryable(),
        error.retry_after().map(|wait| wait.as_secs()),
    );
    let message = error.to_string();
    exception(py, error, base, message).unwrap_or_else(|failure| failure)
}

/// 参数在 Python 侧就能判定不对时抛的 `InvalidInputError`：与 core 判定的一样翻译，说明的写法也一样。
pub(crate) fn invalid_input(py: Python<'_>, message: String) -> PyErr {
    to_pyerr(py, core::Error::InvalidInput(message))
}

/// `error` 对应的异常：剥掉说明位置的包装，记进 `base`，按里面的原因选类。
fn exception(
    py: Python<'_>,
    error: core::Error,
    mut base: HsM3u8Error,
    message: String,
) -> PyResult<PyErr> {
    use core::Error as E;
    use core::Unsupported as U;
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
                url: Address(*url),
            });
            return exception(py, *cause, base, message);
        }
        E::Key { url, cause } => {
            base.key_url = Some(Address(*url));
            return exception(py, *cause, base, message);
        }
        E::Cleanup { failure, leftovers } => {
            base.leftovers = leftovers.into_iter().map(Leftover::from).collect();
            return exception(py, *failure, base, message);
        }
        E::Hook { hook, cause } => {
            let object = instance(
                py,
                HsM3u8Error::chain(base, HookError { hook: hook.into() }),
            )?;
            let error = raised(object, message)?;
            if let Some(PyHookError(original)) = cause.downcast_ref::<PyHookError>() {
                error.set_cause(
                    py,
                    Some(PyErr::from_value(original.bind(py).clone().into_any())),
                );
            }
            return Ok(error);
        }
        E::InvalidInput(_) => instance(py, RequestError::chain(base, InvalidInputError {}))?,
        E::OutputExists(path) => instance(
            py,
            RequestError::chain(base, OutputExistsError { path: path.into() }),
        )?,
        E::OutputOccupied { path, ancestor } => instance(
            py,
            RequestError::chain(
                base,
                OutputOccupiedError {
                    path: path.into(),
                    ancestor: ancestor.map(PyPath::from),
                },
            ),
        )?,
        E::Select(failure) => instance(
            py,
            RequestError::chain(
                base,
                SelectError {
                    reason: failure.into(),
                },
            ),
        )?,
        E::LiveNotEnabled => instance(py, RequestError::chain(base, LiveNotEnabledError {}))?,
        E::Unsupported(U::Mp4(reason)) => instance(
            py,
            RequestError::chain(
                base,
                Mp4UnsupportedError {
                    reason: reason.into(),
                },
            ),
        )?,
        E::Unsupported(U::Hls(reason)) => instance(
            py,
            RequestError::chain(
                base,
                HlsUnsupportedError {
                    reason: reason.into(),
                },
            ),
        )?,
        E::Unsupported(U::EmptyPlaylist(url)) => unsupported_source(
            py,
            base,
            SourceUnsupported::EmptyPlaylist { url: Address(*url) },
        )?,
        E::Unsupported(U::NoTargetDuration(url)) => unsupported_source(
            py,
            base,
            SourceUnsupported::NoTargetDuration { url: Address(*url) },
        )?,
        E::Unsupported(U::Encryption { url, line, what }) => unsupported_source(
            py,
            base,
            SourceUnsupported::Encrypted {
                url: Address(*url),
                line,
                encryption: what.into(),
            },
        )?,
        E::Unsupported(U::InitChangesWithinGroup {
            track,
            discontinuity,
        }) => unsupported_source(
            py,
            base,
            SourceUnsupported::InitChangesWithinGroup {
                track,
                discontinuity,
            },
        )?,
        E::Unsupported(U::DiscontinuityMismatch {
            track,
            first,
            found,
        }) => unsupported_source(
            py,
            base,
            SourceUnsupported::DiscontinuityMismatch {
                track,
                first: Seq(first),
                found: Seq(found),
            },
        )?,
        E::Playlist { url, cause } => instance(
            py,
            SourceError::chain(
                base,
                PlaylistError {
                    url: Address(*url),
                    reason: (*cause).into(),
                },
            ),
        )?,
        E::NotMediaPlaylist { url } => instance(
            py,
            SourceError::chain(base, NotMediaPlaylistError { url: Address(*url) }),
        )?,
        E::Integrity { url, kind } => instance(
            py,
            SourceError::chain(
                base,
                IntegrityError {
                    url: Address(*url),
                    reason: kind.into(),
                },
            ),
        )?,
        E::KeyLength { url, length } => instance(
            py,
            SourceError::chain(
                base,
                KeyLengthError {
                    url: Address(*url),
                    length,
                },
            ),
        )?,
        E::Http { url, kind, .. } => instance(
            py,
            HsM3u8Error::chain(
                base,
                HttpError {
                    url: Address(*url),
                    kind: kind.into(),
                },
            ),
        )?,
        E::LiveStalled { track, cause } => instance(
            py,
            HsM3u8Error::chain(
                base,
                LiveStalledError {
                    track,
                    reason: cause.into(),
                },
            ),
        )?,
        E::Io {
            action,
            path,
            cause,
        } => {
            let os_error = os_error(cause, &path);
            let leaf = FileError {
                action: action.to_owned(),
                path: path.into(),
                errno: errno_of(py, &os_error)?,
            };
            let error = raised(instance(py, HsM3u8Error::chain(base, leaf))?, message)?;
            error.set_cause(py, Some(os_error));
            return Ok(error);
        }
        E::VolumeUnavailable { root, cause } => {
            let os_error = os_error(cause, &root);
            let leaf = VolumeUnavailableError {
                root: root.into(),
                errno: errno_of(py, &os_error)?,
            };
            let error = raised(instance(py, HsM3u8Error::chain(base, leaf))?, message)?;
            error.set_cause(py, Some(os_error));
            return Ok(error);
        }
        E::SystemProxy(_) => instance(py, HsM3u8Error::chain(base, SystemProxyError {}))?,
        E::Remux(_) => instance(py, HsM3u8Error::chain(base, RemuxError {}))?,
        E::WorkDir { path, problem } => instance(
            py,
            HsM3u8Error::chain(
                base,
                WorkDirError {
                    path: path.into(),
                    problem: problem.into(),
                },
            ),
        )?,
        E::NothingRecorded => instance(py, HsM3u8Error::chain(base, NothingRecordedError {}))?,
        E::Cancelled => instance(py, HsM3u8Error::chain(base, JobCancelledError {}))?,
    };
    raised(object, message)
}

/// `cause` 对应的 `OSError`，`filename` 为 `path`：有错误码时 Python 按它选子类（如 `PermissionError`），调用方可按
/// `errno` 区分磁盘满等；没有错误码时按种类选，同 PyO3 的转换。
fn os_error(cause: io::Error, path: &Path) -> PyErr {
    let Some(code) = cause.raw_os_error() else {
        return PyErr::from(cause);
    };
    // 系统给的说明；Rust 在后面加的「(os error N)」与 Python 显示的错误码重复
    let text = cause.to_string();
    let strerror = text
        .strip_suffix(&format!(" (os error {code})"))
        .unwrap_or(&text)
        .to_owned();
    let filename = path.as_os_str().to_os_string();
    // Windows 上的错误码是 GetLastError 的值，作 winerror 传入，由 Python 换算 errno
    if cfg!(windows) {
        PyOSError::new_err((None::<i32>, strerror, filename, code))
    } else {
        PyOSError::new_err((code, strerror, filename))
    }
}

/// `OSError` 的 `errno`：Windows 上是 Python 由系统错误码换算的。
fn errno_of(py: Python<'_>, os_error: &PyErr) -> PyResult<Option<i32>> {
    os_error.value(py).getattr("errno")?.extract()
}

/// 来源用到了不支持的特性：`SourceUnsupportedError`。
fn unsupported_source(
    py: Python<'_>,
    base: HsM3u8Error,
    reason: SourceUnsupported,
) -> PyResult<Bound<'_, PyAny>> {
    instance(
        py,
        SourceError::chain(base, SourceUnsupportedError { reason }),
    )
}

fn instance<'py, L: PyClass>(
    py: Python<'py>,
    chain: PyClassInitializer<L>,
) -> PyResult<Bound<'py, PyAny>> {
    Ok(Bound::new(py, chain)?.into_any())
}

/// 填上说明（`args`，`str(异常)` 即它），做成可以抛出的异常。
fn raised(object: Bound<'_, PyAny>, message: String) -> PyResult<PyErr> {
    object.setattr("args", (message,))?;
    Ok(PyErr::from_value(object))
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    reason::register(module)?;
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
    module.add_class::<SystemProxyError>()?;
    module.add_class::<RemuxError>()?;
    module.add_class::<WorkDirError>()?;
    module.add_class::<NothingRecordedError>()?;
    module.add_class::<HookError>()?;
    module.add_class::<JobCancelledError>()?;
    Ok(())
}
