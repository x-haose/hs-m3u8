//! 探测结果与进度里描述来源的类型：变体、音频与各轨概况。不含地址：地址常带令牌，而这些类型常被打进日志与界面。

use std::time::Duration;

use hs_m3u8_core as core;
use pyo3::prelude::*;

use crate::record::record;

record! {
    /// 分辨率（RESOLUTION 属性）。
    pub struct Resolution {
        /// 宽，像素
        pub width: u32,
        /// 高，像素
        pub height: u32,
    }
}

record! {
    /// 主播放列表里的一个变体。
    pub struct VariantInfo {
        /// 主播放列表里第几个变体（从 0 起），即 `VariantChoice.Index` 用的，也是在 `MasterInfo.variants` 里的位置
        pub index: usize,
        /// BANDWIDTH，bit/s；来源没写时为 None
        pub bandwidth: Option<u64>,
        /// RESOLUTION；来源没写时为 None
        pub resolution: Option<Resolution>,
        /// CODECS 拆开后的各项，如 `avc1.640028`、`mp4a.40.2`
        pub codecs: Vec<String>,
        /// 引用的音频组（AUDIO 属性）；没有时为 None
        pub audio_group: Option<String>,
    }
}

record! {
    /// 主播放列表里的一个音频 rendition。
    pub struct AudioInfo {
        /// 主播放列表里第几个音频 rendition（只数音频，从 0 起），即 `AudioChoice.Index` 用的，也是在
        /// `MasterInfo.audio` 里的位置
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
    }
}

record! {
    /// 选中的变体与音频。
    pub struct Selected {
        /// 选中的变体
        pub variant: VariantInfo,
        /// 独立的音频 rendition；None 表示音频混在变体流里（或没有音频）
        pub audio: Option<AudioInfo>,
    }
}

record! {
    /// 主播放列表里可选的轨与选中的轨。
    pub struct MasterInfo {
        /// 各变体，按出现顺序
        pub variants: Vec<VariantInfo>,
        /// 各音频 rendition，按出现顺序
        pub audio: Vec<AudioInfo>,
        /// 按选轨偏好选中的变体与音频
        pub selected: Selected,
    }
}

record! {
    /// 一条轨的媒体播放列表概况。
    pub struct TrackInfo {
        /// 有 EXT-X-ENDLIST：点播，或已结束的直播
        pub ended: bool,
        /// 分片数
        pub segments: usize,
        /// 分片声明时长之和；直播为当前窗口的
        pub duration: Duration,
        /// 有 AES-128 加密的分片
        pub encrypted: bool,
    }
}

record! {
    /// 探测结果（`Engine.probe`）。
    pub struct Probe {
        /// 来源是主播放列表时：可选的变体与音频，以及按选轨偏好选中的。新开的下载选的与它相同；探测不读任务目录，
        /// 续传时下载按记录找回原来的轨，可能与它不同。来源本身是媒体播放列表时为 None
        pub master: Option<MasterInfo>,
        /// 所选各轨的概况：第 0 条为所选变体（或来源本身），第 1 条（若有）为独立的音频
        pub tracks: Vec<TrackInfo>,
        /// 是否直播：有任一条轨没有 EXT-X-ENDLIST。新开的下载按同一判定走直播或点播；探测不读任务目录，续录
        /// 中途直播已结束的，下载仍按直播继续
        pub live: bool,
    }
}

impl From<core::hls::Resolution> for Resolution {
    fn from(r: core::hls::Resolution) -> Self {
        Resolution {
            width: r.width,
            height: r.height,
        }
    }
}

impl From<core::VariantInfo> for VariantInfo {
    fn from(v: core::VariantInfo) -> Self {
        VariantInfo {
            index: v.index,
            bandwidth: v.bandwidth,
            resolution: v.resolution.map(Resolution::from),
            codecs: v.codecs,
            audio_group: v.audio_group,
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
        }
    }
}

impl From<core::Selected> for Selected {
    fn from(s: core::Selected) -> Self {
        Selected {
            variant: s.variant.into(),
            audio: s.audio.map(AudioInfo::from),
        }
    }
}

impl From<core::MasterInfo> for MasterInfo {
    fn from(m: core::MasterInfo) -> Self {
        MasterInfo {
            variants: m.variants.into_iter().map(VariantInfo::from).collect(),
            audio: m.audio.into_iter().map(AudioInfo::from).collect(),
            selected: m.selected.into(),
        }
    }
}

impl From<core::TrackInfo> for TrackInfo {
    fn from(t: core::TrackInfo) -> Self {
        TrackInfo {
            ended: t.ended,
            segments: t.segments,
            duration: Duration::from_micros(t.duration_us),
            encrypted: t.encrypted,
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
    module.add_class::<Selected>()?;
    module.add_class::<MasterInfo>()?;
    module.add_class::<TrackInfo>()?;
    module.add_class::<Probe>()?;
    Ok(())
}
