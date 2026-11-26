//! WebVTT 文件的解析（W3C WebVTT 第 6 节的解析规则，只取 cue 与 HLS 的 X-TIMESTAMP-MAP）。

use crate::Cue;

/// HLS 字幕分片头里的 `X-TIMESTAMP-MAP=MPEGTS:<90 kHz 时钟值>,LOCAL:<WebVTT 时刻>`：本地时刻 `LOCAL` 即媒体时间线上的
/// `MPEGTS`（RFC 8216 3.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimestampMap {
    /// 90 kHz 时钟值，原样（可能超出 33 位）
    pub mpegts: u64,
    pub local_us: i64,
}

/// 90 kHz 时钟回绕的周期：2^33 个 tick。
const WRAP_TICKS: u64 = 1 << 33;

impl TimestampMap {
    /// 本地时刻 `cue_us` 在媒体时间线上的时刻（90 kHz 时钟上的微秒，四舍五入；MPEGTS 取 33 位以内的值，回绕未展开）。没有
    /// X-TIMESTAMP-MAP 时（`map` 为 None）本地 0 即媒体 0（RFC 8216 3.5）。超出 i64 时为 None（只出在时刻写得离谱时）。
    pub fn media_us(map: Option<&TimestampMap>, cue_us: i64) -> Option<i64> {
        match map {
            None => Some(cue_us),
            Some(map) => {
                // 四舍五入到微秒，与 FFmpeg 换算媒体时间戳（av_rescale）相同
                let ticks = i64::try_from(map.mpegts % WRAP_TICKS).expect("33 位以内");
                ((ticks * 200 + 9) / 18).checked_add(cue_us.checked_sub(map.local_us)?)
            }
        }
    }
}

/// 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebVtt {
    pub map: Option<TimestampMap>,
    /// 按出现顺序
    pub cues: Vec<Cue>,
    /// 按 WebVTT 的解析规则丢弃的块数：时间写坏的、结束不晚于开始的 cue，与既不是 cue、也不是注释或样式的块
    pub skipped: usize,
}

/// 内容不是能用的 WebVTT 字幕分片。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("字幕不是 UTF-8 文本")]
    NotUtf8,
    #[error("不是 WebVTT：第一行应以 WEBVTT 开头")]
    NotWebVtt,
    /// 只记原因，不带原文
    #[error("X-TIMESTAMP-MAP 无法解析：{0}")]
    TimestampMap(&'static str),
}

/// 解析一个 WebVTT 文件（可带 UTF-8 BOM，换行可为 CRLF、CR 或 LF）。
pub fn parse(bytes: &[u8]) -> Result<WebVtt, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::NotUtf8)?;
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = text.split('\n').peekable();
    let first = lines.next().unwrap_or_default();
    let signature = first.strip_prefix("WEBVTT").ok_or(Error::NotWebVtt)?;
    if !(signature.is_empty() || signature.starts_with([' ', '\t'])) {
        return Err(Error::NotWebVtt);
    }
    let mut map = None;
    // 头部：到第一个空行为止；含 `-->` 的行即第一个 cue 的时间行
    while let Some(line) = lines.next_if(|l| !l.is_empty() && !l.contains("-->")) {
        if let Some(value) = line.strip_prefix("X-TIMESTAMP-MAP=") {
            if map.is_some() {
                return Err(Error::TimestampMap("出现了两次"));
            }
            map = Some(timestamp_map(value)?);
        }
    }
    let mut parsed = WebVtt {
        map,
        cues: Vec::new(),
        skipped: 0,
    };
    loop {
        while lines.next_if(|l| l.is_empty()).is_some() {}
        let Some(first) = lines.next() else { break };
        let (id, timing) = if first.contains("-->") {
            (None, first)
        } else if is_keyword(first, "NOTE")
            || is_keyword(first, "STYLE")
            || is_keyword(first, "REGION")
        {
            while lines.next_if(|l| !l.is_empty()).is_some() {}
            continue;
        } else {
            match lines.next_if(|l| l.contains("-->")) {
                Some(timing) => (Some(first.to_owned()), timing),
                None => {
                    while lines.next_if(|l| !l.is_empty()).is_some() {}
                    parsed.skipped += 1;
                    continue;
                }
            }
        };
        let mut payload = Vec::new();
        while let Some(line) = lines.next_if(|l| !l.is_empty() && !l.contains("-->")) {
            payload.push(line);
        }
        match cue_timing(timing) {
            Some((start_us, end_us, settings)) if end_us > start_us => parsed.cues.push(Cue {
                start_us,
                end_us,
                id,
                settings: settings.to_owned(),
                text: payload.join("\n"),
            }),
            _ => parsed.skipped += 1,
        }
    }
    Ok(parsed)
}

