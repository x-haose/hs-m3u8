//! 请求：来源、选轨偏好、发请求的方式、输出与任务的配置。构造时按 core 的规则检查各值，不合要求时抛
//! `InvalidInputError`；只有输出路径（相对路径按开始任务时的当前目录补全）在开始任务时检查。

use std::hash::{Hash, Hasher};
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use hs_m3u8_core as core;
use pyo3::PyTraverseError;
use pyo3::gc::PyVisit;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString, PyTuple};
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods};

use crate::convert::{Address, Count, Headers, Positive, ProxyAddress, PyPath, Span};
use crate::error::{invalid_input, to_pyerr};
use crate::hooks::{self, Hooks};
use crate::record::{field_names, record, reduce, variants};

variants! {
    /// 选哪个变体。
    pub enum VariantChoice {
        /// 分辨率最高者，同分辨率取带宽最高，都相同时取靠后的；有带 RESOLUTION 的变体时，不考虑没有 RESOLUTION 的
        /// （多为纯音频）
        Best {},
        /// 主播放列表里第几个变体（从 0 起，见 `VariantInfo.index`）
        Index {
            /// 下标
            index: Count<usize>,
        },
    }
}

variants! {
    /// 所选变体引用了音频组时，取组里哪个音频；`Default` 与 `Language` 在变体没有引用音频组时不起作用。
    pub enum AudioChoice {
        /// DEFAULT=YES 的，没有则取组内第一个
        Default {},
        /// 组内第一个 LANGUAGE 与它相同（不区分大小写）的
        Language {
            /// 语言，如 `en`、`zh-Hans`
            language: String,
        },
        /// 主播放列表里第几个音频（只数音频，从 0 起，见 `AudioInfo.index`）；须在所选变体的音频组里
        Index {
            /// 下标
            index: Count<usize>,
        },
    }
}

record! {
    @custom_new
    /// 选轨偏好，属于来源：任务目录里有已完成的分片时按记录的变体与音频找回同一条轨，不按偏好重新选。
    pub struct Preference {
        /// 选哪个变体
        pub variant: VariantChoice,
        /// 取哪个音频
        pub audio: AudioChoice,
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Preference {
    /// `variant` 默认 `VariantChoice.Best()`，`audio` 默认 `AudioChoice.Default()`；音频语言含换行时抛
    /// `InvalidInputError`。
    #[new]
    #[pyo3(signature = (
        *,
        variant = Preference::from(core::hls::Preference::default()).variant,
        audio = Preference::from(core::hls::Preference::default()).audio,
    ))]
    fn new(py: Python<'_>, variant: VariantChoice, audio: AudioChoice) -> PyResult<Self> {
        let preference = Preference { variant, audio };
        core::Source::check_preference(&preference.to_core())
            .map_err(|error| to_pyerr(py, error))?;
        Ok(preference)
    }
}

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
    /// 单个请求的重试策略：可重试的失败（超时、连接与传输错误，408、429 与 5xx）按指数退避重试。默认 8 次、0.5 秒起、
    /// 上限 15 秒：重试前的等待合计约 45 秒，足以越过常见的短时故障。
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
        /// 按系统代理设置：先看环境变量（规则同 curl，另认大写的 `HTTP_PROXY`；`no_proxy` 对系统设置的代理同样起作用），
        /// 没设代理时看 macOS 的系统设置或 Windows 的 Internet 选项，其他平台即直连。探测与开始任务时读取；值用不了（写错、
        /// 用了 PAC 等）时抛 `InvalidInputError`，读不出时抛 `SystemProxyError`，可改用 `Proxy.Direct()` 或 `Proxy.Url(...)`
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
    /// 附加到所有请求的请求头，按顺序，同名的可以有多个；跟随重定向时照样带上，跳到别的主机（或端口、协议）时去掉
    /// Cookie、Authorization 与 Proxy-Authorization
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
    /// `RetryPolicy()`。请求头的名或值放不进 HTTP 请求、代理的协议不支持或主机名不合法时抛 `InvalidInputError`。
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

