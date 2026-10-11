//! 操作系统的代理设置：macOS 的系统设置、Windows 的 Internet 选项，原文由 hs-m3u8-sysproxy 读出。取值的含义同
//! Chromium 与 Python：
//! - 没设 http、https 代理时用 SOCKS 代理代替（macOS 的为 SOCKS5，由代理解析主机名；Windows 的 `socks=` 为 SOCKS4）；
//! - 例外里带通配符的按通配符，IP 地址或前缀按网段（可省略后面的 0，如 `169.254/16`，省略掩码时按写出的段数），开头
//!   为 `.` 的含其子域名，`<local>` 指不含点的主机名，其余为恰好这个主机名；
//! - 用了自动代理配置（PAC）、macOS 的自动发现代理时报错：本库不执行 PAC，也不做发现。Windows 的「自动检测设置」
//!   （WPAD）不看：它默认开着，多数网络里并没有可发现的代理。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use hs_m3u8_sysproxy::{MacOs, Server, Settings, Windows};
use url::{Host, Url};

use super::{Bypass, ProxySource, SystemProxy, unusable};

/// 操作系统的代理设置；没设代理、或没有这类设置的平台为 None。用不了时为原因。
pub(super) fn read() -> Result<Option<SystemProxy>, String> {
    match hs_m3u8_sysproxy::read() {
        Err(e) => Err(format!("读不到操作系统的代理设置：{e}")),
        Ok(None) => Ok(None),
        Ok(Some(Settings::MacOs(settings))) => from_macos(settings),
        Ok(Some(Settings::Windows(settings))) => from_windows(settings),
    }
}

fn from_macos(settings: MacOs) -> Result<Option<SystemProxy>, String> {
    if settings.auto_config {
        return Err("macOS 的系统设置开着自动代理配置（PAC），本库不支持".to_owned());
    }
    if settings.auto_discovery {
        return Err("macOS 的系统设置开着自动发现代理，本库不支持".to_owned());
    }
    let server = |server, what, scheme, default_port| {
        macos_server(server, scheme, default_port)
            .map_err(|reason| format!("macOS 的系统设置里的{what}{reason}"))
    };
    let socks = server(settings.socks, "SOCKS 代理", "socks5h", 1080)?;
    let http = server(settings.http, "网页代理（HTTP）", "http", 80)?.or_else(|| socks.clone());
    let https = server(settings.https, "安全网页代理（HTTPS）", "http", 80)?.or(socks);
    if http.is_none() && https.is_none() {
        return Ok(None);
    }
    let mut bypass = exceptions(settings.exceptions.iter().map(String::as_str))
        .map_err(|entry| format!("macOS 的系统设置里的代理例外 {entry:?} 无法识别"))?;
    if settings.exclude_simple_hostnames {
        bypass.push(Bypass::SimpleHostnames);
    }
    Ok(Some(SystemProxy {
        source: ProxySource::MacOs,
        http,
        https,
        bypass,
    }))
}

/// 开着的代理服务器；没填端口（或为 0）时为 `default_port`。用不了时为原因（接在「某某代理」之后）。
fn macos_server(
    server: Option<Server>,
    scheme: &str,
    default_port: u16,
) -> Result<Option<Url>, String> {
    let Some(server) = server else {
        return Ok(None);
    };
    let host = match server.host.as_deref().map(str::trim) {
        Some(host) if !host.is_empty() => host,
        _ => return Err("开着却没填服务器".to_owned()),
    };
    let port = match server.port {
        None | Some(0) => default_port,
        Some(port) => {
            u16::try_from(port).map_err(|_| format!("的端口 {port} 不在 1 到 65535 之间"))?
        }
    };
    let host = match host.parse::<Ipv6Addr>() {
        Ok(_) => format!("[{host}]"),
        Err(_) => host.to_owned(),
    };
    proxy_url(&format!("{scheme}://{host}:{port}")).map(Some)
}

fn from_windows(settings: Windows) -> Result<Option<SystemProxy>, String> {
    if settings
        .auto_config_url
        .as_deref()
        .is_some_and(|url| !url.trim().is_empty())
    {
        return Err("Windows 的 Internet 选项用了设置脚本（PAC），本库不支持".to_owned());
    }
    if !settings.enabled {
        return Ok(None);
    }
    let server = settings
        .server
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    if server.is_empty() {
        return Err("Windows 的 Internet 选项开着代理服务器却没填地址".to_owned());
    }
    let (http, https) = windows_servers(server)
        .map_err(|reason| format!("Windows 的代理服务器设置（ProxyServer）{reason}"))?;
    if http.is_none() && https.is_none() {
        return Ok(None);
    }
    let bypass = exceptions(settings.bypass.as_deref().unwrap_or_default().split(';'))
        .map_err(|entry| format!("Windows 的代理例外（ProxyOverride）{entry:?} 无法识别"))?;
    Ok(Some(SystemProxy {
        source: ProxySource::Windows,
        http,
        https,
        bypass,
    }))
}

