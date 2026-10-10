//! 对外描述来源的类型：探测结果与进度里的变体、音轨与各轨概况。不含地址：地址常带令牌，而这些类型常被打进
//! 日志与界面。

use hs_m3u8_hls::{MasterPlaylist, MediaPlaylist, Rendition, RenditionKind, Resolution, Selection};

/// 一个变体的属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantInfo {
    /// 在主播放列表中的下标，即 [`crate::hls::VariantChoice::Index`] 用的
    pub index: usize,
    /// BANDWIDTH，bit/s；来源没写时为 None
    pub bandwidth: Option<u64>,
    pub resolution: Option<Resolution>,
    /// CODECS 拆开后的各项，如 `avc1.640028`、`mp4a.40.2`
    pub codecs: Vec<String>,
    /// 引用的音频组（AUDIO 属性）
    pub audio_group: Option<String>,
}

/// 一个音频 rendition 的属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioInfo {
    /// 在主播放列表的 rendition 中的下标，即 [`crate::hls::AudioChoice::Index`] 用的
    pub index: usize,
    pub group: String,
    pub name: Option<String>,
    pub language: Option<String>,
    /// DEFAULT=YES
    pub default: bool,
    /// 有独立的媒体播放列表；为 false 时音频混在变体流里，选它即用变体里的音频
    pub separate: bool,
}

/// 主播放列表里可选的轨与选中的轨。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterInfo {
    /// 按出现顺序
    pub variants: Vec<VariantInfo>,
    /// 音频 rendition，按出现顺序
    pub audio: Vec<AudioInfo>,
    pub selected: Selected,
}

/// 选中的变体与音频。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub variant: VariantInfo,
    /// 独立的音频 rendition；None 表示音频混在变体流里（或没有音频）
    pub audio: Option<AudioInfo>,
}

/// 一条轨的媒体播放列表概况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    /// 有 EXT-X-ENDLIST：点播，或已结束的直播
    pub ended: bool,
    pub segments: usize,
    /// 分片声明时长之和，微秒；直播为当前窗口的
    pub duration_us: u64,
    /// 有 AES-128 加密的分片
    pub encrypted: bool,
}

impl MasterInfo {
    pub(crate) fn of(master: &MasterPlaylist, selection: &Selection) -> Self {
        MasterInfo {
            variants: (0..master.variants.len())
                .map(|i| variant_info(master, i))
                .collect(),
            audio: master
                .renditions
                .iter()
                .enumerate()
                .filter(|(_, r)| r.kind == RenditionKind::Audio)
                .map(|(i, r)| audio_info(i, r))
                .collect(),
            selected: Selected::of(master, selection),
        }
    }
}

impl Selected {
    /// `selection` 为从 `master` 中选出的轨。
    pub(crate) fn of(master: &MasterPlaylist, selection: &Selection) -> Self {
        Selected {
            variant: variant_info(master, selection.variant_index),
            audio: selection
                .audio
                .as_ref()
                .map(|a| audio_info(a.index, &a.rendition)),
        }
    }
}

impl TrackInfo {
    pub(crate) fn of(playlist: &MediaPlaylist) -> Self {
        TrackInfo {
            ended: playlist.ended,
            segments: playlist.segments.len(),
            duration_us: playlist
                .segments
                .iter()
                .map(|s| s.duration_us)
                .fold(0u64, u64::saturating_add),
            encrypted: playlist.segments.iter().any(|s| s.key.is_some()),
        }
    }
}

fn variant_info(master: &MasterPlaylist, index: usize) -> VariantInfo {
    let v = &master.variants[index];
    VariantInfo {
        index,
        bandwidth: v.bandwidth,
        resolution: v.resolution,
        codecs: v.codecs.clone(),
        audio_group: v.audio.clone(),
    }
}

fn audio_info(index: usize, rendition: &Rendition) -> AudioInfo {
    AudioInfo {
        index,
        group: rendition.group_id.clone(),
        name: rendition.name.clone(),
        language: rendition.language.clone(),
        default: rendition.default,
        separate: rendition.uri.is_some(),
    }
}
