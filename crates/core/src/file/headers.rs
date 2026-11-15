//! 文件任务用到的响应头：Content-Range、Content-Disposition 的文件名、验证器（ETag、Last-Modified）与 Content-Type。

use reqwest::header::{CONTENT_DISPOSITION, CONTENT_TYPE, ETAG, HeaderMap, LAST_MODIFIED};

/// 206 响应的 Content-Range（RFC 9110 14.4）：`bytes 首-尾/总长`，尾含在内，总长为 `*` 时为 None。写法不对时整个为
/// None。
pub(super) fn content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let (span, total) = value.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (first, last) = span.split_once('-')?;
    let (first, last): (u64, u64) = (first.trim().parse().ok()?, last.trim().parse().ok()?);
    let total = match total.trim() {
        "*" => None,
        total => Some(total.parse().ok()?),
    };
    let fits = first <= last && total.is_none_or(|t| last < t);
    fits.then_some((first, last, total))
}

/// 响应头 `name` 的值；没有或不是可见 ASCII 时为 None。
fn text(headers: &HeaderMap, name: impl reqwest::header::AsHeaderName) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

pub(super) fn etag(headers: &HeaderMap) -> Option<String> {
    text(headers, ETAG)
}

pub(super) fn last_modified(headers: &HeaderMap) -> Option<String> {
    text(headers, LAST_MODIFIED)
}

pub(super) fn content_type(headers: &HeaderMap) -> Option<String> {
    text(headers, CONTENT_TYPE)
}

/// ETag 是强验证器（不以 `W/` 起头）。If-Range 只能用强验证器（RFC 9110 13.1.5），弱的服务器一律当作不符、回整个
/// 文件。
pub(super) fn is_strong(etag: &str) -> bool {
    !etag.starts_with("W/")
}

/// If-Range 用的验证器：强 ETag，没有时 Last-Modified；都没有时为 None。
pub(super) fn validator(etag: Option<&str>, last_modified: Option<&str>) -> Option<String> {
    etag.filter(|e| is_strong(e))
        .or(last_modified)
        .map(str::to_owned)
}

/// Content-Type 是 HTML（媒体类型不分大小写，参数不看）。
pub(super) fn is_html(content_type: &str) -> bool {
    let essence = content_type.split(';').next().unwrap_or("").trim();
    essence.eq_ignore_ascii_case("text/html")
}

/// Content-Disposition（RFC 6266）里的文件名：有 `filename*`（RFC 8187 的编码，字符集为 UTF-8 或 ISO-8859-1）且解得
/// 出时用它，否则用 `filename`；都没有或为空时为 None。原样给出，可能含路径分隔符与 `..`，用作文件名前由调用方清理。
pub(super) fn file_name(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(CONTENT_DISPOSITION)?.as_bytes();
    // 不是 UTF-8 的按 ISO-8859-1 读（HTTP 头的字节原本按它解释）
    let value = match std::str::from_utf8(value) {
        Ok(text) => text.to_owned(),
        Err(_) => value.iter().map(|&b| char::from(b)).collect(),
    };
    disposition_file_name(&value)
}

fn disposition_file_name(value: &str) -> Option<String> {
    let (mut plain, mut extended) = (None, None);
    for (name, raw) in parameters(value) {
        if name.eq_ignore_ascii_case("filename*") {
            extended = extended.or_else(|| ext_value(&raw));
        } else if name.eq_ignore_ascii_case("filename") {
            plain = plain.or(Some(raw));
        }
    }
    extended.or(plain).filter(|name| !name.is_empty())
}

/// 处置类型之后的各参数（名, 值），引号里的值已去掉引号与转义。
fn parameters(value: &str) -> Vec<(String, String)> {
    let mut params = Vec::new();
    let mut chars = value.chars().peekable();
    // 跳过处置类型
    while chars.next_if(|&c| c != ';').is_some() {}
    while chars.next().is_some() {
        let name: String =
            std::iter::from_fn(|| chars.next_if(|&c| c != '=' && c != ';')).collect();
        if chars.next_if_eq(&'=').is_none() {
            continue;
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let mut val = String::new();
        if chars.next_if_eq(&'"').is_some() {
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => val.extend(chars.next()),
                    c => val.push(c),
                }
            }
            while chars.next_if(|&c| c != ';').is_some() {}
        } else {
            val = std::iter::from_fn(|| chars.next_if(|&c| c != ';')).collect();
            val.truncate(val.trim_end().len());
        }
        params.push((name.trim().to_owned(), val));
    }
    params
}

/// RFC 8187 的 ext-value：`字符集'语言'百分号编码的字节`。
fn ext_value(raw: &str) -> Option<String> {
    let mut parts = raw.splitn(3, '\'');
    let (charset, _language, encoded) = (parts.next()?, parts.next()?, parts.next()?);
    let bytes = percent_decode(encoded)?;
    if charset.eq_ignore_ascii_case("utf-8") {
        String::from_utf8(bytes).ok()
    } else if charset.eq_ignore_ascii_case("iso-8859-1") {
        Some(bytes.iter().map(|&b| char::from(b)).collect())
    } else {
        None
    }
}

fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_is_parsed_and_checked() {
        assert_eq!(content_range("bytes 0-99/1000"), Some((0, 99, Some(1000))));
        assert_eq!(content_range(" bytes 5-5/* "), Some((5, 5, None)));
        for bad in [
            "bytes 0-99",
            "bytes 99-0/1000",
            "bytes 0-1000/1000",
            "bytes */1000",
            "items 0-1/2",
        ] {
            assert_eq!(content_range(bad), None, "{bad}");
        }
    }

    /// RFC 6266 附录与 RFC 8187 的例子：filename* 优先，引号里的转义去掉，解不出的 filename* 不用。
    #[test]
    fn file_names_follow_rfc_6266() {
        let cases = [
            ("attachment; filename=example.html", Some("example.html")),
            (
                r#"attachment; filename="a \"b\".txt""#,
                Some(r#"a "b".txt"#),
            ),
            (
                "attachment; filename*= UTF-8''%e2%82%ac%20rates",
                Some("€ rates"),
            ),
            (
                r#"attachment; filename="EURO rates"; filename*=utf-8''%e2%82%ac%20rates"#,
                Some("€ rates"),
            ),
            (
                "attachment; filename*=iso-8859-1'en'%A3%20rates",
                Some("£ rates"),
            ),
            (
                r#"attachment; filename*=gbk''%C4%E3; filename="fallback.bin""#,
                Some("fallback.bin"),
            ),
            ("inline; FILENAME=Report.pdf ", Some("Report.pdf")),
            ("attachment", None),
            (r#"attachment; filename="""#, None),
        ];
        for (value, expected) in cases {
            assert_eq!(disposition_file_name(value).as_deref(), expected, "{value}");
        }
    }

    #[test]
    fn validators_prefer_strong_etags() {
        assert_eq!(
            validator(Some("\"a\""), Some("date")).as_deref(),
            Some("\"a\"")
        );
        assert_eq!(
            validator(Some("W/\"a\""), Some("date")).as_deref(),
            Some("date")
        );
        assert_eq!(validator(Some("W/\"a\""), None), None);
        assert!(is_html("Text/HTML; charset=utf-8") && !is_html("text/plain"));
    }
}
