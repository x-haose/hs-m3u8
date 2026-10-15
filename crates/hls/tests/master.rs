//! 主播放列表解析与选轨。

use hs_m3u8_hls::{
    AudioChoice, AudioKey, Error, Malformed, MasterPlaylist, Playlist, Preference, RenditionKind,
    Resolution, SelectError, SyntaxError, Url, VariantChoice, VariantKey, parse, select,
};

const BASE: &str = "https://video.example.com/ext_tw_video/123/pu/pl/master.m3u8?tag=12";

fn master(text: &str) -> MasterPlaylist {
    match parse(text, &Url::parse(BASE).unwrap()).unwrap() {
        Playlist::Master(m) => m,
        Playlist::Media(_) => panic!("应解析为主播放列表"),
    }
}

/// 音视频分离（视频与音频是两条媒体播放列表）的主播放列表，另带一个纯音频变体与一个 I 帧变体。
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
            zh.group.as_str(),
            zh.name.as_deref(),
            zh.language.as_deref()
        ),
        ("aud", Some("中文"), Some("zh"))
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
        Error::Malformed(Malformed::Syntax {
            line: 2,
            kind: SyntaxError::InfoWithoutUri {
                tag: "EXT-X-STREAM-INF"
            }
        })
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

/// 视频变体没写 RESOLUTION（不合 RFC 8216，但有）时分辨率比不了，按带宽选；只把 CODECS 里全是音频编码的变体当纯音频
/// 排除。
#[test]
fn variants_without_resolution_are_compared_by_bandwidth() {
    let m = master(
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5000000,CODECS=\"avc1.640028,mp4a.40.2\"\nhigh.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=9000000,CODECS=\"mp4a.40.2\"\naudio.m3u8\n",
    );
    let selection = select(&m, &Preference::default()).unwrap();
    assert_eq!(selection.variant.bandwidth, Some(5_000_000));

    let only_audio =
        master("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=64000,CODECS=\"mp4a.40.2\"\na.m3u8\n");
    assert_eq!(
        select(&only_audio, &Preference::default())
            .unwrap()
            .variant_index,
        0
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
        audio: AudioChoice::Language("EN".into()),
    };
    let audio = select(&m, &english).unwrap().audio.unwrap();
    assert_eq!(audio.rendition.name.as_deref(), Some("English"));

    let missing = Preference {
        variant: VariantChoice::Best,
        audio: AudioChoice::Language("ja".into()),
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

/// 按下标选同组里的另一条音轨，下标只数音频 rendition；下标越界、不在所选变体音频组里时报错。
#[test]
fn audio_rendition_is_chosen_by_index() {
    let m = master(SPLIT);
    let by_index = |index| Preference {
        variant: VariantChoice::Best,
        audio: AudioChoice::Index(index),
    };
    let audio = select(&m, &by_index(0)).unwrap().audio.unwrap();
    assert_eq!(audio.rendition.name.as_deref(), Some("English"));
    assert_eq!(
        select(&m, &by_index(2)).unwrap_err(),
        SelectError::AudioIndexOutOfRange { index: 2, count: 2 }
    );

    // 下标只数音频：排在前面的字幕不算
    let with_subtitles = master(&SPLIT.replacen(
        "#EXT-X-MEDIA:TYPE=AUDIO",
        "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"sub\",NAME=\"zh\",URI=\"sub.m3u8\"\n#EXT-X-MEDIA:TYPE=AUDIO",
        1,
    ));
    let audio = select(&with_subtitles, &by_index(1))
        .unwrap()
        .audio
        .unwrap();
    assert_eq!(
        (audio.index, audio.rendition.name.as_deref()),
        (1, Some("中文"))
    );

    let other_group = master(
        "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"x\",URI=\"x.m3u8\"\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"b\",NAME=\"y\",URI=\"y.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1000,AUDIO=\"a\"\nv.m3u8\n",
    );
    assert_eq!(
        select(&other_group, &by_index(1)).unwrap_err(),
        SelectError::NotVariantAudio {
            index: 1,
            group: Some("a".into())
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
        audio: AudioChoice::Default,
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
        audio: AudioChoice::Default,
    };
    assert_eq!(
        select(&m, &out).unwrap_err(),
        SelectError::VariantIndexOutOfRange { index: 9, count: 3 }
    );
}

const EN: &str = "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"en\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"en.m3u8?t=1\"\n";
const LOW: &str = "#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=640x360,AUDIO=\"a\"\nlow.m3u8?t=1\n";

fn matching(master: &MasterPlaylist, variant: usize, audio: usize) -> Preference {
    Preference {
        variant: VariantChoice::Matching(VariantKey::of(master, variant).unwrap()),
        audio: AudioChoice::Matching(AudioKey::of(master, audio).unwrap()),
    }
}

/// 主播放列表前面多了变体与音频、地址换了令牌：按下标会选到别的，按身份仍是原来那两条。
#[test]
fn matching_finds_the_same_tracks_after_the_master_changes() {
    let before = master(&format!("#EXTM3U\n{EN}{LOW}"));
    let preference = matching(&before, 0, 0);
    let after = master(&format!(
        "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"fr\",LANGUAGE=\"fr\",URI=\"fr.m3u8\"\n{}\
         #EXT-X-STREAM-INF:BANDWIDTH=9000,RESOLUTION=1920x1080,AUDIO=\"a\"\nhigh.m3u8\n{}",
        EN.replace("t=1", "t=2"),
        LOW.replace("t=1", "t=2"),
    ));

    let selected = select(&after, &preference).unwrap();
    assert_eq!(
        selected.variant.uri.as_str(),
        "https://video.example.com/ext_tw_video/123/pu/pl/low.m3u8?t=2"
    );
    let audio = selected.audio.unwrap();
    assert_eq!(audio.index, 1);
    assert_eq!(
        audio.uri.as_str(),
        "https://video.example.com/ext_tw_video/123/pu/pl/en.m3u8?t=2"
    );
}

/// 属性变了即不是同一条：报找不到，不退而选别的。
#[test]
fn matching_reports_tracks_that_are_gone() {
    let before = master(&format!("#EXTM3U\n{EN}{LOW}"));
    let preference = matching(&before, 0, 0);
    let without_variant = master(&format!(
        "#EXTM3U\n{EN}{}",
        LOW.replace("BANDWIDTH=1000", "BANDWIDTH=2000")
    ));
    assert_eq!(
        select(&without_variant, &preference).unwrap_err(),
        SelectError::VariantNotFound
    );
    let without_audio = master(&format!("#EXTM3U\n{}{LOW}", EN.replace("\"en\"", "\"fr\"")));
    assert_eq!(
        select(&without_audio, &preference).unwrap_err(),
        SelectError::AudioNotFound
    );
}

/// 属性完全相同的冗余流按排在第几区分。
#[test]
fn redundant_variants_are_told_apart_by_position() {
    let m = master(
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=640x360\na/v.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=640x360\nb/v.m3u8\n",
    );
    for index in [0, 1] {
        let key = VariantKey::of(&m, index).unwrap();
        assert!(
            matches!(key, VariantKey::Attributes { occurrence, .. } if occurrence == index),
            "{key:?}"
        );
        let preference = Preference {
            variant: VariantChoice::Matching(key),
            audio: AudioChoice::Default,
        };
        assert_eq!(select(&m, &preference).unwrap().variant_index, index);
    }
}

/// 同一条流经两条路径分发（RFC 8216bis 的 Content Steering 示例）：两条路径的稳定 ID 相同，组名与地址不同。
const PATHWAYS: &str = "#EXTM3U\n\
    #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"A\",NAME=\"English\",DEFAULT=YES,URI=\"eng.m3u8\",LANGUAGE=\"en\",STABLE-RENDITION-ID=\"Audio-37262\"\n\
    #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"B\",NAME=\"ENGLISH\",DEFAULT=YES,URI=\"https://b.example.com/audio-eng.m3u8\",LANGUAGE=\"en\",STABLE-RENDITION-ID=\"Audio-37262\"\n\
    #EXT-X-STREAM-INF:BANDWIDTH=1280000,AUDIO=\"A\",STABLE-VARIANT-ID=\"Video-128\"\n128/video.m3u8\n\
    #EXT-X-STREAM-INF:BANDWIDTH=7680000,AUDIO=\"A\",STABLE-VARIANT-ID=\"Video-768\"\n768/video.m3u8\n\
    #EXT-X-STREAM-INF:BANDWIDTH=1280000,AUDIO=\"B\",STABLE-VARIANT-ID=\"Video-128\"\nhttps://backup.example.com/128/video.m3u8\n\
    #EXT-X-STREAM-INF:BANDWIDTH=7680000,AUDIO=\"B\",STABLE-VARIANT-ID=\"Video-768\"\nhttps://backup.example.com/768/video.m3u8\n";

/// 写了稳定 ID 时按它与音频组认：之后拉到的主播放列表码率变了、地址换了令牌、前面多了变体、两条路径换了先后，仍选到
/// 同一路径上的那两条。
#[test]
fn stable_ids_find_the_same_tracks_after_attributes_change() {
    let before = master(PATHWAYS);
    assert_eq!(before.variants[3].stable_id.as_deref(), Some("Video-768"));
    assert_eq!(
        before.renditions[1].stable_id.as_deref(),
        Some("Audio-37262")
    );
    let preference = matching(&before, 3, 1);
    assert_eq!(
        preference.variant,
        VariantChoice::Matching(VariantKey::Stable {
            id: "Video-768".into(),
            audio_group: Some("B".into()),
            occurrence: 0
        })
    );
    assert_eq!(
        preference.audio,
        AudioChoice::Matching(AudioKey::Stable {
            id: "Audio-37262".into(),
            occurrence: 0
        })
    );

    let lines: Vec<&str> = PATHWAYS.lines().collect();
    let row = |i: usize| {
        format!(
            "{}\n{}?t=2\n",
            lines[i].replace("7680000", "7700000"),
            lines[i + 1]
        )
    };
    let after = master(&format!(
        "#EXTM3U\n{}\n{}\n#EXT-X-STREAM-INF:BANDWIDTH=1,STABLE-VARIANT-ID=\"Video-2160\"\n2160/video.m3u8\n{}{}{}{}",
        lines[1],
        lines[2],
        row(7),
        row(9),
        row(3),
        row(5),
    ));
    let selected = select(&after, &preference).unwrap();
    assert_eq!(selected.variant_index, 2);
    assert_eq!(
        selected.variant.uri.as_str(),
        "https://backup.example.com/768/video.m3u8?t=2"
    );
    let audio = selected.audio.unwrap();
    assert_eq!((audio.index, audio.rendition.group.as_str()), (1, "B"));
}

/// 同一条视频流按音频组各列一行（稳定 ID 相同）：音频组区分这几行，之后中间多了一组、几行换了先后，按身份仍选到原来
/// 那组的那行。
#[test]
fn rows_of_one_stream_are_told_apart_by_audio_group() {
    let media = |group: &str, id: &str| {
        format!(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{group}\",NAME=\"en\",DEFAULT=YES,URI=\"{id}.m3u8\",STABLE-RENDITION-ID=\"{id}\"\n"
        )
    };
    let row = |group: &str, codec: &str| {
        format!(
            "#EXT-X-STREAM-INF:BANDWIDTH=5000000,CODECS=\"avc1.640028,{codec}\",AUDIO=\"{group}\",STABLE-VARIANT-ID=\"v1080\"\nv1080.m3u8\n"
        )
    };
    let before = master(&format!(
        "#EXTM3U\n{}{}{}{}",
        media("aac", "aac-en"),
        media("ec3", "ec3-en"),
        row("aac", "mp4a.40.2"),
        row("ec3", "ec-3"),
    ));
    let preference = Preference {
        variant: VariantChoice::Matching(VariantKey::of(&before, 1).unwrap()),
        audio: AudioChoice::Default,
    };
    let after = master(&format!(
        "#EXTM3U\n{}{}{}{}{}{}",
        media("aac", "aac-en"),
        media("ac3", "ac3-en"),
        media("ec3", "ec3-en"),
        row("ec3", "ec-3"),
        row("ac3", "ac-3"),
        row("aac", "mp4a.40.2"),
    ));

    let selected = select(&after, &preference).unwrap();

    assert_eq!(selected.variant.audio.as_deref(), Some("ec3"));
    assert_eq!(
        selected.audio.unwrap().rendition.stable_id.as_deref(),
        Some("ec3-en")
    );
}

/// 音频按身份在所选变体的音频组里找：另一条路径上的变体配它那一组里稳定 ID 相同的音频；变体没有引用音频组时报找不到。
#[test]
fn matching_audio_is_looked_up_in_the_variant_group() {
    let m = master(PATHWAYS);
    let preference = Preference {
        variant: VariantChoice::Index(1),
        audio: AudioChoice::Matching(AudioKey::of(&m, 1).unwrap()),
    };
    let audio = select(&m, &preference).unwrap().audio.unwrap();
    assert_eq!((audio.index, audio.rendition.group.as_str()), (0, "A"));

    let without_group = master(&format!(
        "#EXTM3U\n{EN}#EXT-X-STREAM-INF:BANDWIDTH=1000\nv.m3u8\n"
    ));
    let preference = Preference {
        variant: VariantChoice::Best,
        audio: AudioChoice::Matching(AudioKey::of(&without_group, 0).unwrap()),
    };
    assert_eq!(
        select(&without_group, &preference).unwrap_err(),
        SelectError::AudioNotFound
    );
}

/// 没写稳定 ID 时按属性认，之后来源写上了稳定 ID 也找得回。
#[test]
fn attribute_keys_find_tracks_that_gained_stable_ids() {
    let before = master(&format!("#EXTM3U\n{EN}{LOW}"));
    let preference = matching(&before, 0, 0);
    assert!(matches!(
        preference.variant,
        VariantChoice::Matching(VariantKey::Attributes { .. })
    ));
    let after = master(&format!(
        "#EXTM3U\n{}{}",
        EN.replace("DEFAULT=YES", "DEFAULT=YES,STABLE-RENDITION-ID=\"en\""),
        LOW.replace("AUDIO=", "STABLE-VARIANT-ID=\"low\",AUDIO="),
    ));
    let selected = select(&after, &preference).unwrap();
    assert_eq!(selected.variant_index, 0);
    assert_eq!(selected.audio.unwrap().index, 0);
}
