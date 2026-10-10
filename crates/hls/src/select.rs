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
    /// `MasterPlaylist::renditions` 中的下标；须是所选变体音频组里的音频 rendition
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
    /// 在 `MasterPlaylist::renditions` 中的下标
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
    #[error("rendition 下标 {index} 超出范围（共 {count} 个）")]
    RenditionIndexOutOfRange { index: usize, count: usize },
    /// 指定的 rendition 不是音频，或不在所选变体引用的音频组里（变体没有引用音频组时 `group` 为 None）
    #[error("第 {index} 个 rendition 不是所选变体的音频组（{}）里的音频", group.as_deref().unwrap_or("无"))]
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
    let rendition = match (&preference.audio, group) {
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
        audio: rendition.and_then(|index| separate_audio(master, index)),
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

fn is_audio_of(rendition: &Rendition, group: &str) -> bool {
    rendition.kind == RenditionKind::Audio && rendition.group_id == group
}

/// 按下标指定的 rendition，须是 `group` 组里的音频。
fn audio_by_index(
    master: &MasterPlaylist,
    index: usize,
    group: Option<&str>,
) -> Result<usize, SelectError> {
    let rendition = master
        .renditions
        .get(index)
        .ok_or(SelectError::RenditionIndexOutOfRange {
            index,
            count: master.renditions.len(),
        })?;
    if group.is_some_and(|g| is_audio_of(rendition, g)) {
        Ok(index)
    } else {
        Err(SelectError::NotVariantAudio {
            index,
            group: group.map(str::to_owned),
        })
    }
}

/// 组里第一个语言相同（不区分大小写）的音频。
fn audio_by_language(
    master: &MasterPlaylist,
    group: &str,
    language: &str,
) -> Result<usize, SelectError> {
    let in_group = || {
        master
            .renditions
            .iter()
            .enumerate()
            .filter(|(_, r)| is_audio_of(r, group))
    };
    in_group()
        .find(|(_, r)| {
            r.language
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case(language))
        })
        .map(|(index, _)| index)
        .ok_or_else(|| SelectError::AudioLanguageNotFound {
            group: group.to_owned(),
            language: language.to_owned(),
            available: in_group().filter_map(|(_, r)| r.language.clone()).collect(),
        })
}

/// 组里 DEFAULT=YES 的音频，没有则取第一个；组里没有音频时为 None。
fn default_audio(master: &MasterPlaylist, group: &str) -> Option<usize> {
    let mut in_group = master
        .renditions
        .iter()
        .enumerate()
        .filter(|(_, r)| is_audio_of(r, group));
    let first = in_group.clone().next().map(|(index, _)| index);
    in_group
        .find(|(_, r)| r.default)
        .map(|(index, _)| index)
        .or(first)
}

/// 第 `index` 个 rendition 有独立的媒体播放列表时为它；没有 URI 的混在变体流里。
fn separate_audio(master: &MasterPlaylist, index: usize) -> Option<SelectedAudio> {
    let rendition = &master.renditions[index];
    rendition.uri.clone().map(|uri| SelectedAudio {
        index,
        uri,
        rendition: rendition.clone(),
    })
}
