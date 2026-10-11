//! 系统代理（[`crate::Proxy::System`]）：由本库读取并逐项检查，用不了的值报错。不交给 HTTP 库去读：它读到用不了的
//! 值时不报错、请求直连，用户却以为走了代理。
//!
//! 先看环境变量（见 [`mod@env`]）；环境变量里没设代理时看操作系统的设置（见 [`system`]）：macOS 的系统设置、Windows 的
//! Internet 选项，其他平台即直连。

mod env;
mod system;

use std::net::IpAddr;

use url::{Host, Url};

use crate::Error;
use crate::ident::bare_url;

/// 系统代理设置。不实现 `Debug`：代理地址可能带口令。
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SystemProxy {
    /// 从哪里读到的
    pub source: ProxySource,
    /// 访问 http 地址时经的代理；None 时直连
    pub http: Option<Url>,
    /// 访问 https 地址时经的代理；None 时直连
    pub https: Option<Url>,
    /// 不经代理的主机：任一条匹配即直连
    pub bypass: Vec<Bypass>,
}

impl SystemProxy {
    /// 访问 `url` 时经的代理；不经代理时为 None。
    pub fn proxy_for(&self, url: &Url) -> Option<&Url> {
        let proxy = match url.scheme() {
            "http" => self.http.as_ref(),
            "https" => self.https.as_ref(),
            _ => None,
        }?;
        let host = url.host()?;
        (!self.bypass.iter().any(|rule| rule.matches(&host))).then_some(proxy)
    }
}

/// 系统代理设置从哪里读到。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProxySource {
    /// 环境变量（`http_proxy`、`https_proxy`、`all_proxy`、`no_proxy` 等）
    Environment,
    /// macOS 的系统设置（网络 → 代理）
    MacOs,
    /// Windows 的 Internet 选项（局域网设置 → 代理服务器）
    Windows,
}

/// 一条不经代理的规则；主机名不区分大小写。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Bypass {
    /// 所有主机
    All,
    /// 这个域名及其子域名；小写，不含首尾的点
    Domain(String),
    /// 恰好这个主机名；小写
    Host(String),
    /// 通配符：`*` 匹配任意个字符，`?` 匹配一个，对整个主机名（IPv6 地址带方括号）；小写
    Pattern(String),
    /// 这个网段里的 IP 地址；单个地址的前缀为全长
    Network { address: IpAddr, prefix: u8 },
    /// 不含点的主机名（如 `localhost`、内网机器名）
    SimpleHostnames,
}

impl Bypass {
    fn matches(&self, host: &Host<&str>) -> bool {
        match (self, host) {
            (Bypass::All, _) => true,
            (Bypass::Domain(domain), Host::Domain(name)) => {
                let name = name.trim_end_matches('.');
                let Some(start) = name.len().checked_sub(domain.len()) else {
                    return false;
                };
                name.get(start..)
                    .is_some_and(|tail| tail.eq_ignore_ascii_case(domain))
                    && (start == 0 || name.as_bytes()[start - 1] == b'.')
            }
            (Bypass::Host(expected), Host::Domain(name)) => {
                name.trim_end_matches('.').eq_ignore_ascii_case(expected)
            }
            (Bypass::Pattern(pattern), _) => glob(pattern, &host.to_string().to_ascii_lowercase()),
            (Bypass::Network { address, prefix }, Host::Ipv4(ip)) => {
                in_network(IpAddr::V4(*ip), *address, *prefix)
            }
            (Bypass::Network { address, prefix }, Host::Ipv6(ip)) => {
                in_network(IpAddr::V6(*ip), *address, *prefix)
            }
            (Bypass::SimpleHostnames, Host::Domain(name)) => !name.contains('.'),
            _ => false,
        }
    }
}

fn in_network(ip: IpAddr, network: IpAddr, prefix: u8) -> bool {
    match (ip, network) {
        (IpAddr::V4(ip), IpAddr::V4(network)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            u32::from(ip) & mask == u32::from(network) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(network)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            u128::from(ip) & mask == u128::from(network) & mask
        }
        _ => false,
    }
}

