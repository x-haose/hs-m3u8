//! 身份与摘要：判定「是不是同一个来源、同一个分片、同一个 init 段」，以及错误信息里地址的写法。
//!
//! 指纹与摘要都基于 SHA-256，写进 job.json 与文件名，属于任务目录格式：算法或输入的编码改变时，
//! 必须同时升 job.json 的格式版本。

use std::fmt;

use hs_m3u8_hls::{AudioChoice, ByteRange, Preference, VariantChoice};
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
    ///
    /// 简化：只靠查询串区分分片的站点（如 `seg.php?id=…`），各分片的身份相同，比对退化为只看序号；
    /// 遇到这类站点时，把查询串里区分分片的参数计入身份。
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

/// 来源摘要（SHA-256 十六进制）：来源地址（见 [`source_address`]）与选轨偏好。语言代码与选轨一样不区分
/// ASCII 大小写。每项一行；地址与语言里不会有换行（语言由 [`crate::Source`] 的校验保证），编码没有歧义。
pub(crate) fn source_digest(url: &Url, preference: &Preference) -> String {
    let variant = match preference.variant {
        VariantChoice::Best => "best".to_owned(),
        VariantChoice::Index(index) => format!("index {index}"),
    };
    let audio = match &preference.audio {
        AudioChoice::Default => "default".to_owned(),
        AudioChoice::Language(language) => format!("language {}", language.to_ascii_lowercase()),
        AudioChoice::Index(index) => format!("index {index}"),
    };
    digest_hex(format!(
        "url {}\nvariant {variant}\naudio {audio}\n",
        source_address(url)
    ))
}

/// 来源摘要里的地址：去掉用户名、密码、查询串与片段，这些部分常带每次会话不同的凭据或令牌。属于任务目录
/// 格式，输出改变须升 job.json 的格式版本。
fn source_address(url: &Url) -> String {
    match url.host_str() {
        Some(host) => {
            let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
            format!("{}://{host}{port}{}", url.scheme(), url.path())
        }
        None => format!("{}:{}", url.scheme(), url.path()),
    }
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

/// 错误信息里地址的写法：去掉用户名、密码、查询串与片段，这些部分常带凭据或令牌。
pub(crate) fn bare_url(url: &Url) -> String {
    match url.host_str() {
        Some(host) => {
            let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
            format!("{}://{host}{port}{}", url.scheme(), url.path())
        }
        None => format!("{}:{}", url.scheme(), url.path()),
    }
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

    /// 指纹与摘要写在文件名与 job.json 里，值变了即任务目录格式变了，须升格式版本。期望值是按各函数文档
    /// 所述的编码，用另一个 SHA-256 实现算出的。
    #[test]
    fn fingerprints_and_digests_are_fixed() {
        let segment = Url::parse("https://edge.cdn/tok/v/seg100.ts?sig=1").unwrap();
        let range = ByteRange {
            offset: 0,
            length: 10,
        };
        assert_eq!(
            Fingerprint::of_segment(&segment, None).to_string(),
            "16f512ec546fd802"
        );
        assert_eq!(
            Fingerprint::of_segment(&segment, Some(range)).to_string(),
            "9d32d5f86c96c5d9"
        );

        let source = Url::parse("https://user:pw@a.example:8443/live.m3u8?token=1").unwrap();
        let best = Preference {
            variant: VariantChoice::Best,
            audio: AudioChoice::Language("EN".into()),
        };
        assert_eq!(
            source_digest(&source, &best),
            "a9106f2006973de9187a043d77a969aa977790875deb871ecc8da7c70e9dee62"
        );
        let index = Preference {
            variant: VariantChoice::Index(2),
            audio: AudioChoice::Default,
        };
        assert_eq!(
            source_digest(&source, &index),
            "0e42d3b0802e2f2d74ae0d8dc420518949c503b570451b6a988c73e8930028d5"
        );

        let live = Url::parse("https://a.example:8443/live.m3u8?token=1").unwrap();
        assert_eq!(
            url_digest(&live),
            "afd3a584486c3d8f1240bf03a547070da7f147f4a8dd68e49a419956c1b2e743"
        );
    }

    #[test]
    fn bare_url_drops_credentials_query_and_fragment() {
        let url = Url::parse("https://user:pass@a.example:8443/v/x.m3u8?token=1#t").unwrap();
        assert_eq!(bare_url(&url), "https://a.example:8443/v/x.m3u8");
        let default_port = Url::parse("http://a.example:80/x").unwrap();
        assert_eq!(bare_url(&default_port), "http://a.example/x");
        let ipv6 = Url::parse("http://[::1]:9/x?k=v").unwrap();
        assert_eq!(bare_url(&ipv6), "http://[::1]:9/x");
    }

    #[test]
    fn source_digest_ignores_query_and_language_case_only() {
        let url = |s: &str| Url::parse(s).unwrap();
        let preference = |variant, language: Option<&str>| Preference {
            variant,
            audio: language.map_or(AudioChoice::Default, |l| AudioChoice::Language(l.into())),
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
            source_digest(
                &url("https://a.example/live.m3u8"),
                &Preference {
                    variant: VariantChoice::Best,
                    audio: AudioChoice::Index(0),
                },
            ),
        ] {
            assert_ne!(base, other);
        }
    }
}
