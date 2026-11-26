//! 选轨偏好：按什么挑变体、音频与字幕。

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

variants! {
    /// 变体的身份（见 `VariantInfo.key`）：来源写了稳定 ID 时以它为主，没写时是属性；再加上身份相同的变体里排第几。
    pub enum VariantKey {
        /// 来源写了 STABLE-VARIANT-ID：按它与引用的音频组认，码率、编码等属性变了也是同一条。同一条流常按音频组各列一行
        /// （如 AAC 与 E-AC-3 各一组），稳定 ID 相同，靠音频组区分
        #[pyo3(constructor = (*, id, audio_group, occurrence))]
        Stable {
            /// STABLE-VARIANT-ID
            id: String,
            /// 引用的音频组；没有时为 None
            audio_group: Option<String>,
            /// 上面两项都相同的变体里排第几（从 0 起）；同一条流经几条路径分发而组名相同时有几个
            occurrence: Count<usize>,
        },
        /// 来源没写稳定 ID：各项同 `VariantInfo` 的同名属性。找的时候不看变体有没有稳定 ID，来源后来写上了也找得回
        #[pyo3(constructor = (*, bandwidth, resolution, codecs, audio_group, occurrence))]
        Attributes {
            /// BANDWIDTH，bit/s；来源没写时为 None
            bandwidth: Option<Count<u64>>,
            /// RESOLUTION；来源没写时为 None
            resolution: Option<Resolution>,
            /// CODECS 拆开后的各项
            codecs: Seq<String>,
            /// 引用的音频组；没有时为 None
            audio_group: Option<String>,
            /// 上面几项完全相同的变体里排第几（从 0 起）
            occurrence: Count<usize>,
        },
    }
}

variants! {
    /// 取哪些音频。`Default`、`Language`、`All` 在变体没有引用音频组时不起作用（用变体里混着的音频）；变体引用的组在主
    /// 播放列表里一条也没有时，`Default`、`All` 同样用混着的，点名的（`Language`、`Index`、`Matching`、`These`）报找不到。
    /// `Language(x)`、`Index(i)`、`Matching(k)` 与只有同一项的 `These((RenditionChoice 的同一项,))` 选出的音频相同，任务
    /// 目录记的选轨偏好也相同（同一个任务目录可以换着用），只是两者 `==` 不相等。
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
        /// 所选变体的音频组里身份相同的音频（由 `AudioInfo.choice()` 得到）
        Matching {
            /// 身份
            key: AudioKey,
        },
        /// 一个也不要：只取视频，变体里混着的音频也不要
        Off {},
        /// 组里全部，按出现顺序各成一条音轨
        All {},
        /// 逐个指定，按给出的顺序各成一条音轨；至少一项，两项选到同一条时抛 `SelectError`（`SelectFailure.Duplicate`）
        These {
            /// 各项
            choices: Seq<RenditionChoice>,
        },
    }
}

variants! {
    /// 取哪些字幕（WebVTT 分段字幕；fMP4 里的字幕与隐藏字幕不支持：自动选到的跳过并列在 `Selected.skipped_subtitles`，
    /// 点名的报错）。变体没有引用字幕组时，按语言点名的抛 `SelectError`（`SelectFailure.NoGroup`），其余不选。
    pub enum Subtitles {
        /// 一个也不要
        Off {},
        /// 组里 DEFAULT=YES 的；没有则不选
        Default {},
        /// 组里全部，按出现顺序各成一条字幕轨
        All {},
        /// 逐个指定，按给出的顺序各成一条字幕轨；至少一项，两项选到同一条时抛 `SelectError`（`SelectFailure.Duplicate`）
        These {
            /// 各项
            choices: Seq<RenditionChoice>,
        },
    }
}

variants! {
    /// 指定一条音频或字幕 rendition（用在 `AudioChoice.These`、`Subtitles.These` 里）。
    pub enum RenditionChoice {
        /// 组内第一个 LANGUAGE 与它相同（不区分大小写）的
        Language {
            /// 语言，如 `en`、`zh-Hans`
            language: String,
        },
        /// 主播放列表里第几个这类 rendition（音频只数音频，字幕只数字幕，从 0 起，见 `AudioInfo.index`、
        /// `SubtitleInfo.index`）；须在所选变体的组里。只对同一份主播放列表有效，探测之后开始任务用
        /// `AudioInfo.rendition_choice()`、`SubtitleInfo.choice()`
        Index {
            /// 下标
            index: Count<usize>,
        },
        /// 所选变体的组里身份相同的（由 `AudioInfo.rendition_choice()`、`SubtitleInfo.choice()` 得到）
        Matching {
            /// 身份
            key: RenditionKey,
        },
    }
}

