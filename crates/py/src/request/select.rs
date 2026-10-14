//! 选轨偏好：按什么挑变体与音频。

use std::hash::Hash;

use hs_m3u8_core as core;
use pyo3::prelude::*;
use pyo3_stub_gen::derive::gen_stub_pymethods;

use crate::convert::{Count, Seq};
use crate::error::to_pyerr;
use crate::info::Resolution;
use crate::record::{record, variants};

variants! {
    /// 选哪个变体。
    pub enum VariantChoice {
        /// 画质最高者：确定是纯音频的变体（写了 CODECS 且全是音频或字幕编码）在有别的变体时不考虑；其余各变体都写了
        /// RESOLUTION 时取分辨率最高、同分辨率取带宽最高，有没写的（分辨率比不了）时取带宽最高；都相同时取靠后的
        Best {},
        /// 主播放列表里第几个变体（从 0 起，见 `VariantInfo.index`）；只对同一份主播放列表有效，探测之后开始任务用
        /// `VariantInfo.choice()`
        Index {
            /// 下标
            index: Count<usize>,
        },
        /// 身份相同的变体（由 `VariantInfo.choice()` 得到）：探测之后主播放列表增删重排了变体也选到同一个
        Matching {
            /// 身份
            key: VariantKey,
        },
    }
}

record! {
    /// 变体的身份：属性，以及属性完全相同的变体里排第几。各项同 `VariantInfo` 的同名属性。
    pub struct VariantKey {
        /// BANDWIDTH，bit/s；来源没写时为 None
        pub bandwidth: Option<Count<u64>>,
        /// RESOLUTION；来源没写时为 None
        pub resolution: Option<Resolution>,
        /// CODECS 拆开后的各项
        pub codecs: Seq<String>,
        /// 引用的音频组；没有时为 None
        pub audio_group: Option<String>,
        /// 上面几项完全相同的变体里排第几（从 0 起）
        pub occurrence: Count<usize>,
    }
}

variants! {
    /// 所选变体引用了音频组时，取组里哪个音频；`Default` 与 `Language` 在变体没有引用音频组时不起作用。
    pub enum AudioChoice {
        /// DEFAULT=YES 的，没有则取组内第一个
        Default {},
        /// 组内第一个 LANGUAGE 与它相同（不区分大小写）的
        Language {
            /// 语言，如 `en`、`zh-Hans`
            language: String,
        },
        /// 主播放列表里第几个音频（只数音频，从 0 起，见 `AudioInfo.index`）；须在所选变体的音频组里。只对同一份主播放
        /// 列表有效，探测之后开始任务用 `AudioInfo.choice()`
        Index {
            /// 下标
            index: Count<usize>,
        },
        /// 身份相同的音频（由 `AudioInfo.choice()` 得到）；须在所选变体的音频组里
        Matching {
            /// 身份
            key: AudioKey,
        },
    }
}

record! {
    /// 音频 rendition 的身份：属性，以及属性完全相同的音频里排第几。各项同 `AudioInfo` 的同名属性。
    pub struct AudioKey {
        /// GROUP-ID
        pub group: String,
        /// NAME；来源没写时为 None
        pub name: Option<String>,
        /// LANGUAGE；来源没写时为 None
        pub language: Option<String>,
        /// 上面几项完全相同的音频里排第几（从 0 起）
        pub occurrence: Count<usize>,
    }
}

record! {
    @custom_new
    /// 选轨偏好。来源是主播放列表时，与任务目录记录的不同抛 `WorkDirError`（`WorkDirProblem.PreferenceChanged`）；
    /// 任务目录里有已完成的分片时按记录的变体与音频找回同一条轨，不按偏好重新选。按身份选的（`Matching`）续传时要给
    /// 当初那个，重新探测后再 `choice()` 得到的属性可能变了。
    pub struct Preference {
        /// 选哪个变体
        pub variant: VariantChoice,
        /// 取哪个音频
        pub audio: AudioChoice,
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Preference {
    /// `variant` 默认 `VariantChoice.Best()`，`audio` 默认 `AudioChoice.Default()`；音频语言含换行时抛
    /// `InvalidInputError`。
    #[new]
    #[pyo3(signature = (
        *,
        variant = Preference::from(core::hls::Preference::default()).variant,
        audio = Preference::from(core::hls::Preference::default()).audio,
    ))]
    fn new(py: Python<'_>, variant: VariantChoice, audio: AudioChoice) -> PyResult<Self> {
        let preference = Preference { variant, audio };
        core::Source::check_preference(&preference.to_core())
            .map_err(|error| to_pyerr(py, error))?;
        Ok(preference)
    }
}

impl From<core::hls::Preference> for Preference {
    fn from(p: core::hls::Preference) -> Self {
        Preference {
            variant: match p.variant {
                core::hls::VariantChoice::Best => VariantChoice::Best {},
                core::hls::VariantChoice::Index(index) => VariantChoice::Index {
                    index: Count(index),
                },
                core::hls::VariantChoice::Matching(key) => {
                    VariantChoice::Matching { key: key.into() }
                }
            },
            audio: match p.audio {
                core::hls::AudioChoice::Default => AudioChoice::Default {},
                core::hls::AudioChoice::Language(language) => AudioChoice::Language { language },
                core::hls::AudioChoice::Index(index) => AudioChoice::Index {
                    index: Count(index),
                },
                core::hls::AudioChoice::Matching(key) => AudioChoice::Matching { key: key.into() },
            },
        }
    }
}

impl Preference {
    pub(super) fn to_core(&self) -> core::hls::Preference {
        core::hls::Preference {
            variant: match &self.variant {
                VariantChoice::Best {} => core::hls::VariantChoice::Best,
                VariantChoice::Index { index } => core::hls::VariantChoice::Index(index.0),
                VariantChoice::Matching { key } => {
                    core::hls::VariantChoice::Matching(key.to_core())
                }
            },
            audio: match &self.audio {
                AudioChoice::Default {} => core::hls::AudioChoice::Default,
                AudioChoice::Language { language } => {
                    core::hls::AudioChoice::Language(language.clone())
                }
                AudioChoice::Index { index } => core::hls::AudioChoice::Index(index.0),
                AudioChoice::Matching { key } => core::hls::AudioChoice::Matching(key.to_core()),
            },
        }
    }
}

impl From<core::hls::VariantKey> for VariantKey {
    fn from(key: core::hls::VariantKey) -> Self {
        VariantKey {
            bandwidth: key.bandwidth.map(Count),
            resolution: key.resolution.map(Resolution::from),
            codecs: Seq(key.codecs),
            audio_group: key.audio_group,
            occurrence: Count(key.occurrence),
        }
    }
}

impl VariantKey {
    fn to_core(&self) -> core::hls::VariantKey {
        core::hls::VariantKey {
            bandwidth: self.bandwidth.map(|b| b.0),
            resolution: self.resolution.as_ref().map(Resolution::to_core),
            codecs: self.codecs.0.clone(),
            audio_group: self.audio_group.clone(),
            occurrence: self.occurrence.0,
        }
    }
}

impl From<core::hls::AudioKey> for AudioKey {
    fn from(key: core::hls::AudioKey) -> Self {
        AudioKey {
            group: key.group,
            name: key.name,
            language: key.language,
            occurrence: Count(key.occurrence),
        }
    }
}

impl AudioKey {
    fn to_core(&self) -> core::hls::AudioKey {
        core::hls::AudioKey {
            group: self.group.clone(),
            name: self.name.clone(),
            language: self.language.clone(),
            occurrence: self.occurrence.0,
        }
    }
}