/// 行是 `keyword` 本身，或其后跟空格、制表符。
fn is_keyword(line: &str, keyword: &str) -> bool {
    line.strip_prefix(keyword)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
}

/// `MPEGTS:<整数>,LOCAL:<时刻>`，两项顺序不限、都须有。
fn timestamp_map(value: &str) -> Result<TimestampMap, Error> {
    let (mut mpegts, mut local) = (None, None);
    for item in value.split(',') {
        let item = item.trim();
        if let Some(ticks) = item.strip_prefix("MPEGTS:") {
            let ticks = ticks
                .trim()
                .parse()
                .map_err(|_| Error::TimestampMap("MPEGTS 不是非负整数"))?;
            mpegts = Some(ticks);
        } else if let Some(time) = item.strip_prefix("LOCAL:") {
            let (us, rest) =
                timestamp(time.trim()).ok_or(Error::TimestampMap("LOCAL 不是 WebVTT 时刻"))?;
            if !rest.is_empty() {
                return Err(Error::TimestampMap("LOCAL 不是 WebVTT 时刻"));
            }
            local = Some(us);
        } else {
            return Err(Error::TimestampMap("有认不出的项"));
        }
    }
    match (mpegts, local) {
        (Some(mpegts), Some(local_us)) => Ok(TimestampMap { mpegts, local_us }),
        _ => Err(Error::TimestampMap("缺少 MPEGTS 或 LOCAL")),
    }
}

/// 时间行 `<开始> --> <结束>[ <设置>]`：开始、结束（微秒）与设置；写坏时为 None。
fn cue_timing(line: &str) -> Option<(i64, i64, &str)> {
    let (start, rest) = timestamp(line.trim_start())?;
    let rest = rest.trim_start_matches([' ', '\t']).strip_prefix("-->")?;
    let (end, rest) = timestamp(rest.trim_start_matches([' ', '\t']))?;
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    Some((start, end, rest.trim_matches([' ', '\t'])))
}

