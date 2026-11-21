//! 请求按桌面版 Chrome 补的请求头：调用方与回调没给的才补；它们给了 User-Agent 时一个都不补（请求冒充的是那个客户端，
//! 补上 Chrome 的会与它对不上）。Accept-Encoding 固定为 identity（本库要原样的字节），与浏览器不同。
//!
//! - 都补：User-Agent（本机平台的 Chrome）、Accept-Language（系统的首选语言；读不出或为 `C`、`POSIX` 时同 Chrome 在这种
//!   系统上的 `en-US,en;q=0.9`）；https 地址另补 Chrome 的 User-Agent Client Hints（Sec-CH-UA、Sec-CH-UA-Mobile、
//!   Sec-CH-UA-Platform，Chrome 只发给安全来源）。Accept 照 HTTP 库默认的 `*/*`（下载器的习惯：图床等按 Accept 里有没有
//!   `text/html` 决定给文件本身还是跳到网页），不补 Sec-Fetch-*（不知道发起请求的网页是哪个）。
//! - 按在地址栏打开发的请求（`navigate`）另补网页导航的 Accept 与 Upgrade-Insecure-Requests；https 地址在没给 Referer
//!   与任一 Sec-Fetch-* 时另补 Sec-Fetch-* 四项（Site 为 none：不是从别的网页发起的）。给了 Referer 时不知道与它是不是
//!   同一站点，给了某一项时补齐的会与它对不上，都不补。
//!
//! 简化：按发出的第一个地址判断 https 与否，跟随重定向时不重算（http 跳到 https 后缺 Client Hints，反过来多出）；网站
//! 按这一点拦截时改为自己跟随重定向、逐跳重算。

use std::sync::LazyLock;

use url::Url;

/// Chrome 稳定版的主版本。User-Agent 里只写主版本，其余固定为 0.0.0。
///
/// 简化：写死，本库发版时核对；落后到被网站按版本拦截时改为按 Chrome 的发布节奏推算。
const MAJOR: u32 = 159;

/// 本机平台在 User-Agent 里的写法与 Sec-CH-UA-Platform 的值：Chrome 在 macOS 上一律写 Intel 与 10_15_7，在 Windows 上
/// 一律写 NT 10.0 与 x64。
#[cfg(target_os = "macos")]
const PLATFORM: (&str, &str) = ("Macintosh; Intel Mac OS X 10_15_7", "macOS");
#[cfg(target_os = "windows")]
const PLATFORM: (&str, &str) = ("Windows NT 10.0; Win64; x64", "Windows");
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const PLATFORM: (&str, &str) = ("X11; Linux x86_64", "Linux");

/// Chrome 在地址栏打开网址时的 Accept。
const NAVIGATE_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7";

static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Mozilla/5.0 ({}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{MAJOR}.0.0.0 Safari/537.36",
        PLATFORM.0
    )
});

static BRANDS: LazyLock<String> = LazyLock::new(|| brands(MAJOR));

/// Chrome 在系统语言读不出或为 `C`、`POSIX` 时用的。
const FALLBACK_LANGUAGE: &str = "en-US,en;q=0.9";

static ACCEPT_LANGUAGE: LazyLock<String> = LazyLock::new(|| {
    sys_locale::get_locale()
        .and_then(|locale| accept_language(&locale))
        .unwrap_or_else(|| FALLBACK_LANGUAGE.to_owned())
});

/// 发往 `url` 的请求要补的请求头，`navigate` 为按在地址栏打开发；`given` 为调用方与回调给的（见模块说明）。
pub(super) fn headers(
    navigate: bool,
    url: &Url,
    given: &[(String, String)],
) -> Vec<(&'static str, String)> {
    let has = |name: &str| given.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
    if has("user-agent") {
        return Vec::new();
    }
    let secure = url.scheme() == "https";
    let mut headers = Vec::new();
    if secure {
        headers.extend([
            ("Sec-CH-UA", BRANDS.clone()),
            ("Sec-CH-UA-Mobile", "?0".to_owned()),
            ("Sec-CH-UA-Platform", format!("\"{}\"", PLATFORM.1)),
        ]);
    }
    if navigate {
        headers.push(("Upgrade-Insecure-Requests", "1".to_owned()));
    }
    headers.push(("User-Agent", USER_AGENT.clone()));
    if navigate {
        headers.push(("Accept", NAVIGATE_ACCEPT.to_owned()));
    }
    let fetch_metadata = given.iter().any(|(n, _)| {
        n.get(..10)
            .is_some_and(|p| p.eq_ignore_ascii_case("sec-fetch-"))
    });
    if navigate && secure && !has("referer") && !fetch_metadata {
        headers.extend([
            ("Sec-Fetch-Site", "none".to_owned()),
            ("Sec-Fetch-Mode", "navigate".to_owned()),
            ("Sec-Fetch-User", "?1".to_owned()),
            ("Sec-Fetch-Dest", "document".to_owned()),
        ]);
    }
    headers.push(("Accept-Language", ACCEPT_LANGUAGE.clone()));
    headers.retain(|(name, _)| !has(name));
    headers
}