/// 来源与访问它的方式；探测（`Engine.probe`）与下载共用。按值比较，回调按对象身份比较。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen, eq, hash, get_all)]
pub struct Source {
    /// 主播放列表或媒体播放列表的地址，只支持 http/https；读到的是解析后的规范写法（主机名小写、补上路径 `/`、
    /// 百分号编码等），可能与传入的不同。它去掉用户名、密码、查询串与片段后，连同 `preference`，称为来源：任务目录
    /// 记录的来源不同即是另一个任务；来源相同而完整地址不同（如换了令牌）时，直播按 `LiveOptions` 的规则续录
    pub url: Address,
    /// 选轨偏好
    pub preference: Preference,
    /// 发请求的方式
    pub http: HttpOptions,
    /// 站点适配回调；None 时不做改动
    pub hooks: Option<Py<Hooks>>,
}

impl Source {
    /// 回调对象的身份：回调一般不定义按值比较。
    fn hooks_identity(&self) -> Option<*mut pyo3::ffi::PyObject> {
        self.hooks.as_ref().map(Py::as_ptr)
    }
}

impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
            && self.preference == other.preference
            && self.http == other.http
            && self.hooks_identity() == other.hooks_identity()
    }
}

impl Eq for Source {}

impl Hash for Source {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.url.hash(state);
        self.preference.hash(state);
        self.http.hash(state);
        self.hooks_identity().hash(state);
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Source {
    /// `preference`、`http` 默认取 `Preference()`、`HttpOptions()`；`url` 解析不了或不是 http/https 时抛
    /// `InvalidInputError`。
    #[new]
    #[pyo3(signature = (
        url,
        *,
        preference = Preference::from(core::hls::Preference::default()),
        http = HttpOptions::from(core::HttpOptions::default()),
        hooks = None,
    ))]
    fn new(
        py: Python<'_>,
        url: Address,
        preference: Preference,
        http: HttpOptions,
        hooks: Option<Py<Hooks>>,
    ) -> PyResult<Self> {
        core::Source::check_url(&url.0).map_err(|error| to_pyerr(py, error))?;
        Ok(Source {
            url,
            preference,
            http,
            hooks,
        })
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let hooks = match &self.hooks {
            Some(hooks) => hooks.bind(py).repr()?.to_string(),
            None => "None".to_owned(),
        };
        Ok(format!(
            "Source(url={}, preference={}, http={}, hooks={hooks})",
            PyString::new(py, &self.url.bare()).repr()?,
            Bound::new(py, self.preference.clone())?.repr()?,
            Bound::new(py, self.http.clone())?.repr()?,
        ))
    }

    #[gen_stub(skip)]
    fn __reduce__<'py>(
        slf: &Bound<'py, Self>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyTuple>)> {
        reduce(
            slf.as_any(),
            field_names!(Source {
                url,
                preference,
                http,
                hooks,
            }),
        )
    }

    /// 回调对象常引用来源（如存着用到它的请求），引用环要靠垃圾回收器经这里找到。
    #[gen_stub(skip)]
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.hooks)
    }
}

variants! {
    /// 输出什么。各路径与任务目录须以文件名结尾，不能相同或互相包含（按字面比较，不跟随符号链接），所在目录不存在时
    /// 在任务开头与写出前创建；相对路径按开始任务时的当前目录补全，之后改变当前目录不影响任务；补全后最后一段变了
    /// （Windows 去掉结尾的点与空格）时报参数错误。
    pub enum Target {
        /// MP4 文件
        Mp4 {
            /// MP4 的路径
            path: PyPath,
        },
        /// 可直接播放的本地 HLS 目录：不经 FFmpeg，原样保留分片（已解密），不受 MP4 合并对编码的限制。入口为其中的
        /// `index.m3u8`，布局见 `Output.hls`
        Hls {
            /// 目录的路径
            path: PyPath,
        },
        /// 两者都要，内容相同
        #[pyo3(constructor = (*, mp4, hls))]
        Both {
            /// MP4 的路径
            mp4: PyPath,
            /// 本地 HLS 目录的路径
            hls: PyPath,
        },
    }
}

