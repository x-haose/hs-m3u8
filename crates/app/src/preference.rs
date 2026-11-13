//! 选轨偏好在任务存储里的写法：核心的 [`Preference`] 没有序列化形式，这里是它的镜像。
//!
//! 只存跨次拉取仍有效的偏好：变体为最高画质或按身份（[`VariantKey`]），音频为默认、按语言或按身份。按下标选
//! （`Index`）只对同一份主播放列表有效，重开应用、续传时可能选到别的，不存。

use hs_m3u8_core::hls::{AudioChoice, AudioKey, Preference, Resolution, VariantChoice, VariantKey};
use serde::{Deserialize, Serialize};

use crate::Error;

/// [`Preference`] 的存储形式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredPreference {
    variant: StoredVariant,
    audio: StoredAudio,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum StoredVariant {
    Best,
    Stable {
        id: String,
        audio_group: Option<String>,
        occurrence: usize,
    },
    Attributes {
        bandwidth: Option<u64>,
        /// [宽, 高]
        resolution: Option<[u32; 2]>,
        codecs: Vec<String>,
        audio_group: Option<String>,
        occurrence: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum StoredAudio {
    Default,
    Language(String),
    Stable {
        id: String,
        occurrence: usize,
    },
    Attributes {
        group: String,
        name: Option<String>,
        language: Option<String>,
        occurrence: usize,
    },
}

impl TryFrom<&Preference> for StoredPreference {
    type Error = Error;

    /// 按下标选的偏好报 [`Error::InvalidInput`]：界面按探测结果选轨时用身份（`VariantInfo::choice`）。
    fn try_from(preference: &Preference) -> Result<Self, Error> {
        let variant = match &preference.variant {
            VariantChoice::Best => StoredVariant::Best,
            VariantChoice::Index(_) => {
                return Err(Error::InvalidInput(
                    "按下标选的变体不能存：重开后可能选到别的".to_owned(),
                ));
            }
            VariantChoice::Matching(VariantKey::Stable {
                id,
                audio_group,
                occurrence,
            }) => StoredVariant::Stable {
                id: id.clone(),
                audio_group: audio_group.clone(),
                occurrence: *occurrence,
            },
            VariantChoice::Matching(VariantKey::Attributes {
                bandwidth,
                resolution,
                codecs,
                audio_group,
                occurrence,
            }) => StoredVariant::Attributes {
                bandwidth: *bandwidth,
                resolution: resolution.map(|r| [r.width, r.height]),
                codecs: codecs.clone(),
                audio_group: audio_group.clone(),
                occurrence: *occurrence,
            },
        };
        let audio = match &preference.audio {
            AudioChoice::Default => StoredAudio::Default,
            AudioChoice::Language(language) => StoredAudio::Language(language.clone()),
            AudioChoice::Index(_) => {
                return Err(Error::InvalidInput(
                    "按下标选的音频不能存：重开后可能选到别的".to_owned(),
                ));
            }
            AudioChoice::Matching(AudioKey::Stable { id, occurrence }) => StoredAudio::Stable {
                id: id.clone(),
                occurrence: *occurrence,
            },
            AudioChoice::Matching(AudioKey::Attributes {
                group,
                name,
                language,
                occurrence,
            }) => StoredAudio::Attributes {
                group: group.clone(),
                name: name.clone(),
                language: language.clone(),
                occurrence: *occurrence,
            },
        };
        Ok(StoredPreference { variant, audio })
    }
}

impl From<&StoredPreference> for Preference {
    fn from(stored: &StoredPreference) -> Self {
        let variant = match &stored.variant {
            StoredVariant::Best => VariantChoice::Best,
            StoredVariant::Stable {
                id,
                audio_group,
                occurrence,
            } => VariantChoice::Matching(VariantKey::Stable {
                id: id.clone(),
                audio_group: audio_group.clone(),
                occurrence: *occurrence,
            }),
            StoredVariant::Attributes {
                bandwidth,
                resolution,
                codecs,
                audio_group,
                occurrence,
            } => VariantChoice::Matching(VariantKey::Attributes {
                bandwidth: *bandwidth,
                resolution: resolution.map(|[width, height]| Resolution { width, height }),
                codecs: codecs.clone(),
                audio_group: audio_group.clone(),
                occurrence: *occurrence,
            }),
        };
        let audio = match &stored.audio {
            StoredAudio::Default => AudioChoice::Default,
            StoredAudio::Language(language) => AudioChoice::Language(language.clone()),
            StoredAudio::Stable { id, occurrence } => AudioChoice::Matching(AudioKey::Stable {
                id: id.clone(),
                occurrence: *occurrence,
            }),
            StoredAudio::Attributes {
                group,
                name,
                language,
                occurrence,
            } => AudioChoice::Matching(AudioKey::Attributes {
                group: group.clone(),
                name: name.clone(),
                language: language.clone(),
                occurrence: *occurrence,
            }),
        };
        Preference { variant, audio }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_preferences_round_trip_and_index_choices_are_rejected() {
        let preferences = [
            Preference::default(),
            Preference {
                variant: VariantChoice::Matching(VariantKey::Attributes {
                    bandwidth: Some(6_300_000),
                    resolution: Some(Resolution {
                        width: 1920,
                        height: 1080,
                    }),
                    codecs: vec!["avc1.640028".into()],
                    audio_group: Some("aud".into()),
                    occurrence: 1,
                }),
                audio: AudioChoice::Matching(AudioKey::Attributes {
                    group: "aud".into(),
                    name: Some("English".into()),
                    language: Some("en".into()),
                    occurrence: 0,
                }),
            },
            Preference {
                variant: VariantChoice::Matching(VariantKey::Stable {
                    id: "v1".into(),
                    audio_group: None,
                    occurrence: 0,
                }),
                audio: AudioChoice::Language("zh".into()),
            },
        ];
        for preference in preferences {
            let stored = StoredPreference::try_from(&preference).unwrap();
            let json = serde_json::to_string(&stored).unwrap();
            let read: StoredPreference = serde_json::from_str(&json).unwrap();
            assert_eq!(Preference::from(&read), preference);
        }
        for rejected in [
            Preference {
                variant: VariantChoice::Index(0),
                audio: AudioChoice::Default,
            },
            Preference {
                variant: VariantChoice::Best,
                audio: AudioChoice::Index(0),
            },
        ] {
            assert!(matches!(
                StoredPreference::try_from(&rejected),
                Err(Error::InvalidInput(_))
            ));
        }
    }
}
