//! 字幕的纯计算部分，不做文件与网络 IO：解析 HLS 的分段 WebVTT（含 X-TIMESTAMP-MAP，RFC 8216 3.5），把 cue 换到媒体
//! 时间线上，写成 SRT、WebVTT 文件，或 MP4 的 mov_text（3GPP TS 26.245）样本；另有 BCP 47 语言标签到 ISO 639-2 的换算。
//!
//! 时刻一律为微秒。解析出的 cue 时刻是 WebVTT 文件自己的时间（「本地时间」）；[`TimestampMap::media_us`] 换到媒体时间线
//! （MPEG-TS 90 kHz 时钟上的微秒，回绕由调用方按媒体的起点展开）；写出的 cue 时刻是输出时间线上的，不能为负。

mod iso639;
mod markup;
mod webvtt;
mod write;

pub use markup::{plain_text, srt_text};
pub use webvtt::{Error, TimestampMap, WebVtt, parse};
pub use write::{MOV_TEXT_SAMPLE_ENTRY, Slice, TooLong, mov_text_sample, slices, srt, webvtt};

/// 一条 cue。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_us: i64,
    /// 大于 `start_us`
    pub end_us: i64,
    /// cue 标识（时间行之前的那行）；没有时为 None
    pub id: Option<String>,
    /// 时间行 `-->` 之后的设置（位置、对齐等），原样；没有时为空
    pub settings: String,
    /// 正文，原样（WebVTT 的标签与字符引用未处理），多行以 `\n` 连接，不含首尾空行
    pub text: String,
}

/// 去掉重复的 cue（起止与正文都相同的只留第一条）并按起点排序（起点相同的保持原有先后）。HLS 的字幕分片常把跨分片的
/// cue 在相邻两片里各写一遍（RFC 8216 3.5 允许），换到同一时间线上之后即重复。
pub fn dedup(cues: &mut Vec<Cue>) {
    cues.sort_by_key(|c| c.start_us);
    let mut kept: Vec<Cue> = Vec::with_capacity(cues.len());
    for cue in cues.drain(..) {
        let duplicate = kept
            .iter()
            .rev()
            .take_while(|k| k.start_us == cue.start_us)
            .any(|k| k.end_us == cue.end_us && k.text == cue.text);
        if !duplicate {
            kept.push(cue);
        }
    }
    *cues = kept;
}

/// 语言标签（BCP 47，如 `en`、`zh-Hans`、`pt-BR`）的主标签对应的 ISO 639-2 代码：(书目代码 639-2/B, 术语代码 639-2/T)，
/// 两者多数相同（不同的如德语 `ger` / `deu`、中文 `chi` / `zho`）。Matroska 的 Language 用书目代码，MP4 的 mdhd 用术语
/// 代码。主标签是 ISO 639-1 两字母代码或 639-2 三字母代码（不分大小写）时认得；ISO 639-3 才有的（如 `yue`）、私用与
/// 认不出的为 None。
pub fn iso639_2(tag: &str) -> Option<(&'static str, &'static str)> {
    let primary = tag.split(['-', '_']).next()?.to_ascii_lowercase();
    iso639::CODES
        .iter()
        .find(|(b, t, a2)| match primary.len() {
            2 => *a2 == primary,
            3 => *b == primary || *t == primary,
            _ => false,
        })
        .map(|&(b, t, _)| (b, t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_tags_map_to_both_iso_639_2_forms() {
        assert_eq!(iso639_2("en"), Some(("eng", "eng")));
        assert_eq!(iso639_2("zh-Hans"), Some(("chi", "zho")));
        assert_eq!(iso639_2("DE-at"), Some(("ger", "deu")));
        assert_eq!(iso639_2("deu"), Some(("ger", "deu")));
        assert_eq!(iso639_2("yue"), None);
        assert_eq!(iso639_2("x-klingon"), None);
        assert_eq!(iso639_2(""), None);
    }

    fn cue(start_us: i64, end_us: i64, text: &str) -> Cue {
        Cue {
            start_us,
            end_us,
            id: None,
            settings: String::new(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn duplicates_across_segments_are_removed() {
        let mut cues = vec![
            cue(5, 9, "b"),
            cue(0, 6, "a"),
            cue(5, 9, "b"),
            cue(5, 9, "c"),
            cue(5, 8, "b"),
        ];
        dedup(&mut cues);
        assert_eq!(
            cues,
            [
                cue(0, 6, "a"),
                cue(5, 9, "b"),
                cue(5, 9, "c"),
                cue(5, 8, "b")
            ]
        );
    }
}
