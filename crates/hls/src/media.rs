//! 媒体播放列表：把 KEY、MAP、BYTERANGE、DISCONTINUITY 的作用范围展开到每个分片。

use std::collections::BTreeMap;

use url::Url;

use crate::line::{Attributes, LineKind, lines};
use crate::{Error, SyntaxError, Unsupported, parse_seconds_us, parse_u64, resolve};

/// 媒体播放列表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPlaylist {
    /// EXT-X-TARGETDURATION，微秒；缺失时为 None
    pub target_duration_us: Option<u64>,
    /// 第一个分片的媒体序号（EXT-X-MEDIA-SEQUENCE，缺省 0）
    pub media_sequence: u64,
    /// 是否有 EXT-X-ENDLIST；没有即为直播
    pub ended: bool,
    pub playlist_type: Option<PlaylistType>,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistType {
    Vod,
    Event,
}

/// 一个分片，所有作用于它的标签都已展开。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// 媒体序号
    pub sequence: u64,
    pub uri: Url,
    /// EXTINF 声明的时长，微秒
    pub duration_us: u64,
    pub byte_range: Option<ByteRange>,
    /// 不连续段序号：EXT-X-DISCONTINUITY-SEQUENCE 加上此前出现的 EXT-X-DISCONTINUITY 个数
    pub discontinuity: u64,
    /// None 表示不加密
    pub key: Option<SegmentKey>,
    /// fMP4 的 init 段（EXT-X-MAP）
    pub init: Option<InitSection>,
}

/// 分片的 AES-128 解密参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentKey {
    pub uri: Url,
    /// 显式 IV，或缺省时该分片媒体序号的 16 字节大端编码（RFC 8216 5.2）
    pub iv: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitSection {
    pub uri: Url,
    pub byte_range: Option<ByteRange>,
}

/// 资源中的一段字节：[offset, offset + length)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

/// 一个 EXT-X-KEY 标签（METHOD 不为 NONE）。
struct KeyTag {
    line: usize,
    method: String,
    uri: Url,
    iv: Option<[u8; 16]>,
}

pub(crate) fn parse(text: &str, url: &Url) -> Result<MediaPlaylist, Error> {
    let mut playlist = MediaPlaylist {
        target_duration_us: None,
        media_sequence: 0,
        ended: false,
        playlist_type: None,
        segments: Vec::new(),
    };
    let mut discontinuity = 0u64;
    // 当前生效的 key，按 KEYFORMAT 区分（RFC 8216 4.3.2.4）
    let mut keys: BTreeMap<String, KeyTag> = BTreeMap::new();
    let mut init: Option<InitSection> = None;
    let mut pending_duration: Option<(usize, u64)> = None;
    let mut pending_range: Option<(usize, u64, Option<u64>)> = None;
    let mut pending_discontinuity = false;
    // 上一个带字节范围的分片：(资源, 结束偏移)，供省略偏移的 BYTERANGE 接续
    let mut last_range: Option<(Url, u64)> = None;

    for line in lines(text).skip(1) {
        let at = |kind| Error::Syntax {
            line: line.number,
            kind,
        };
        match line.kind {
            LineKind::Tag { name, value } => match name.as_str() {
                "EXTINF" => {
                    let value = value.ok_or(at(SyntaxError::MissingValue { tag: "EXTINF" }))?;
                    let duration = value.split_once(',').map_or(value, |(d, _)| d);
                    pending_duration = Some((
                        line.number,
                        parse_seconds_us("EXTINF", duration).map_err(at)?,
                    ));
                }
                "EXT-X-BYTERANGE" => {
                    let value = value.ok_or(at(SyntaxError::MissingValue {
                        tag: "EXT-X-BYTERANGE",
                    }))?;
                    let (length, offset) = parse_byte_range(value).map_err(at)?;
                    pending_range = Some((line.number, length, offset));
                }
                "EXT-X-DISCONTINUITY" => pending_discontinuity = true,
                "EXT-X-KEY" => {
                    let value = value.ok_or(at(SyntaxError::MissingValue { tag: "EXT-X-KEY" }))?;
                    let attrs = Attributes::parse(value).map_err(at)?;
                    let method = attrs
                        .require("EXT-X-KEY", "METHOD")
                        .map_err(at)?
                        .to_ascii_uppercase();
                    if method == "NONE" {
                        keys.clear();
                    } else {
                        let uri = resolve(url, attrs.require("EXT-X-KEY", "URI").map_err(at)?)
                            .map_err(at)?;
                        let iv = attrs.get("IV").map(parse_iv).transpose().map_err(at)?;
                        let format = attrs
                            .get("KEYFORMAT")
                            .unwrap_or("identity")
                            .to_ascii_lowercase();
                        keys.insert(
                            format,
                            KeyTag {
                                line: line.number,
                                method,
                                uri,
                                iv,
                            },
                        );
                    }
                }
                "EXT-X-MAP" => {
                    let value = value.ok_or(at(SyntaxError::MissingValue { tag: "EXT-X-MAP" }))?;
                    let attrs = Attributes::parse(value).map_err(at)?;
                    let uri =
                        resolve(url, attrs.require("EXT-X-MAP", "URI").map_err(at)?).map_err(at)?;
                    let byte_range = match attrs.get("BYTERANGE") {
                        Some(range) => match parse_byte_range(range).map_err(at)? {
                            (length, Some(offset)) => Some(ByteRange { offset, length }),
                            // EXT-X-MAP 的 BYTERANGE 必须带偏移（RFC 8216 4.3.2.5）
                            (_, None) => return Err(at(SyntaxError::ByteRange(range.to_owned()))),
                        },
                        None => None,
                    };
                    init = Some(InitSection { uri, byte_range });
                }
                "EXT-X-MEDIA-SEQUENCE" | "EXT-X-DISCONTINUITY-SEQUENCE" => {
                    if !playlist.segments.is_empty() {
                        return Err(at(SyntaxError::SequenceAfterSegments));
                    }
                    let tag = if name == "EXT-X-MEDIA-SEQUENCE" {
                        "EXT-X-MEDIA-SEQUENCE"
                    } else {
                        "EXT-X-DISCONTINUITY-SEQUENCE"
                    };
                    let number =
                        parse_u64(tag, value.ok_or(at(SyntaxError::MissingValue { tag }))?)
                            .map_err(at)?;
                    if tag == "EXT-X-MEDIA-SEQUENCE" {
                        playlist.media_sequence = number;
                    } else {
                        discontinuity = number;
                    }
                }
                "EXT-X-TARGETDURATION" => {
                    let value = value.ok_or(at(SyntaxError::MissingValue {
                        tag: "EXT-X-TARGETDURATION",
                    }))?;
                    playlist.target_duration_us =
                        Some(parse_seconds_us("EXT-X-TARGETDURATION", value).map_err(at)?);
                }
                "EXT-X-ENDLIST" => playlist.ended = true,
                "EXT-X-PLAYLIST-TYPE" => {
                    playlist.playlist_type = match value.map(str::to_ascii_uppercase).as_deref() {
                        Some("VOD") => Some(PlaylistType::Vod),
                        Some("EVENT") => Some(PlaylistType::Event),
                        _ => None,
                    };
                }
                _ => {}
            },
            LineKind::Uri(uri) => {
                let (_, duration_us) = pending_duration
                    .take()
                    .ok_or(at(SyntaxError::UriWithoutInfo { expected: "EXTINF" }))?;
                let sequence = playlist.media_sequence + playlist.segments.len() as u64;
                let uri = resolve(url, uri).map_err(at)?;
                if std::mem::take(&mut pending_discontinuity) {
                    discontinuity += 1;
                }
                let byte_range = match pending_range.take() {
                    Some((range_line, length, offset)) => {
                        let offset = match offset {
                            Some(offset) => offset,
                            None => match &last_range {
                                Some((resource, end)) if *resource == uri => *end,
                                _ => {
                                    return Err(Error::Syntax {
                                        line: range_line,
                                        kind: SyntaxError::ByteRangeWithoutOffset,
                                    });
                                }
                            },
                        };
                        last_range = Some((uri.clone(), offset + length));
                        Some(ByteRange { offset, length })
                    }
                    None => {
                        last_range = None;
                        None
                    }
                };
                playlist.segments.push(Segment {
                    sequence,
                    duration_us,
                    byte_range,
                    discontinuity,
                    key: segment_key(&keys, sequence)?,
                    init: init.clone(),
                    uri,
                });
            }
        }
    }
    if let Some((line, _)) = pending_duration {
        return Err(Error::Syntax {
            line,
            kind: SyntaxError::InfoWithoutUri { tag: "EXTINF" },
        });
    }
    if let Some((line, _, _)) = pending_range {
        return Err(Error::Syntax {
            line,
            kind: SyntaxError::InfoWithoutUri {
                tag: "EXT-X-BYTERANGE",
            },
        });
    }
    Ok(playlist)
}