record! {
    @custom_new
    /// 输出到哪里、任务目录在哪里；下载（`JobRequest.output`）与只合并（`Engine.merge_recorded`）共用。
    pub struct OutputOptions {
        /// 输出什么
        pub target: Target,
        /// 任务目录；None 时与输出同级，名为输出的主名加 `.hsdl`：主名为 MP4 文件名去掉扩展名，只输出 HLS 时为 HLS
        /// 目录名。`a.mp4` 与 HLS 目录 `a` 共用 `a.hsdl`，改变输出种类后接着用已下载的分片。只能是空目录、不存在的目录
        /// 或本库建立的任务目录
        pub work_dir: Option<PyPath>,
        /// 输出已存在时替换：MP4 文件直接替换；HLS 目录只在其中全是本库写出的文件时替换。路径被别的东西占用（见
        /// `OutputOccupiedError`）时覆盖也不替换。为 False 时 HLS 目录须不存在或为空（系统自动生成的元数据文件
        /// 不算），否则报 `OutputExistsError`
        pub overwrite: bool,
        /// 成功后保留任务目录
        pub keep_work_dir: bool,
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl OutputOptions {
    /// 各参数即同名属性。
    #[new]
    #[pyo3(signature = (target, *, work_dir = None, overwrite = false, keep_work_dir = false))]
    fn new(target: Target, work_dir: Option<PyPath>, overwrite: bool, keep_work_dir: bool) -> Self {
        OutputOptions {
            target,
            work_dir,
            overwrite,
            keep_work_dir,
        }
    }

    /// 实际使用的任务目录：`work_dir`，未指定时见 `work_dir` 的说明；与开始任务时一样补全为绝对路径、校验，不合
    /// 要求时抛 `InvalidInputError`。
    fn resolved_work_dir(&self, py: Python<'_>) -> PyResult<PyPath> {
        self.to_core()
            .resolved_work_dir()
            .map(PyPath::from)
            .map_err(|error| to_pyerr(py, error))
    }
}

/// 自定义 key：加密的分片一律用它解密，不请求播放列表里的 key 地址，也不经过 `Hooks.on_key`。用于 key 地址取不到、
/// 或返回的不是真正的 key，而调用方已知 key 的站点；不加密的分片不受影响。`repr` 不显示 key 与 IV。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen, eq, hash, from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct KeyOverride {
    key: [u8; 16],
    iv: Option<[u8; 16]>,
}

#[gen_stub_pymethods]
#[pymethods]
impl KeyOverride {
    /// `key`、`iv` 须为 16 字节，否则抛 `InvalidInputError`。
    #[new]
    #[pyo3(signature = (key, *, iv = None))]
    fn new(
        py: Python<'_>,
        key: &Bound<'_, PyBytes>,
        iv: Option<&Bound<'_, PyBytes>>,
    ) -> PyResult<Self> {
        let block = |what: &str, bytes: &Bound<'_, PyBytes>| {
            let bytes = bytes.as_bytes();
            <[u8; 16]>::try_from(bytes).map_err(|_| {
                invalid_input(
                    py,
                    format!("{what} 应为 16 字节，实际 {} 字节", bytes.len()),
                )
            })
        };
        Ok(KeyOverride {
            key: block("key", key)?,
            iv: iv.map(|iv| block("iv", iv)).transpose()?,
        })
    }

    /// 16 字节的 key
    #[getter]
    fn key<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.key)
    }

    /// 所有加密分片都用的 IV；None 时用各分片自己的（播放列表写明的，或由媒体序号推出）。IV 只影响每个分片解密后的
    /// 头 16 字节，给错了校验发现不了、错的内容照样写进输出，只在确知时给
    #[getter]
    fn iv<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyBytes>> {
        self.iv.map(|iv| PyBytes::new(py, &iv))
    }

    fn __repr__(&self) -> String {
        let iv = if self.iv.is_some() { "..." } else { "None" };
        format!("KeyOverride(key=..., iv={iv})")
    }

    #[gen_stub(skip)]
    fn __reduce__<'py>(
        slf: &Bound<'py, Self>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyTuple>)> {
        reduce(slf.as_any(), field_names!(KeyOverride { key, iv }))
    }
}

