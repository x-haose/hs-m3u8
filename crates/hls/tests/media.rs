//! 媒体播放列表的解析与规范化。

use hs_m3u8_hls::{
    ByteRange, Error, InitSection, MediaPlaylist, Playlist, PlaylistType, SegmentKey, SyntaxError,
    Unsupported, Url, parse,
};

fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

const BASE: &str = "https://cdn.example.com/video/720p/index.m3u8?token=abc";

fn media(text: &str) -> MediaPlaylist {
    match parse(text, &url(BASE)).unwrap() {
        Playlist::Media(m) => m,
        Playlist::Master(_) => panic!("应解析为媒体播放列表"),
    }
}

fn error(text: &str) -> Error {
    parse(text, &url(BASE)).unwrap_err()
}

fn sequence_iv(sequence: u64) -> [u8; 16] {
    u128::from(sequence).to_be_bytes()
}

#[test]
fn uris_resolve_against_the_final_playlist_url() {
    let m = media(
        "#EXTM3U\n#EXT-X-TARGETDURATION:10\n\
         #EXTINF:10,\nseg0.ts\n\
         #EXTINF:10,\n/root/seg1.ts\n\
         #EXTINF:10,\nhttps://other.example.com/seg2.ts\n\
         #EXTINF:10,\n../1080p/seg3.ts?sig=x\n\
         #EXTINF:10,\nvideo/http_seg4.ts\n\
         #EXTINF:10,\nseg 5.ts\n#EXT-X-ENDLIST\n",
    );
    let uris: Vec<&str> = m.segments.iter().map(|s| s.uri.as_str()).collect();
    assert_eq!(
        uris,
        [
            "https://cdn.example.com/video/720p/seg0.ts",
            "https://cdn.example.com/root/seg1.ts",
            "https://other.example.com/seg2.ts",
            "https://cdn.example.com/video/1080p/seg3.ts?sig=x",
            "https://cdn.example.com/video/720p/video/http_seg4.ts",
            "https://cdn.example.com/video/720p/seg%205.ts",
        ]
    );
}

#[test]
fn durations_are_exact_microseconds() {
    let m = media(
        "#EXTM3U\n#EXTINF:10,\na.ts\n#EXTINF:9.966667,\nb.ts\n#EXTINF:2.12345649,\nc.ts\n\
         #EXTINF:0.0000005,\nd.ts\n#EXTINF:4\ne.ts\n",
    );
    let durations: Vec<u64> = m.segments.iter().map(|s| s.duration_us).collect();
    assert_eq!(durations, [10_000_000, 9_966_667, 2_123_456, 1, 4_000_000]);
}

#[test]
fn sequence_numbers_follow_media_sequence() {
    let m = media(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n#EXTINF:4,\na.ts\n#EXTINF:4,\nb.ts\n#EXTINF:4,\nc.ts\n",
    );
    assert_eq!(m.media_sequence, 7);
    assert_eq!(
        m.segments.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        [7, 8, 9]
    );
}

#[test]
fn key_applies_until_the_next_key_and_iv_defaults_to_the_sequence_number() {
    let m = media(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:5\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"k1.key\"\n#EXTINF:4,\na.ts\n#EXTINF:4,\nb.ts\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example.com/k2\",IV=0x000102030405060708090A0B0C0D0E0F\n\
         #EXTINF:4,\nc.ts\n\
         #EXT-X-KEY:METHOD=NONE\n#EXTINF:4,\nd.ts\n",
    );
    let k1 = url("https://cdn.example.com/video/720p/k1.key");
    let k2 = url("https://keys.example.com/k2");
    let keys: Vec<Option<SegmentKey>> = m.segments.iter().map(|s| s.key.clone()).collect();
    assert_eq!(
        keys,
        [
            Some(SegmentKey {
                uri: k1.clone(),
                iv: sequence_iv(5)
            }),
            Some(SegmentKey {
                uri: k1,
                iv: sequence_iv(6)
            }),
            Some(SegmentKey {
                uri: k2,
                iv: core::array::from_fn(|i| i as u8)
            }),
            None,
        ]
    );
}