/// ProxyServer 里 http、https 地址各用的代理：`主机:端口` 用于所有协议；分协议的 `http=…;https=…;socks=…` 中没设的
/// 用 socks 的，其他协议（如 ftp）的用不到。
fn windows_servers(server: &str) -> Result<(Option<Url>, Option<Url>), String> {
    if !server.contains('=') {
        let proxy = windows_proxy(server, "http")?;
        return Ok((Some(proxy.clone()), Some(proxy)));
    }
    let (mut http, mut https, mut socks) = (None, None, None);
    for entry in server.split(';').map(str::trim).filter(|e| !e.is_empty()) {
        let (protocol, address) = entry.split_once('=').ok_or("里有一项不是「协议=地址」")?;
        match protocol.trim().to_ascii_lowercase().as_str() {
            "http" => http = Some(windows_proxy(address, "http")?),
            "https" => https = Some(windows_proxy(address, "http")?),
            "socks" => socks = Some(windows_proxy(address, "socks4")?),
            _ => {}
        }
    }
    Ok((http.or_else(|| socks.clone()), https.or(socks)))
}

/// 没写协议的地址按 `scheme` 补上。
fn windows_proxy(address: &str, scheme: &str) -> Result<Url, String> {
    let address = address.trim();
    if address.contains("://") {
        proxy_url(address)
    } else {
        proxy_url(&format!("{scheme}://{address}"))
    }
}

/// 用不了时为原因（接在「某某设置」之后；不带地址，可能有口令）。
fn proxy_url(text: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|e| format!("的地址无法解析（{e}）"))?;
    match unusable(&url) {
        Some(reason) => Err(format!("：{reason}")),
        None => Ok(url),
    }
}

/// 系统设置里的例外，空项略过；无法识别时为那一项。
fn exceptions<'a>(entries: impl Iterator<Item = &'a str>) -> Result<Vec<Bypass>, &'a str> {
    entries
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| exception(entry).ok_or(entry))
        .collect()
}

fn exception(entry: &str) -> Option<Bypass> {
    if entry.eq_ignore_ascii_case("<local>") {
        return Some(Bypass::SimpleHostnames);
    }
    if let Some(network) = ip_prefix(entry) {
        return Some(network);
    }
    let entry = entry.to_ascii_lowercase();
    if let Some(domain) = entry.strip_prefix('.') {
        return Some(Bypass::Pattern(format!("*.{domain}")));
    }
    if entry.contains(['*', '?']) {
        return Some(Bypass::Pattern(entry));
    }
    match Host::parse(&entry) {
        Ok(Host::Domain(host)) => Some(Bypass::Host(host)),
        _ => None,
    }
}

