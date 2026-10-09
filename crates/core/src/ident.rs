//! 身份与摘要：判定「是不是同一个来源、同一个分片、同一个 init 段」，以及错误信息里地址的写法。
//!
//! 指纹与摘要都基于 SHA-256，写进 job.json 与文件名，属于任务目录格式：算法或输入的编码改变时，
//! 必须同时升 job.json 的格式版本。

use std::fmt;

use hs_m3u8_hls::{ByteRange, Preference, VariantChoice};
use sha2::{Digest, Sha256};
use url::Url;

/// 64 位指纹：SHA-256 的前 8 字节，文件名中写作 16 位小写十六进制。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Fingerprint(u64);

impl Fingerprint {
    /// 内容的指纹：init 段按内容命名，地址不同、内容相同的共用一个文件。
    pub(crate) fn of_content(data: &[u8]) -> Self {
        let hash = Sha256::digest(data);
        let head: [u8; 8] = hash[..8].try_into().expect("SHA-256 有 32 字节");
        Fingerprint(u64::from_be_bytes(head))
    }

    /// 分片的身份：地址的最后一段（不含查询串）与字节范围。CDN 常在主机、路径前段或查询串里放令牌，
    /// 每次刷新、每次会话都可能不同，最后一段才稳定。
    pub(crate) fn of_segment(uri: &Url, range: Option<ByteRange>) -> Self {
        let range = range.map_or_else(String::new, |r| format!("{}@{}", r.length, r.offset));
        Self::of_content(format!("{}\n{range}", file_name(uri)).as_bytes())
    }

    /// [`fmt::Display`] 的逆；只认 16 位小写十六进制。
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let canonical =
            text.len() == 16 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        canonical.then(|| u64::from_str_radix(text, 16).ok().map(Fingerprint))?
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// 来源摘要（SHA-256 十六进制）：去掉查询串的地址与选轨偏好。查询串常带每次会话不同的令牌，不计入；
/// 语言代码与选轨一样不区分 ASCII 大小写。每项一行、地址里不会有换行，编码没有歧义。
pub(crate) fn source_digest(url: &Url, preference: &Preference) -> String {
    let variant = match preference.variant {
        VariantChoice::Best => "best".to_owned(),
        VariantChoice::Index(index) => format!("index {index}"),
    };
    let audio = preference
        .audio_language
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    digest_hex(format!(
        "url {}\nvariant {variant}\naudio {audio}\n",
        strip_query(url)
    ))
}

/// 完整地址（含查询串）的摘要（SHA-256 十六进制）。
pub(crate) fn url_digest(url: &Url) -> String {
    digest_hex(url.as_str())
}

/// SHA-256 的十六进制。
fn digest_hex(data: impl AsRef<[u8]>) -> String {
    hex(&Sha256::digest(data))
}

/// 小写十六进制。
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 去掉查询串与片段后的地址；错误信息与摘要都用它，查询串常带令牌。
pub(crate) fn strip_query(url: &Url) -> String {
    let mut url = url.clone();
    url.set_query(None);
    url.set_fragment(None);
    url.into()
}

/// 地址路径的最后一段，不含查询串。
fn file_name(url: &Url) -> &str {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_text_round_trips_and_rejects_other_forms() {
        let fingerprint = Fingerprint::of_content(b"abc");
        // SHA-256("abc") 的前 8 字节
        assert_eq!(fingerprint.to_string(), "ba7816bf8f01cfea");
        assert_eq!(Fingerprint::parse("ba7816bf8f01cfea"), Some(fingerprint));
        for other in [
            "BA7816BF8F01CFEA",
            "ba7816bf8f01cfe",
            "ba7816bf8f01cfeaa",
            "+a7816bf8f01cfea",
        ] {
            assert_eq!(Fingerprint::parse(other), None, "{other}");
        }
    }

    #[test]
    fn segment_identity_ignores_host_path_prefix_and_query_but_not_range() {
        let a = Url::parse("https://edge1.cdn/tok1/v/seg100.ts?sig=1").unwrap();
        let b = Url::parse("https://edge9.cdn/tok2/v/seg100.ts?sig=2").unwrap();
        let c = Url::parse("https://edge1.cdn/tok1/v/seg101.ts?sig=1").unwrap();
        assert_eq!(
            Fingerprint::of_segment(&a, None),
            Fingerprint::of_segment(&b, None)
        );
        assert_ne!(
            Fingerprint::of_segment(&a, None),
            Fingerprint::of_segment(&c, None)
        );
        let range = ByteRange {
            offset: 0,
            length: 10,
        };
        assert_ne!(
            Fingerprint::of_segment(&a, None),
            Fingerprint::of_segment(&a, Some(range))
        );
    }

    #[test]
    fn source_digest_ignores_query_and_language_case_only() {
        let url = |s: &str| Url::parse(s).unwrap();
        let preference = |variant, language: Option<&str>| Preference {
            variant,
            audio_language: language.map(str::to_owned),
        };
        let base = source_digest(
            &url("https://a.example/live.m3u8?token=1"),
            &preference(VariantChoice::Best, Some("EN")),
        );
        assert_eq!(
            base,
            source_digest(
                &url("https://a.example/live.m3u8?token=2"),
                &preference(VariantChoice::Best, Some("en")),
            )
        );
        for other in [
            source_digest(
                &url("https://a.example/other.m3u8"),
                &preference(VariantChoice::Best, Some("en")),
            ),
            source_digest(
                &url("https://a.example/live.m3u8"),
                &preference(VariantChoice::Index(0), Some("en")),
            ),
            source_digest(
                &url("https://a.example/live.m3u8"),
                &preference(VariantChoice::Best, None),
            ),
        ] {
            assert_ne!(base, other);
        }
    }
}
