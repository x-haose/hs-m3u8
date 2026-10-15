//! CODECS 属性里的编码（RFC 6381 的写法，如 `avc1.640028`、`mp4a.40.2`）：按点号前的那段认种类。

/// 一项编码的种类；认不出的算视频（可能有视频）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecKind {
    Video,
    Audio,
    Subtitles,
}

impl CodecKind {
    pub fn of(codec: &str) -> CodecKind {
        const AUDIO: [&str; 10] = [
            "mp4a", "ac-3", "ec-3", "ac-4", "opus", "flac", "alac", "dtsc", "dtse", "dtsh",
        ];
        const SUBTITLES: [&str; 2] = ["wvtt", "stpp"];
        let family = codec_family(codec);
        if AUDIO.contains(&family.as_str()) {
            CodecKind::Audio
        } else if SUBTITLES.contains(&family.as_str()) {
            CodecKind::Subtitles
        } else {
            CodecKind::Video
        }
    }
}

/// 编码的种类名：点号前的那段，小写，如 `avc1`、`mp4a`；不含编码的档次与级别。
pub fn codec_family(codec: &str) -> String {
    codec
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}
