//! 分片内容校验：拦下解密错误、伪装页面、错误响应等不是媒体数据的内容，不写盘。

use crate::Integrity;

/// 没有 init 段的分片：MPEG-TS（偏移 0 处、且长度够时偏移 188 处为同步字节 0x47）、ADTS 音频，
/// 或以 ID3 标签开头的打包音频。
pub(crate) fn check_standalone_segment(data: &[u8]) -> Result<(), Integrity> {
    let ts = data.first() == Some(&0x47) && (data.len() < 376 || data[188] == 0x47);
    let adts = data.len() >= 2 && data[0] == 0xFF && data[1] & 0xF0 == 0xF0;
    let id3 = data.starts_with(b"ID3");
    if ts || adts || id3 {
        Ok(())
    } else {
        Err(Integrity::UnrecognizedSegment(head(data)))
    }
}

/// fMP4 分片或 init 段：以合法的 ISO BMFF box 开头。
pub(crate) fn check_fmp4(data: &[u8]) -> Result<(), Integrity> {
    const BOXES: [&[u8; 4]; 9] = [
        b"ftyp", b"styp", b"moof", b"moov", b"sidx", b"emsg", b"prft", b"free", b"skip",
    ];
    let ok = data.len() >= 8 && BOXES.iter().any(|b| &data[4..8] == *b) && {
        let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        size == 1 || size >= 8
    };
    if ok {
        Ok(())
    } else {
        Err(Integrity::NotFmp4(head(data)))
    }
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
    fn standalone_segments_are_ts_adts_or_id3() {
        assert_eq!(check_standalone_segment(&ts_packets(3)), Ok(()));
        // 不足两个包时只看首字节
        assert_eq!(check_standalone_segment(&ts_packets(1)), Ok(()));
        assert_eq!(check_standalone_segment(&[0xFF, 0xF1, 0x50]), Ok(()));
        assert_eq!(check_standalone_segment(b"ID3\x04\x00"), Ok(()));

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
    fn fmp4_starts_with_a_known_box() {
        let boxed = |size: u32, kind: &[u8; 4]| [&size.to_be_bytes()[..], kind].concat();
        assert_eq!(check_fmp4(&boxed(16, b"moof")), Ok(()));
        assert_eq!(check_fmp4(&boxed(24, b"ftyp")), Ok(()));
        // size 为 1 表示后跟 64 位长度
        assert_eq!(
            check_fmp4(&boxed(1, b"mdat")),
            Err(Integrity::NotFmp4("00 00 00 01 6d 64 61 74".into()))
        );
        assert_eq!(check_fmp4(&boxed(1, b"styp")), Ok(()));
        assert!(check_fmp4(&boxed(7, b"moof")).is_err());
        assert!(check_fmp4(&boxed(16, b"html")).is_err());
        assert!(check_fmp4(b"moof").is_err());
    }
}
