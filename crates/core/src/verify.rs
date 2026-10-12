//! 分片内容校验：拦下解密错误、伪装页面、错误响应等不是媒体数据的内容，不写盘。

use std::io::Cursor;

use hs_m3u8_remux::standalone;

use crate::Integrity;

/// 没有 init 段的分片须能识别出格式（见 [`standalone::read_head`]）。
pub(crate) fn check_standalone_segment(data: &[u8]) -> Result<(), Integrity> {
    match standalone::read_head(&mut Cursor::new(data)).expect("读内存不会出错") {
        Some(_) => Ok(()),
        None => Err(Integrity::UnrecognizedSegment(head(data))),
    }
}

/// fMP4 分片或 init 段：以认得的 ISO BMFF box 开头，顶层 box 首尾相接、恰好铺满数据。只看开头时，截断的分片（源站给出
/// 没写完的文件）能通过，合并时 mov 解复用器按 mdat 声明的长度越过它，读进下一个分片的字节。
pub(crate) fn check_fmp4(data: &[u8]) -> Result<(), Integrity> {
    const BOXES: [&[u8; 4]; 9] = [
        b"ftyp", b"styp", b"moof", b"moov", b"sidx", b"emsg", b"prft", b"free", b"skip",
    ];
    if !(data.len() >= 8 && BOXES.iter().any(|b| &data[4..8] == *b)) {
        return Err(Integrity::NotFmp4(head(data)));
    }
    let len = data.len() as u64;
    let mut at = 0;
    while at < len {
        let rest = &data[at as usize..];
        let field = |range: std::ops::Range<usize>| rest.get(range);
        let (header, size) =
            match field(0..4).map(|b| u32::from_be_bytes(b.try_into().expect("切片长 4"))) {
                None => (8, 0),
                // 长度为 0：到数据末尾
                Some(0) => (8, len - at),
                // 长度为 1：真实长度是头之后的 64 位整数
                Some(1) => match field(8..16) {
                    Some(b) => (16, u64::from_be_bytes(b.try_into().expect("切片长 8"))),
                    None => (16, 0),
                },
                Some(size) => (8, u64::from(size)),
            };
        let end = at.saturating_add(size.max(header));
        if size < header || end > len {
            return Err(Integrity::Fmp4Boxes { at, end, len });
        }
        at = end;
    }
    Ok(())
}

/// 开头最多 16 字节的十六进制，用于错误信息。
fn head(data: &[u8]) -> String {
    data.iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts_packets(count: usize) -> Vec<u8> {
        let mut data = vec![0u8; 188 * count];
        for packet in data.chunks_mut(188) {
            packet[0] = 0x47;
        }
        data
    }

    #[test]
    fn standalone_segments_must_be_recognized() {
        assert_eq!(check_standalone_segment(&ts_packets(3)), Ok(()));
        assert_eq!(
            check_standalone_segment(&[0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC]),
            Ok(())
        );

        let mut shifted = ts_packets(3);
        shifted[188] = 0;
        assert!(check_standalone_segment(&shifted).is_err());
        assert_eq!(
            check_standalone_segment(b"<html>"),
            Err(Integrity::UnrecognizedSegment("3c 68 74 6d 6c 3e".into()))
        );
        assert!(check_standalone_segment(&[]).is_err());
    }

    #[test]
    fn fmp4_is_tiled_by_complete_top_level_boxes() {
        let boxed = |size: u32, kind: &[u8; 4], payload: usize| {
            [&size.to_be_bytes()[..], kind, &vec![0; payload]].concat()
        };
        let segment = [boxed(16, b"moof", 8), boxed(12, b"mdat", 4)].concat();
        assert_eq!(check_fmp4(&segment), Ok(()));
        assert_eq!(check_fmp4(&boxed(24, b"ftyp", 16)), Ok(()));
        // 长度为 1 时后跟 64 位长度；为 0 时到末尾
        let large = [
            &1u32.to_be_bytes()[..],
            b"styp",
            &20u64.to_be_bytes(),
            &[0; 4],
        ]
        .concat();
        assert_eq!(check_fmp4(&large), Ok(()));
        let to_end = [boxed(8, b"styp", 0), boxed(0, b"mdat", 30)].concat();
        assert_eq!(check_fmp4(&to_end), Ok(()));
        // 第一个 box 认不出
        assert_eq!(
            check_fmp4(&boxed(16, b"mdat", 8)),
            Err(Integrity::NotFmp4(
                "00 00 00 10 6d 64 61 74 00 00 00 00 00 00 00 00".into()
            ))
        );
        assert!(check_fmp4(&boxed(16, b"html", 8)).is_err());
        assert!(check_fmp4(b"moof").is_err());
        // mdat 截断、末尾多出凑不成 box 头的字节、长度字段小于头长
        let truncated = &segment[..segment.len() - 2];
        assert_eq!(
            check_fmp4(truncated),
            Err(Integrity::Fmp4Boxes {
                at: 16,
                end: 28,
                len: 26
            })
        );
        let trailing = [segment.clone(), vec![0; 3]].concat();
        assert_eq!(
            check_fmp4(&trailing),
            Err(Integrity::Fmp4Boxes {
                at: 28,
                end: 36,
                len: 31
            })
        );
        assert_eq!(
            check_fmp4(&boxed(7, b"moof", 0)),
            Err(Integrity::Fmp4Boxes {
                at: 0,
                end: 8,
                len: 8
            })
        );
    }
}