variants! {
    /// 音频或字幕 rendition 的身份（见 `AudioInfo.rendition_key`、`SubtitleInfo.key`），在所选变体引用的组里认，同
    /// `AudioKey`。
    pub enum RenditionKey {
        /// 来源写了 STABLE-RENDITION-ID：只按它认
        #[pyo3(constructor = (*, id, occurrence))]
        Stable {
            /// STABLE-RENDITION-ID
            id: String,
            /// 同一组里稳定 ID 相同的里排第几（从 0 起）
            occurrence: Count<usize>,
        },
        /// 来源没写稳定 ID：各项同 `AudioInfo`、`SubtitleInfo` 的同名属性
        #[pyo3(constructor = (*, group, name, language, occurrence))]
        Attributes {
            /// GROUP-ID
            group: String,
            /// NAME；来源没写时为 None
            name: Option<String>,
            /// LANGUAGE；来源没写时为 None
            language: Option<String>,
            /// 上面几项完全相同的里排第几（从 0 起）
            occurrence: Count<usize>,
        },
    }
}

variants! {
    /// 音频 rendition 的身份（见 `AudioInfo.key`），在所选变体引用的音频组里认（组由变体给出），同 `VariantKey`。
    pub enum AudioKey {
        /// 来源写了 STABLE-RENDITION-ID：只按它认
        #[pyo3(constructor = (*, id, occurrence))]
        Stable {
            /// STABLE-RENDITION-ID
            id: String,
            /// 同一组里稳定 ID 相同的音频里排第几（从 0 起）
            occurrence: Count<usize>,
        },
        /// 来源没写稳定 ID：各项同 `AudioInfo` 的同名属性
        #[pyo3(constructor = (*, group, name, language, occurrence))]
        Attributes {
            /// GROUP-ID
            group: String,
            /// NAME；来源没写时为 None
            name: Option<String>,
            /// LANGUAGE；来源没写时为 None
            language: Option<String>,
            /// 上面几项完全相同的音频里排第几（从 0 起）
            occurrence: Count<usize>,
        },
    }
}

record! {
    @custom_new
    /// 选轨偏好。来源是主播放列表时，与任务目录记录的不同抛 `WorkDirError`（`WorkDirProblem.PreferenceChanged`）；任务
    /// 目录里有已完成的分片时按记录的身份找回同一条轨，不按偏好重新选（`JobRequest.reselect` 时重新选）；找回的变体分辨
    /// 率、编码种类须与记录的相符，来源把同一稳定 ID 给了另一档时不算同一条。按身份选的（`Matching`）续传时要给当初那
    /// 个：重新探测后再 `choice()` 得到的身份可能不同（来源没写稳定 ID 时身份含码率等属性），选的仍是同一内容时用
    /// `JobRequest.reselect`。
    pub struct Preference {
        /// 选哪个变体
        pub variant: VariantChoice,
        /// 取哪些音频
        pub audio: AudioChoice,
        /// 取哪些字幕
        pub subtitles: Subtitles,
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Preference {
    /// `variant` 默认 `VariantChoice.Best()`，`audio` 默认 `AudioChoice.Default()`，`subtitles` 默认 `Subtitles.Off()`；
    /// 语言含换行、`These` 一项也没给时抛 `InvalidInputError`。
    #[new]
    #[pyo3(signature = (
        *,
        variant = Preference::core_default().variant,
        audio = Preference::core_default().audio,
        subtitles = Preference::core_default().subtitles,
    ))]
    fn new(
        py: Python<'_>,
        variant: VariantChoice,
        audio: AudioChoice,
        subtitles: Subtitles,
    ) -> PyResult<Self> {
        let preference = Preference {
            variant,
            audio,
            subtitles,
        };
        core::Source::check_preference(&preference.to_core())
            .map_err(|error| to_pyerr(py, error))?;
        Ok(preference)
    }
}

impl Preference {
    /// 核心的默认偏好（最高画质的变体、组里默认的音频、不要字幕）对应的 Python 写法；签名里的默认值用它，与核心一致。
    pub(crate) fn core_default() -> Self {
        let core::hls::Preference {
            variant,
            audio,
            subtitles,
        } = core::hls::Preference::default();
        Preference {
            variant: match variant {
                core::hls::VariantChoice::Best => VariantChoice::Best {},
                core::hls::VariantChoice::Index(index) => VariantChoice::Index {
                    index: Count(index),
                },
                core::hls::VariantChoice::Matching(key) => {
                    VariantChoice::Matching { key: key.into() }
                }
            },
            audio: match audio {
                core::hls::Renditions::Default => AudioChoice::Default {},
                other => unreachable!("核心的默认音频是组里默认的，不是 {other:?}"),
            },
            subtitles: match subtitles {
                core::hls::Renditions::None => Subtitles::Off {},
                other => unreachable!("核心默认不要字幕，不是 {other:?}"),
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
                AudioChoice::Default {} => core::hls::Renditions::Default,
                AudioChoice::Language { language } => {
                    core::hls::Renditions::These(vec![core::hls::RenditionChoice::Language(
                        language.clone(),
                    )])
                }
                AudioChoice::Index { index } => {
                    core::hls::Renditions::These(vec![core::hls::RenditionChoice::Index(index.0)])
                }
                AudioChoice::Matching { key } => {
                    core::hls::Renditions::These(vec![core::hls::RenditionChoice::Matching(
                        key.to_core(),
                    )])
                }
                AudioChoice::Off {} => core::hls::Renditions::None,
                AudioChoice::All {} => core::hls::Renditions::All,
                AudioChoice::These { choices } => core::hls::Renditions::These(
                    choices.0.iter().map(RenditionChoice::to_core).collect(),
                ),
            },
            subtitles: match &self.subtitles {
                Subtitles::Off {} => core::hls::Renditions::None,
                Subtitles::Default {} => core::hls::Renditions::Default,
                Subtitles::All {} => core::hls::Renditions::All,
                Subtitles::These { choices } => core::hls::Renditions::These(
                    choices.0.iter().map(RenditionChoice::to_core).collect(),
                ),
            },
        }
    }
}

