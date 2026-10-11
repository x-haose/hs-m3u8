//! 环境变量里的代理设置，规则同 curl：
//! - http 地址看 `http_proxy`，https 地址看 `https_proxy`、`HTTPS_PROXY`，都没设时看 `all_proxy`、`ALL_PROXY`；同一项
//!   小写的优先，设了就用它，值为空即这类地址不经代理；
//! - 大写的 `HTTP_PROXY` curl 不认，这里与 Python 等一样也认，只在设了 `REQUEST_METHOD` 时不认（CGI 程序里，请求头
//!   `Proxy` 会变成这个变量）；
//! - 值没写协议时为 http；
//! - `no_proxy`、`NO_PROXY` 为逗号分隔的例外：`*`、域名（含其子域名，可带开头的 `.` 或 `*.`）、IP 地址、CIDR 网段。

use url::{Host, Url};

use super::{Bypass, ProxySource, SystemProxy, unusable};

/// 读环境变量：没设时为 None，值不是 UTF-8 时为原因。
pub(super) type Lookup<'a> = &'a dyn Fn(&str) -> Result<Option<String>, String>;

/// 一个设了的变量：名字与值。
type Found = Option<(&'static str, String)>;

/// 环境变量里的代理设置；各代理变量都没设或为空时为 None，此时看操作系统的设置。用不了时为原因。
pub(super) fn read(var: Lookup<'_>) -> Result<Option<SystemProxy>, String> {
    let http_names: &[&'static str] = if var("REQUEST_METHOD")?.is_some() {
        &["http_proxy"]
    } else {
        &["http_proxy", "HTTP_PROXY"]
    };
    let all = first(var, &["all_proxy", "ALL_PROXY"])?;
    let http = first(var, http_names)?.or_else(|| all.clone());
    let https = first(var, &["https_proxy", "HTTPS_PROXY"])?.or(all);
    let set = |found: &Found| found.as_ref().is_some_and(|(_, value)| !value.is_empty());
    if !set(&http) && !set(&https) {
        return Ok(None);
    }
    let bypass = match first(var, &["no_proxy", "NO_PROXY"])? {
        Some((name, value)) => exceptions(name, &value)?,
        None => Vec::new(),
    };
    Ok(Some(SystemProxy {
        source: ProxySource::Environment,
        http: proxy(http)?,
        https: proxy(https)?,
        bypass,
    }))
}

/// `names` 里第一个设了的变量。
fn first(var: Lookup<'_>, names: &[&'static str]) -> Result<Found, String> {
    for &name in names {
        if let Some(value) = var(name)? {
            return Ok(Some((name, value)));
        }
    }
    Ok(None)
}

/// 代理变量的值；值为空时为 None。
fn proxy(found: Found) -> Result<Option<Url>, String> {
    let Some((name, value)) = found else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let text = if value.contains("://") {
        value
    } else {
        format!("http://{value}")
    };
    // 原因里不带值：可能有口令
    let url =
        Url::parse(&text).map_err(|e| format!("环境变量 {name} 的代理地址无法解析（{e}）"))?;
    match unusable(&url) {
        Some(reason) => Err(format!("环境变量 {name}：{reason}")),
        None => Ok(Some(url)),
    }
}

fn exceptions(name: &str, value: &str) -> Result<Vec<Bypass>, String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            exception(entry).ok_or_else(|| {
                format!(
                    "环境变量 {name} 里的 {entry:?} 无法识别（应为 *、域名、IP 地址或 CIDR 网段）"
                )
            })
        })
        .collect()
}

fn exception(entry: &str) -> Option<Bypass> {
    if entry == "*" {
        return Some(Bypass::All);
    }
    if let Some(network) = network(entry) {
        return Some(network);
    }
    let domain = entry.strip_prefix("*.").unwrap_or(entry).trim_matches('.');
    match Host::parse(domain) {
        Ok(Host::Domain(domain)) if !domain.contains('*') => Some(Bypass::Domain(domain)),
        _ => None,
    }
}

