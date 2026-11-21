//! 请求按桌面版 Chrome 补的请求头：调用方与回调没给的才补；它们给了 User-Agent 时一个都不补（请求冒充的是那个客户端，
//! 补上 Chrome 的会与它对不上）。Accept-Encoding 固定为 identity（本库要原样的字节），与浏览器不同。
//!
//! - 都补：User-Agent（本机平台的 Chrome）、Accept-Language（系统的首选语言，读不出时不补）；https 地址另补 Chrome 的
//!   User-Agent Client Hints（Sec-CH-UA、Sec-CH-UA-Mobile、Sec-CH-UA-Platform，Chrome 只发给安全来源）。
//! - [`Purpose::File`]（认出地址、文件任务、取回小资源）按在地址栏打开它补：网页导航的 Accept 与
//!   Upgrade-Insecure-Requests；https 地址在没给 Referer 时另补 Sec-Fetch-*（Site 为 none：不是从别的网页发起的）。
//!   给了 Referer 时不知道与它是不是同一站点，不补 Sec-Fetch-*。
//! - 其余用途（播放列表、key、init 段、分片、调用接口）是网页里脚本发的请求，Accept 照 HTTP 库默认的 `*/*`；不知道
//!   发起它的网页是哪个，不补 Sec-Fetch-*。

use std::sync::LazyLock;

use url::Url;

use crate::hooks::Purpose;

/// Chrome 稳定版的主版本（2026-11-18 发布 159.0.8154.13）。User-Agent 里只写主版本，其余固定为 0.0.0。
///
/// 简化：写死，随本库发版更新；网站开始按版本过旧拦截时改为按日期推算。
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

static ACCEPT_LANGUAGE: LazyLock<Option<String>> =
    LazyLock::new(|| sys_locale::get_locale().and_then(|locale| accept_language(&locale)));

/// 用途为 `purpose`、发往 `url` 的请求要补的请求头；`given` 为调用方与回调给的（见模块说明）。
pub(super) fn headers(
    purpose: Purpose,
    url: &Url,
    given: &[(String, String)],
) -> Vec<(&'static str, String)> {
    let has = |name: &str| given.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
    if has("user-agent") {
        return Vec::new();
    }
    let secure = url.scheme() == "https";
    let navigate = purpose == Purpose::File;
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
    if let Some(language) = ACCEPT_LANGUAGE.as_ref() {
        headers.push(("Accept-Language", language.clone()));
    }
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
        let headers = headers(Purpose::File, &url, &[]);
        let mut expected = vec![
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
        ];
        if ACCEPT_LANGUAGE.is_some() {
            expected.push("Accept-Language");
        }
        assert_eq!(names(&headers), expected);
    }

    #[test]
    fn plain_http_gets_no_client_hints_or_fetch_metadata() {
        let url = Url::parse("http://a.example/watch").unwrap();
        let headers = headers(Purpose::File, &url, &[]);
        assert!(names(&headers).iter().all(|name| !name.starts_with("Sec-")));
        let playlist = super::headers(Purpose::Playlist, &url, &[]);
        assert!(!names(&playlist).contains(&"Accept"));
        assert!(!names(&playlist).contains(&"Upgrade-Insecure-Requests"));
    }

    /// 给了的不补；给了 Referer 或任一 Sec-Fetch-* 时 Sec-Fetch-* 一个都不补；给了 User-Agent 时什么都不补。
    #[test]
    fn given_headers_win() {
        let url = Url::parse("https://a.example/watch").unwrap();
        let given = |name: &str| vec![(name.to_owned(), "x".to_owned())];
        let accept = headers(Purpose::File, &url, &given("accept"));
        assert!(!names(&accept).contains(&"Accept"));
        for name in ["Referer", "sec-fetch-mode"] {
            let headers = headers(Purpose::File, &url, &given(name));
            assert!(
                names(&headers).iter().all(|n| !n.starts_with("Sec-Fetch-")),
                "{name}"
            );
            assert!(names(&headers).contains(&"Sec-CH-UA"), "{name}");
        }
        assert!(headers(Purpose::File, &url, &given("User-Agent")).is_empty());
    }
}
