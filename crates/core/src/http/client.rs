//! HTTP 客户端：按任务的超时、TLS、Cookie 与代理设置建；跟系统代理时随系统设置更新。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::HeaderValue;
use tokio::time::Instant;
use url::Url;

use super::chain_text;
use crate::Error;
use crate::ident::bare_url;
use crate::proxy::{self, SystemProxy};
use crate::request::{HttpOptions, Proxy, Timeouts};

/// 系统代理多久重读一次：设置改了（如代理软件换了端口）几秒内新的请求就照新设置走，又不必每个请求都读。
const SYSTEM_PROXY_TTL: Duration = Duration::from_secs(5);

/// 本任务的 Cookie 存储：调用方给的（见 [`crate::CookieStore`]）或本任务自己的，换客户端时沿用。
pub(super) struct Cookies(pub(super) Box<dyn reqwest::cookie::CookieStore>);

impl reqwest::cookie::CookieStore for Cookies {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        self.0.set_cookies(headers, url);
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        self.0.cookies(url)
    }
}

/// 建客户端要的设置（代理除外）。
struct Config {
    timeouts: Timeouts,
    insecure: bool,
    cookies: Arc<Cookies>,
}

/// 请求经不经代理、经哪个。
enum Route<'a> {
    Direct,
    Fixed(&'a Url),
    System(&'a SystemProxy),
}

/// 发请求用的客户端。
pub(super) struct Clients(Kind);

enum Kind {
    /// 代理固定：直连或调用方给的地址
    Fixed(reqwest::Client),
    /// 跟系统代理（[`Proxy::System`]）
    System(Box<Following>),
}

/// 跟系统代理时的客户端。
struct Following {
    config: Config,
    current: Mutex<Current>,
}

/// 跟系统代理时当前的客户端。
struct Current {
    /// 读系统代理的时刻
    read_at: Instant,
    proxy: Option<SystemProxy>,
    client: reqwest::Client,
}

impl Clients {
    /// 按 `options`（已校验过）建；跟系统代理时读一次系统代理，读不出或值用不了时报 [`Error::SystemProxy`]。
    pub(super) fn new(options: &HttpOptions, cookies: Cookies) -> Result<Self, Error> {
        let config = Config {
            timeouts: options.timeouts,
            insecure: options.insecure,
            cookies: Arc::new(cookies),
        };
        Ok(Clients(match &options.proxy {
            Proxy::Direct => Kind::Fixed(build(&config, Route::Direct)?),
            Proxy::Url(url) => Kind::Fixed(build(&config, Route::Fixed(url))?),
            Proxy::System => {
                let proxy = proxy::system_proxy()?;
                let client = build(&config, system_route(proxy.as_ref()))?;
                let current = Current {
                    read_at: Instant::now(),
                    proxy,
                    client,
                };
                Kind::System(Box::new(Following {
                    config,
                    current: Mutex::new(current),
                }))
            }
        }))
    }

    /// 这次尝试用的客户端。跟系统代理时距上次读取满 [`SYSTEM_PROXY_TTL`] 就重读，设置变了就换一个按新设置建的；
    /// 读不出或值用不了时报 [`Error::SystemProxy`]，同开始任务时。
    pub(super) fn get(&self) -> Result<reqwest::Client, Error> {
        let following = match &self.0 {
            Kind::Fixed(client) => return Ok(client.clone()),
            Kind::System(following) => following,
        };
        let mut current = following
            .current
            .lock()
            .expect("持锁期间只读系统代理、建客户端，不会 panic");
        let now = Instant::now();
        if now.duration_since(current.read_at) >= SYSTEM_PROXY_TTL {
            let proxy = proxy::system_proxy()?;
            if proxy != current.proxy {
                current.client = build(&following.config, system_route(proxy.as_ref()))?;
                current.proxy = proxy;
            }
            current.read_at = now;
        }
        Ok(current.client.clone())
    }
}

fn system_route(proxy: Option<&SystemProxy>) -> Route<'_> {
    proxy.map_or(Route::Direct, Route::System)
}

fn build(config: &Config, route: Route<'_>) -> Result<reqwest::Client, Error> {
    let t = config.timeouts;
    // 不自动写 Referer：开着时 reqwest 跟随重定向会把它换成上一跳的地址，盖掉调用方给的
    let builder = reqwest::Client::builder()
        .user_agent(concat!("hs-m3u8/", env!("CARGO_PKG_VERSION")))
        .referer(false)
        .cookie_provider(config.cookies.clone())
        .connect_timeout(t.connect)
        .read_timeout(t.read_idle)
        .timeout(t.request)
        .tls_danger_accept_invalid_certs(config.insecure);
    let builder = match route {
        Route::Direct => builder.no_proxy(),
        Route::Fixed(url) => {
            let proxy = reqwest::Proxy::all(url.as_str()).map_err(|e| {
                Error::InvalidInput(format!(
                    "代理地址不可用 {}：{}",
                    bare_url(url),
                    e.without_url()
                ))
            })?;
            builder.proxy(proxy)
        }
        Route::System(system) => {
            let system = system.clone();
            builder.proxy(reqwest::Proxy::custom(move |url| {
                system.proxy_for(url).cloned()
            }))
        }
    };
    builder
        .build()
        .map_err(|e| Error::HttpClientInit(chain_text(&e)))
}