/// IP 地址或 CIDR 网段（IPv6 地址可带方括号）。
fn network(entry: &str) -> Option<Bypass> {
    let (address, prefix) = match entry.split_once('/') {
        Some((address, prefix)) => (address, Some(prefix)),
        None => (entry, None),
    };
    let address: std::net::IpAddr = address
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(address)
        .parse()
        .ok()?;
    let full = if address.is_ipv4() { 32 } else { 128 };
    let prefix = match prefix {
        None => full,
        Some(prefix) => prefix.parse().ok().filter(|prefix| *prefix <= full)?,
    };
    Some(Bypass::Network { address, prefix })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn read_from(vars: &[(&str, &str)]) -> Result<Option<SystemProxy>, String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        read(&|name| Ok(vars.get(name).cloned()))
    }

    fn proxies(vars: &[(&str, &str)]) -> (Option<String>, Option<String>) {
        let found = read_from(vars).unwrap().expect("设了代理");
        let text = |url: Option<Url>| url.map(String::from);
        (text(found.http), text(found.https))
    }

    fn some(url: &str) -> Option<String> {
        Some(url.to_owned())
    }

    #[test]
    fn variables_are_chosen_as_curl_does() {
        assert!(read_from(&[]).unwrap().is_none());
        assert!(
            read_from(&[("http_proxy", ""), ("no_proxy", "*")])
                .unwrap()
                .is_none()
        );
        // 小写的优先；没写协议为 http
        assert_eq!(
            proxies(&[("http_proxy", "lower:1"), ("HTTP_PROXY", "http://upper:2")]),
            (some("http://lower:1/"), None)
        );
        // all_proxy 补上没设的，设了但为空的不补
        assert_eq!(
            proxies(&[
                ("ALL_PROXY", "socks5h://all:3"),
                ("https_proxy", "http://s:4")
            ]),
            (some("socks5h://all:3"), some("http://s:4/"))
        );
        assert_eq!(
            proxies(&[("all_proxy", "http://all:3"), ("http_proxy", "")]),
            (None, some("http://all:3/"))
        );
        // CGI 程序里不认大写的 HTTP_PROXY
        assert_eq!(
            proxies(&[
                ("HTTP_PROXY", "http://upper:2"),
                ("HTTPS_PROXY", "http://s:4")
            ]),
            (some("http://upper:2/"), some("http://s:4/"))
        );
        assert_eq!(
            proxies(&[
                ("HTTP_PROXY", "http://upper:2"),
                ("HTTPS_PROXY", "http://s:4"),
                ("REQUEST_METHOD", "GET")
            ]),
            (None, some("http://s:4/"))
        );
    }

    #[test]
    fn unusable_values_are_reported_by_name_without_the_value() {
        for value in [
            "sock5://user:secret@p:1",
            "socks5://例子:1",
            "http://user:secret@[bad",
        ] {
            let reason = read_from(&[("https_proxy", value)]).err().unwrap();
            assert!(reason.contains("https_proxy"), "{reason}");
            assert!(!reason.contains("secret"), "{reason}");
        }
        let reason = read_from(&[("http_proxy", "p:1"), ("no_proxy", "a.example, b:80")])
            .err()
            .unwrap();
        assert!(reason.contains("\"b:80\""), "{reason}");
    }

    #[test]
    fn exceptions_are_parsed_as_curl_does() {
        let found = read_from(&[
            ("http_proxy", "p:1"),
            (
                "NO_PROXY",
                "*, .Example.com., *.b.example, 10.0.0.0/8, [::1], 192.0.2.1, ,",
            ),
        ])
        .unwrap()
        .unwrap();
        let domain = |name: &str| Bypass::Domain(name.into());
        assert_eq!(
            found.bypass,
            [
                Bypass::All,
                domain("example.com"),
                domain("b.example"),
                Bypass::Network {
                    address: "10.0.0.0".parse().unwrap(),
                    prefix: 8
                },
                Bypass::Network {
                    address: "::1".parse().unwrap(),
                    prefix: 128
                },
                Bypass::Network {
                    address: "192.0.2.1".parse().unwrap(),
                    prefix: 32
                },
            ]
        );
    }
}