/// 分片使用的 key：有 identity 格式的 key 就用它；只有其他 KEYFORMAT（DRM）时不支持。
fn segment_key(
    keys: &BTreeMap<String, KeyTag>,
    sequence: u64,
) -> Result<Option<SegmentKey>, Error> {
    let Some(identity) = keys.get("identity") else {
        return match keys.keys().next() {
            Some(format) => Err(Error::Unsupported {
                line: keys[format].line,
                what: Unsupported::Drm {
                    keyformat: format.clone(),
                },
            }),
            None => Ok(None),
        };
    };
    let unsupported = |what| Error::Unsupported {
        line: identity.line,
        what,
    };
    match identity.method.as_str() {
        "AES-128" => Ok(Some(SegmentKey {
            uri: identity.uri.clone(),
            iv: identity.iv.unwrap_or(u128::from(sequence).to_be_bytes()),
        })),
        "SAMPLE-AES" | "SAMPLE-AES-CTR" => Err(unsupported(Unsupported::SampleAes)),
        other => Err(unsupported(Unsupported::Method(other.to_owned()))),
    }
}

/// `<长度>[@<偏移>]`，长度至少为 1。
fn parse_byte_range(value: &str) -> Result<(u64, Option<u64>), SyntaxError> {
    let bad = || SyntaxError::ByteRange(value.to_owned());
    let (length, offset) = match value.trim().split_once('@') {
        Some((length, offset)) => (length, Some(offset)),
        None => (value.trim(), None),
    };
    let length: u64 = length.trim().parse().map_err(|_| bad())?;
    if length == 0 {
        return Err(bad());
    }
    let offset = offset
        .map(|o| o.trim().parse().map_err(|_| bad()))
        .transpose()?;
    Ok((length, offset))
}

/// 32 位十六进制，`0x` / `0X` 前缀可省略，大小写不限。
fn parse_iv(value: &str) -> Result<[u8; 16], SyntaxError> {
    let bad = || SyntaxError::Iv(value.to_owned());
    let hex = value.trim();
    let hex = hex
        .strip_prefix("0x")
        .or_else(|| hex.strip_prefix("0X"))
        .unwrap_or(hex);
    if hex.len() != 32 || !hex.is_ascii() {
        return Err(bad());
    }
    let mut iv = [0u8; 16];
    for (i, byte) in iv.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
    }
    Ok(iv)
}