impl RenditionChoice {
    fn to_core(&self) -> core::hls::RenditionChoice {
        match self {
            RenditionChoice::Language { language } => {
                core::hls::RenditionChoice::Language(language.clone())
            }
            RenditionChoice::Index { index } => core::hls::RenditionChoice::Index(index.0),
            RenditionChoice::Matching { key } => {
                core::hls::RenditionChoice::Matching(key.to_core())
            }
        }
    }
}

impl From<core::hls::RenditionKey> for RenditionKey {
    fn from(key: core::hls::RenditionKey) -> Self {
        match key {
            core::hls::RenditionKey::Stable { id, occurrence } => RenditionKey::Stable {
                id,
                occurrence: Count(occurrence),
            },
            core::hls::RenditionKey::Attributes {
                group,
                name,
                language,
                occurrence,
            } => RenditionKey::Attributes {
                group,
                name,
                language,
                occurrence: Count(occurrence),
            },
        }
    }
}

impl RenditionKey {
    fn to_core(&self) -> core::hls::RenditionKey {
        match self.clone() {
            RenditionKey::Stable { id, occurrence } => core::hls::RenditionKey::Stable {
                id,
                occurrence: occurrence.0,
            },
            RenditionKey::Attributes {
                group,
                name,
                language,
                occurrence,
            } => core::hls::RenditionKey::Attributes {
                group,
                name,
                language,
                occurrence: occurrence.0,
            },
        }
    }
}

impl From<core::hls::VariantKey> for VariantKey {
    fn from(key: core::hls::VariantKey) -> Self {
        match key {
            core::hls::VariantKey::Stable {
                id,
                audio_group,
                occurrence,
            } => VariantKey::Stable {
                id,
                audio_group,
                occurrence: Count(occurrence),
            },
            core::hls::VariantKey::Attributes {
                bandwidth,
                resolution,
                codecs,
                audio_group,
                occurrence,
            } => VariantKey::Attributes {
                bandwidth: bandwidth.map(Count),
                resolution: resolution.map(Resolution::from),
                codecs: Seq(codecs),
                audio_group,
                occurrence: Count(occurrence),
            },
        }
    }
}

impl VariantKey {
    fn to_core(&self) -> core::hls::VariantKey {
        match self.clone() {
            VariantKey::Stable {
                id,
                audio_group,
                occurrence,
            } => core::hls::VariantKey::Stable {
                id,
                audio_group,
                occurrence: occurrence.0,
            },
            VariantKey::Attributes {
                bandwidth,
                resolution,
                codecs,
                audio_group,
                occurrence,
            } => core::hls::VariantKey::Attributes {
                bandwidth: bandwidth.map(|b| b.0),
                resolution: resolution.as_ref().map(Resolution::to_core),
                codecs: codecs.0,
                audio_group,
                occurrence: occurrence.0,
            },
        }
    }
}

impl From<core::hls::RenditionKey> for AudioKey {
    fn from(key: core::hls::RenditionKey) -> Self {
        match key {
            core::hls::RenditionKey::Stable { id, occurrence } => AudioKey::Stable {
                id,
                occurrence: Count(occurrence),
            },
            core::hls::RenditionKey::Attributes {
                group,
                name,
                language,
                occurrence,
            } => AudioKey::Attributes {
                group,
                name,
                language,
                occurrence: Count(occurrence),
            },
        }
    }
}

impl AudioKey {
    fn to_core(&self) -> core::hls::RenditionKey {
        match self.clone() {
            AudioKey::Stable { id, occurrence } => core::hls::RenditionKey::Stable {
                id,
                occurrence: occurrence.0,
            },
            AudioKey::Attributes {
                group,
                name,
                language,
                occurrence,
            } => core::hls::RenditionKey::Attributes {
                group,
                name,
                language,
                occurrence: occurrence.0,
            },
        }
    }
}
