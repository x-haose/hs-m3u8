//! 选轨：从主播放列表里选出要下载的视频变体，以及它引用的组里的音频与字幕 rendition。

use crate::{
    CodecKind, MasterPlaylist, Rendition, RenditionKey, RenditionKind, Variant, VariantKey,
};

/// 调用方对选轨的要求；默认取最高画质的变体、其音频组里的默认音频，不要字幕。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preference {
    pub variant: VariantChoice,
    /// 所选变体的音频组（AUDIO 属性）里取哪些音频
    pub audio: Renditions,
    /// 所选变体的字幕组（SUBTITLES 属性）里取哪些字幕
    pub subtitles: Renditions,
}

impl Default for Preference {
    fn default() -> Self {
        Preference {
            variant: VariantChoice::Best,
            audio: Renditions::Default,
            subtitles: Renditions::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum VariantChoice {
    /// 画质最高者：确定是纯音频的变体（写了 CODECS 且全是音频或字幕编码）在有别的变体时不考虑；其余各变体都写了
    /// RESOLUTION 时取分辨率最高、同分辨率取带宽最高，有没写的（分辨率比不了）时取带宽最高；都相同时取靠后的
    #[default]
    Best,
    /// `MasterPlaylist::variants` 中的下标；只对同一份主播放列表有效，之后拉到的增删重排了变体时选到的是别的
    Index(usize),
    /// 身份相同的变体（见 [`VariantKey`]）：之后拉到的主播放列表增删重排了变体，也选到同一个
    Matching(VariantKey),
}

/// 一类 rendition（音频或字幕）选哪些，都在所选变体引用的那一组里找。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Renditions {
    /// 一个也不要。音频为它时只取视频（变体里混着的音频也不要）
    None,
    /// 组里 DEFAULT=YES 的；音频没有时取组里第一个，字幕没有时不选。变体没有引用这类组时：音频用变体里混着的，字幕不选
    Default,
    /// 组里全部，按出现顺序
    All,
    /// 逐个指定，按给出的顺序成轨；两项选到同一条时报 [`SelectError::Duplicate`]。至少一项，空的报
    /// [`SelectError::NoChoices`]：一条也不要用 [`Renditions::None`]
    These(Vec<RenditionChoice>),
}

/// 指定一条音频或字幕 rendition。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenditionChoice {
    /// 组内第一个 LANGUAGE 与它相同（不区分大小写）的。变体没有引用音频组时音频的不起作用（用变体里混着的）；字幕的报
    /// [`SelectError::LanguageNotFound`]
    Language(String),
    /// 主播放列表里第几个这类 rendition（音频只数 TYPE=AUDIO 的，字幕只数 TYPE=SUBTITLES 的，按出现顺序，从 0 起）；
    /// 须在所选变体的组里。只对同一份主播放列表有效，同 [`VariantChoice::Index`]
    Index(usize),
    /// 所选变体的组里身份相同的（见 [`RenditionKey`]）
    Matching(RenditionKey),
}

/// 选轨结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// 所选变体在 `MasterPlaylist::variants` 中的下标
    pub variant_index: usize,
    pub variant: Variant,
    /// 用变体流里混着的音频：音频不为 [`Renditions::None`]，而变体没有引用音频组、引用的组在主播放列表里没有一条，或选中
    /// 的音频里有没写 URI 的
    pub muxed_audio: bool,
    /// 选中的、有独立媒体播放列表的音频，按选的顺序
    pub audio: Vec<SelectedRendition>,
    /// 选中的字幕，按选的顺序；都有媒体播放列表（没写 URI 的字幕报 [`SelectError::SubtitlesWithoutUri`]）
    pub subtitles: Vec<SelectedRendition>,
}