#[test]
fn every_segment_keeps_its_own_iv() {
    let m = media(
        "#EXTM3U\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0x2eef4b545dd1efaaf2b39a74b1510383\n#EXTINF:10,\na.ts\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0x2d0d98351f49d32243daa4fb8c3b2606\n#EXTINF:10,\nb.ts\n",
    );
    let ivs: Vec<[u8; 16]> = m
        .segments
        .iter()
        .map(|s| s.key.as_ref().unwrap().iv)
        .collect();
    assert_eq!(ivs[0][0], 0x2e);
    assert_eq!(ivs[1][0], 0x2d);
    assert_ne!(ivs[0], ivs[1]);
}

#[test]
fn identity_key_is_used_alongside_drm_keys() {
    let m = media(
        "#EXTM3U\n\
         #EXT-X-KEY:METHOD=AES-128,URI=\"k.key\"\n\
         #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"data:text/plain;base64,AAAA\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\"\n\
         #EXTINF:4,\na.ts\n",
    );
    let key = m.segments[0].key.as_ref().unwrap();
    assert_eq!(key.uri.as_str(), "https://cdn.example.com/video/720p/k.key");
}

#[test]
fn drm_only_and_sample_aes_are_unsupported() {
    let drm = error(
        "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:4,\na.ts\n",
    );
    assert_eq!(
        drm,
        Error::Unsupported {
            line: 2,
            what: Unsupported::Drm {
                keyformat: "com.apple.streamingkeydelivery".into()
            }
        }
    );
    let sample_aes = error("#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"k\"\n#EXTINF:4,\na.ts\n");
    assert_eq!(
        sample_aes,
        Error::Unsupported {
            line: 2,
            what: Unsupported::SampleAes
        }
    );
}

#[test]
fn lenient_formatting_is_accepted() {
    let text = "\u{FEFF}#EXTM3U\r\n\
                #EXT-X-VERSION:3\r\n\
                #EXT-X-TARGETDURATION:10.0\r\n\
                # 普通注释\r\n\
                #EXT-X-CUSTOM-TAG:whatever\r\n\
                #EXT-X-KEY:method=aes-128,uri=k.key,iv=000102030405060708090a0b0c0d0e0f\r\n\
                \r\n\
                #EXTINF:12.5,第一集, 片头\r\n\
                seg0.ts  \r\n\
                #EXTINF:3\r\n\
                seg1.ts\r\n\
                #EXT-X-ENDLIST\r\n";
    let m = media(text);
    assert_eq!(m.target_duration_us, Some(10_000_000));
    assert_eq!(m.segments.len(), 2);
    assert_eq!(m.segments[0].duration_us, 12_500_000);
    assert_eq!(
        m.segments[0].uri.as_str(),
        "https://cdn.example.com/video/720p/seg0.ts"
    );
    let key = m.segments[1].key.as_ref().unwrap();
    assert_eq!(key.uri.as_str(), "https://cdn.example.com/video/720p/k.key");
    assert_eq!(key.iv, core::array::from_fn::<u8, 16, _>(|i| i as u8));
    assert!(m.ended);
}

#[test]
fn init_section_follows_map_tags() {
    let m = media(
        "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\na.m4s\n#EXTINF:4,\nb.m4s\n\
         #EXT-X-MAP:URI=\"/other/init.mp4\",BYTERANGE=\"720@0\"\n#EXTINF:4,\nc.m4s\n",
    );
    let first = InitSection {
        uri: url("https://cdn.example.com/video/720p/init.mp4"),
        byte_range: None,
    };
    let second = InitSection {
        uri: url("https://cdn.example.com/other/init.mp4"),
        byte_range: Some(ByteRange {
            offset: 0,
            length: 720,
        }),
    };
    let inits: Vec<Option<InitSection>> = m.segments.iter().map(|s| s.init.clone()).collect();
    assert_eq!(inits, [Some(first.clone()), Some(first), Some(second)]);
}