/// 开头的 WebVTT 时刻 `[<时>:]<分>:<秒>.<毫秒>`（时至少两位、分与秒两位且小于 60、毫秒三位），换成微秒；另返回其后的
/// 内容。
fn timestamp(text: &str) -> Option<(i64, &str)> {
    let digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
    let mut fields: Vec<&str> = Vec::with_capacity(3);
    let mut rest = text;
    loop {
        let n = digits(rest);
        if n == 0 {
            return None;
        }
        fields.push(&rest[..n]);
        rest = &rest[n..];
        match rest.strip_prefix(':') {
            Some(next) if fields.len() < 3 => rest = next,
            _ => break,
        }
    }
    let millis = rest.strip_prefix('.')?;
    if digits(millis) != 3 {
        return None;
    }
    let (millis, rest) = millis.split_at(3);
    let (hours, minutes, seconds) = match fields[..] {
        [m, s] => ("0", m, s),
        [h, m, s] if h.len() >= 2 => (h, m, s),
        _ => return None,
    };
    if minutes.len() != 2 || seconds.len() != 2 {
        return None;
    }
    let (minutes, seconds): (i64, i64) = (minutes.parse().ok()?, seconds.parse().ok()?);
    if minutes >= 60 || seconds >= 60 {
        return None;
    }
    let hours: i64 = hours.parse().ok()?;
    let millis: i64 = millis.parse().ok()?;
    let ms = hours
        .checked_mul(3_600_000)?
        .checked_add(minutes * 60_000 + seconds * 1000 + millis)?;
    Some((ms.checked_mul(1000)?, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cues_and_timestamp_map_are_parsed() {
        let vtt = "\u{FEFF}WEBVTT - title\r\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:900000\r\n\r\n\
                   NOTE 注释\r\n占两行\r\n\r\n1\r\n00:00:01.000 --> 00:00:02.500 align:start line:0\r\n\
                   <i>第一行</i>\r\n第二行\r\n\r\n01:00:00.000 --> 01:00:01.000\r\n一小时\r\n";
        let parsed = parse(vtt.as_bytes()).unwrap();
        assert_eq!(
            parsed.map,
            Some(TimestampMap {
                mpegts: 900_000,
                local_us: 0
            })
        );
        assert_eq!(parsed.skipped, 0);
        assert_eq!(parsed.cues.len(), 2);
        let first = &parsed.cues[0];
        assert_eq!((first.start_us, first.end_us), (1_000_000, 2_500_000));
        assert_eq!(first.id.as_deref(), Some("1"));
        assert_eq!(first.settings, "align:start line:0");
        assert_eq!(first.text, "<i>第一行</i>\n第二行");
        assert_eq!(parsed.cues[1].start_us, 3_600_000_000);
        assert_eq!(
            TimestampMap::media_us(parsed.map.as_ref(), 1_000_000),
            Some(11_000_000)
        );
        assert_eq!(TimestampMap::media_us(None, 1_000_000), Some(1_000_000));
    }

    /// 写坏的 cue 与认不出的块按规则丢弃并计数；没有空行隔开的下一个时间行另起一条 cue。
    #[test]
    fn malformed_blocks_are_skipped_and_counted() {
        let vtt = "WEBVTT\n00:00:01.000 --> 00:00:02.000\na\n00:00:03.000 --> 00:00:04.000\nb\n\n\
                   00:00:05,000 --> 00:00:06,000\nsrt 写法\n\n00:00:07.000 --> 00:00:07.000\n空\n\n\
                   随便写的\n一段\n\n00:61.000 --> 00:62.000\nx\n";
        let parsed = parse(vtt.as_bytes()).unwrap();
        let texts: Vec<&str> = parsed.cues.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["a", "b"]);
        assert_eq!(parsed.skipped, 4);
    }

    #[test]
    fn bad_headers_are_errors() {
        assert_eq!(parse(b"WEBVTTX\n").unwrap_err(), Error::NotWebVtt);
        assert_eq!(parse(b"\n").unwrap_err(), Error::NotWebVtt);
        assert_eq!(parse(b"WEBVTT\n\xff").unwrap_err(), Error::NotUtf8);
        assert!(matches!(
            parse(b"WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:1\n").unwrap_err(),
            Error::TimestampMap(_)
        ));
        assert!(matches!(
            parse(b"WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:x,LOCAL:00:00.000\n").unwrap_err(),
            Error::TimestampMap(_)
        ));
        assert!(parse(b"WEBVTT").unwrap().cues.is_empty());
    }

    /// MPEGTS 超出 33 位时按回绕取值；LOCAL 不为 0 时减去。
    #[test]
    fn media_time_uses_33_bit_mpegts() {
        let map = TimestampMap {
            mpegts: (1 << 33) + 90_000,
            local_us: 500_000,
        };
        assert_eq!(
            TimestampMap::media_us(Some(&map), 1_500_000),
            Some(2_000_000)
        );
        assert_eq!(TimestampMap::media_us(Some(&map), i64::MIN), None);
    }
}
