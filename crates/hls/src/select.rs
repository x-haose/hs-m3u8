//! 选轨：从主播放列表里选出要下载的视频变体与音频 rendition。

use url::Url;

use crate::{MasterPlaylist, Rendition, RenditionKind, Variant};

/// 选轨偏好。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Preference {
    pub variant: VariantChoice,
    /// 音频语言（与 LANGUAGE 比较，不区分大小写）；None 时取 DEFAULT=YES 的 rendition，没有则取组内第一个
    pub audio_language: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VariantChoice {
    /// 分辨率最高者，同分辨率取带宽最高；有带分辨率的变体时不考虑纯音频变体
    #[default]
    Best,
    /// `MasterPlaylist::variants` 中的下标
    Index(usize),
}

/// 选轨结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub variant: Variant,
    /// 独立的音频 rendition；None 表示音频混在变体流里（或没有音频）
    pub audio: Option<SelectedAudio>,
}

/// 有独立媒体播放列表的音频 rendition。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedAudio {
    /// 该 rendition 的媒体播放列表地址
    pub uri: Url,
    pub rendition: Rendition,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    #[error("主播放列表里没有变体")]
    NoVariants,
    #[error("变体下标 {index} 超出范围（共 {count} 个）")]
    IndexOutOfRange { index: usize, count: usize },
    #[error("音频组 {group} 中没有语言为 {language} 的 rendition，可选：{available:?}")]
    AudioLanguageNotFound {
        group: String,
        language: String,
        available: Vec<String>,
    },
}

pub fn select(master: &MasterPlaylist, preference: &Preference) -> Result<Selection, SelectError> {
    let variant = match preference.variant {
        VariantChoice::Index(index) => {
            master
                .variants
                .get(index)
                .ok_or(SelectError::IndexOutOfRange {
                    index,
                    count: master.variants.len(),
                })?
        }
        VariantChoice::Best => {
            let with_video = master.variants.iter().any(|v| v.resolution.is_some());
            master
                .variants
                .iter()
                .filter(|v| !with_video || v.resolution.is_some())
                .max_by_key(|v| {
                    let pixels = v
                        .resolution
                        .map_or(0, |r| u64::from(r.width) * u64::from(r.height));
                    (pixels, v.bandwidth.unwrap_or(0))
                })
                .ok_or(SelectError::NoVariants)?
        }
    };
    Ok(Selection {
        variant: variant.clone(),
        audio: select_audio(master, variant, preference)?,
    })
}

fn select_audio(
    master: &MasterPlaylist,
    variant: &Variant,
    preference: &Preference,
) -> Result<Option<SelectedAudio>, SelectError> {
    let Some(group) = &variant.audio else {
        return Ok(None);
    };
    let candidates: Vec<&Rendition> = master
        .renditions
        .iter()
        .filter(|r| r.kind == RenditionKind::Audio && &r.group_id == group)
        .collect();
    let chosen = match &preference.audio_language {
        Some(language) => Some(
            candidates
                .iter()
                .find(|r| {
                    r.language
                        .as_deref()
                        .is_some_and(|l| l.eq_ignore_ascii_case(language))
                })
                .ok_or_else(|| SelectError::AudioLanguageNotFound {
                    group: group.clone(),
                    language: language.clone(),
                    available: candidates
                        .iter()
                        .filter_map(|r| r.language.clone())
                        .collect(),
                })?,
        ),
        None => candidates.iter().find(|r| r.default).or(candidates.first()),
    };
    Ok(chosen.and_then(|r| {
        Some(SelectedAudio {
            uri: r.uri.clone()?,
            rendition: (*r).clone(),
        })
    }))
}
