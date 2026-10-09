//! 主播放列表：变体（EXT-X-STREAM-INF）与 rendition（EXT-X-MEDIA）。

use url::Url;

use crate::line::{Attributes, LineKind, lines};
use crate::{Error, SyntaxError, parse_u64, resolve};

#[derive(Debug, Clone, PartialEq)]
pub struct MasterPlaylist {
    /// 按出现顺序
    pub variants: Vec<Variant>,
    pub renditions: Vec<Rendition>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub uri: Url,
    /// BANDWIDTH，bit/s；规范要求必填，缺失时为 None
    pub bandwidth: Option<u64>,
    pub resolution: Option<Resolution>,
    /// CODECS 拆开后的各项，如 `avc1.640028`、`mp4a.40.2`
    pub codecs: Vec<String>,
    /// AUDIO 属性：引用的音频 rendition 组
    pub audio: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendition {
    pub kind: RenditionKind,
    pub group_id: String,
    pub name: String,
    pub language: Option<String>,
    pub default: bool,
    /// None 表示该 rendition 的媒体混在变体流里
    pub uri: Option<Url>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenditionKind {
    Audio,
    Video,
    Subtitles,
    ClosedCaptions,
}

pub(crate) fn parse(text: &str, url: &Url) -> Result<MasterPlaylist, Error> {
    let mut playlist = MasterPlaylist {
        variants: Vec::new(),
        renditions: Vec::new(),
    };
    // 已读到 EXT-X-STREAM-INF、等待其 URI 行的变体：(行号, 变体去掉 URI 的部分)
    let mut pending: Option<(usize, Variant)> = None;

    for line in lines(text).skip(1) {
        let at = |kind| Error::Syntax {
            line: line.number,
            kind,
        };
        match line.kind {
            LineKind::Tag { name, value } => match name.as_str() {
                "EXT-X-STREAM-INF" => {
                    if let Some((line, _)) = pending {
                        return Err(Error::Syntax {
                            line,
                            kind: SyntaxError::InfoWithoutUri {
                                tag: "EXT-X-STREAM-INF",
                            },
                        });
                    }
                    let value = value.ok_or(at(SyntaxError::MissingValue {
                        tag: "EXT-X-STREAM-INF",
                    }))?;
                    pending = Some((line.number, parse_stream_inf(value, url).map_err(at)?));
                }
                "EXT-X-MEDIA" => {
                    let value =
                        value.ok_or(at(SyntaxError::MissingValue { tag: "EXT-X-MEDIA" }))?;
                    playlist
                        .renditions
                        .push(parse_media(value, url).map_err(at)?);
                }
                _ => {}
            },
            LineKind::Uri(uri) => {
                let (_, mut variant) = pending.take().ok_or(at(SyntaxError::UriWithoutInfo {
                    expected: "EXT-X-STREAM-INF",
                }))?;
                variant.uri = resolve(url, uri).map_err(at)?;
                playlist.variants.push(variant);
            }
        }
    }
    if let Some((line, _)) = pending {
        return Err(Error::Syntax {
            line,
            kind: SyntaxError::InfoWithoutUri {
                tag: "EXT-X-STREAM-INF",
            },
        });
    }
    Ok(playlist)
}

/// 解析 STREAM-INF 的属性；`uri` 先填为播放列表自身地址，读到 URI 行后替换。
fn parse_stream_inf(value: &str, url: &Url) -> Result<Variant, SyntaxError> {
    let attrs = Attributes::parse(value)?;
    let resolution = attrs.get("RESOLUTION").map(parse_resolution).transpose()?;
    Ok(Variant {
        uri: url.clone(),
        bandwidth: attrs
            .get("BANDWIDTH")
            .map(|b| parse_u64("BANDWIDTH", b))
            .transpose()?,
        resolution,
        codecs: attrs
            .get("CODECS")
            .map(|c| {
                c.split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        audio: attrs.get("AUDIO").map(str::to_owned),
    })
}

fn parse_media(value: &str, url: &Url) -> Result<Rendition, SyntaxError> {
    let attrs = Attributes::parse(value)?;
    let kind = attrs.require("EXT-X-MEDIA", "TYPE")?;
    let kind = match kind.to_ascii_uppercase().as_str() {
        "AUDIO" => RenditionKind::Audio,
        "VIDEO" => RenditionKind::Video,
        "SUBTITLES" => RenditionKind::Subtitles,
        "CLOSED-CAPTIONS" => RenditionKind::ClosedCaptions,
        _ => return Err(SyntaxError::RenditionType(kind.to_owned())),
    };
    Ok(Rendition {
        kind,
        group_id: attrs.require("EXT-X-MEDIA", "GROUP-ID")?.to_owned(),
        name: attrs.get("NAME").unwrap_or_default().to_owned(),
        language: attrs.get("LANGUAGE").map(str::to_owned),
        default: attrs
            .get("DEFAULT")
            .is_some_and(|d| d.eq_ignore_ascii_case("YES")),
        uri: attrs.get("URI").map(|u| resolve(url, u)).transpose()?,
    })
}

/// `<宽>x<高>`，`x` 大小写不限。
fn parse_resolution(value: &str) -> Result<Resolution, SyntaxError> {
    let bad = || SyntaxError::Resolution(value.to_owned());
    let (width, height) = value.split_once(['x', 'X']).ok_or_else(bad)?;
    Ok(Resolution {
        width: width.trim().parse().map_err(|_| bad())?,
        height: height.trim().parse().map_err(|_| bad())?,
    })
}
