//! 记进任务目录的选轨：续传、续录时按其中的身份在主播放列表中找回同一条轨，不受主播放列表增删变体、地址换令牌的
//! 影响；本地 HLS 的主播放列表按其中的属性写变体与音频（只合并时只有它可用）。

use hs_m3u8_hls::{
    AudioKey, CodecKind, MasterPlaylist, Resolution, SelectedAudio, Selection, VariantKey,
    codec_family,
};
use hs_m3u8_remux::Streams;

/// 一次选轨。属性是首次选出这些轨时来源写的，之后身份相同即沿用、不随拉到的主播放列表更新：身份相同的轨内容相同。
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

    /// 在 `master` 中找回这次选轨：音频在找到的变体引用的音频组里找，且须有独立的媒体播放列表；找到的变体分辨率或编码
    /// 种类与记录的不同（见 [`VariantRecord::agrees`]）也算找不到。
    pub(crate) fn find(&self, master: &MasterPlaylist) -> Option<Selection> {
        let variant_index = self.variant.key.find(master)?;
        let variant = &master.variants[variant_index];
        if !self.variant.agrees(variant.resolution, &variant.codecs) {
            return None;
        }
        let audio = match &self.audio {
            None => None,
            Some(audio) => {
                let (index, rendition) = audio.key.find(master, variant.audio.as_deref()?)?;
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
}

impl VariantRecord {
    /// 与 `other` 是同一条变体：身份相同，且分辨率、编码种类不矛盾（见 [`VariantRecord::agrees`]）。
    pub(crate) fn same_as(&self, other: &VariantRecord) -> bool {
        self.key == other.key && self.agrees(other.resolution, &other.codecs)
    }

    /// 与分辨率 `resolution`、编码 `codecs` 不矛盾：两边都写了的才比；编码比种类（见 [`codec_family`]），不比码率与编码
    /// 的档次（会重新测量、改写）。引用了音频组时 CODECS 还列出组里所有音频的编码（组里加一种语言就会变），只比视频的；
    /// 没引用时音频混在流里，比视频与音频的；字幕的都不比。按属性认的身份本身含这些属性，只有按稳定 ID 认的会不符：来源
    /// 把同一稳定 ID 给了另一档，违反了稳定 ID 的约定。
    fn agrees(&self, resolution: Option<Resolution>, codecs: &[String]) -> bool {
        let (VariantKey::Stable { audio_group, .. } | VariantKey::Attributes { audio_group, .. }) =
            &self.key;
        let compared = |kind| match kind {
            CodecKind::Video => true,
            CodecKind::Audio => audio_group.is_none(),
            CodecKind::Subtitles => false,
        };
        let families = |codecs: &[String]| {
            let mut families: Vec<String> = codecs
                .iter()
                .filter(|c| compared(CodecKind::of(c)))
                .map(|c| codec_family(c))
                .collect();
            families.sort();
            families
        };
        let resolution = match (self.resolution, resolution) {
            (Some(recorded), Some(current)) => recorded == current,
            _ => true,
        };
        let codecs = self.codecs.is_empty()
            || codecs.is_empty()
            || families(&self.codecs) == families(codecs);
        resolution && codecs
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

#[cfg(test)]
mod tests {
    use hs_m3u8_hls::{Playlist, Preference, Url, parse, select};

    use super::*;

    /// 一个写了稳定 ID 的变体，属性为 `stream_inf`；另有音频组 aud，变体写了 `AUDIO="aud"` 时取用它。
    fn master(stream_inf: &str) -> MasterPlaylist {
        let text = format!(
            "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",DEFAULT=YES,URI=\"en.m3u8\"\n\
             #EXT-X-STREAM-INF:{stream_inf},STABLE-VARIANT-ID=\"v\"\nv.m3u8\n"
        );
        let url = Url::parse("https://example.com/master.m3u8").unwrap();
        match parse(&text, &url).unwrap() {
            Playlist::Master(master) => master,
            Playlist::Media(_) => panic!("应为主播放列表"),
        }
    }

    /// 选出 `before` 的变体后，按记录在 `after` 里各是否找得回。
    fn found_in(before: &str, after: &[(&str, bool)]) {
        let before = master(before);
        let selection = select(&before, &Preference::default()).unwrap();
        let record = SelectionRecord::of(&selection, &before);
        for &(stream_inf, found) in after {
            assert_eq!(
                record.find(&master(stream_inf)).is_some(),
                found,
                "{stream_inf}"
            );
        }
    }

    /// 按稳定 ID 找回的变体，码率、编码档次变了或加了字幕照样是它；分辨率、视频编码种类、混在流里的音频编码种类变了，
    /// 即来源把这个稳定 ID 给了另一档，不当作同一条。
    #[test]
    fn a_stable_id_given_to_another_rendition_is_not_found() {
        found_in(
            "BANDWIDTH=1000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\"",
            &[
                (
                    "BANDWIDTH=2000,RESOLUTION=640x360,CODECS=\"mp4a.40.5,AVC1.640028\"",
                    true,
                ),
                ("BANDWIDTH=2000", true),
                (
                    "BANDWIDTH=1000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2,wvtt\"",
                    true,
                ),
                (
                    "BANDWIDTH=1000,RESOLUTION=1280x720,CODECS=\"avc1.4d401e,mp4a.40.2\"",
                    false,
                ),
                (
                    "BANDWIDTH=1000,RESOLUTION=640x360,CODECS=\"hvc1.1.6.L93.B0,mp4a.40.2\"",
                    false,
                ),
                (
                    "BANDWIDTH=1000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,ac-3\"",
                    false,
                ),
            ],
        );
    }

    /// 变体引用了音频组时，CODECS 还列出组里所有音频的编码：组里加了别的编码的音频不算变了，视频编码种类变了才算。
    #[test]
    fn audio_group_codecs_do_not_count_against_the_variant() {
        found_in(
            "BANDWIDTH=1000,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"aud\"",
            &[
                (
                    "BANDWIDTH=1000,CODECS=\"avc1.4d401e,mp4a.40.2,ac-3\",AUDIO=\"aud\"",
                    true,
                ),
                (
                    "BANDWIDTH=1000,CODECS=\"hvc1.1.6.L93.B0,mp4a.40.2\",AUDIO=\"aud\"",
                    false,
                ),
            ],
        );
    }
}