#[test]
fn byte_ranges_without_offset_continue_the_previous_sub_range() {
    let m = media(
        "#EXTM3U\n#EXT-X-BYTERANGE:1000@0\n#EXTINF:4,\nmain.ts\n#EXT-X-BYTERANGE:500\n#EXTINF:4,\nmain.ts\n\
         #EXT-X-BYTERANGE:300\n#EXTINF:4,\nmain.ts\n",
    );
    let ranges: Vec<Option<ByteRange>> = m.segments.iter().map(|s| s.byte_range).collect();
    assert_eq!(
        ranges,
        [
            Some(ByteRange {
                offset: 0,
                length: 1000
            }),
            Some(ByteRange {
                offset: 1000,
                length: 500
            }),
            Some(ByteRange {
                offset: 1500,
                length: 300
            }),
        ]
    );
}

#[test]
fn byte_range_without_offset_after_another_resource_is_an_error() {
    let err = error(
        "#EXTM3U\n#EXT-X-BYTERANGE:1000@0\n#EXTINF:4,\na.ts\n#EXT-X-BYTERANGE:500\n#EXTINF:4,\nb.ts\n",
    );
    assert_eq!(
        err,
        Error::Syntax {
            line: 5,
            kind: SyntaxError::ByteRangeWithoutOffset
        }
    );
}

#[test]
fn discontinuity_numbers_start_from_the_discontinuity_sequence() {
    let m = media(
        "#EXTM3U\n#EXT-X-DISCONTINUITY-SEQUENCE:3\n#EXTINF:4,\na.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:4,\nb.ts\n\
         #EXTINF:4,\nc.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:4,\nd.ts\n",
    );
    assert_eq!(
        m.segments
            .iter()
            .map(|s| s.discontinuity)
            .collect::<Vec<_>>(),
        [3, 4, 4, 5]
    );
}

#[test]
fn live_and_vod_are_told_apart() {
    let live = media("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4,\na.ts\n");
    assert!(!live.ended);
    let vod = media("#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n");
    assert!(vod.ended);
    assert_eq!(vod.playlist_type, Some(PlaylistType::Vod));
}

#[test]
fn malformed_input_is_reported_with_its_line() {
    assert_eq!(
        error("<html><body>403 Forbidden</body></html>"),
        Error::NotAPlaylist
    );
    assert_eq!(
        error("#EXTM3U\n#EXTINF:4,\na.ts\n#EXTINF:4,\n"),
        Error::Syntax {
            line: 4,
            kind: SyntaxError::InfoWithoutUri { tag: "EXTINF" }
        }
    );
    assert_eq!(
        error("#EXTM3U\na.ts\n"),
        Error::Syntax {
            line: 2,
            kind: SyntaxError::UriWithoutInfo { expected: "EXTINF" }
        }
    );
    assert!(matches!(
        error("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0x1234\n#EXTINF:4,\na.ts\n"),
        Error::Syntax {
            line: 2,
            kind: SyntaxError::Iv(_)
        }
    ));
    assert_eq!(
        error("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128\n#EXTINF:4,\na.ts\n"),
        Error::Syntax {
            line: 2,
            kind: SyntaxError::MissingAttribute {
                tag: "EXT-X-KEY",
                name: "URI"
            }
        }
    );
    assert!(matches!(
        error("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\n#EXTINF:4,\na.ts\n"),
        Error::Syntax {
            line: 2,
            kind: SyntaxError::Attributes(_)
        }
    ));
    assert_eq!(
        error("#EXTM3U\n#EXTINF:4,\na.ts\n#EXT-X-MEDIA-SEQUENCE:3\n"),
        Error::Syntax {
            line: 4,
            kind: SyntaxError::SequenceAfterSegments
        }
    );
    assert_eq!(
        error("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n#EXTINF:4,\na.ts\n"),
        Error::Mixed
    );
    // 长度为 0 的字节范围拼不出合法的 Range 请求
    assert_eq!(
        error("#EXTM3U\n#EXT-X-BYTERANGE:0@10\n#EXTINF:4,\na.ts\n"),
        Error::Syntax {
            line: 2,
            kind: SyntaxError::ByteRange("0@10".into())
        }
    );
}
