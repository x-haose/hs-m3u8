//! 写出：SRT、WebVTT 文件，与 MP4 的 mov_text 样本。传入的 cue 都在输出时间线上，时刻不为负。

use std::fmt::Write as _;

use crate::{Cue, plain_text, srt_text};

/// SRT 文件：从 1 起编号，时刻 `HH:MM:SS,mmm`（毫秒向下取整），正文见 [`srt_text`]。空行在 SRT 里是 cue 的分隔：正文
/// 去掉标签后只剩空白的行不写，一行都不剩的 cue 写一个空格。
pub fn srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (n, cue) in cues.iter().enumerate() {
        let text = srt_text(&cue.text);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let text = if lines.is_empty() {
            " ".to_owned()
        } else {
            lines.join("\n")
        };
        writeln!(
            out,
            "{}\n{} --> {}\n{text}\n",
            n + 1,
            clock(cue.start_us, ','),
            clock(cue.end_us, ',')
        )
        .expect("写进 String 不会失败");
    }
    out
}

/// WebVTT 文件：标识、设置与正文原样，时刻 `HH:MM:SS.mmm`。
pub fn webvtt(cues: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues {
        if let Some(id) = &cue.id {
            writeln!(out, "{id}").expect("写进 String 不会失败");
        }
        let settings = if cue.settings.is_empty() {
            String::new()
        } else {
            format!(" {}", cue.settings)
        };
        writeln!(
            out,
            "{} --> {}{settings}\n{}\n",
            clock(cue.start_us, '.'),
            clock(cue.end_us, '.'),
            cue.text
        )
        .expect("写进 String 不会失败");
    }
    out
}

/// `HH:MM:SS<sep>mmm`；`us` 为输出时间线上的时刻，不为负。
fn clock(us: i64, sep: char) -> String {
    let ms = u64::try_from(us).expect("输出时间线上的时刻不为负") / 1000;
    format!(
        "{:02}:{:02}:{:02}{sep}{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// 时间上不重叠的一段字幕：mov_text 同一时刻只显示一个样本，同时显示的几条 cue 合成一段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice {
    pub start_us: i64,
    /// 大于 `start_us`
    pub end_us: i64,
    /// 这段时间里显示的各 cue 的纯文字（见 [`plain_text`]），按 cue 的先后以换行连接
    pub text: String,
}

/// 把 `cues`（按起点排序）切成互不重叠的段：每个 cue 的起止都是切点，相邻两点之间显示的 cue 合成一段，没有 cue 的空档
/// 不出段；相邻两段文字相同时合成一段。
pub fn slices(cues: &[Cue]) -> Vec<Slice> {
    let mut points: Vec<i64> = cues.iter().flat_map(|c| [c.start_us, c.end_us]).collect();
    points.sort_unstable();
    points.dedup();
    let mut out: Vec<Slice> = Vec::new();
    for pair in points.windows(2) {
        let (start_us, end_us) = (pair[0], pair[1]);
        let showing: Vec<String> = cues
            .iter()
            .filter(|c| c.start_us <= start_us && c.end_us >= end_us)
            .map(|c| plain_text(&c.text))
            .collect();
        if showing.is_empty() {
            continue;
        }
        let text = showing.join("\n");
        match out.last_mut() {
            Some(last) if last.end_us == start_us && last.text == text => last.end_us = end_us,
            _ => out.push(Slice {
                start_us,
                end_us,
                text,
            }),
        }
    }
    out
}

/// mov_text 的样本描述（tx3g 样本项里 SampleEntry 头之后的部分，3GPP TS 26.245 5.16）：居中靠下、背景透明，默认样式
/// 为 18 号白色 Serif，与链接的 FFmpeg 的 mov_text 编码器没有 ASS 样式时写的相同。
pub const MOV_TEXT_SAMPLE_ENTRY: [u8; 48] = [
    0, 0, 0, 0, // displayFlags
    0x01, 0xFF, // 水平居中、垂直靠下
    0, 0, 0, 0, // 背景 RGBA
    0, 0, 0, 0, 0, 0, 0, 0, // BoxRecord
    0, 0, 0, 0, 0, 0x01, 0, 0x12, 0xFF, 0xFF, 0xFF,
    0xFF, // StyleRecord：字体 1、18 号、白色
    0, 0, 0, 0x12, b'f', b't', b'a', b'b', 0, 0x01, // FontTableBox，一项
    0, 0x01, 5, b'S', b'e', b'r', b'i', b'f', // 字体 1 为 Serif
];

/// 一段的文字超出 mov_text 样本能放的 65535 字节。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("一段字幕有 {0} 字节，超出 mov_text 的上限 65535")]
pub struct TooLong(pub usize);

/// 一个 mov_text 样本：16 位大端长度加 UTF-8 文字，不带样式。空串即清屏的样本。
/// 简化：不写样式框（styl），粗体、斜体丢掉；要保留时按 3GPP 的字符偏移写 styl。
pub fn mov_text_sample(text: &str) -> Result<Vec<u8>, TooLong> {
    let len = u16::try_from(text.len()).map_err(|_| TooLong(text.len()))?;
    let mut sample = Vec::with_capacity(2 + text.len());
    sample.extend_from_slice(&len.to_be_bytes());
    sample.extend_from_slice(text.as_bytes());
    Ok(sample)
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
    fn srt_and_webvtt_files() {
        let mut first = cue(1_234_567, 3_723_000_999, "<b>a</b>\n\n<c.x>b</c>");
        first.id = Some("x".into());
        first.settings = "line:0".into();
        let cues = [
            first,
            cue(4_000_000_000, 4_000_001_000, "<v A></v>"),
            cue(
                4_000_002_000,
                4_000_003_000,
                "<c.x></c>\n你好\n<c></c>\n<c></c>\n再见",
            ),
        ];
        assert_eq!(
            srt(&cues),
            "1\n00:00:01,234 --> 01:02:03,000\n<b>a</b>\nb\n\n2\n01:06:40,000 --> 01:06:40,001\n \n\n\
             3\n01:06:40,002 --> 01:06:40,003\n你好\n再见\n\n"
        );
        assert_eq!(
            webvtt(&cues[..1]),
            "WEBVTT\n\nx\n00:00:01.234 --> 01:02:03.000 line:0\n<b>a</b>\n\n<c.x>b</c>\n\n"
        );
    }

    /// 重叠的 cue 切成不重叠的段；空档不出段，文字不变的相邻段合并。
    #[test]
    fn overlapping_cues_are_sliced() {
        let cues = [
            cue(0, 4, "<i>a</i>"),
            cue(2, 6, "b"),
            cue(6, 7, "b"),
            cue(9, 10, "c"),
        ];
        let slice = |start_us, end_us, text: &str| Slice {
            start_us,
            end_us,
            text: text.to_owned(),
        };
        assert_eq!(
            slices(&cues),
            [
                slice(0, 2, "a"),
                slice(2, 4, "a\nb"),
                slice(4, 7, "b"),
                slice(9, 10, "c")
            ]
        );
    }

    #[test]
    fn mov_text_samples_carry_a_length() {
        assert_eq!(mov_text_sample("字").unwrap(), [0, 3, 0xE5, 0xAD, 0x97]);
        assert_eq!(mov_text_sample("").unwrap(), [0, 0]);
        assert_eq!(
            mov_text_sample(&"x".repeat(65536)).unwrap_err(),
            TooLong(65536)
        );
    }
}