/// IP 地址或网段；IPv4 可省略后面的 0，省略掩码时按写出的段数（`10` 即 `10.0.0.0/8`）；IPv6 地址可带方括号。
fn ip_prefix(entry: &str) -> Option<Bypass> {
    let (address, prefix) = match entry.split_once('/') {
        Some((address, prefix)) => (address, Some(prefix)),
        None => (entry, None),
    };
    let address = address
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(address);
    let (address, written) = match address.parse::<Ipv6Addr>() {
        Ok(v6) => (IpAddr::V6(v6), 128),
        Err(_) => {
            let parts: Vec<&str> = address.split('.').collect();
            if parts.len() > 4 {
                return None;
            }
            let mut octets = [0u8; 4];
            for (octet, part) in octets.iter_mut().zip(&parts) {
                if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                *octet = part.parse().ok()?;
            }
            (
                IpAddr::V4(Ipv4Addr::from(octets)),
                8 * u8::try_from(parts.len()).ok()?,
            )
        }
    };
    let full = if address.is_ipv4() { 32 } else { 128 };
    let prefix = match prefix {
        None => written,
        Some(prefix) => prefix.parse().ok().filter(|prefix| *prefix <= full)?,
    };
    Some(Bypass::Network { address, prefix })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(host: &str, port: Option<i32>) -> Option<Server> {
        Some(Server {
            host: Some(host.to_owned()),
            port,
        })
    }

    fn urls(proxy: &SystemProxy) -> (Option<String>, Option<String>) {
        let text = |url: &Option<Url>| url.as_ref().map(Url::to_string);
        (text(&proxy.http), text(&proxy.https))
    }

    fn some(url: &str) -> Option<String> {
        Some(url.to_owned())
    }

    fn net(address: &str, prefix: u8) -> Bypass {
        Bypass::Network {
            address: address.parse().unwrap(),
            prefix,
        }
    }

    #[test]
    fn macos_settings_follow_chromium() {
        assert!(from_macos(MacOs::default()).unwrap().is_none());
        // SOCKS 代替没设的；端口没填时用默认端口；IPv6 地址加方括号
        let settings = MacOs {
            http: server("::1", Some(8080)),
            socks: server("127.0.0.1", None),
            exceptions: vec![
                "127.0.0.1".into(),
                "169.254/16".into(),
                "*.Local".into(),
                "localhost".into(),
                ".corp.example".into(),
                "<local>".into(),
                " ".into(),
            ],
            exclude_simple_hostnames: true,
            ..MacOs::default()
        };
        let found = from_macos(settings).unwrap().unwrap();
        assert_eq!(
            urls(&found),
            (some("http://[::1]:8080/"), some("socks5h://127.0.0.1:1080"))
        );
        assert_eq!(
            found.bypass,
            [
                net("127.0.0.1", 32),
                net("169.254.0.0", 16),
                Bypass::Pattern("*.local".into()),
                Bypass::Host("localhost".into()),
                Bypass::Pattern("*.corp.example".into()),
                Bypass::SimpleHostnames,
                Bypass::SimpleHostnames,
            ]
        );
    }

    #[test]
    fn unusable_macos_settings_are_reported() {
        let reason = |settings: MacOs| from_macos(settings).err().unwrap();
        assert!(
            reason(MacOs {
                auto_config: true,
                http: server("p", Some(1)),
                ..MacOs::default()
            })
            .contains("PAC")
        );
        assert!(
            reason(MacOs {
                auto_discovery: true,
                ..MacOs::default()
            })
            .contains("自动发现")
        );
        assert!(
            reason(MacOs {
                https: server("p", Some(70000)),
                ..MacOs::default()
            })
            .contains("70000")
        );
        assert!(
            reason(MacOs {
                socks: Some(Server {
                    host: None,
                    port: Some(1080)
                }),
                ..MacOs::default()
            })
            .contains("没填服务器")
        );
        assert!(
            reason(MacOs {
                http: server("p", Some(1)),
                exceptions: vec!["a b".into()],
                ..MacOs::default()
            })
            .contains("\"a b\"")
        );
    }

    #[test]
    fn windows_settings_follow_wininet() {
        let windows = |server: &str, bypass: &str| Windows {
            enabled: true,
            server: Some(server.to_owned()),
            bypass: Some(bypass.to_owned()),
            auto_config_url: None,
        };
        assert!(
            from_windows(Windows {
                enabled: false,
                ..windows("p:1", "")
            })
            .unwrap()
            .is_none()
        );
        let single = from_windows(windows(
            "127.0.0.1:7890",
            "<local>;10.*;192.168.1.1;*.Example.com",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            urls(&single),
            (
                some("http://127.0.0.1:7890/"),
                some("http://127.0.0.1:7890/")
            )
        );
        assert_eq!(
            single.bypass,
            [
                Bypass::SimpleHostnames,
                Bypass::Pattern("10.*".into()),
                net("192.168.1.1", 32),
                Bypass::Pattern("*.example.com".into()),
            ]
        );
        // 分协议：没设的用 socks 的（SOCKS4），用不到的协议略过，写了协议的照用
        let split = from_windows(windows("ftp=f:21; socks=s:1080;https=https://h:443;", ""))
            .unwrap()
            .unwrap();
        assert_eq!(urls(&split), (some("socks4://s:1080"), some("https://h/")));
        assert!(from_windows(windows("ftp=f:21", "")).unwrap().is_none());
    }

    #[test]
    fn unusable_windows_settings_are_reported() {
        let reason = |server: &str, auto_config_url: Option<&str>| {
            from_windows(Windows {
                enabled: true,
                server: Some(server.to_owned()),
                bypass: None,
                auto_config_url: auto_config_url.map(str::to_owned),
            })
            .err()
            .unwrap()
        };
        assert!(reason("p:1", Some("http://wpad/proxy.pac")).contains("PAC"));
        assert!(reason(" ", None).contains("没填地址"));
        assert!(reason("http=p:1;s:2", None).contains("协议=地址"));
        assert!(reason("http=ftp://user:secret@p:1", None).contains("只支持"));
        assert!(!reason("http=ftp://user:secret@p:1", None).contains("secret"));
    }
}
