//! 变体与音频 rendition 的身份：属性，以及属性完全相同的里面排第几。地址常带令牌、主播放列表会增删重排变体，按身份
//! 能在后来拉到的主播放列表里找回同一条轨（见 [`crate::VariantChoice::Matching`]）。

use crate::{MasterPlaylist, Rendition, Resolution, Variant, audio_renditions};

/// 变体的身份。各项取自 [`Variant`] 的同名属性（`audio_group` 即 [`Variant::audio`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantKey {
    /// BANDWIDTH，bit/s
    pub bandwidth: Option<u64>,
    pub resolution: Option<Resolution>,
    pub codecs: Vec<String>,
    /// 引用的音频组（AUDIO 属性）
    pub audio_group: Option<String>,
    /// 主播放列表中上面几项与它完全相同的变体里排第几（从 0 起）；冗余流（RFC 8216 6.2.3）的属性完全相同
    pub occurrence: usize,
}

impl VariantKey {
    /// `master` 里第 `index` 个变体的身份；下标超出范围时为 None。
    pub fn of(master: &MasterPlaylist, index: usize) -> Option<VariantKey> {
        let variant = master.variants.get(index)?;
        let mut key = VariantKey {
            bandwidth: variant.bandwidth,
            resolution: variant.resolution,
            codecs: variant.codecs.clone(),
            audio_group: variant.audio.clone(),
            occurrence: 0,
        };
        key.occurrence = master.variants[..index]
            .iter()
            .filter(|v| key.same_attributes(v))
            .count();
        Some(key)
    }

    /// 身份是它的变体在 `master` 里的下标；没有时为 None。
    pub fn find(&self, master: &MasterPlaylist) -> Option<usize> {
        master
            .variants
            .iter()
            .enumerate()
            .filter(|(_, v)| self.same_attributes(v))
            .nth(self.occurrence)
            .map(|(index, _)| index)
    }

    fn same_attributes(&self, variant: &Variant) -> bool {
        let VariantKey {
            bandwidth,
            resolution,
            codecs,
            audio_group,
            occurrence: _,
        } = self;
        variant.bandwidth == *bandwidth
            && variant.resolution == *resolution
            && variant.codecs == *codecs
            && variant.audio == *audio_group
    }
}

/// 音频 rendition 的身份。各项取自 [`Rendition`] 的同名属性（`group` 即 [`Rendition::group`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioKey {
    /// GROUP-ID
    pub group: String,
    /// NAME
    pub name: Option<String>,
    /// LANGUAGE
    pub language: Option<String>,
    /// 主播放列表中上面几项与它完全相同的音频 rendition 里排第几（从 0 起）；同组同语言同名的音频（违反 RFC 8216
    /// 4.3.4.1.1 的 NAME 唯一，但确有其事）只能这样区分
    pub occurrence: usize,
}

impl AudioKey {
    /// `master` 里第 `index` 个音频 rendition（口径同 [`crate::AudioChoice::Index`]）的身份；下标超出范围时为 None。
    pub fn of(master: &MasterPlaylist, index: usize) -> Option<AudioKey> {
        let (_, rendition) = audio_renditions(master).nth(index)?;
        let mut key = AudioKey {
            group: rendition.group.clone(),
            name: rendition.name.clone(),
            language: rendition.language.clone(),
            occurrence: 0,
        };
        key.occurrence = audio_renditions(master)
            .take(index)
            .filter(|(_, r)| key.same_attributes(r))
            .count();
        Some(key)
    }

    /// 身份是它的音频 rendition 在 `master` 里的位置（口径同 [`crate::AudioChoice::Index`]）；没有时为 None。
    pub fn find(&self, master: &MasterPlaylist) -> Option<usize> {
        audio_renditions(master)
            .filter(|(_, r)| self.same_attributes(r))
            .nth(self.occurrence)
            .map(|(index, _)| index)
    }

    fn same_attributes(&self, rendition: &Rendition) -> bool {
        let AudioKey {
            group,
            name,
            language,
            occurrence: _,
        } = self;
        rendition.group == *group && rendition.name == *name && rendition.language == *language
    }
}