/// 选中的一条有媒体播放列表的 rendition。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedRendition {
    /// 主播放列表里第几个这类 rendition，口径同 [`RenditionChoice::Index`]
    pub index: usize,
    /// 该 rendition 的媒体播放列表地址
    pub uri: crate::Url,
    pub rendition: Rendition,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    #[error("主播放列表里没有变体")]
    NoVariants,
    #[error("变体下标 {index} 超出范围（共 {count} 个）")]
    VariantIndexOutOfRange { index: usize, count: usize },
    /// 主播放列表里没有身份与 [`VariantChoice::Matching`] 相同的变体
    #[error("主播放列表里没有身份相同的变体")]
    VariantNotFound,
    /// `group` 为所选变体引用的组，没有引用时为 None
    #[error("{}组 {} 中没有语言为 {language} 的 rendition，可选：{available:?}", kind_name(*kind), group.as_deref().unwrap_or("（变体没有引用）"))]
    LanguageNotFound {
        kind: RenditionKind,
        group: Option<String>,
        language: String,
        available: Vec<String>,
    },
    /// 口径同 [`RenditionChoice::Index`]，`count` 为这类 rendition 的个数
    #[error("{}下标 {index} 超出范围（共 {count} 个）", kind_name(*kind))]
    IndexOutOfRange {
        kind: RenditionKind,
        index: usize,
        count: usize,
    },
    /// 所选变体引用的组里没有身份与 [`RenditionChoice::Matching`] 相同的（变体没有引用这类组时也是）
    #[error("所选变体的{}组里没有身份相同的", kind_name(*kind))]
    NotFound { kind: RenditionKind },
    /// 按下标指定的 rendition 不在所选变体引用的组里（变体没有引用这类组时 `group` 为 None）
    #[error("第 {index} 个{}不在所选变体的组（{}）里", kind_name(*kind), group.as_deref().unwrap_or("无"))]
    NotInGroup {
        kind: RenditionKind,
        index: usize,
        group: Option<String>,
    },
    /// [`Renditions::These`] 里两项选到了同一条（`index` 口径同 [`RenditionChoice::Index`]）
    #[error("第 {index} 个{}被选了两次", kind_name(*kind))]
    Duplicate { kind: RenditionKind, index: usize },
    /// [`Renditions::These`] 是空的
    #[error("指定{}时一项也没给", kind_name(*kind))]
    NoChoices { kind: RenditionKind },
    /// 选中的字幕没写 URI（RFC 8216 4.3.4.1 要求 SUBTITLES 必须有）
    #[error("第 {index} 个字幕没有 URI")]
    SubtitlesWithoutUri { index: usize },
}

fn kind_name(kind: RenditionKind) -> &'static str {
    match kind {
        RenditionKind::Audio => "音频",
        RenditionKind::Subtitles => "字幕",
        RenditionKind::Video => "视频",
        RenditionKind::ClosedCaptions => "隐藏字幕",
    }
}

pub fn select(master: &MasterPlaylist, preference: &Preference) -> Result<Selection, SelectError> {
    let variant_index = match &preference.variant {
        &VariantChoice::Index(index) if index < master.variants.len() => index,
        &VariantChoice::Index(index) => {
            return Err(SelectError::VariantIndexOutOfRange {
                index,
                count: master.variants.len(),
            });
        }
        VariantChoice::Matching(key) => key.find(master).ok_or(SelectError::VariantNotFound)?,
        VariantChoice::Best => best_variant(master).ok_or(SelectError::NoVariants)?,
    };
    let variant = &master.variants[variant_index];
    let audio = pick(
        master,
        RenditionKind::Audio,
        variant.audio.as_deref(),
        &preference.audio,
    )?;
    let subtitles = pick(
        master,
        RenditionKind::Subtitles,
        variant.subtitles.as_deref(),
        &preference.subtitles,
    )?;
    // 变体引用的音频组在主播放列表里没有一条 rendition 时（来源写错了组名）与没有引用一样，用混着的音频
    let group_listed = variant.audio.as_deref().is_some_and(|group| {
        renditions(master, RenditionKind::Audio).any(|(_, r)| r.group == group)
    });
    let muxed_audio = match &preference.audio {
        Renditions::None => false,
        _ if !group_listed => true,
        _ => audio.iter().any(|(_, r)| r.uri.is_none()),
    };
    let subtitles = subtitles
        .into_iter()
        .map(|(index, rendition)| {
            separate(index, rendition).ok_or(SelectError::SubtitlesWithoutUri { index })
        })
        .collect::<Result<_, _>>()?;
    Ok(Selection {
        variant_index,
        variant: variant.clone(),
        muxed_audio,
        audio: audio
            .into_iter()
            .filter_map(|(index, rendition)| separate(index, rendition))
            .collect(),
        subtitles,
    })
}

