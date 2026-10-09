//! 所选变体与音频 rendition 的身份：记在任务目录里，续传、续录时按它在主播放列表中找回同一条轨，
//! 不受主播放列表增删变体、地址换令牌的影响。

use hs_m3u8_hls::{
    MasterPlaylist, Rendition, RenditionKind, Resolution, SelectedAudio, Selection, Variant,
};
use hs_m3u8_remux::Streams;

/// 一次选轨的身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectionKey {
    pub variant: VariantKey,
    /// 独立的音频 rendition；None 表示音频混在变体流里（或没有音频）
    pub audio: Option<RenditionKey>,
}

/// 变体的身份：属性，以及属性完全相同的变体里排第几。地址常带令牌，不作身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VariantKey {
    pub attributes: VariantAttributes,
    /// 主播放列表中属性与它完全相同的变体里排第几（从 0 起）；冗余流（RFC 8216 6.2.3）的属性完全相同
    pub occurrence: usize,
}

/// 变体的属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VariantAttributes {
    pub bandwidth: Option<u64>,
    pub resolution: Option<Resolution>,
    pub codecs: Vec<String>,
    pub audio_group: Option<String>,
}

/// 音频 rendition 的属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenditionKey {
    pub group_id: String,
    pub language: Option<String>,
    pub name: Option<String>,
}

impl SelectionKey {
    /// `selection` 为从 `master` 中选出的轨。
    pub(crate) fn of(selection: &Selection, master: &MasterPlaylist) -> Self {
        let position = master
            .variants
            .iter()
            .position(|v| *v == selection.variant)
            .expect("所选变体来自这份主播放列表");
        let attributes = VariantAttributes::of(&selection.variant);
        let occurrence = master.variants[..position]
            .iter()
            .filter(|v| VariantAttributes::of(v) == attributes)
            .count();
        SelectionKey {
            variant: VariantKey {
                attributes,
                occurrence,
            },
            audio: selection
                .audio
                .as_ref()
                .map(|a| RenditionKey::of(&a.rendition)),
        }
    }

    /// 在 `master` 中找回这次选轨；找不到时为 None。
    pub(crate) fn find(&self, master: &MasterPlaylist) -> Option<Selection> {
        let variant = master
            .variants
            .iter()
            .filter(|v| VariantAttributes::of(v) == self.variant.attributes)
            .nth(self.variant.occurrence)?;
        let audio = match &self.audio {
            None => None,
            Some(key) => {
                let rendition = master.renditions.iter().find(|r| {
                    r.kind == RenditionKind::Audio && r.uri.is_some() && RenditionKey::of(r) == *key
                })?;
                Some(SelectedAudio {
                    uri: rendition.uri.clone()?,
                    rendition: rendition.clone(),
                })
            }
        };
        Some(Selection {
            variant: variant.clone(),
            audio,
        })
    }
}

impl VariantAttributes {
    fn of(variant: &Variant) -> Self {
        VariantAttributes {
            bandwidth: variant.bandwidth,
            resolution: variant.resolution,
            codecs: variant.codecs.clone(),
            audio_group: variant.audio.clone(),
        }
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

impl RenditionKey {
    fn of(rendition: &Rendition) -> Self {
        RenditionKey {
            group_id: rendition.group_id.clone(),
            language: rendition.language.clone(),
            name: rendition.name.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use hs_m3u8_hls::{Playlist, Preference, Url, VariantChoice, parse, select};

    use super::*;

    fn master(text: &str) -> MasterPlaylist {
        match parse(text, &Url::parse("https://a.example/m.m3u8").unwrap()).unwrap() {
            Playlist::Master(m) => m,
            Playlist::Media(_) => panic!("应为主播放列表"),
        }
    }

    const AUDIO: &str = "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"en\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"en.m3u8?t=1\"\n";
    const LOW: &str =
        "#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=640x360,AUDIO=\"a\"\nlow.m3u8?t=1\n";

    #[test]
    fn selection_is_found_again_after_variants_are_added_and_tokens_change() {
        let before = master(&format!("#EXTM3U\n{AUDIO}{LOW}"));
        let key = SelectionKey::of(&select(&before, &Preference::default()).unwrap(), &before);

        // 多了一个更高的变体，地址的令牌也换了：按 Best 会选到新变体，按记录仍是原来那个
        let after = master(&format!(
            "#EXTM3U\n{}{}#EXT-X-STREAM-INF:BANDWIDTH=9000,RESOLUTION=1920x1080,AUDIO=\"a\"\nhigh.m3u8\n",
            AUDIO.replace("t=1", "t=2"),
            LOW.replace("t=1", "t=2"),
        ));
        let found = key.find(&after).unwrap();
        assert_eq!(found.variant.uri.as_str(), "https://a.example/low.m3u8?t=2");
        assert_eq!(
            found.audio.unwrap().uri.as_str(),
            "https://a.example/en.m3u8?t=2"
        );
    }

    #[test]
    fn missing_variant_or_rendition_is_not_found() {
        let before = master(&format!("#EXTM3U\n{AUDIO}{LOW}"));
        let key = SelectionKey::of(&select(&before, &Preference::default()).unwrap(), &before);
        let without_variant = master(&format!(
            "#EXTM3U\n{AUDIO}{}",
            LOW.replace("BANDWIDTH=1000", "BANDWIDTH=2000")
        ));
        assert_eq!(key.find(&without_variant), None);
        let without_audio = master(&format!(
            "#EXTM3U\n{}{LOW}",
            AUDIO.replace("\"en\"", "\"fr\"")
        ));
        assert_eq!(key.find(&without_audio), None);
    }

    #[test]
    fn redundant_variants_are_told_apart_by_position() {
        let redundant = "#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=640x360\na/v.m3u8\n\
                         #EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=640x360\nb/v.m3u8\n";
        let m = master(&format!("#EXTM3U\n{redundant}"));
        for index in [0, 1] {
            let preference = Preference {
                variant: VariantChoice::Index(index),
                audio_language: None,
            };
            let selection = select(&m, &preference).unwrap();
            let key = SelectionKey::of(&selection, &m);
            assert_eq!(key.variant.occurrence, index);
            assert_eq!(key.find(&m).unwrap().variant.uri, selection.variant.uri);
        }
    }
}