record! {
    @custom_new
    /// 直播的录制方式。
    ///
    /// 录制从当前播放列表里的全部分片开始，按 RFC 8216 6.3.4 的节奏刷新，直到所有轨出现 EXT-X-ENDLIST、调用
    /// `Job.stop`、各轨都录满 `max_duration`、任一轨停滞，或服务器的播放列表前后矛盾（序号回退、同一序号换了分片）。
    /// 结束后把已拉到的播放列表处理完、已列出的分片下完再合并，结束原因见 `LiveEnd`。
    ///
    /// 停滞：任一轨持续 `stall_timeout` 没有录到新分片（有分片还在排队或下载时不算）。播放列表不再出新分片或已被删除
    /// （404/410），且其他轨也不再出，视为直播已结束，照常合并；刷新一直失败、一直不返回，仍在出新分片却一个都下载
    /// 不成功，或只有这条轨不再出新分片，任务失败（`LiveStalledError`），任务目录保留，可稍后续录。
    ///
    /// 窗口已滑过或取不到（404/410、重试后仍失败的临时故障）的分片记为缺失，录制继续；成片保留原时间戳，缺失处时间线
    /// 留空，各轨同步不受影响。其他失败（如 403、key 失败、校验失败）使任务失败。
    ///
    /// 续录：任务中断后用同样的请求再次运行。各轨当前的窗口都与各自之前录到的接得上时，接着录，并补上窗口内之前没录完
    /// 的分片，时间线连续；否则另起一段，与之前的首尾相接，之前录过的分片不重录。来源相同而完整地址与记录的不同（如换
    /// 了令牌）时，要录的各轨都接得上才续录并改记新地址，否则报 `WorkDirProblem.SourceUnverified`。不联网、只合并已录到
    /// 的部分用 `Engine.merge_recorded`。
    pub struct LiveOptions {
        /// 每条轨的分片声明时长之和达到此值后不再录，各轨都达到即结束；计入之前各会话已录到的、本次排入下载的、本次
        /// 列出了但取不到的，不计窗口已滑过的与之前运行里缺失的。None 不限，不能为 0
        pub max_duration: Option<Duration>,
        /// 停滞判定的时长，默认 60 秒，不能为 0；实际至少等三个目标时长
        pub stall_timeout: Duration,
    }
}

