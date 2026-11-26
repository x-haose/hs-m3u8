//! 任意字节当作 WebVTT 解析：不 panic；解析出的 cue 写成 WebVTT 再解析，得到同样的 cue；写成 SRT 每条 cue 一块、块里没有
//! 空行；切成 mov_text 的段互不重叠。
#![no_main]

use hs_m3u8_subtitle::{TimestampMap, dedup, mov_text_sample, parse, slices, srt, webvtt};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(parsed) = parse(data) else {
        return;
    };
    for cue in &parsed.cues {
        TimestampMap::media_us(parsed.map.as_ref(), cue.start_us);
    }
    let mut cues = parsed.cues;
    dedup(&mut cues);
    let reparsed = parse(webvtt(&cues).as_bytes()).expect("写出的是 WebVTT");
    assert_eq!(reparsed.cues, cues);
    assert_eq!(reparsed.skipped, 0);
    // SRT 里空行是 cue 的分隔：每条 cue 写出的块（编号、时间行、正文）里不能有空行
    let written = srt(&cues);
    assert_eq!(
        written.split("\n\n").filter(|b| !b.is_empty()).count(),
        cues.len()
    );
    let mut end = i64::MIN;
    for slice in slices(&cues) {
        assert!(slice.start_us >= end && slice.end_us > slice.start_us);
        end = slice.end_us;
        let _ = mov_text_sample(&slice.text);
    }
});
