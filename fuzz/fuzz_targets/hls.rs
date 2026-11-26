//! 任意字节当作播放列表解析：不 panic；主播放列表再按几种偏好选轨，选出的变体与 rendition 按身份找得回原处。
#![no_main]

use hs_m3u8_hls::{
    Playlist, Preference, RenditionKey, RenditionKind, Renditions, Url, VariantKey, parse_bytes,
    select,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let url = Url::parse("https://example.com/a/index.m3u8").expect("常量地址");
    let Ok(Playlist::Master(master)) = parse_bytes(data, &url) else {
        return;
    };
    for (audio, subtitles) in [
        (Renditions::Default, Renditions::None),
        (Renditions::All, Renditions::All),
        (Renditions::None, Renditions::Default),
    ] {
        let preference = Preference {
            audio,
            subtitles,
            ..Preference::default()
        };
        let Ok(selection) = select(&master, &preference) else {
            continue;
        };
        let key = VariantKey::of(&master, selection.variant_index).expect("选出的变体在列表里");
        assert_eq!(key.find(&master), Some(selection.variant_index));
        let kinds = [
            (
                RenditionKind::Audio,
                &selection.audio,
                &selection.variant.audio,
            ),
            (
                RenditionKind::Subtitles,
                &selection.subtitles,
                &selection.variant.subtitles,
            ),
        ];
        for (kind, selected, group) in kinds {
            for s in selected {
                let key = RenditionKey::of(&master, kind, s.index).expect("选出的在列表里");
                let group = group.as_deref().expect("选出的在变体引用的组里");
                let (index, _) = key.find(&master, kind, group).expect("按身份找得回");
                assert_eq!(index, s.index);
            }
        }
    }
});
