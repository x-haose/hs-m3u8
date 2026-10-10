//! 分片内容校验：拦下解密错误、伪装页面、错误响应等不是媒体数据的内容，不写盘；识别没有 init 段的分片的格式。

use crate::Integrity;

/// 没有 init 段的分片的格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standalone {
    Ts,
    Aac,
    Mp3,
    Ac3,
    Eac3,
}

impl Standalone {
    /// 全部格式；增加格式时同时加在这里。
    pub(crate) const ALL: [Standalone; 5] = [
        Standalone::Ts,
        Standalone::Aac,
        Standalone::Mp3,
        Standalone::Ac3,
        Standalone::Eac3,
    ];
}

/// 识别没有 init 段的分片：MPEG-TS（偏移 0 处、长度够时偏移 188 处为同步字节 0x47），或 RFC 8216 3.4 的打包
/// 音频（AAC 的 ADTS、MP3、AC-3、E-AC-3），音频前面可有 ID3 标签。
pub(crate) fn standalone_format(data: &[u8]) -> Option<Standalone> {
    if data.first() == Some(&0x47) {
        return (data.len() < 376 || data[188] == 0x47).then_some(Standalone::Ts);
    }
    match skip_id3(data)? {
        // ADTS：12 位同步字，layer 为 0
        [0xFF, b, ..] if b & 0xF6 == 0xF0 => Some(Standalone::Aac),
        // MPEG 音频帧：11 位同步字，layer 不为 0
        [0xFF, b, ..] if b & 0xE0 == 0xE0 && b & 0x06 != 0 => Some(Standalone::Mp3),
        // AC-3 与 E-AC-3 同步字相同，按第 6 字节高 5 位的 bsid 区分（≤ 10 为 AC-3，11..=16 为 E-AC-3）
        [0x0B, 0x77, _, _, _, b, ..] => match b >> 3 {
            0..=10 => Some(Standalone::Ac3),
            11..=16 => Some(Standalone::Eac3),
            _ => None,
        },
        _ => None,
    }
}

/// 跳过开头的 ID3v2 标签（可有多个）；标签不完整或长度字段不合法时为 None。
fn skip_id3(mut data: &[u8]) -> Option<&[u8]> {
    while data.starts_with(b"ID3") {
        let header = data.get(..10)?;
        // 标签长度是 4 个各 7 位的字节（最高位为 0），不含 10 字节的头；有尾部时另加 10 字节
        let size = header[6..10].iter().try_fold(0usize, |size, &b| {
            (b < 0x80).then_some(size << 7 | usize::from(b))
        })?;
        let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };
        data = data.get(10 + size + footer..)?;
    }
    Some(data)
}

/// 没有 init 段的分片须能识别出格式（见 [`standalone_format`]）。
pub(crate) fn check_standalone_segment(data: &[u8]) -> Result<(), Integrity> {
    match standalone_format(data) {
        Some(_) => Ok(()),
        None => Err(Integrity::UnrecognizedSegment(head(data))),
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

    /// ID3v2.4 标签：`body` 为帧数据，`footer` 时带 10 字节尾部。
    fn id3(body: &[u8], footer: bool) -> Vec<u8> {
        let size = body.len();
        let syncsafe = [size >> 21, size >> 14, size >> 7, size].map(|b| (b & 0x7F) as u8);
        let flags = if footer { 0x10 } else { 0 };
        let mut tag = [b"ID3\x04\x00".as_slice(), &[flags], &syncsafe, body].concat();
        if footer {
            tag.extend_from_slice(b"3DI\x04\x00\x00\x00\x00\x00\x00");
        }
        tag
    }

    #[test]
    fn standalone_formats_are_told_apart() {
        use Standalone::*;
        let adts = [0xFF, 0xF1, 0x50, 0x80];
        let mp3 = [0xFF, 0xFB, 0x90, 0x64];
        let ac3 = [0x0B, 0x77, 0, 0, 0, 8 << 3, 0];
        let eac3 = [0x0B, 0x77, 0, 0, 0, 16 << 3, 0];
        assert_eq!(standalone_format(&ts_packets(3)), Some(Ts));
        // 不足两个包时只看首字节
        assert_eq!(standalone_format(&ts_packets(1)), Some(Ts));
        for (data, format) in [(&adts[..], Aac), (&mp3, Mp3), (&ac3, Ac3), (&eac3, Eac3)] {
            assert_eq!(standalone_format(data), Some(format));
            // 打包音频前的 ID3 标签，可有多个、可带尾部
            let tagged = [id3(b"PRIV", false), id3(&[0; 300], true), data.to_vec()].concat();
            assert_eq!(standalone_format(&tagged), Some(format));
        }
        // ID3 之后不是音频、标签不完整、长度字段最高位为 1
        assert_eq!(
            standalone_format(&[id3(b"PRIV", false), b"<html>".to_vec()].concat()),
            None
        );
        assert_eq!(standalone_format(&id3(&[0; 20], false)[..15]), None);
        assert_eq!(
            standalone_format(b"ID3\x04\x00\x00\x00\x00\x00\x80\xFF\xF1"),
            None
        );
        // bsid 超出 AC-3 与 E-AC-3 的范围
        assert_eq!(standalone_format(&[0x0B, 0x77, 0, 0, 0, 20 << 3]), None);
    }

    #[test]
    fn standalone_segments_must_be_recognized() {
        assert_eq!(check_standalone_segment(&ts_packets(3)), Ok(()));
        assert_eq!(check_standalone_segment(&[0xFF, 0xF1, 0x50]), Ok(()));

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
