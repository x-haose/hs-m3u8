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
    /// 分辨率最高者，同分辨率取带宽最高；有带 RESOLUTION 的变体时，不考虑没有 RESOLUTION 的（多为纯音频）
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
    #[error("rendition 下标 {index} 超出范围（共 {count} 个）")]
    RenditionIndexOutOfRange { index: usize, count: usize },
    /// 指定的 rendition 不是音频，或不在所选变体引用的音频组里（变体没有引用音频组时 `group` 为 None）
    #[error("第 {index} 个 rendition 不是所选变体的音频组（{}）里的音频", group.as_deref().unwrap_or("无"))]
    NotVariantAudio { index: usize, group: Option<String> },
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
    let is_audio_of =
        |r: &Rendition, group: &str| r.kind == RenditionKind::Audio && r.group_id == group;
    let chosen = match (&preference.audio, &variant.audio) {
        (AudioChoice::Index(index), group) => {
            let rendition =
                master
                    .renditions
                    .get(*index)
                    .ok_or(SelectError::RenditionIndexOutOfRange {
                        index: *index,
                        count: master.renditions.len(),
                    })?;
            if !group.as_deref().is_some_and(|g| is_audio_of(rendition, g)) {
                return Err(SelectError::NotVariantAudio {
                    index: *index,
                    group: group.clone(),
                });
            }
            rendition
        }
        (_, None) => return Ok(None),
        (choice, Some(group)) => {
            let candidates: Vec<&Rendition> = master
                .renditions
                .iter()
                .filter(|r| is_audio_of(r, group))
                .collect();
            let found = match choice {
                AudioChoice::Language(language) => candidates.iter().copied().find(|r| {
                    r.language
                        .as_deref()
                        .is_some_and(|l| l.eq_ignore_ascii_case(language))
                }),
                _ => candidates
                    .iter()
                    .copied()
                    .find(|r| r.default)
                    .or(candidates.first().copied()),
            };
            match (found, choice) {
                (Some(rendition), _) => rendition,
                (None, AudioChoice::Language(language)) => {
                    return Err(SelectError::AudioLanguageNotFound {
                        group: group.clone(),
                        language: language.clone(),
                        available: candidates
                            .iter()
                            .filter_map(|r| r.language.clone())
                            .collect(),
                    });
                }
                (None, _) => return Ok(None),
            }
        }
    };
    // 没有 URI 的 rendition 混在变体流里，没有独立的媒体播放列表
    Ok(chosen.uri.clone().map(|uri| SelectedAudio {
        uri,
        rendition: chosen.clone(),
    }))
}
