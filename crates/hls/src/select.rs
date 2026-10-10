//! 选轨：从主播放列表里选出要下载的视频变体与音频 rendition。

use url::Url;

use crate::{MasterPlaylist, Rendition, RenditionKind, Variant};

/// 调用方对选轨的要求；默认取最高画质的变体与其音频组里的默认 rendition。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Preference {
    pub variant: VariantChoice,
    /// 所选变体引用了音频组（AUDIO 属性）时取组里哪个 rendition；`Default` 与 `Language` 在变体没有引用音频组时
    /// 不起作用
    pub audio: AudioChoice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VariantChoice {
    /// 分辨率最高者，同分辨率取带宽最高，都相同时取靠后的；有带 RESOLUTION 的变体时，不考虑没有 RESOLUTION 的
    /// （多为纯音频）
    #[default]
    Best,
    /// `MasterPlaylist::variants` 中的下标
    Index(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AudioChoice {
    /// DEFAULT=YES 的 rendition，没有则取组内第一个
    #[default]
    Default,
    /// 组内第一个 LANGUAGE 与它相同（不区分大小写）的
    Language(String),
    /// 主播放列表里第几个音频 rendition（只数 TYPE=AUDIO 的，按出现顺序，从 0 起）；须在所选变体的音频组里
    Index(usize),
}

/// 选轨结果：所选变体，以及（音频不在变体里时）独立的音频 rendition。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// 所选变体在 `MasterPlaylist::variants` 中的下标
    pub variant_index: usize,
    pub variant: Variant,
    /// 独立的音频 rendition；None 表示音频混在变体流里（或没有音频）
    pub audio: Option<SelectedAudio>,
}

/// 有独立媒体播放列表的音频 rendition。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedAudio {
    /// 主播放列表里第几个音频 rendition，口径同 [`AudioChoice::Index`]
    pub index: usize,
    /// 该 rendition 的媒体播放列表地址
    pub uri: Url,
    pub rendition: Rendition,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    #[error("主播放列表里没有变体")]
    NoVariants,
    #[error("变体下标 {index} 超出范围（共 {count} 个）")]
    VariantIndexOutOfRange { index: usize, count: usize },
    #[error("音频组 {group} 中没有语言为 {language} 的 rendition，可选：{available:?}")]
    AudioLanguageNotFound {
        group: String,
        language: String,
        available: Vec<String>,
    },
    /// 口径同 [`AudioChoice::Index`]，`count` 为音频 rendition 的个数
    #[error("音频下标 {index} 超出范围（共 {count} 个）")]
    AudioIndexOutOfRange { index: usize, count: usize },
    /// 指定的音频 rendition 不在所选变体引用的音频组里（变体没有引用音频组时 `group` 为 None）
    #[error("第 {index} 个音频不在所选变体的音频组（{}）里", group.as_deref().unwrap_or("无"))]
    NotVariantAudio { index: usize, group: Option<String> },
}

pub fn select(master: &MasterPlaylist, preference: &Preference) -> Result<Selection, SelectError> {
    let variant_index = match preference.variant {
        VariantChoice::Index(index) if index < master.variants.len() => index,
        VariantChoice::Index(index) => {
            return Err(SelectError::VariantIndexOutOfRange {
                index,
                count: master.variants.len(),
            });
        }
        VariantChoice::Best => best_variant(master).ok_or(SelectError::NoVariants)?,
    };
    let variant = &master.variants[variant_index];
    let group = variant.audio.as_deref();
    let audio = match (&preference.audio, group) {
        (AudioChoice::Index(index), group) => Some(audio_by_index(master, *index, group)?),
        (AudioChoice::Language(language), Some(group)) => {
            Some(audio_by_language(master, group, language)?)
        }
        (AudioChoice::Default, Some(group)) => default_audio(master, group),
        (AudioChoice::Language(_) | AudioChoice::Default, None) => None,
    };
    Ok(Selection {
        variant_index,
        variant: variant.clone(),
        audio: audio.and_then(|(index, rendition)| separate(index, rendition)),
    })
}

/// 分辨率最高者，同分辨率取带宽最高（并列时取靠后的）；有带 RESOLUTION 的变体时，不考虑没有 RESOLUTION 的。
fn best_variant(master: &MasterPlaylist) -> Option<usize> {
    let with_video = master.variants.iter().any(|v| v.resolution.is_some());
    master
        .variants
        .iter()
        .enumerate()
        .filter(|(_, v)| !with_video || v.resolution.is_some())
        .max_by_key(|(_, v)| {
            let pixels = v
                .resolution
                .map_or(0, |r| u64::from(r.width) * u64::from(r.height));
            (pixels, v.bandwidth.unwrap_or(0))
        })
        .map(|(index, _)| index)
}

/// 主播放列表里的音频 rendition 与它们的位置（口径同 [`AudioChoice::Index`]）。
pub fn audio_renditions(master: &MasterPlaylist) -> impl Iterator<Item = (usize, &Rendition)> {
    master
        .renditions
        .iter()
        .filter(|r| r.kind == RenditionKind::Audio)
        .enumerate()
}

/// 按位置指定的音频，须在 `group` 组里。
fn audio_by_index<'a>(
    master: &'a MasterPlaylist,
    index: usize,
    group: Option<&str>,
) -> Result<(usize, &'a Rendition), SelectError> {
    let (_, rendition) =
        audio_renditions(master)
            .nth(index)
            .ok_or_else(|| SelectError::AudioIndexOutOfRange {
                index,
                count: audio_renditions(master).count(),
            })?;
    if group == Some(rendition.group_id.as_str()) {
        Ok((index, rendition))
    } else {
        Err(SelectError::NotVariantAudio {
            index,
            group: group.map(str::to_owned),
        })
    }
}

/// 组里第一个语言相同（不区分大小写）的音频。
fn audio_by_language<'a>(
    master: &'a MasterPlaylist,
    group: &str,
    language: &str,
) -> Result<(usize, &'a Rendition), SelectError> {
    let in_group = || audio_renditions(master).filter(|(_, r)| r.group_id == group);
    in_group()
        .find(|(_, r)| {
            r.language
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case(language))
        })
        .ok_or_else(|| SelectError::AudioLanguageNotFound {
            group: group.to_owned(),
            language: language.to_owned(),
            available: in_group().filter_map(|(_, r)| r.language.clone()).collect(),
        })
}

/// 组里 DEFAULT=YES 的音频，没有则取第一个；组里没有音频时为 None。
fn default_audio<'a>(master: &'a MasterPlaylist, group: &str) -> Option<(usize, &'a Rendition)> {
    let in_group = || audio_renditions(master).filter(|(_, r)| r.group_id == group);
    in_group()
        .find(|(_, r)| r.default)
        .or_else(|| in_group().next())
}

/// 有独立媒体播放列表时为它；没有 URI 的 rendition 混在变体流里。
fn separate(index: usize, rendition: &Rendition) -> Option<SelectedAudio> {
    rendition.uri.clone().map(|uri| SelectedAudio {
        index,
        uri,
        rendition: rendition.clone(),
    })
}