/// 按 `choice` 从 `group` 组里选出 `kind` 类 rendition，与它们的位置（口径同 [`RenditionChoice::Index`]）。
fn pick<'a>(
    master: &'a MasterPlaylist,
    kind: RenditionKind,
    group: Option<&str>,
    choice: &Renditions,
) -> Result<Vec<(usize, &'a Rendition)>, SelectError> {
    let in_group =
        || renditions(master, kind).filter(move |(_, r)| Some(r.group.as_str()) == group);
    match choice {
        Renditions::None => Ok(Vec::new()),
        Renditions::All => Ok(in_group().collect()),
        Renditions::Default => {
            let default = in_group().find(|(_, r)| r.default);
            Ok(match (default, kind) {
                (Some(found), _) => vec![found],
                (None, RenditionKind::Audio) => in_group().next().into_iter().collect(),
                (None, _) => Vec::new(),
            })
        }
        Renditions::These(choices) if choices.is_empty() => Err(SelectError::NoChoices { kind }),
        Renditions::These(choices) => {
            let mut picked: Vec<(usize, &Rendition)> = Vec::with_capacity(choices.len());
            for choice in choices {
                if kind == RenditionKind::Audio
                    && group.is_none()
                    && matches!(choice, RenditionChoice::Language(_))
                {
                    continue;
                }
                let found = one(master, kind, group, choice)?;
                if picked.iter().any(|(index, _)| *index == found.0) {
                    return Err(SelectError::Duplicate {
                        kind,
                        index: found.0,
                    });
                }
                picked.push(found);
            }
            Ok(picked)
        }
    }
}

/// 按 `choice` 指定的一条。
fn one<'a>(
    master: &'a MasterPlaylist,
    kind: RenditionKind,
    group: Option<&str>,
    choice: &RenditionChoice,
) -> Result<(usize, &'a Rendition), SelectError> {
    match choice {
        &RenditionChoice::Index(index) => {
            let (_, rendition) = renditions(master, kind).nth(index).ok_or_else(|| {
                SelectError::IndexOutOfRange {
                    kind,
                    index,
                    count: renditions(master, kind).count(),
                }
            })?;
            if group == Some(rendition.group.as_str()) {
                Ok((index, rendition))
            } else {
                Err(SelectError::NotInGroup {
                    kind,
                    index,
                    group: group.map(str::to_owned),
                })
            }
        }
        RenditionChoice::Matching(key) => group
            .and_then(|group| key.find(master, kind, group))
            .ok_or(SelectError::NotFound { kind }),
        RenditionChoice::Language(language) => {
            let in_group =
                || renditions(master, kind).filter(|(_, r)| Some(r.group.as_str()) == group);
            in_group()
                .find(|(_, r)| {
                    r.language
                        .as_deref()
                        .is_some_and(|l| l.eq_ignore_ascii_case(language))
                })
                .ok_or_else(|| SelectError::LanguageNotFound {
                    kind,
                    group: group.map(str::to_owned),
                    language: language.to_owned(),
                    available: in_group().filter_map(|(_, r)| r.language.clone()).collect(),
                })
        }
    }
}

/// 见 [`VariantChoice::Best`]。
fn best_variant(master: &MasterPlaylist) -> Option<usize> {
    let all = || master.variants.iter().enumerate();
    let with_video: Vec<(usize, &Variant)> = all().filter(|(_, v)| !audio_only(v)).collect();
    let candidates = if with_video.is_empty() {
        all().collect()
    } else {
        with_video
    };
    let comparable = candidates.iter().all(|(_, v)| v.resolution.is_some());
    candidates
        .into_iter()
        .max_by_key(|(_, v)| {
            let pixels = v
                .resolution
                .filter(|_| comparable)
                .map_or(0, |r| u64::from(r.width) * u64::from(r.height));
            (pixels, v.bandwidth.unwrap_or(0))
        })
        .map(|(index, _)| index)
}

/// 变体确定是纯音频：写了 CODECS，且列出的都是音频或字幕编码（见 [`CodecKind`]）。没写 CODECS、或有认不出的编码时
/// 不算：可能有视频。
fn audio_only(variant: &Variant) -> bool {
    !variant.codecs.is_empty()
        && variant
            .codecs
            .iter()
            .all(|codec| CodecKind::of(codec) != CodecKind::Video)
}

/// 主播放列表里 `kind` 类的 rendition 与它们的位置（口径同 [`RenditionChoice::Index`]）。
pub fn renditions(
    master: &MasterPlaylist,
    kind: RenditionKind,
) -> impl Iterator<Item = (usize, &Rendition)> {
    master
        .renditions
        .iter()
        .filter(move |r| r.kind == kind)
        .enumerate()
}

/// 有独立媒体播放列表时为它；没有 URI 的 rendition 混在变体流里。
fn separate(index: usize, rendition: &Rendition) -> Option<SelectedRendition> {
    rendition.uri.clone().map(|uri| SelectedRendition {
        index,
        uri,
        rendition: rendition.clone(),
    })
}
