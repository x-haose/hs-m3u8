//! 对外描述来源的类型：探测结果与进度里的变体、音轨与各轨概况。不含地址：地址常带令牌，而这些类型常被打进
//! 日志与界面。

use hs_m3u8_hls::{
    AudioChoice, AudioKey, MasterPlaylist, MediaPlaylist, Resolution, Selection, VariantChoice,
    VariantKey, audio_renditions,
};

/// 一个变体的属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantInfo {
    /// 主播放列表里第几个变体，即 [`crate::hls::VariantChoice::Index`] 用的，也是在 [`MasterInfo::variants`] 里的位置
    pub index: usize,
    /// BANDWIDTH，bit/s；来源没写时为 None
    pub bandwidth: Option<u64>,
    pub resolution: Option<Resolution>,
    /// CODECS 拆开后的各项，如 `avc1.640028`、`mp4a.40.2`
    pub codecs: Vec<String>,
    /// 引用的音频组（AUDIO 属性）
    pub audio_group: Option<String>,
    /// 下标以外的上面几项完全相同的变体里排第几（从 0 起）
    pub occurrence: usize,
}

impl VariantInfo {
    /// 按身份选这个变体（[`VariantChoice::Matching`]）：探测之后主播放列表增删重排了变体，开始任务时也选到它。
    pub fn choice(&self) -> VariantChoice {
        VariantChoice::Matching(VariantKey {
            bandwidth: self.bandwidth,
            resolution: self.resolution,
            codecs: self.codecs.clone(),
            audio_group: self.audio_group.clone(),
            occurrence: self.occurrence,
        })
    }
}

/// 一个音频 rendition 的属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioInfo {
    /// 主播放列表里第几个音频 rendition（只数音频），即 [`crate::hls::AudioChoice::Index`] 用的，也是在
    /// [`MasterInfo::audio`] 里的位置
    pub index: usize,
    pub group: String,
    pub name: Option<String>,
    pub language: Option<String>,
    /// DEFAULT=YES
    pub default: bool,
    /// 有独立的媒体播放列表；为 false 时音频混在变体流里，选它即用变体里的音频
    pub separate: bool,
    /// 组、名称、语言都相同的音频 rendition 里排第几（从 0 起）
    pub occurrence: usize,
}

impl AudioInfo {
    /// 按身份选这个音频 rendition（[`AudioChoice::Matching`]），同 [`VariantInfo::choice`]。
    pub fn choice(&self) -> AudioChoice {
        AudioChoice::Matching(AudioKey {
            group_id: self.group.clone(),
            language: self.language.clone(),
            name: self.name.clone(),
            occurrence: self.occurrence,
        })
    }
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
            audio: audio_renditions(master)
                .map(|(i, _)| audio_info(master, i))
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
                .map(|a| audio_info(master, a.index)),
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
    let key = VariantKey::of(master, index).expect("下标在范围内");
    VariantInfo {
        index,
        bandwidth: key.bandwidth,
        resolution: key.resolution,
        codecs: key.codecs,
        audio_group: key.audio_group,
        occurrence: key.occurrence,
    }
}

/// 第 `index` 个音频 rendition（口径同 [`AudioChoice::Index`]）。
fn audio_info(master: &MasterPlaylist, index: usize) -> AudioInfo {
    let (_, rendition) = audio_renditions(master).nth(index).expect("下标在范围内");
    let key = AudioKey::of(master, index).expect("下标在范围内");
    AudioInfo {
        index,
        group: key.group_id,
        name: key.name,
        language: key.language,
        default: rendition.default,
        separate: rendition.uri.is_some(),
        occurrence: key.occurrence,
    }
}