/// `pattern` 里 `*` 匹配任意个字符，`?` 匹配一个，其余按原样比较；调用方先把两边都转成小写。
fn glob(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    // 最近的 `*` 在 pattern 中的位置，与它当时对应到 text 的位置：失配时让它多吞一个字符再试
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(&c) if c == '?' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((star_p, star_t)) => {
                    star = Some((star_p, star_t + 1));
                    p = star_p + 1;
                    t = star_t + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// 本机的系统代理设置，见模块说明；没设代理时为 None。读不出或读到用不了的值时报 [`Error::InvalidInput`]，说明是
/// 哪一项。
pub(crate) fn system_proxy() -> Result<Option<SystemProxy>, Error> {
    let found = match env::read(&env_var) {
        Ok(None) => system::read(),
        found => found,
    };
    found.map_err(|reason| {
        Error::InvalidInput(format!(
            "系统代理设置用不了，可改为直连或指定代理地址：{reason}"
        ))
    })
}

/// 环境变量的值；没设时为 None，不是 UTF-8 时为原因。
fn env_var(name: &str) -> Result<Option<String>, String> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("环境变量 {name} 不是 UTF-8")),
    }
}

/// 代理地址 HTTP 库用不用得上；用不上时为原因。它用不上的地址（协议不认识，或按 `http::Uri` 解析不出主机，如
/// socks 地址里的国际化域名）不报错，只是不用代理、请求直连，用户却以为走了代理；这里按同样的条件先拒绝。
pub(crate) fn unusable(proxy: &Url) -> Option<String> {
    if !matches!(
        proxy.scheme(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    ) {
        return Some(format!(
            "代理只支持 http、https、socks4、socks4a、socks5、socks5h：{}",
            bare_url(proxy)
        ));
    }
    let usable = proxy
        .as_str()
        .parse::<http::Uri>()
        .is_ok_and(|uri| uri.authority().is_some());
    (!usable).then(|| {
        format!(
            "代理地址的主机名不合法（国际化域名须先转成 punycode）：{}",
            bare_url(proxy)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(bypass: Vec<Bypass>) -> SystemProxy {
        SystemProxy {
            source: ProxySource::Environment,
            http: Some(Url::parse("http://proxy.example:8080").unwrap()),
            https: None,
            bypass,
        }
    }

    fn via(proxy: &SystemProxy, url: &str) -> bool {
        proxy.proxy_for(&Url::parse(url).unwrap()).is_some()
    }

    #[test]
    fn each_scheme_uses_its_own_proxy() {
        let proxy = proxy(Vec::new());
        assert!(via(&proxy, "http://a.example/x.m3u8"));
        assert!(!via(&proxy, "https://a.example/x.m3u8"));
    }

    #[test]
    fn bypass_rules_match_as_documented() {
        let cases = [
            (Bypass::All, "http://anything/", true),
            (
                Bypass::Domain("example.com".into()),
                "http://EXAMPLE.com./",
                true,
            ),
            (
                Bypass::Domain("example.com".into()),
                "http://a.b.example.com/",
                true,
            ),
            (
                Bypass::Domain("example.com".into()),
                "http://notexample.com/",
                false,
            ),
            (
                Bypass::Host("example.com".into()),
                "http://Example.com/",
                true,
            ),
            (
                Bypass::Host("example.com".into()),
                "http://a.example.com/",
                false,
            ),
            (Bypass::Pattern("*.local".into()), "http://box.LOCAL/", true),
            (Bypass::Pattern("*.local".into()), "http://local/", false),
            (Bypass::Pattern("10.*".into()), "http://10.1.2.3/", true),
            (Bypass::Pattern("a?c".into()), "http://abc/", true),
            (Bypass::Pattern("a?c".into()), "http://ac/", false),
            (Bypass::Pattern("*a*b".into()), "http://xaxxb/", true),
            (Bypass::Pattern("*a*b".into()), "http://xaxxbc/", false),
            (net("10.0.0.0", 8), "http://10.255.0.1/", true),
            (net("10.0.0.0", 8), "http://11.0.0.1/", false),
            (net("0.0.0.0", 0), "http://203.0.113.9/", true),
            (net("127.0.0.1", 32), "http://127.0.0.1:9/", true),
            (net("fe80::", 10), "http://[fe80::1]/", true),
            (net("fe80::", 10), "http://[fec0::1]/", false),
            (net("10.0.0.0", 8), "http://[::1]/", false),
            (Bypass::SimpleHostnames, "http://intranet/", true),
            (Bypass::SimpleHostnames, "http://a.example/", false),
            (Bypass::SimpleHostnames, "http://10.0.0.1/", false),
        ];
        for (rule, url, bypassed) in cases {
            let label = format!("{rule:?} {url}");
            assert_eq!(!via(&proxy(vec![rule]), url), bypassed, "{label}");
        }
    }

    fn net(address: &str, prefix: u8) -> Bypass {
        Bypass::Network {
            address: address.parse().unwrap(),
            prefix,
        }
    }
}
