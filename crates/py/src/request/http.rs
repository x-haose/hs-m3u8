//! 发请求的方式：请求头、代理、超时与重试。

use std::hash::Hash;
use std::num::NonZeroU32;
use std::time::Duration;

use hs_m3u8_core as core;
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};

use crate::convert::{Address, Headers, Positive, ProxyAddress, Span};
use crate::error::to_pyerr;
use crate::record::{field_names, record, reduce, variants};

record! {
    @custom_new
    /// 超时，都不能为 0。
    pub struct Timeouts {
        /// 连接超时，默认 15 秒
        pub connect: Duration,
        /// 两次读到数据之间的最长间隔，默认 30 秒
        pub read_idle: Duration,
        /// 单次尝试从发出到读完的上限，默认 600 秒：几 MB 的分片在几十 KB/s 时仍能读完
        pub request: Duration,
    }
}

// 签名手写，默认值写 `...`，见 crate 文档
pyo3_stub_gen::inventory::submit! {
    pyo3_stub_gen::derive::gen_methods_from_python! {
        r#"
        import datetime

        class Timeouts:
            def __new__(
                cls,
                *,
                connect: datetime.timedelta = ...,
                read_idle: datetime.timedelta = ...,
                request: datetime.timedelta = ...,
            ) -> Timeouts:
                """各项的默认值见同名属性；为负或为 0 时抛 `InvalidInputError`。"""
        "#
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Timeouts {
    #[new]
    #[gen_stub(skip)]
    #[pyo3(signature = (
        *,
        connect = Span(core::Timeouts::default().connect),
        read_idle = Span(core::Timeouts::default().read_idle),
        request = Span(core::Timeouts::default().request),
    ))]
    fn new(py: Python<'_>, connect: Span, read_idle: Span, request: Span) -> PyResult<Self> {
        let timeouts = core::Timeouts {
            connect: connect.0,
            read_idle: read_idle.0,
            request: request.0,
        };
        timeouts.validate().map_err(|error| to_pyerr(py, error))?;
        Ok(timeouts.into())
    }
}

record! {
    @custom_new
    /// 单个请求的重试策略：可重试的失败（超时、连接与传输错误，内容没有写完，408、429 与 5xx）按指数退避重试。
    /// 默认 8 次、0.5 秒起、上限 15 秒：重试前的等待合计约 45 秒，足以越过常见的短时故障。
    pub struct RetryPolicy {
        /// 总尝试次数（含首次），至少 1
        pub attempts: Positive<NonZeroU32>,
        /// 第 n 次重试前等待 base_delay × 2^(n-1)
        pub base_delay: Duration,
        /// 指数等待的上限；实际等待再乘以 [0.75, 1.25) 的随机抖动。服务器在 429/503 中要求（Retry-After）的等待超过
        /// 它时不再重试，直接失败
        pub max_delay: Duration,
    }
}

// 签名手写，默认值写 `...`，见 crate 文档
pyo3_stub_gen::inventory::submit! {
    pyo3_stub_gen::derive::gen_methods_from_python! {
        r#"
        import builtins
        import datetime

        class RetryPolicy:
            def __new__(
                cls,
                *,
                attempts: builtins.int = ...,
                base_delay: datetime.timedelta = ...,
                max_delay: datetime.timedelta = ...,
            ) -> RetryPolicy:
                """各项的默认值见类的说明；`attempts` 小于 1、时长为负时抛 `InvalidInputError`。"""
        "#
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl RetryPolicy {
    #[new]
    #[gen_stub(skip)]
    #[pyo3(signature = (
        *,
        attempts = Positive(core::RetryPolicy::default().attempts),
        base_delay = Span(core::RetryPolicy::default().base_delay),
        max_delay = Span(core::RetryPolicy::default().max_delay),
    ))]
    fn new(attempts: Positive<NonZeroU32>, base_delay: Span, max_delay: Span) -> Self {
        RetryPolicy {
            attempts,
            base_delay: base_delay.0,
            max_delay: max_delay.0,
        }
    }
}

variants! {
    /// 请求经不经代理、经哪个。
    pub enum Proxy {
        /// 按系统代理设置：先看环境变量（规则同 curl，另认大写的 `HTTP_PROXY`；`no_proxy` 对系统设置的代理同样起作
        /// 用），没设代理时看 macOS 的系统设置或 Windows 的 Internet 选项，其他平台即直连。探测与开始任务时读取，任务中
        /// 每隔几秒重读，设置改了之后的请求照新设置走；值用不了（写错、用了 PAC 等）或读不出时抛 `SystemProxyError`，
        /// 可改用 `Proxy.Direct()` 或 `Proxy.Url(...)`
        System {},
        /// 不经代理，系统设置了代理也直连
        Direct {},
        /// 经这个 HTTP 或 SOCKS 代理
        Url {
            /// 代理的地址，协议为 http、https、socks4、socks4a、socks5、socks5h（构造时检查，不合要求时抛
            /// `InvalidInputError`）；读到的是解析后的规范写法，可能与传入的不同；`repr` 只显示到路径，不含用户名与口令
            url: ProxyAddress,
        },
    }
}

