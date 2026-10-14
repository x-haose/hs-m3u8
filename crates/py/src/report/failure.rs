//! 异常与结果共用的类型：请求失败、分片取不到、直播刷新失败与停滞的原因，以及收尾没删掉的残留。

use std::time::Duration;

use hs_m3u8_core as core;
use pyo3::prelude::*;

use crate::convert::PyPath;
use crate::record::{record, variants};

variants! {
    /// 请求失败的原因。
    pub enum HttpErrorKind {
        /// 服务器返回了非 2xx 的状态
        Status {
            /// HTTP 状态码
            code: u16,
        },
        /// 字节范围请求应返回 206，服务器却返回了别的状态
        RangeIgnored {
            /// HTTP 状态码
            code: u16,
        },
        /// 字节范围请求返回了 206，Content-Range 却不是请求的那一段（多为 CDN 缓存 206 响应时没按 Range 区分）
        RangeMismatch {
            /// 收到的 Content-Range；没有时为 None
            content_range: Option<String>,
        },
        /// 连接、读空闲或单次请求超时
        Timeout {},
        /// TLS 证书校验失败（自签名、过期、与域名不符等）；确认来源可信时可设 `HttpOptions.insecure`
        Certificate {
            /// 说明
            detail: String,
        },
        /// 跟随重定向失败（循环、次数过多）
        Redirect {
            /// 说明
            detail: String,
        },
        /// 连不上服务器或代理
        Connect {
            /// 说明
            detail: String,
        },
        /// 代理要求认证或拒绝了这次连接，出在代理的设置或策略上：HTTP 代理对 CONNECT 回 407，或 SOCKS 代理认证失败、
        /// 不允许或不支持这次连接。要检查代理设置与口令。CONNECT 回了别的非成功状态、SOCKS 代理报目标连不上的算连接失败
        Proxy {
            /// 说明
            detail: String,
        },
        /// 请求无法构造：地址能解析，HTTP 库却不接受（如超过 65534 字节）
        InvalidRequest {
            /// 说明
            detail: String,
        },
        /// 读取响应体等其他传输错误
        Transport {
            /// 说明
            detail: String,
        },
    }
}

record! {
    /// 取不到的请求：失败的原因，与服务器要求的最短等待。
    pub struct HttpFailure {
        /// 失败的原因
        pub kind: HttpErrorKind,
        /// 临时故障，重试可能成功：超时、连接与传输错误，408、429 与 5xx
        pub retryable: bool,
        /// 服务器在 429/503 中要求（Retry-After）的最短等待；没有要求时为 None
        pub retry_after: Option<Duration>,
    }
}

variants! {
    /// 分片或 init 段取不到的原因的种类。
    pub enum FetchErrorKind {
        /// 请求取不到：404/410，或重试后仍失败的临时故障
        Http {
            /// 请求失败的原因
            kind: HttpErrorKind,
        },
        /// 重试后取到的内容仍没写完（见 `IncompleteError`）
        Incomplete {},
    }
}

variants! {
    /// 取不到的分片或 init 段。
    pub enum FetchFailure {
        /// 请求取不到：404/410，或重试后仍失败的临时故障
        Http {
            /// 失败的请求
            failure: HttpFailure,
        },
        /// 重试后取到的内容仍没写完（见 `IncompleteError`），稍后重试可能成功
        Incomplete {},
    }
}

variants! {
    /// 直播一次刷新失败的原因。
    pub enum RefreshReason {
        /// 取不到：404/410 或临时故障（刷新只发一次请求，失败了等下次刷新）
        Http {
            /// 失败的请求
            failure: HttpFailure,
        },
        /// 内容为空，多为服务器还没写完
        Empty {},
        /// 结尾截在多字节字符中间，多为服务器还没写完
        Truncated {},
        /// 语法错误，多为服务器还没写完
        #[pyo3(constructor = (*, line, detail))]
        Syntax {
            /// 出错的行，从 1 起
            line: usize,
            /// 错在哪里
            detail: String,
        },
    }
}

record! {
    /// 直播一次刷新失败；这类失败等下次刷新，不使任务立即失败。
    pub struct RefreshCause {
        /// 失败的原因
        pub reason: RefreshReason,
        /// 一直这样失败到停滞时，稍后续录可能成功：取不到的看原因；内容不完整的当作暂时故障，同样可以
        pub retryable: bool,
        /// 服务器要求的最短等待，下次刷新不早于它；没有要求时为 None
        pub retry_after: Option<Duration>,
    }
}

variants! {
    /// 一条轨停滞时看起来已结束的原因。
    pub enum StallCause {
        /// 刷新成功，但不再出现新分片
        NoNewSegments {},
        /// 播放列表已被删除
        PlaylistGone {
            /// 最近一次刷新返回的 HTTP 状态（404 或 410）
            status: u16,
        },
    }
}