// 签名手写，默认值写 `...`，见 crate 文档
pyo3_stub_gen::inventory::submit! {
    pyo3_stub_gen::derive::gen_methods_from_python! {
        r#"
        import datetime
        import typing

        class LiveOptions:
            def __new__(
                cls,
                *,
                max_duration: typing.Optional[datetime.timedelta] = None,
                stall_timeout: datetime.timedelta = ...,
            ) -> LiveOptions:
                """各项的默认值见同名属性；为负或为 0 时抛 `InvalidInputError`。"""
        "#
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl LiveOptions {
    #[new]
    #[gen_stub(skip)]
    #[pyo3(signature = (*, max_duration = None, stall_timeout = Span(core::LiveOptions::default().stall_timeout)))]
    fn new(py: Python<'_>, max_duration: Option<Span>, stall_timeout: Span) -> PyResult<Self> {
        let live = core::LiveOptions {
            max_duration: max_duration.map(|span| span.0),
            stall_timeout: stall_timeout.0,
        };
        live.validate().map_err(|error| to_pyerr(py, error))?;
        Ok(live.into())
    }
}

/// 一个下载任务的配置。按值比较（见 `Source`）。
#[gen_stub_pyclass]
#[pyclass(module = "hs_m3u8._native", frozen, eq, hash, get_all)]
pub struct JobRequest {
    /// 来源与访问它的方式
    pub source: Py<Source>,
    /// 输出到哪里、任务目录在哪里
    pub output: OutputOptions,
    /// 自定义 key；None 时按播放列表取 key
    pub key: Option<KeyOverride>,
    /// 本任务同时下载的分片数，至少 1，默认 16；站点限流时调低
    pub concurrency: Positive<NonZeroUsize>,
    /// 直播的录制方式；None 时遇到直播抛 `LiveNotEnabledError`
    pub live: Option<LiveOptions>,
}

impl PartialEq for JobRequest {
    fn eq(&self, other: &Self) -> bool {
        self.source.get() == other.source.get()
            && self.output == other.output
            && self.key == other.key
            && self.concurrency == other.concurrency
            && self.live == other.live
    }
}

impl Eq for JobRequest {}

impl Hash for JobRequest {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.source.get().hash(state);
        self.output.hash(state);
        self.key.hash(state);
        self.concurrency.hash(state);
        self.live.hash(state);
    }
}

// 签名手写，默认值写 `...`，见 crate 文档
pyo3_stub_gen::inventory::submit! {
    pyo3_stub_gen::derive::gen_methods_from_python! {
        r#"
        import builtins
        import typing

        class JobRequest:
            def __new__(
                cls,
                source: Source,
                output: OutputOptions,
                *,
                key: typing.Optional[KeyOverride] = None,
                concurrency: builtins.int = ...,
                live: typing.Optional[LiveOptions] = ...,
            ) -> JobRequest:
                """`concurrency` 默认 16，小于 1 时抛 `InvalidInputError`；`live` 默认 `LiveOptions()`。"""
        "#
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl JobRequest {
    #[new]
    #[gen_stub(skip)]
    #[pyo3(signature = (
        source,
        output,
        *,
        key = None,
        concurrency = Positive(core::JobRequest::DEFAULT_CONCURRENCY),
        live = Some(LiveOptions::from(core::LiveOptions::default())),
    ))]
    fn new(
        source: Py<Source>,
        output: OutputOptions,
        key: Option<KeyOverride>,
        concurrency: Positive<NonZeroUsize>,
        live: Option<LiveOptions>,
    ) -> Self {
        JobRequest {
            source,
            output,
            key,
            concurrency,
            live,
        }
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let source = self.source.bind(py).repr()?;
        let output = Bound::new(py, self.output.clone())?.repr()?;
        let key = self.key.clone().into_pyobject(py)?.repr()?;
        let live = self.live.clone().into_pyobject(py)?.repr()?;
        Ok(format!(
            "JobRequest(source={source}, output={output}, key={key}, concurrency={}, live={live})",
            self.concurrency.0
        ))
    }

    #[gen_stub(skip)]
    fn __reduce__<'py>(
        slf: &Bound<'py, Self>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyTuple>)> {
        reduce(
            slf.as_any(),
            field_names!(JobRequest {
                source,
                output,
                key,
                concurrency,
                live,
            }),
        )
    }

    /// 见 `Source.__traverse__`。
    #[gen_stub(skip)]
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.source)
    }
}

variants! {
    /// 放弃任务（`Engine.discard`）时，任务目录或记录里的输出所在的盘读不到，或收拾输出出错，怎么办。
    pub enum Unsettled {
        /// 报错（盘读不到时为 `VolumeUnavailableError`），任务目录与记录都留着，接上或排除后重试
        Fail {},
        /// 记为残留，照常放弃：每个收拾不了的输出，它旁边可能留着的写了一半的临时输出与替换时挪开的旧输出各记一项
        /// （盘读不到时核实不了，不一定还在），怎么处理见 `Leftover.kind`；任务目录本身读不到时记一项
        /// `LeftoverKind.WorkDir`。用于盘不再接上、用户确认仍要放弃时
        Report {},
    }
}

impl Source {
    /// 换成 core 的来源；接上回调时读取它的属性，出错时抛那个异常。
    pub(crate) fn to_core(&self, py: Python<'_>) -> PyResult<core::Source> {
        let hooks: Arc<dyn core::Hooks> = match &self.hooks {
            Some(hooks) => hooks::to_core(hooks.bind(py))?,
            None => Arc::new(core::NoHooks),
        };
        Ok(core::Source {
            url: self.url.0.clone(),
            preference: self.preference.to_core(),
            http: self.http.to_core(),
            hooks,
        })
    }
}