/// 发请求的方式。`repr` 不显示请求头的值。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen, eq, hash, get_all, from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct HttpOptions {
    /// 附加到请求的请求头，按顺序，同名的可以有多个。其中的 Cookie（`名=值; 名=值`）记进本任务的 Cookie 存储，作为只属
    /// 于来源地址的主机的 Cookie：只发往这个主机，服务器给这个主机下发同名的（不带 Domain 或 Domain 即这个主机）即被替
    /// 换，给上级域下发的与它并存、一同发出；Authorization 只发往与来源地址协议、主机、端口都相同的地址；其余的发往所有
    /// 地址。跟随重定向时照样带上，跳到别的主机（或端口、协议）时去掉 Authorization 与 Proxy-Authorization
    pub headers: Headers,
    /// 经不经代理、经哪个
    pub proxy: Proxy,
    /// 不校验 TLS 证书
    pub insecure: bool,
    /// 超时
    pub timeouts: Timeouts,
    /// 重试策略
    pub retry: RetryPolicy,
}

#[gen_stub_pymethods]
#[pymethods]
impl HttpOptions {
    /// `headers` 为 None 时没有附加的请求头；`proxy`、`timeouts`、`retry` 默认取 `Proxy.System()`、`Timeouts()`、
    /// `RetryPolicy()`。请求头的名或值放不进 HTTP 请求、是本库控制的头（Accept-Encoding、Range 与条件请求的
    /// If-*：它们决定响应体是什么、怎么编码）、Cookie 不是 `名=值; 名=值` 的写法、代理的协议不支持或主机名不合法时抛
    /// `InvalidInputError`。
    #[new]
    #[pyo3(signature = (
        *,
        headers = None,
        proxy = Proxy::System {},
        insecure = false,
        timeouts = Timeouts::from(core::Timeouts::default()),
        retry = RetryPolicy::from(core::RetryPolicy::default()),
    ))]
    fn new(
        py: Python<'_>,
        headers: Option<Headers>,
        proxy: Proxy,
        insecure: bool,
        timeouts: Timeouts,
        retry: RetryPolicy,
    ) -> PyResult<Self> {
        let options = HttpOptions {
            headers: headers.unwrap_or_default(),
            proxy,
            insecure,
            timeouts,
            retry,
        };
        options
            .to_core()
            .validate()
            .map_err(|error| to_pyerr(py, error))?;
        Ok(options)
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(format!(
            "HttpOptions(headers={}, proxy={}, insecure={}, timeouts={}, retry={})",
            PyTuple::new(py, self.headers.names())?.repr()?,
            self.proxy.clone().into_pyobject(py)?.into_any().repr()?,
            if self.insecure { "True" } else { "False" },
            Bound::new(py, self.timeouts.clone())?.repr()?,
            Bound::new(py, self.retry.clone())?.repr()?,
        ))
    }

    #[gen_stub(skip)]
    fn __reduce__<'py>(
        slf: &Bound<'py, Self>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyTuple>)> {
        reduce(
            slf.as_any(),
            field_names!(HttpOptions {
                headers,
                proxy,
                insecure,
                timeouts,
                retry,
            }),
        )
    }
}

impl HttpOptions {
    pub(super) fn to_core(&self) -> core::HttpOptions {
        core::HttpOptions {
            headers: self.headers.0.clone(),
            // 简化：Python 不提供跨任务的 Cookie 存储，每个任务各用一个；Python 用户要跨任务保存服务器下发的 Cookie
            // 时，提供一个在 Rust 里实现、Python 能读写的存储（存储在请求路径上同步调用，不回调 Python）
            cookies: None,
            proxy: match &self.proxy {
                Proxy::System {} => core::Proxy::System,
                Proxy::Direct {} => core::Proxy::Direct,
                Proxy::Url { url } => core::Proxy::Url(url.0.0.clone()),
            },
            insecure: self.insecure,
            timeouts: core::Timeouts {
                connect: self.timeouts.connect,
                read_idle: self.timeouts.read_idle,
                request: self.timeouts.request,
            },
            retry: core::RetryPolicy {
                attempts: self.retry.attempts.0,
                base_delay: self.retry.base_delay,
                max_delay: self.retry.max_delay,
            },
        }
    }
}

impl From<core::Timeouts> for Timeouts {
    fn from(t: core::Timeouts) -> Self {
        Timeouts {
            connect: t.connect,
            read_idle: t.read_idle,
            request: t.request,
        }
    }
}

impl From<core::RetryPolicy> for RetryPolicy {
    fn from(r: core::RetryPolicy) -> Self {
        RetryPolicy {
            attempts: Positive(r.attempts),
            base_delay: r.base_delay,
            max_delay: r.max_delay,
        }
    }
}

impl From<core::HttpOptions> for HttpOptions {
    fn from(h: core::HttpOptions) -> Self {
        HttpOptions {
            headers: Headers(h.headers),
            proxy: match h.proxy {
                core::Proxy::System => Proxy::System {},
                core::Proxy::Direct => Proxy::Direct {},
                core::Proxy::Url(url) => Proxy::Url {
                    url: ProxyAddress(Address(url)),
                },
            },
            insecure: h.insecure,
            timeouts: h.timeouts.into(),
            retry: h.retry.into(),
        }
    }
}