variants! {
    /// 残留是什么，决定怎么处理。
    pub enum LeftoverKind {
        /// 本库写的、可以直接删除的东西：写了一半的临时输出、已被新输出替换的旧输出、任务目录里的记录
        Removable {},
        /// 没能放回原处的旧输出。失败的异常里报出的多数仍记在任务目录里：下次用同一任务目录运行或放弃任务时，原处
        /// 空着或是空目录就放回，原处是输出（更新的输出已装上）就删掉，原处是别的东西就留着；放弃任务返回的已没有
        /// 记录，之后用同一任务目录运行时不动它，在任务开头报出来。留着的要用户处理
        Displaced {
            /// 原来的路径
            target: PyPath,
        },
        /// 写输出失败后没能撤下的新输出：完整可用，留在原处
        Installed {},
        /// 没删干净的任务目录：里面有不是本库写的文件（例如经符号链接写进去的输出本身），要用户处理；或删除时
        /// 出错，可用 `Engine.discard` 重试。不要整个删除
        WorkDir {},
    }
}

record! {
    /// 收尾时没能删掉或放回的一样东西。
    pub struct Leftover {
        /// 它现在的路径
        pub path: PyPath,
        /// 是什么，决定怎么处理
        pub kind: LeftoverKind,
        /// 没能删掉或放回的原因
        pub cause: String,
    }
}

impl From<core::HttpErrorKind> for HttpErrorKind {
    fn from(kind: core::HttpErrorKind) -> Self {
        match kind {
            core::HttpErrorKind::Status(code) => HttpErrorKind::Status { code },
            core::HttpErrorKind::RangeIgnored(code) => HttpErrorKind::RangeIgnored { code },
            core::HttpErrorKind::RangeMismatch(content_range) => {
                HttpErrorKind::RangeMismatch { content_range }
            }
            core::HttpErrorKind::Timeout => HttpErrorKind::Timeout {},
            core::HttpErrorKind::Certificate(detail) => HttpErrorKind::Certificate { detail },
            core::HttpErrorKind::Redirect(detail) => HttpErrorKind::Redirect { detail },
            core::HttpErrorKind::Connect(detail) => HttpErrorKind::Connect { detail },
            core::HttpErrorKind::Proxy(detail) => HttpErrorKind::Proxy { detail },
            core::HttpErrorKind::InvalidRequest(detail) => HttpErrorKind::InvalidRequest { detail },
            core::HttpErrorKind::Transport(detail) => HttpErrorKind::Transport { detail },
        }
    }
}

impl From<core::FetchErrorKind> for FetchErrorKind {
    fn from(kind: core::FetchErrorKind) -> Self {
        match kind {
            core::FetchErrorKind::Http(kind) => FetchErrorKind::Http { kind: kind.into() },
            core::FetchErrorKind::Incomplete => FetchErrorKind::Incomplete {},
        }
    }
}

impl From<core::FetchFailure> for FetchFailure {
    fn from(failure: core::FetchFailure) -> Self {
        match failure {
            core::FetchFailure::Http(failure) => FetchFailure::Http {
                failure: failure.into(),
            },
            core::FetchFailure::Incomplete => FetchFailure::Incomplete {},
        }
    }
}

impl From<core::HttpFailure> for HttpFailure {
    fn from(failure: core::HttpFailure) -> Self {
        HttpFailure {
            retryable: failure.kind.retryable(),
            kind: failure.kind.into(),
            retry_after: failure.retry_after,
        }
    }
}

impl From<core::RefreshReason> for RefreshCause {
    fn from(cause: core::RefreshReason) -> Self {
        let (retryable, retry_after) = (cause.retryable(), cause.retry_after());
        let reason = match cause {
            core::RefreshReason::Http(failure) => RefreshReason::Http {
                failure: failure.into(),
            },
            core::RefreshReason::Empty => RefreshReason::Empty {},
            core::RefreshReason::Truncated => RefreshReason::Truncated {},
            core::RefreshReason::Syntax { line, kind } => RefreshReason::Syntax {
                line,
                detail: kind.to_string(),
            },
        };
        RefreshCause {
            reason,
            retryable,
            retry_after,
        }
    }
}

impl From<core::StallCause> for StallCause {
    fn from(cause: core::StallCause) -> Self {
        match cause {
            core::StallCause::NoNewSegments => StallCause::NoNewSegments {},
            core::StallCause::PlaylistGone(status) => StallCause::PlaylistGone { status },
        }
    }
}

impl From<core::LeftoverKind> for LeftoverKind {
    fn from(kind: core::LeftoverKind) -> Self {
        match kind {
            core::LeftoverKind::Removable => LeftoverKind::Removable {},
            core::LeftoverKind::Displaced { target } => LeftoverKind::Displaced {
                target: target.into(),
            },
            core::LeftoverKind::Installed => LeftoverKind::Installed {},
            core::LeftoverKind::WorkDir => LeftoverKind::WorkDir {},
        }
    }
}

impl From<core::Leftover> for Leftover {
    fn from(leftover: core::Leftover) -> Self {
        Leftover {
            path: leftover.path.into(),
            kind: leftover.kind.into(),
            cause: leftover.cause,
        }
    }
}
