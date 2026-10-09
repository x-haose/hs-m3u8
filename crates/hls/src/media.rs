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
    /// EXT-X-DISCONTINUITY-SEQUENCE；None 表示没写，此时各分片的不连续段序号只在本次播放列表内有意义
    /// （窗口前移、带 DISCONTINUITY 的分片滑出后编号会整体变小）
    pub discontinuity_sequence: Option<u64>,
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

/// 资源中的一段字节：[offset, offset + length)。解析结果保证 length ≥ 1 且 offset + length 不溢出。
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

/// 已读到、等待 URI 行的分片属性。
#[derive(Default)]
struct Pending {
    /// (EXTINF 所在行, 时长微秒)
    duration: Option<(usize, u64)>,
    /// (EXT-X-BYTERANGE 所在行, 长度, 偏移)
    range: Option<(usize, u64, Option<u64>)>,
    discontinuity: bool,
}

/// 逐行解析时的状态：各标签的作用范围持续到被下一个同类标签替换。
struct Parser<'u> {
    url: &'u Url,
    playlist: MediaPlaylist,
    discontinuity: u64,
    /// 当前生效的 key，按 KEYFORMAT 区分（RFC 8216 4.3.2.4）
    keys: BTreeMap<String, KeyTag>,
    init: Option<InitSection>,
    pending: Pending,
    /// 上一个带字节范围的分片：(资源, 结束偏移)，供省略偏移的 BYTERANGE 接续
    last_range: Option<(Url, u64)>,
}

pub(crate) fn parse(text: &str, url: &Url) -> Result<MediaPlaylist, Error> {
    let mut parser = Parser {
        url,
        playlist: MediaPlaylist {
            target_duration_us: None,
            media_sequence: 0,
            discontinuity_sequence: None,
            ended: false,
            playlist_type: None,
            segments: Vec::new(),
        },
        discontinuity: 0,
        keys: BTreeMap::new(),
        init: None,
        pending: Pending::default(),
        last_range: None,
    };
    for line in lines(text).skip(1) {
        match line.kind {
            LineKind::Tag { name, value } => parser.tag(line.number, &name, value)?,
            LineKind::Uri(uri) => parser.uri(line.number, uri)?,
        }
    }
    parser.finish()
}

