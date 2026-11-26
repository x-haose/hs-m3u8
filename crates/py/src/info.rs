//! 探测结果与进度里描述来源的类型：变体、音频、字幕与各轨概况。不含地址：地址常带令牌，而这些类型常被打进日志与界面。

use std::time::Duration;

use hs_m3u8_core as core;
use pyo3::prelude::*;
use pyo3_stub_gen::derive::gen_stub_pymethods;

use crate::convert::{Count, Seq};
use crate::error::invalid_input;
use crate::record::{record, variants};
use crate::report::FetchFailure;
use crate::request::{
    AudioChoice, AudioKey, RenditionChoice, RenditionKey, VariantChoice, VariantKey,
};

record! {
    /// 分辨率（RESOLUTION 属性）。
    pub struct Resolution {
        /// 宽，像素
        pub width: Count<u32>,
        /// 高，像素
        pub height: Count<u32>,
    }
}

record! {
    /// 主播放列表里的一个变体。
    pub struct VariantInfo {
        /// 主播放列表里第几个变体（从 0 起），也是在 `MasterInfo.variants` 里的位置；只对这一份主播放列表有效，开始
        /// 任务按 `choice()` 选
        pub index: usize,
        /// BANDWIDTH，bit/s；来源没写时为 None
        pub bandwidth: Option<u64>,
        /// RESOLUTION；来源没写时为 None
        pub resolution: Option<Resolution>,
        /// CODECS 拆开后的各项，如 `avc1.640028`、`mp4a.40.2`
        pub codecs: Seq<String>,
        /// 引用的音频组（AUDIO 属性）；没有时为 None
        pub audio_group: Option<String>,
        /// 身份，开始任务按它选（`choice()`）
        pub key: VariantKey,
        /// 引用的字幕组（SUBTITLES 属性）；没有时为 None
        pub subtitle_group: Option<String> = None,
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl VariantInfo {
    /// 按身份选这个变体：探测之后主播放列表增删重排了变体，开始任务时也选到它。续传时给当初得到的那个，见
    /// `Preference`。
    #[gen_stub(override_return_type(type_repr = "VariantChoice.Matching"))]
    fn choice(&self) -> VariantChoice {
        VariantChoice::Matching {
            key: self.key.clone(),
        }
    }
}

record! {
    /// 主播放列表里的一个音频 rendition。
    pub struct AudioInfo {
        /// 主播放列表里第几个音频 rendition（只数音频，从 0 起），也是在 `MasterInfo.audio` 里的位置；只对这一份主
        /// 播放列表有效，开始任务按 `choice()` 选
        pub index: usize,
        /// GROUP-ID
        pub group: String,
        /// NAME；规范要求必填，来源没写时为 None
        pub name: Option<String>,
        /// LANGUAGE；来源没写时为 None
        pub language: Option<String>,
        /// DEFAULT=YES
        pub default: bool,
        /// 有独立的媒体播放列表；为 False 时音频混在变体流里，选它即用变体里的音频
        pub separate: bool,
        /// 身份，开始任务按它选（`choice()`）
        pub key: AudioKey,
        /// CHANNELS 原样，如 `2`、`6`、`16/JOC`；来源没写时为 None
        pub channels: Option<String> = None,
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl AudioInfo {
    /// 按身份选这个音频，同 `VariantInfo.choice()`。
    #[gen_stub(override_return_type(type_repr = "AudioChoice.Matching"))]
    fn choice(&self) -> AudioChoice {
        AudioChoice::Matching {
            key: self.key.clone(),
        }
    }

    /// 身份，同 `key`，写成音频与字幕共用的 `RenditionKey`（用在 `RenditionChoice.Matching` 里）。
    #[getter]
    fn rendition_key(&self) -> RenditionKey {
        match self.key.clone() {
            AudioKey::Stable { id, occurrence } => RenditionKey::Stable { id, occurrence },
            AudioKey::Attributes {
                group,
                name,
                language,
                occurrence,
            } => RenditionKey::Attributes {
                group,
                name,
                language,
                occurrence,
            },
        }
    }

    /// 按身份选这个音频，用在 `AudioChoice.These` 里：`AudioChoice.These((a.rendition_choice(), b.rendition_choice()))`
    /// 选两条音轨。
    #[gen_stub(override_return_type(type_repr = "RenditionChoice.Matching"))]
    fn rendition_choice(&self) -> RenditionChoice {
        RenditionChoice::Matching {
            key: self.rendition_key(),
        }
    }
}

record! {
    /// 主播放列表里的一个字幕 rendition。
    pub struct SubtitleInfo {
        /// 主播放列表里第几个字幕 rendition（只数字幕，从 0 起），也是在 `MasterInfo.subtitles` 里的位置；只对这一份
        /// 主播放列表有效，开始任务按 `choice()` 选
        pub index: usize,
        /// GROUP-ID
        pub group: String,
        /// NAME；规范要求必填，来源没写时为 None
        pub name: Option<String>,
        /// LANGUAGE，BCP 47（如 `zh-Hans`）；来源没写时为 None
        pub language: Option<String>,
        /// DEFAULT=YES
        pub default: bool,
        /// AUTOSELECT=YES
        pub autoselect: bool,
        /// FORCED=YES：只含画面里外语对白等必须显示的内容
        pub forced: bool,
        /// 能不能下：要读它的媒体播放列表才知道
        pub support: SubtitleSupport,
        /// 身份，开始任务按它选（`choice()`）
        pub key: RenditionKey,
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl SubtitleInfo {
    /// 按身份选这条字幕，用在 `SubtitleChoice.These` 里，同 `VariantInfo.choice()`。
    #[gen_stub(override_return_type(type_repr = "RenditionChoice.Matching"))]
    fn choice(&self) -> RenditionChoice {
        RenditionChoice::Matching {
            key: self.key.clone(),
        }
    }
}

variants! {
    /// 一条字幕能不能下（`SubtitleInfo.support`）。
    pub enum SubtitleSupport {
        /// 没读它的媒体播放列表：不在所选变体的字幕组里（选不到），或没写 URI（不合 RFC 8216，选到时报错）；开始任务时
        /// 只读选到的，其余的也是这样
        NotRead {},
        /// 分段的 WebVTT 文本，下得了
        Supported {},
        /// 本库不处理的格式：自动选到的跳过，点名的抛 `SourceUnsupportedError`
        Unsupported {
            /// 什么格式
            format: SubtitleFormat,
        },
        /// 读不了它的媒体播放列表（只在探测时出现：开始任务时选到的读不了，任务以那次的错误失败；没选到的不读）
        Unreadable {
            /// 原因
            problem: SubtitleProblem,
        },
    }
}

variants! {
    /// 本库不处理的字幕格式。
    pub enum SubtitleFormat {
        /// 分片是 fMP4（有 EXT-X-MAP），里面是 WebVTT（wvtt）或 TTML（stpp）
        Fmp4 {
            /// 所选变体 CODECS 里的字幕编码；没写时为空
            codecs: Seq<String>,
        },
        /// 分片地址的扩展名表明不是 WebVTT 文本（如 `.ttml`、`.xml`、`.dfxp` 的 TTML）
        Extension {
            /// 扩展名
            extension: String,
        },
    }
}

variants! {
    /// 探测时读不了字幕的媒体播放列表的原因。
    pub enum SubtitleProblem {
        /// 拉不到（重试之后）：404/410、临时故障或内容没写完
        Fetch {
            /// 取不到的原因
            failure: FetchFailure,
        },
        /// 拉到了而用不了：播放列表写坏、用了不支持的特性、回调出错等
        Unusable {
            /// 错误说明
            detail: String,
        },
    }
}

record! {
    @custom_new
    /// 选中的轨，顺序即任务里的轨号：变体之后是各音频（`audios`）、各字幕（`subtitles`）。
    pub struct Selected {
        /// 选中的变体
        pub variant: VariantInfo,
        /// 第一条独立的音频 rendition；None 表示音频混在变体流里（或没有音频）。选了几条音频时见 `audios`
        pub audio: Option<AudioInfo>,
        /// 有独立媒体播放列表的音频，按选的顺序
        pub audios: Seq<AudioInfo>,
        /// 用变体流里混着的音频
        pub muxed_audio: bool,
        /// 字幕，按选的顺序
        pub subtitles: Seq<SubtitleInfo>,
        /// 选到而没选上的字幕，原因见各自的 `support`：按 `SubtitleChoice.Default`、`SubtitleChoice.All` 自动选到而格式不支持的；
        /// 探测时另有读不了的与点名而格式不支持的（开始任务时这两种使任务失败）
        pub skipped_subtitles: Seq<SubtitleInfo>,
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl Selected {
    /// 各参数即同名属性；`subtitles`、`skipped_subtitles` 为 None 时为空。`audio` 与 `audios` 给一边即可，另一边由它
    /// 得出（`audio` 为 `audios` 的第一项）；`muxed_audio` 为 None 时为没有独立音频。两边都给了而 `audio` 不是 `audios`
    /// 的第一项时抛 `InvalidInputError`。
    #[new]
    #[pyo3(signature = (*, variant, audio = None, audios = None, muxed_audio = None, subtitles = None, skipped_subtitles = None))]
    fn new(
        py: Python<'_>,
        variant: VariantInfo,
        audio: Option<AudioInfo>,
        audios: Option<Seq<AudioInfo>>,
        muxed_audio: Option<bool>,
        subtitles: Option<Seq<SubtitleInfo>>,
        skipped_subtitles: Option<Seq<SubtitleInfo>>,
    ) -> PyResult<Self> {
        let audios = audios.unwrap_or_else(|| audio.iter().cloned().collect());
        let audio = audio.or_else(|| audios.0.first().cloned());
        if audios.0.first() != audio.as_ref() {
            return Err(invalid_input(
                py,
                "audio 应为 audios 的第一项（audios 为空时为 None）".to_owned(),
            ));
        }
        Ok(Selected {
            variant,
            muxed_audio: muxed_audio.unwrap_or(audio.is_none()),
            audio,
            audios,
            subtitles: subtitles.unwrap_or_default(),
            skipped_subtitles: skipped_subtitles.unwrap_or_default(),
        })
    }
}

record! {
    @custom_new
    /// 主播放列表里可选的轨与选中的轨。
    pub struct MasterInfo {
        /// 各变体，按出现顺序
        pub variants: Seq<VariantInfo>,
        /// 各音频 rendition，按出现顺序
        pub audio: Seq<AudioInfo>,
        /// 按选轨偏好选中的轨
        pub selected: Selected,
        /// 各字幕 rendition，按出现顺序
        pub subtitles: Seq<SubtitleInfo>,
    }
}

#[gen_stub_pymethods]
#[pymethods]
impl MasterInfo {
    /// 各参数即同名属性；`subtitles` 为 None 时为空。
    #[new]
    #[pyo3(signature = (*, variants, audio, selected, subtitles = None))]
    fn new(
        variants: Seq<VariantInfo>,
        audio: Seq<AudioInfo>,
        selected: Selected,
        subtitles: Option<Seq<SubtitleInfo>>,
    ) -> Self {
        MasterInfo {
            variants,
            audio,
            selected,
            subtitles: subtitles.unwrap_or_default(),
        }
    }
}

record! {
    /// 一条轨的媒体播放列表概况。
    pub struct TrackInfo {
        /// 有 EXT-X-ENDLIST：点播，或已结束的直播
        pub ended: bool,
        /// 分片数
        pub segments: usize,
        /// 来源标了 EXT-X-GAP 的分片数（含在 `segments` 里，下载时不取）
        pub gaps: usize,
        /// 分片声明时长之和；直播为当前窗口的
        pub duration: Duration,
        /// 有 AES-128 加密的分片或 init 段
        pub encrypted: bool,
        /// 有没标 EXT-X-GAP 的分片或它的 init 段加密而所用的 EXT-X-KEY 没写 URI：要给自定义 key（`JobRequest.key`）
        /// 才下得了
        pub needs_custom_key: bool,
    }
}

record! {
    /// 探测结果（`Engine.probe`）。
    pub struct Probe {
        /// 来源是主播放列表时：可选的变体与音频，以及按选轨偏好选中的。新开的下载选的与它相同；探测不读任务目录，
        /// 续传时下载怎样选见 `Preference`，可能与它不同。来源本身是媒体播放列表时为 None
        pub master: Option<MasterInfo>,
        /// 所选各轨的概况：第 0 条为所选变体（或来源本身），其后依次是各独立的音频、各字幕（同 `Selected`）
        pub tracks: Seq<TrackInfo>,
        /// 是否直播：有任一条轨没有 EXT-X-ENDLIST。新开的下载按同一判定走直播或点播；探测不读任务目录，续录
        /// 中途直播已结束的，下载仍按直播继续
        pub live: bool,
    }
}

impl From<core::hls::Resolution> for Resolution {
    fn from(r: core::hls::Resolution) -> Self {
        Resolution {
            width: Count(r.width),
            height: Count(r.height),
        }
    }
}

impl Resolution {
    pub(crate) fn to_core(&self) -> core::hls::Resolution {
        core::hls::Resolution {
            width: self.width.0,
            height: self.height.0,
        }
    }
}

impl From<core::VariantInfo> for VariantInfo {
    fn from(v: core::VariantInfo) -> Self {
        VariantInfo {
            index: v.index,
            bandwidth: v.bandwidth,
            resolution: v.resolution.map(Resolution::from),
            codecs: Seq(v.codecs),
            audio_group: v.audio_group,
            key: v.key.into(),
            subtitle_group: v.subtitle_group,
        }
    }
}

impl From<core::AudioInfo> for AudioInfo {
    fn from(a: core::AudioInfo) -> Self {
        AudioInfo {
            index: a.index,
            group: a.group,
            name: a.name,
            language: a.language,
            default: a.default,
            separate: a.separate,
            key: a.key.into(),
            channels: a.channels,
        }
    }
}

impl From<core::SubtitleInfo> for SubtitleInfo {
    fn from(s: core::SubtitleInfo) -> Self {
        SubtitleInfo {
            index: s.index,
            group: s.group,
            name: s.name,
            language: s.language,
            default: s.default,
            autoselect: s.autoselect,
            forced: s.forced,
            support: match s.support {
                core::SubtitleSupport::NotRead => SubtitleSupport::NotRead {},
                core::SubtitleSupport::Supported => SubtitleSupport::Supported {},
                core::SubtitleSupport::Unsupported(format) => SubtitleSupport::Unsupported {
                    format: format.into(),
                },
                core::SubtitleSupport::Unreadable(problem) => SubtitleSupport::Unreadable {
                    problem: match problem {
                        core::SubtitleProblem::Fetch(failure) => SubtitleProblem::Fetch {
                            failure: failure.into(),
                        },
                        core::SubtitleProblem::Unusable(detail) => {
                            SubtitleProblem::Unusable { detail }
                        }
                    },
                },
            },
            key: s.key.into(),
        }
    }
}

impl From<core::SubtitleFormat> for SubtitleFormat {
    fn from(format: core::SubtitleFormat) -> Self {
        match format {
            core::SubtitleFormat::Fmp4 { codecs } => SubtitleFormat::Fmp4 {
                codecs: Seq(codecs),
            },
            core::SubtitleFormat::Extension(extension) => SubtitleFormat::Extension { extension },
        }
    }
}

impl From<core::Selected> for Selected {
    fn from(s: core::Selected) -> Self {
        let audios: Seq<AudioInfo> = s.audio.into_iter().map(AudioInfo::from).collect();
        Selected {
            variant: s.variant.into(),
            audio: audios.0.first().cloned(),
            audios,
            muxed_audio: s.muxed_audio,
            subtitles: s.subtitles.into_iter().map(SubtitleInfo::from).collect(),
            skipped_subtitles: s
                .skipped_subtitles
                .into_iter()
                .map(SubtitleInfo::from)
                .collect(),
        }
    }
}

impl From<core::MasterInfo> for MasterInfo {
    fn from(m: core::MasterInfo) -> Self {
        MasterInfo {
            variants: m.variants.into_iter().map(VariantInfo::from).collect(),
            audio: m.audio.into_iter().map(AudioInfo::from).collect(),
            selected: m.selected.into(),
            subtitles: m.subtitles.into_iter().map(SubtitleInfo::from).collect(),
        }
    }
}

impl From<core::TrackInfo> for TrackInfo {
    fn from(t: core::TrackInfo) -> Self {
        TrackInfo {
            ended: t.ended,
            segments: t.segments,
            gaps: t.gaps,
            duration: t.duration,
            encrypted: t.encrypted,
            needs_custom_key: t.needs_custom_key,
        }
    }
}

impl From<core::Probe> for Probe {
    fn from(p: core::Probe) -> Self {
        Probe {
            master: p.master.map(MasterInfo::from),
            tracks: p.tracks.into_iter().map(TrackInfo::from).collect(),
            live: p.live,
        }
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Resolution>()?;
    module.add_class::<VariantInfo>()?;
    module.add_class::<AudioInfo>()?;
    module.add_class::<SubtitleInfo>()?;
    module.add_class::<SubtitleSupport>()?;
    module.add_class::<SubtitleFormat>()?;
    module.add_class::<SubtitleProblem>()?;
    module.add_class::<Selected>()?;
    module.add_class::<MasterInfo>()?;
    module.add_class::<TrackInfo>()?;
    module.add_class::<Probe>()?;
    Ok(())
}
