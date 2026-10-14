//! 所选变体与音频 rendition 的身份：记在任务目录里，续传、续录时按它在主播放列表中找回同一条轨，
//! 不受主播放列表增删变体、地址换令牌的影响。本地 HLS 的主播放列表也用它写变体与音频的属性（只合并时
//! 只有它可用），身份里去掉的属性不会出现在那里。

use hs_m3u8_hls::{
    AudioKey, MasterPlaylist, SelectedAudio, Selection, VariantKey, audio_renditions,
};
use hs_m3u8_remux::Streams;

/// 一次选轨的身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectionKey {
    pub variant: VariantKey,
    /// 独立的音频 rendition；None 表示音频混在变体流里（或没有音频）
    pub audio: Option<AudioKey>,
}

impl SelectionKey {
    /// `selection` 为从 `master` 中选出的轨。
    pub(crate) fn of(selection: &Selection, master: &MasterPlaylist) -> Self {
        SelectionKey {
            variant: VariantKey::of(master, selection.variant_index)
                .expect("选出的变体在主播放列表里"),
            audio: selection
                .audio
                .as_ref()
                .map(|a| AudioKey::of(master, a.index).expect("选出的音频在主播放列表里")),
        }
    }

    /// 在 `master` 中找回这次选轨；找不到时为 None。
    pub(crate) fn find(&self, master: &MasterPlaylist) -> Option<Selection> {
        let variant_index = self.variant.find(master)?;
        let audio = match &self.audio {
            None => None,
            Some(key) => {
                let index = key.find(master)?;
                let (_, rendition) = audio_renditions(master).nth(index)?;
                Some(SelectedAudio {
                    index,
                    uri: rendition.uri.clone()?,
                    rendition: rendition.clone(),
                })
            }
        };
        Some(Selection {
            variant_index,
            variant: master.variants[variant_index].clone(),
            audio,
        })
    }
}

/// 各轨的取流方式：选了独立音频 rendition 时，第 0 条轨只取视频（变体里混着的音频不用，与播放器一致），
/// 第 1 条轨取音频；否则只有一条轨，全取。来源本身是媒体播放列表时 `selection` 为 None。
pub(crate) fn track_streams(selection: Option<&SelectionKey>) -> Vec<Streams> {
    match selection.and_then(|s| s.audio.as_ref()) {
        Some(_) => vec![Streams::Video, Streams::Audio],
        None => vec![Streams::All],
    }
}