impl Parser<'_> {
    fn tag(&mut self, line: usize, name: &str, value: Option<&str>) -> Result<(), Error> {
        let at = |kind| Error::Syntax { line, kind };
        let required = |tag| value.ok_or(at(SyntaxError::MissingValue { tag }));
        match name {
            "EXTINF" => {
                let value = required("EXTINF")?;
                let duration = value.split_once(',').map_or(value, |(d, _)| d);
                let us = parse_seconds_us("EXTINF", duration).map_err(at)?;
                self.pending.duration = Some((line, us));
            }
            "EXT-X-BYTERANGE" => {
                let (length, offset) =
                    parse_byte_range(required("EXT-X-BYTERANGE")?).map_err(at)?;
                self.pending.range = Some((line, length, offset));
            }
            "EXT-X-DISCONTINUITY" => self.pending.discontinuity = true,
            "EXT-X-KEY" => self.key(line, required("EXT-X-KEY")?).map_err(at)?,
            "EXT-X-MAP" => self.map(required("EXT-X-MAP")?).map_err(at)?,
            "EXT-X-MEDIA-SEQUENCE" => {
                let tag = "EXT-X-MEDIA-SEQUENCE";
                self.playlist.media_sequence = self.sequence_tag(tag, value).map_err(at)?;
            }
            "EXT-X-DISCONTINUITY-SEQUENCE" => {
                let tag = "EXT-X-DISCONTINUITY-SEQUENCE";
                self.discontinuity = self.sequence_tag(tag, value).map_err(at)?;
                self.playlist.discontinuity_sequence = Some(self.discontinuity);
            }
            "EXT-X-TARGETDURATION" => {
                let value = required("EXT-X-TARGETDURATION")?;
                let us = parse_seconds_us("EXT-X-TARGETDURATION", value).map_err(at)?;
                self.playlist.target_duration_us = Some(us);
            }
            "EXT-X-ENDLIST" => self.playlist.ended = true,
            "EXT-X-PLAYLIST-TYPE" => {
                self.playlist.playlist_type = match value.map(str::to_ascii_uppercase).as_deref() {
                    Some("VOD") => Some(PlaylistType::Vod),
                    Some("EVENT") => Some(PlaylistType::Event),
                    _ => None,
                };
            }
            _ => {}
        }
        Ok(())
    }

    /// EXT-X-KEY：METHOD=NONE 清除全部 key，否则按 KEYFORMAT（缺省 identity）记录。
    fn key(&mut self, line: usize, value: &str) -> Result<(), SyntaxError> {
        let attrs = Attributes::parse(value)?;
        let method = attrs.require("EXT-X-KEY", "METHOD")?.to_ascii_uppercase();
        if method == "NONE" {
            self.keys.clear();
            return Ok(());
        }
        let uri = resolve(self.url, attrs.require("EXT-X-KEY", "URI")?)?;
        let iv = attrs.get("IV").map(parse_iv).transpose()?;
        let format = attrs
            .get("KEYFORMAT")
            .unwrap_or("identity")
            .to_ascii_lowercase();
        self.keys.insert(
            format,
            KeyTag {
                line,
                method,
                uri,
                iv,
            },
        );
        Ok(())
    }

    /// EXT-X-MAP；其 BYTERANGE 必须带偏移（RFC 8216 4.3.2.5）。
    fn map(&mut self, value: &str) -> Result<(), SyntaxError> {
        let attrs = Attributes::parse(value)?;
        let uri = resolve(self.url, attrs.require("EXT-X-MAP", "URI")?)?;
        let byte_range = match attrs.get("BYTERANGE") {
            Some(range) => match parse_byte_range(range)? {
                (length, Some(offset)) if offset.checked_add(length).is_some() => {
                    Some(ByteRange { offset, length })
                }
                _ => return Err(SyntaxError::ByteRange(range.to_owned())),
            },
            None => None,
        };
        self.init = Some(InitSection { uri, byte_range });
        Ok(())
    }

    /// EXT-X-MEDIA-SEQUENCE / EXT-X-DISCONTINUITY-SEQUENCE 的值；必须出现在第一个分片之前。
    fn sequence_tag(&self, tag: &'static str, value: Option<&str>) -> Result<u64, SyntaxError> {
        if !self.playlist.segments.is_empty() {
            return Err(SyntaxError::SequenceAfterSegments);
        }
        parse_u64(tag, value.ok_or(SyntaxError::MissingValue { tag })?)
    }

    /// URI 行：用此前读到的属性与当前生效的 key、init 段组成一个分片。
    fn uri(&mut self, line: usize, uri: &str) -> Result<(), Error> {
        let at = |kind| Error::Syntax { line, kind };
        let (_, duration_us) = self
            .pending
            .duration
            .take()
            .ok_or(at(SyntaxError::UriWithoutInfo { expected: "EXTINF" }))?;
        let index = u64::try_from(self.playlist.segments.len()).expect("分片数不超过 u64");
        let sequence = self
            .playlist
            .media_sequence
            .checked_add(index)
            .ok_or(at(SyntaxError::SequenceOverflow))?;
        let uri = resolve(self.url, uri).map_err(at)?;
        if std::mem::take(&mut self.pending.discontinuity) {
            self.discontinuity = self
                .discontinuity
                .checked_add(1)
                .ok_or(at(SyntaxError::SequenceOverflow))?;
        }
        let byte_range = self.byte_range(&uri)?;
        let key = segment_key(&self.keys, sequence)?;
        self.playlist.segments.push(Segment {
            sequence,
            duration_us,
            byte_range,
            discontinuity: self.discontinuity,
            key,
            init: self.init.clone(),
            uri,
        });
        Ok(())
    }

    /// 本分片的字节范围；省略偏移时接着同一资源上一个子区间往后取。
    fn byte_range(&mut self, uri: &Url) -> Result<Option<ByteRange>, Error> {
        let Some((line, length, offset)) = self.pending.range.take() else {
            self.last_range = None;
            return Ok(None);
        };
        let at = |kind| Error::Syntax { line, kind };
        let offset = match offset {
            Some(offset) => offset,
            None => match &self.last_range {
                Some((resource, end)) if resource == uri => *end,
                _ => return Err(at(SyntaxError::ByteRangeWithoutOffset)),
            },
        };
        let end = offset
            .checked_add(length)
            .ok_or(at(SyntaxError::ByteRange(format!("{length}@{offset}"))))?;
        self.last_range = Some((uri.clone(), end));
        Ok(Some(ByteRange { offset, length }))
    }

    /// 末尾不能留下没有 URI 行的 EXTINF 或 BYTERANGE。
    fn finish(self) -> Result<MediaPlaylist, Error> {
        let dangling = |line, tag| Error::Syntax {
            line,
            kind: SyntaxError::InfoWithoutUri { tag },
        };
        if let Some((line, _)) = self.pending.duration {
            return Err(dangling(line, "EXTINF"));
        }
        if let Some((line, _, _)) = self.pending.range {
            return Err(dangling(line, "EXT-X-BYTERANGE"));
        }
        Ok(self.playlist)
    }
}

/// 分片使用的 key：有 identity 格式的 key 就用它；只有其他 KEYFORMAT（DRM）时不支持。
fn segment_key(
    keys: &BTreeMap<String, KeyTag>,
    sequence: u64,
) -> Result<Option<SegmentKey>, Error> {
    let Some(identity) = keys.get("identity") else {
        return match keys.iter().next() {
            Some((format, tag)) => Err(Error::Unsupported {
                line: tag.line,
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