/// 主版本为 `major` 的 Chrome 的 Sec-CH-UA：GREASE 品牌、Chromium、Google Chrome 三项，GREASE 品牌的写法、版本与三项的
/// 顺序都由主版本决定，同 Chromium 的 `components/embedder_support/user_agent_utils.cc`。
fn brands(major: u32) -> String {
    const GREASEY: [char; 11] = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'];
    const GREASED_VERSIONS: [&str; 3] = ["8", "99", "24"];
    const ORDERS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let seed = major as usize;
    let grease = format!(
        "Not{}A{}Brand",
        GREASEY[seed % GREASEY.len()],
        GREASEY[(seed + 1) % GREASEY.len()]
    );
    let major = major.to_string();
    let listed = [
        (
            grease.as_str(),
            GREASED_VERSIONS[seed % GREASED_VERSIONS.len()],
        ),
        ("Chromium", major.as_str()),
        ("Google Chrome", major.as_str()),
    ];
    let order = ORDERS[seed % ORDERS.len()];
    let mut shuffled = [("", ""); 3];
    for (brand, &at) in listed.into_iter().zip(&order) {
        shuffled[at] = brand;
    }
    shuffled
        .iter()
        .map(|(brand, version)| format!("\"{brand}\";v=\"{version}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 系统首选语言 `locale`（BCP 47，如 `zh-Hant-HK`；也认 POSIX 的 `zh_CN.UTF-8`）对应的 Accept-Language：有地区时为
/// `语言-地区,语言;q=0.9`，没有时为语言本身；去掉书写系统等其余部分。认不出语言（含 `C`、`POSIX`）时为 None。
fn accept_language(locale: &str) -> Option<String> {
    let tag = locale.split(['.', '@']).next()?;
    let mut parts = tag.split(['-', '_']);
    let language = parts.next()?.to_ascii_lowercase();
    if !(2..=3).contains(&language.len()) || !language.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    let region = parts.find(|part| {
        (part.len() == 2 && part.bytes().all(|b| b.is_ascii_alphabetic()))
            || (part.len() == 3 && part.bytes().all(|b| b.is_ascii_digit()))
    });
    Some(match region {
        Some(region) => format!(
            "{language}-{},{language};q=0.9",
            region.to_ascii_uppercase()
        ),
        None => language,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 Chrome 120、124、131 实际发出的 Sec-CH-UA 逐字相同。
    #[test]
    fn brands_follow_chromes_grease_rules() {
        assert_eq!(
            brands(120),
            r#""Not_A Brand";v="8", "Chromium";v="120", "Google Chrome";v="120""#
        );
        assert_eq!(
            brands(124),
            r#""Chromium";v="124", "Google Chrome";v="124", "Not-A.Brand";v="99""#
        );
        assert_eq!(
            brands(131),
            r#""Google Chrome";v="131", "Chromium";v="131", "Not_A Brand";v="24""#
        );
    }

    #[test]
    fn accept_language_keeps_language_and_region() {
        assert_eq!(
            accept_language("zh-Hant-HK").as_deref(),
            Some("zh-HK,zh;q=0.9")
        );
        assert_eq!(accept_language("en-US").as_deref(), Some("en-US,en;q=0.9"));
        assert_eq!(
            accept_language("de_DE.UTF-8").as_deref(),
            Some("de-DE,de;q=0.9")
        );
        assert_eq!(
            accept_language("es-419").as_deref(),
            Some("es-419,es;q=0.9")
        );
        assert_eq!(accept_language("ja").as_deref(), Some("ja"));
        assert_eq!(accept_language("zh-Hans").as_deref(), Some("zh"));
        for unknown in ["C", "POSIX", "", "C.UTF-8"] {
            assert_eq!(accept_language(unknown), None, "{unknown}");
        }
    }

    fn names(headers: &[(&'static str, String)]) -> Vec<&'static str> {
        headers.iter().map(|(name, _)| *name).collect()
    }

    #[test]
    fn navigation_over_https_looks_like_typing_the_address() {
        let url = Url::parse("https://a.example/watch").unwrap();
        let headers = headers(true, &url, &[]);
        let expected = vec![
            "Sec-CH-UA",
            "Sec-CH-UA-Mobile",
            "Sec-CH-UA-Platform",
            "Upgrade-Insecure-Requests",
            "User-Agent",
            "Accept",
            "Sec-Fetch-Site",
            "Sec-Fetch-Mode",
            "Sec-Fetch-User",
            "Sec-Fetch-Dest",
            "Accept-Language",
        ];
        assert_eq!(names(&headers), expected);
    }

    /// 不按地址栏打开时 Accept 留给 HTTP 库（`*/*`），不补导航的头；http 地址没有 Client Hints 与 Sec-Fetch-*。
    #[test]
    fn other_requests_keep_the_downloaders_accept() {
        let secure = Url::parse("https://a.example/a.jpg").unwrap();
        assert_eq!(
            names(&headers(false, &secure, &[])),
            [
                "Sec-CH-UA",
                "Sec-CH-UA-Mobile",
                "Sec-CH-UA-Platform",
                "User-Agent",
                "Accept-Language"
            ]
        );
        let plain = Url::parse("http://a.example/watch").unwrap();
        let navigation = headers(true, &plain, &[]);
        assert!(
            names(&navigation)
                .iter()
                .all(|name| !name.starts_with("Sec-"))
        );
        assert!(names(&navigation).contains(&"Accept"));
    }

    /// 给了的不补；给了 Referer 或任一 Sec-Fetch-* 时 Sec-Fetch-* 一个都不补；给了 User-Agent 时什么都不补。
    #[test]
    fn given_headers_win() {
        let url = Url::parse("https://a.example/watch").unwrap();
        let given = |name: &str| vec![(name.to_owned(), "x".to_owned())];
        let accept = headers(true, &url, &given("accept"));
        assert!(!names(&accept).contains(&"Accept"));
        for name in ["Referer", "sec-fetch-mode"] {
            let headers = headers(true, &url, &given(name));
            assert!(
                names(&headers).iter().all(|n| !n.starts_with("Sec-Fetch-")),
                "{name}"
            );
            assert!(names(&headers).contains(&"Sec-CH-UA"), "{name}");
        }
        assert!(headers(true, &url, &given("User-Agent")).is_empty());
    }
}
