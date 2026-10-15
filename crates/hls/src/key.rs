//! 变体与音频 rendition 的身份：来源写了稳定 ID 时以它为主，没写时是属性；再加上身份相同的里面排第几。地址常带令牌、
//! 主播放列表会增删重排变体，按身份能在后来拉到的主播放列表里找回同一条轨（见 [`crate::VariantChoice::Matching`]）。

use crate::{MasterPlaylist, Rendition, Resolution, Variant, audio_renditions};

/// 变体的身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariantKey {
    /// 来源写了 STABLE-VARIANT-ID：按它与引用的音频组认，码率、编码等属性变了也是同一条。同一条流常按音频组各列一行
    /// （如 AAC 与 E-AC-3 各一组），稳定 ID 相同，靠音频组区分
    Stable {
        id: String,
        /// 引用的音频组（AUDIO 属性）
        audio_group: Option<String>,
        /// 主播放列表中上面两项都相同的变体里排第几（从 0 起）；同一条流经几条路径分发而组名相同时有几个
        occurrence: usize,
    },
    /// 来源没写稳定 ID：各项取自 [`Variant`] 的同名属性（`audio_group` 即 [`Variant::audio`]）。找的时候不看变体有没有
    /// 稳定 ID，来源后来写上了也找得回
    Attributes {
        /// BANDWIDTH，bit/s
        bandwidth: Option<u64>,
        resolution: Option<Resolution>,
        codecs: Vec<String>,
        /// 引用的音频组（AUDIO 属性）
        audio_group: Option<String>,
        /// 主播放列表中上面几项与它完全相同的变体里排第几（从 0 起）；冗余流（RFC 8216 6.2.3）的属性完全相同
        occurrence: usize,
    },
}

impl VariantKey {
    /// `master` 里第 `index` 个变体的身份；下标超出范围时为 None。
    pub fn of(master: &MasterPlaylist, index: usize) -> Option<VariantKey> {
        let variant = master.variants.get(index)?;
        let mut key = match &variant.stable_id {
            Some(id) => VariantKey::Stable {
                id: id.clone(),
                audio_group: variant.audio.clone(),
                occurrence: 0,
            },
            None => VariantKey::Attributes {
                bandwidth: variant.bandwidth,
                resolution: variant.resolution,
                codecs: variant.codecs.clone(),
                audio_group: variant.audio.clone(),
                occurrence: 0,
            },
        };
        let earlier = master.variants[..index]
            .iter()
            .filter(|v| key.matches(v))
            .count();
        let (VariantKey::Stable { occurrence, .. } | VariantKey::Attributes { occurrence, .. }) =
            &mut key;
        *occurrence = earlier;
        Some(key)
    }

    /// 身份是它的变体在 `master` 里的下标；没有时为 None。
    pub fn find(&self, master: &MasterPlaylist) -> Option<usize> {
        let (VariantKey::Stable { occurrence, .. } | VariantKey::Attributes { occurrence, .. }) =
            self;
        master
            .variants
            .iter()
            .enumerate()
            .filter(|(_, v)| self.matches(v))
            .nth(*occurrence)
            .map(|(index, _)| index)
    }

    /// 除排第几以外与 `variant` 相同。
    fn matches(&self, variant: &Variant) -> bool {
        match self {
            VariantKey::Stable {
                id,
                audio_group,
                occurrence: _,
            } => variant.stable_id.as_ref() == Some(id) && variant.audio == *audio_group,
            VariantKey::Attributes {
                bandwidth,
                resolution,
                codecs,
                audio_group,
                occurrence: _,
            } => {
                variant.bandwidth == *bandwidth
                    && variant.resolution == *resolution
                    && variant.codecs == *codecs
                    && variant.audio == *audio_group
            }
        }
    }
}

/// 音频 rendition 的身份，在所选变体引用的音频组里认（组由变体给出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioKey {
    /// 来源写了 STABLE-RENDITION-ID：只按它认，同 [`VariantKey::Stable`]
    Stable {
        id: String,
        /// 同一组里稳定 ID 相同的音频 rendition 里排第几（从 0 起）
        occurrence: usize,
    },
    /// 来源没写稳定 ID：各项取自 [`Rendition`] 的同名属性（`group` 即 [`Rendition::group`]），同
    /// [`VariantKey::Attributes`]
    Attributes {
        /// GROUP-ID
        group: String,
        /// NAME
        name: Option<String>,
        /// LANGUAGE
        language: Option<String>,
        /// 主播放列表中上面几项与它完全相同的音频 rendition 里排第几（从 0 起）；同组同语言同名的音频（违反 RFC 8216
        /// 4.3.4.1.1 的 NAME 唯一，但确有其事）只能这样区分
        occurrence: usize,
    },
}

impl AudioKey {
    /// `master` 里第 `index` 个音频 rendition（口径同 [`crate::AudioChoice::Index`]）的身份；下标超出范围时为 None。
    pub fn of(master: &MasterPlaylist, index: usize) -> Option<AudioKey> {
        let (_, rendition) = audio_renditions(master).nth(index)?;
        let mut key = match &rendition.stable_id {
            Some(id) => AudioKey::Stable {
                id: id.clone(),
                occurrence: 0,
            },
            None => AudioKey::Attributes {
                group: rendition.group.clone(),
                name: rendition.name.clone(),
                language: rendition.language.clone(),
                occurrence: 0,
            },
        };
        let earlier = audio_renditions(master)
            .take(index)
            .filter(|(_, r)| r.group == rendition.group && key.matches(r))
            .count();
        let (AudioKey::Stable { occurrence, .. } | AudioKey::Attributes { occurrence, .. }) =
            &mut key;
        *occurrence = earlier;
        Some(key)
    }

    /// 身份是它的、在 `group` 组里的音频 rendition，与它在 `master` 里的位置（口径同 [`crate::AudioChoice::Index`]）；
    /// 没有时为 None。
    pub fn find<'a>(
        &self,
        master: &'a MasterPlaylist,
        group: &str,
    ) -> Option<(usize, &'a Rendition)> {
        let (AudioKey::Stable { occurrence, .. } | AudioKey::Attributes { occurrence, .. }) = self;
        audio_renditions(master)
            .filter(|(_, r)| r.group == group && self.matches(r))
            .nth(*occurrence)
    }

    /// 除排第几以外与 `rendition` 相同。
    fn matches(&self, rendition: &Rendition) -> bool {
        match self {
            AudioKey::Stable { id, occurrence: _ } => rendition.stable_id.as_ref() == Some(id),
            AudioKey::Attributes {
                group,
                name,
                language,
                occurrence: _,
            } => {
                rendition.group == *group
                    && rendition.name == *name
                    && rendition.language == *language
            }
        }
    }
}
