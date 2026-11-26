//! 字幕的纯计算部分，不做文件与网络 IO：解析 HLS 的分段 WebVTT（含 X-TIMESTAMP-MAP，RFC 8216 3.5），把 cue 换到媒体
//! 时间线上，写成 SRT、WebVTT 文件，或 MP4 的 mov_text（3GPP TS 26.245）样本。
//!
//! 时刻一律为微秒。解析出的 cue 时刻是 WebVTT 文件自己的时间（「本地时间」）；[`TimestampMap::media_us`] 换到媒体时间线
//! （MPEG-TS 90 kHz 时钟上的微秒，回绕由调用方按媒体的起点展开）；写出的 cue 时刻是输出时间线上的，不能为负。

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

#[cfg(test)]
mod tests {
    use super::*;

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