impl HttpOptions {
    fn to_core(&self) -> core::HttpOptions {
        core::HttpOptions {
            headers: self.headers.0.clone(),
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

impl Target {
    fn to_core(&self) -> core::Target {
        match self {
            Target::Mp4 { path } => core::Target::Mp4(path.0.clone()),
            Target::Hls { path } => core::Target::Hls(path.0.clone()),
            Target::Both { mp4, hls } => core::Target::Both {
                mp4: mp4.0.clone(),
                hls: hls.0.clone(),
            },
        }
    }
}

impl OutputOptions {
    pub(crate) fn to_core(&self) -> core::OutputOptions {
        let mut options = core::OutputOptions::new(self.target.to_core());
        options.work_dir = self.work_dir.as_ref().map(|dir| dir.0.clone());
        options.overwrite = self.overwrite;
        options.keep_work_dir = self.keep_work_dir;
        options
    }
}

impl JobRequest {
    /// 换成 core 的请求；接上回调时读取它的属性，出错时抛那个异常。
    pub(crate) fn to_core(&self, py: Python<'_>) -> PyResult<core::JobRequest> {
        let mut request =
            core::JobRequest::new(self.source.get().to_core(py)?, self.output.to_core());
        request.key = self.key.as_ref().map(|key| core::KeyOverride {
            key: key.key,
            iv: key.iv,
        });
        request.concurrency = self.concurrency.0;
        request.live = self.live.as_ref().map(|live| core::LiveOptions {
            max_duration: live.max_duration,
            stall_timeout: live.stall_timeout,
        });
        Ok(request)
    }
}

impl From<core::LiveOptions> for LiveOptions {
    fn from(live: core::LiveOptions) -> Self {
        LiveOptions {
            max_duration: live.max_duration,
            stall_timeout: live.stall_timeout,
        }
    }
}

impl From<&Unsettled> for core::Unsettled {
    fn from(unsettled: &Unsettled) -> Self {
        match unsettled {
            Unsettled::Fail {} => core::Unsettled::Fail,
            Unsettled::Report {} => core::Unsettled::Report,
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

impl From<core::hls::Preference> for Preference {
    fn from(p: core::hls::Preference) -> Self {
        Preference {
            variant: match p.variant {
                core::hls::VariantChoice::Best => VariantChoice::Best {},
                core::hls::VariantChoice::Index(index) => VariantChoice::Index {
                    index: Count(index),
                },
            },
            audio: match p.audio {
                core::hls::AudioChoice::Default => AudioChoice::Default {},
                core::hls::AudioChoice::Language(language) => AudioChoice::Language { language },
                core::hls::AudioChoice::Index(index) => AudioChoice::Index {
                    index: Count(index),
                },
            },
        }
    }
}

impl Preference {
    fn to_core(&self) -> core::hls::Preference {
        core::hls::Preference {
            variant: match &self.variant {
                VariantChoice::Best {} => core::hls::VariantChoice::Best,
                VariantChoice::Index { index } => core::hls::VariantChoice::Index(index.0),
            },
            audio: match &self.audio {
                AudioChoice::Default {} => core::hls::AudioChoice::Default,
                AudioChoice::Language { language } => {
                    core::hls::AudioChoice::Language(language.clone())
                }
                AudioChoice::Index { index } => core::hls::AudioChoice::Index(index.0),
            },
        }
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<VariantChoice>()?;
    module.add_class::<AudioChoice>()?;
    module.add_class::<Preference>()?;
    module.add_class::<Timeouts>()?;
    module.add_class::<RetryPolicy>()?;
    module.add_class::<Proxy>()?;
    module.add_class::<HttpOptions>()?;
    module.add_class::<Source>()?;
    module.add_class::<Target>()?;
    module.add_class::<OutputOptions>()?;
    module.add_class::<KeyOverride>()?;
    module.add_class::<LiveOptions>()?;
    module.add_class::<JobRequest>()?;
    module.add_class::<Unsettled>()?;
    Ok(())
}
