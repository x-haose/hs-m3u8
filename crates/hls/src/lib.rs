//! HLS 播放列表解析与规范化（RFC 8216），纯计算，不做网络与文件 IO。
//!
//! 输入播放列表文本和它的最终 URL（跟随重定向之后），输出规范化模型：
//! 每个分片带上已解析为绝对地址的 URI、已定值的 key 与 IV、init 段、字节范围与不连续段序号。
//!
//! 容忍格式上的不规范：BOM、CRLF、空白、属性名与枚举值的大小写、值不加引号、IV 不带 `0x`、
//! 小数形式的 TARGETDURATION、超出 TARGETDURATION 的分片时长、未知标签。
//! 影响语义的标签（KEY、MAP、BYTERANGE、STREAM-INF、MEDIA 等）写坏时报错并给出行号，不静默丢弃。

mod line;
mod master;
mod media;
mod select;

pub use master::{MasterPlaylist, Rendition, RenditionKind, Resolution, Variant};
pub use media::{ByteRange, InitSection, MediaPlaylist, PlaylistType, Segment, SegmentKey};
pub use select::{Preference, SelectError, SelectedAudio, Selection, VariantChoice, select};
pub use url::Url;

use line::{LineKind, lines};

/// 解析结果：主播放列表或媒体播放列表。
#[derive(Debug, Clone, PartialEq)]
pub enum Playlist {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// 内容为空或只有空白（如服务器还没写完）
    #[error("播放列表为空")]
    Empty,
    #[error("不是 HLS 播放列表：第一行应为 #EXTM3U")]
    NotAPlaylist,
    #[error("同时含有主播放列表（EXT-X-STREAM-INF）与媒体播放列表（EXTINF）的标签")]
    Mixed,
    #[error("第 {line} 行：{kind}")]
    Syntax { line: usize, kind: SyntaxError },
    #[error("第 {line} 行的 EXT-X-KEY：{what}")]
    Unsupported { line: usize, what: Unsupported },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyntaxError {
    #[error("{tag} 缺少值")]
    MissingValue { tag: &'static str },
    /// 原因；不含属性原文
    #[error("属性列表无法解析：{0}")]
    Attributes(String),
    #[error("{tag} 缺少属性 {name}")]
    MissingAttribute {
        tag: &'static str,
        name: &'static str,
    },
    #[error("{what} 不是有效的数值：{value:?}")]
    Number { what: &'static str, value: String },
    #[error("IV 应为 16 字节的十六进制：{0:?}")]
    Iv(String),
    #[error("分辨率应为 <宽>x<高>：{0:?}")]
    Resolution(String),
    #[error("字节范围应为 <长度>[@<偏移>]：{0:?}")]
    ByteRange(String),
    #[error("字节范围省略了偏移，但上一个分片不是同一资源")]
    ByteRangeWithoutOffset,
    #[error("URI 行之前缺少 {expected}")]
    UriWithoutInfo { expected: &'static str },
    #[error("{tag} 之后缺少 URI 行")]
    InfoWithoutUri { tag: &'static str },
    /// `uri` 为播放列表中的原文；信息中只显示查询串之前的部分（查询串常带令牌）
    #[error("无法解析为 URL：{:?}（{reason}）", without_query(.uri))]
    Url { uri: String, reason: String },
    #[error("EXT-X-MEDIA 的 TYPE 无法识别：{0:?}")]
    RenditionType(String),
    #[error("EXT-X-MEDIA-SEQUENCE / EXT-X-DISCONTINUITY-SEQUENCE 必须出现在第一个分片之前")]
    SequenceAfterSegments,
    #[error("媒体序号或不连续段序号超出 64 位整数范围")]
    SequenceOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    #[error("SAMPLE-AES 加密")]
    SampleAes,
    #[error("DRM 加密（KEYFORMAT={keyformat}）")]
    Drm { keyformat: String },
    #[error("未知的加密方式 {0}")]
    Method(String),
}

/// 解析播放列表。`url` 为该播放列表的最终 URL（跟随重定向之后），所有相对 URI 按它解析。
pub fn parse(text: &str, url: &Url) -> Result<Playlist, Error> {
    if text.trim_start_matches('\u{FEFF}').trim().is_empty() {
        return Err(Error::Empty);
    }
    let mut iter = lines(text);
    match iter.next() {
        Some(first) if matches!(&first.kind, LineKind::Tag { name, .. } if name == "EXTM3U") => {}
        _ => return Err(Error::NotAPlaylist),
    }
    let mut has_stream_inf = false;
    let mut has_extinf = false;
    for line in iter {
        if let LineKind::Tag { name, .. } = &line.kind {
            has_stream_inf |= name == "EXT-X-STREAM-INF";
            has_extinf |= name == "EXTINF";
        }
    }
    match (has_stream_inf, has_extinf) {
        (true, true) => Err(Error::Mixed),
        (true, false) => master::parse(text, url).map(Playlist::Master),
        (false, _) => media::parse(text, url).map(Playlist::Media),
    }
}

/// 地址原文中查询串与片段之前的部分。
fn without_query(uri: &str) -> &str {
    uri.split(['?', '#']).next().unwrap_or_default()
}

/// 把 `uri` 按 `base` 解析为绝对 URL。
fn resolve(base: &Url, uri: &str) -> Result<Url, SyntaxError> {
    base.join(uri).map_err(|e| SyntaxError::Url {
        uri: uri.to_owned(),
        reason: e.to_string(),
    })
}

/// 十进制非负整数。
fn parse_u64(what: &'static str, value: &str) -> Result<u64, SyntaxError> {
    value.trim().parse().map_err(|_| SyntaxError::Number {
        what,
        value: value.to_owned(),
    })
}

/// 十进制非负秒数（整数或小数）换成微秒，第 7 位小数起四舍五入。
fn parse_seconds_us(what: &'static str, value: &str) -> Result<u64, SyntaxError> {
    let bad = || SyntaxError::Number {
        what,
        value: value.to_owned(),
    };
    let value = value.trim();
    let (int, frac) = value.split_once('.').unwrap_or((value, ""));
    if (int.is_empty() && frac.is_empty())
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }
    let int: u64 = if int.is_empty() {
        0
    } else {
        int.parse().map_err(|_| bad())?
    };
    let mut micros = 0u64;
    for (i, digit) in frac.bytes().take(6).enumerate() {
        micros += u64::from(digit - b'0') * 10u64.pow(5 - i as u32);
    }
    if frac.as_bytes().get(6).is_some_and(|&d| d >= b'5') {
        micros += 1;
    }
    int.checked_mul(1_000_000)
        .and_then(|us| us.checked_add(micros))
        .ok_or_else(bad)
}
