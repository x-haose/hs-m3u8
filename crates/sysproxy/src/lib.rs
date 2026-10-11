//! 读取操作系统的代理设置原文：macOS 的系统设置（SystemConfiguration 的 SCDynamicStore），Windows 的 Internet 选项
//! （注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings`）。只读不解析：取值的含义与校验在
//! 使用方。其他平台没有这类设置。

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// 读到的代理设置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settings {
    MacOs(MacOs),
    Windows(Windows),
}

/// macOS 系统设置里的代理（与 `scutil --proxy` 列出的是同一份，系统已规范化，见 `macos::read`）。各代理在开关打开
/// 时才有。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MacOs {
    /// 网页代理（HTTP）
    pub http: Option<Server>,
    /// 安全网页代理（HTTPS）
    pub https: Option<Server>,
    /// SOCKS 代理
    pub socks: Option<Server>,
    /// 「忽略这些主机与域的代理设置」，原样，如 `*.local`、`169.254/16`
    pub exceptions: Vec<String>,
    /// 「不包括简单主机名」：不含点的主机名不经代理
    pub exclude_simple_hostnames: bool,
    /// 开着「自动代理配置」（PAC）
    pub auto_config: bool,
    /// 开着「自动发现代理」（WPAD）
    pub auto_discovery: bool,
}

/// 代理服务器，原样：开关开着却没填时为 None。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub host: Option<String>,
    pub port: Option<i32>,
}

/// Windows 的 Internet 选项里的代理（注册表原文）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Windows {
    /// 「为 LAN 使用代理服务器」（ProxyEnable）开着
    pub enabled: bool,
    /// ProxyServer：`主机:端口`，或分协议的 `http=主机:端口;https=…;socks=…`
    pub server: Option<String>,
    /// ProxyOverride：`;` 分隔的例外，可带通配符，`<local>` 指不含点的主机名
    pub bypass: Option<String>,
    /// AutoConfigURL：「使用设置脚本」（PAC）的地址
    pub auto_config_url: Option<String>,
}

/// 读不出代理设置：系统接口失败，或某一项不是该有的类型。值为原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// 读本机的代理设置。没有这类设置的平台、Windows 上没有 Internet 选项时为 None。
pub fn read() -> Result<Option<Settings>, Error> {
    #[cfg(target_os = "macos")]
    return macos::read().map(|settings| Some(Settings::MacOs(settings)));
    #[cfg(windows)]
    return windows::read().map(|settings| settings.map(Settings::Windows));
    #[cfg(not(any(target_os = "macos", windows)))]
    Ok(None)
}
