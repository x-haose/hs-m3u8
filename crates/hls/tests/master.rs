//! 主播放列表解析与选轨。

use hs_m3u8_hls::{
    Error, MasterPlaylist, Playlist, Preference, RenditionKind, Resolution, SelectError,
    SyntaxError, Url, VariantChoice, parse, select,
};

const BASE: &str = "https://video.example.com/ext_tw_video/123/pu/pl/master.m3u8?tag=12";

fn master(text: &str) -> MasterPlaylist {
    match parse(text, &Url::parse(BASE).unwrap()).unwrap() {
        Playlist::Master(m) => m,
        Playlist::Media(_) => panic!("应解析为主播放列表"),
    }
}

/// 音视频分流（视频与音频是两条媒体播放列表）的主播放列表，另带一个纯音频变体与一个 I 帧变体。
const SPLIT: &str = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-INDEPENDENT-SEGMENTS\n\
    #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=NO,URI=\"/aud/en/index.m3u8\"\n\
    #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"中文\",LANGUAGE=\"zh\",DEFAULT=YES,URI=\"/aud/zh/index.m3u8\"\n\
    #EXT-X-STREAM-INF:BANDWIDTH=288000,RESOLUTION=480x270,CODECS=\"avc1.4d001e,mp4a.40.2\",AUDIO=\"aud\"\n\
    /vid/480x270/index.m3u8\n\
    #EXT-X-STREAM-INF:BANDWIDTH=2176000,RESOLUTION=1280x720,CODECS=\"avc1.640020,mp4a.40.2\",AUDIO=\"aud\"\n\
    /vid/1280x720/index.m3u8\n\
    #EXT-X-STREAM-INF:BANDWIDTH=9000000,CODECS=\"mp4a.40.2\",AUDIO=\"aud\"\n\
    audio_only.m3u8\n\
    #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=100000,URI=\"iframe.m3u8\"\n";

#[test]
fn variants_and_renditions_are_parsed() {
    let m = master(SPLIT);
    assert_eq!(m.variants.len(), 3);
    let v = &m.variants[1];
    assert_eq!(
        v.uri.as_str(),
        "https://video.example.com/vid/1280x720/index.m3u8"
    );
    assert_eq!(v.bandwidth, Some(2_176_000));
    assert_eq!(
        v.resolution,
        Some(Resolution {
            width: 1280,
            height: 720
        })
    );
    assert_eq!(v.codecs, ["avc1.640020", "mp4a.40.2"]);
    assert_eq!(v.audio.as_deref(), Some("aud"));
    assert_eq!(
        m.variants[2].uri.as_str(),
        "https://video.example.com/ext_tw_video/123/pu/pl/audio_only.m3u8"
    );

    assert_eq!(m.renditions.len(), 2);
    let zh = &m.renditions[1];
    assert_eq!(zh.kind, RenditionKind::Audio);
    assert_eq!(
        (
            zh.group_id.as_str(),
            zh.name.as_str(),
            zh.language.as_deref()
        ),
        ("aud", "中文", Some("zh"))
    );
    assert!(zh.default);
    assert_eq!(
        zh.uri.as_ref().unwrap().as_str(),
        "https://video.example.com/aud/zh/index.m3u8"
    );
}

#[test]
fn stream_inf_without_uri_is_an_error() {
    let err = parse(
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n#EXT-X-STREAM-INF:BANDWIDTH=2\nb.m3u8\n",
        &Url::parse(BASE).unwrap(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        Error::Syntax {
            line: 2,
            kind: SyntaxError::InfoWithoutUri {
                tag: "EXT-X-STREAM-INF"
            }
        }
    );
}

#[test]
fn best_variant_has_the_highest_resolution_and_ignores_audio_only_variants() {
    let m = master(SPLIT);
    let selection = select(&m, &Preference::default()).unwrap();
    assert_eq!(
        selection.variant.resolution,
        Some(Resolution {
            width: 1280,
            height: 720
        })
    );
}

#[test]
fn equal_resolutions_fall_back_to_bandwidth() {
    let m = master(
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=1920x1080\nlow.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=5000,RESOLUTION=1920X1080\nhigh.m3u8\n",
    );
    let selection = select(&m, &Preference::default()).unwrap();
    assert_eq!(selection.variant.bandwidth, Some(5000));
    assert_eq!(selection.audio, None);
}

#[test]
fn audio_rendition_is_chosen_by_default_flag_or_language() {
    let m = master(SPLIT);
    let default = select(&m, &Preference::default()).unwrap();
    let audio = default.audio.unwrap();
    assert_eq!(audio.rendition.language.as_deref(), Some("zh"));
    assert_eq!(
        audio.uri.as_str(),
        "https://video.example.com/aud/zh/index.m3u8"
    );

    let english = Preference {
        variant: VariantChoice::Best,
        audio_language: Some("EN".into()),
    };
    let audio = select(&m, &english).unwrap().audio.unwrap();
    assert_eq!(audio.rendition.name, "English");

    let missing = Preference {
        variant: VariantChoice::Best,
        audio_language: Some("ja".into()),
    };
    assert_eq!(
        select(&m, &missing).unwrap_err(),
        SelectError::AudioLanguageNotFound {
            group: "aud".into(),
            language: "ja".into(),
            available: vec!["en".into(), "zh".into()],
        }
    );
}

#[test]
fn rendition_without_uri_means_audio_is_muxed_into_the_variant() {
    let m = master(
        "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"main\",DEFAULT=YES\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=640x360,AUDIO=\"a\"\nv.m3u8\n",
    );
    assert_eq!(select(&m, &Preference::default()).unwrap().audio, None);
}

#[test]
fn variant_index_is_bounds_checked() {
    let m = master(SPLIT);
    let first = Preference {
        variant: VariantChoice::Index(0),
        audio_language: None,
    };
    assert_eq!(
        select(&m, &first).unwrap().variant.resolution,
        Some(Resolution {
            width: 480,
            height: 270
        })
    );
    let out = Preference {
        variant: VariantChoice::Index(9),
        audio_language: None,
    };
    assert_eq!(
        select(&m, &out).unwrap_err(),
        SelectError::IndexOutOfRange { index: 9, count: 3 }
    );
}
