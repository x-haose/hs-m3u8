//! 记进任务目录的选轨：续传、续录时按其中的身份在主播放列表中找回同一条轨，不受主播放列表增删变体、地址换令牌的
//! 影响；本地 HLS 的主播放列表按其中的属性写变体与音频（只合并时只有它可用）。

use hs_m3u8_hls::{
    AudioKey, MasterPlaylist, Resolution, SelectedAudio, Selection, VariantKey, audio_renditions,
};
use hs_m3u8_remux::Streams;

/// 一次选轨。属性是选轨时来源写的，续传沿用记录、不随之后拉到的主播放列表更新：身份相同的轨内容相同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectionRecord {
    pub variant: VariantRecord,
    /// 独立的音频 rendition；None 表示音频混在变体流里（或没有音频）
    pub audio: Option<AudioRecord>,
}

/// 所选变体的身份与属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VariantRecord {
    pub key: VariantKey,
    /// BANDWIDTH，bit/s
    pub bandwidth: Option<u64>,
    pub resolution: Option<Resolution>,
    /// CODECS 拆开后的各项
    pub codecs: Vec<String>,
}

/// 所选音频 rendition 的身份与属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudioRecord {
    pub key: AudioKey,
    /// NAME
    pub name: Option<String>,
    /// LANGUAGE
    pub language: Option<String>,
}

impl SelectionRecord {
    /// `selection` 为从 `master` 中选出的轨。
    pub(crate) fn of(selection: &Selection, master: &MasterPlaylist) -> Self {
        let variant = &selection.variant;
        SelectionRecord {
            variant: VariantRecord {
                key: VariantKey::of(master, selection.variant_index)
                    .expect("选出的变体在主播放列表里"),
                bandwidth: variant.bandwidth,
                resolution: variant.resolution,
                codecs: variant.codecs.clone(),
            },
            audio: selection.audio.as_ref().map(|a| AudioRecord {
                key: AudioKey::of(master, a.index).expect("选出的音频在主播放列表里"),
                name: a.rendition.name.clone(),
                language: a.rendition.language.clone(),
            }),
        }
    }

    /// 在 `master` 中找回这次选轨：音频在找到的变体引用的音频组里找，且须有独立的媒体播放列表；找不到时为 None。
    pub(crate) fn find(&self, master: &MasterPlaylist) -> Option<Selection> {
        let variant_index = self.variant.key.find(master)?;
        let variant = &master.variants[variant_index];
        let audio = match &self.audio {
            None => None,
            Some(audio) => {
                let index = audio.key.find(master, variant.audio.as_deref()?)?;
                let (_, rendition) = audio_renditions(master)
                    .nth(index)
                    .expect("找到的位置在范围内");
                Some(SelectedAudio {
                    index,
                    uri: rendition.uri.clone()?,
                    rendition: rendition.clone(),
                })
            }
        };
        Some(Selection {
            variant_index,
            variant: variant.clone(),
            audio,
        })
    }

    /// 身份：两次选轨身份相同即同一组轨，属性不比较。
    pub(crate) fn keys(&self) -> (&VariantKey, Option<&AudioKey>) {
        (&self.variant.key, self.audio.as_ref().map(|a| &a.key))
    }
}

/// 各轨的取流方式：选了独立音频 rendition 时，第 0 条轨只取视频（变体里混着的音频不用，与播放器一致），
/// 第 1 条轨取音频；否则只有一条轨，全取。来源本身是媒体播放列表时 `selection` 为 None。
pub(crate) fn track_streams(selection: Option<&SelectionRecord>) -> Vec<Streams> {
    match selection.and_then(|s| s.audio.as_ref()) {
        Some(_) => vec![Streams::Video, Streams::Audio],
        None => vec![Streams::All],
    }
}
