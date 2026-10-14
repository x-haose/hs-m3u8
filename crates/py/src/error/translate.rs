//! core 的 `Error` 翻译为 Python 异常：剥掉说明位置的包装，按里面的原因选类。

use std::io;
use std::path::Path;

use hs_m3u8_core as core;
use pyo3::exceptions::PyOSError;
use pyo3::{PyClass, PyClassInitializer, PyTraverseError, PyVisit};

use super::reason::{SegmentRef, SourceUnsupported};
use super::*;
use crate::convert::{Address, PyPath, Seq};
use crate::hooks::PyHookError;
use crate::report::Leftover;

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
    let base = HsM3u8Error::plain(error.retryable(), error.retry_after());
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
        E::Unsupported(U::IFramesOnly { url, line }) => unsupported_source(
            py,
            base,
            SourceUnsupported::IFramesOnly {
                url: Address(*url),
                line,
            },
        )?,
        E::Unsupported(U::DeltaUpdate { url, line }) => unsupported_source(
            py,
            base,
            SourceUnsupported::DeltaUpdate {
                url: Address(*url),
                line,
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
        E::Unsupported(U::NoMergeableGroup(url)) => unsupported_source(
            py,
            base,
            SourceUnsupported::NoMergeableGroup { url: Address(*url) },
        )?,
        E::Unsupported(U::Fmp4BaseDataOffset { url, at }) => unsupported_source(
            py,
            base,
            SourceUnsupported::Fmp4BaseDataOffset {
                url: Address(*url),
                at,
            },
        )?,
        E::Unsupported(U::KeyWithoutUri(url)) => unsupported_source(
            py,
            base,
            SourceUnsupported::KeyWithoutUri { url: Address(*url) },
        )?,
        E::Unsupported(U::TsInit(url)) => {
            unsupported_source(py, base, SourceUnsupported::TsInit { url: Address(*url) })?
        }
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
        E::ResponseTooLarge {
            url,
            purpose,
            limit,
        } => instance(
            py,
            SourceError::chain(
                base,
                ResponseTooLargeError {
                    url: Address(*url),
                    purpose: purpose.into(),
                    limit,
                },
            ),
        )?,
        E::Incomplete { url } => instance(
            py,
            HsM3u8Error::chain(base, IncompleteError { url: Address(*url) }),
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
        E::HttpClientInit(_) => instance(py, HsM3u8Error::chain(base, HttpClientInitError {}))?,
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
