//! 分片内容校验：拦下解密错误、伪装页面、错误响应等不是媒体数据的内容与没写完的内容，不写盘。

use std::io::Cursor;

use hs_m3u8_remux::standalone;
use url::Url;

use crate::crypto::DecryptError;
use crate::{Error, HttpError, Integrity};

/// 没有 init 段的分片须能识别出格式（见 [`standalone::read_head`]）。
pub(crate) fn check_standalone_segment(data: &[u8]) -> Result<(), Integrity> {
    match standalone::read_head(&mut Cursor::new(data)).expect("读内存不会出错") {
        Some(_) => Ok(()),
        None => Err(Integrity::UnrecognizedSegment(head(data))),
    }
}

/// 分片内容校验失败的原因。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ContentError {
    /// 没写完：fMP4 的顶层 box 超出了数据或末尾的字节凑不成 box 头，或密文不足整块而整块部分认得出（见
    /// [`decrypt_failure`]）
    Incomplete,
    Invalid(Integrity),
}

impl ContentError {
    /// 取自 `url` 的数据的失败：没写完的是可重试的请求失败（[`HttpError::Incomplete`]），其余为校验失败。
    pub(crate) fn at(self, url: &Url) -> Error {
        let url = Box::new(url.clone());
        match self {
            ContentError::Incomplete => Error::Http {
                url,
                kind: HttpError::Incomplete,
                retry_after: None,
            },
            ContentError::Invalid(kind) => Error::Integrity { url, kind },
        }
    }
}

/// 解密失败的分片：密文不足整块、而整块部分解出的明文认得出是这类分片（TS、打包音频，或以认得的 box 开头、只差没写完
/// 的 fMP4）时为没写完；认不出时为密文长度不对（如源站返回的是错误页面）。`fmp4` 为分片有 init 段。
pub(crate) fn decrypt_failure(error: DecryptError, fmp4: bool) -> ContentError {
    match error {
        DecryptError::Padding => ContentError::Invalid(Integrity::Padding),
        DecryptError::Unaligned { len, mut whole } => {
            let recognized = if fmp4 {
                !matches!(check_fmp4(&mut whole), Err(ContentError::Invalid(_)))
            } else {
                check_standalone_segment(&whole).is_ok()
            };
            if recognized {
                ContentError::Incomplete
            } else {
                ContentError::Invalid(Integrity::CipherLength(len))
            }
        }
    }
}

/// fMP4 分片或 init 段：以认得的 ISO BMFF box 开头，顶层 box 首尾相接、恰好铺满数据（没写完的分片合并时，mov 解复用器
/// 按 mdat 声明的长度读进下一个分片的字节）。最后一个 box 的长度写为 0（到数据末尾）时改写成实际长度：合并时一条轨的
/// init 段与分片接成一个输入读，到末尾就成了到所有分片的末尾。
pub(crate) fn check_fmp4(data: &mut [u8]) -> Result<(), ContentError> {
    const BOXES: [&[u8; 4]; 9] = [
        b"ftyp", b"styp", b"moof", b"moov", b"sidx", b"emsg", b"prft", b"free", b"skip",
    ];
    if !(data.len() >= 8 && BOXES.iter().any(|b| &data[4..8] == *b)) {
        return Err(ContentError::Invalid(Integrity::NotFmp4(head(data))));
    }
    let len = data.len() as u64;
    let mut at = 0;
    while at < len {
        let rest = &data[at as usize..];
        if rest.len() < 8 {
            return Err(ContentError::Incomplete);
        }
        let field: [u8; 4] = rest[..4].try_into().expect("切片长 4");
        let (header, size) = match u32::from_be_bytes(field) {
            0 => (8, len - at),
            // 长度为 1：真实长度是头之后的 64 位整数
            1 => {
                let large = rest.get(8..16).ok_or(ContentError::Incomplete)?;
                (16, u64::from_be_bytes(large.try_into().expect("切片长 8")))
            }
            size => (8, u64::from(size)),
        };
        if size < header {
            return Err(ContentError::Invalid(Integrity::Fmp4BoxSize { at, size }));
        }
        if size > len - at {
            return Err(ContentError::Incomplete);
        }
        if field == [0; 4] {
            // 64 位的长度要多 8 字节的头，原地放不下
            let actual = u32::try_from(size)
                .map_err(|_| ContentError::Invalid(Integrity::Fmp4BoxTooLarge { at, size }))?;
            let at = at as usize;
            data[at..at + 4].copy_from_slice(&actual.to_be_bytes());
        }
        at += size;
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
    fn unaligned_ciphertext_is_incomplete_only_when_the_whole_blocks_are_recognized() {
        let unaligned = |whole: Vec<u8>| DecryptError::Unaligned { len: 1000, whole };
        let moof = [
            &16u32.to_be_bytes()[..],
            b"moof",
            &[0; 8],
            &40u32.to_be_bytes(),
            b"mdat",
        ]
        .concat();
        assert_eq!(
            decrypt_failure(unaligned(ts_packets(3)), false),
            ContentError::Incomplete
        );
        assert_eq!(
            decrypt_failure(unaligned(moof), true),
            ContentError::Incomplete
        );
        // 错误页面、不足一块
        let invalid = ContentError::Invalid(Integrity::CipherLength(1000));
        assert_eq!(
            decrypt_failure(unaligned(b"<html>not found</html>".to_vec()), false),
            invalid
        );
        assert_eq!(
            decrypt_failure(unaligned(b"<html>not found</html>".to_vec()), true),
            invalid
        );
        assert_eq!(decrypt_failure(unaligned(Vec::new()), false), invalid);
        assert_eq!(
            decrypt_failure(DecryptError::Padding, false),
            ContentError::Invalid(Integrity::Padding)
        );
    }

    #[test]
    fn fmp4_is_tiled_by_complete_top_level_boxes() {
        let boxed = |size: u32, kind: &[u8; 4], payload: usize| {
            [&size.to_be_bytes()[..], kind, &vec![0; payload]].concat()
        };
        let check = |mut data: Vec<u8>| check_fmp4(&mut data).map(|()| data);
        let segment = [boxed(16, b"moof", 8), boxed(12, b"mdat", 4)].concat();
        assert_eq!(check(segment.clone()), Ok(segment.clone()));
        assert!(check(boxed(24, b"ftyp", 16)).is_ok());
        // 长度为 1 时后跟 64 位长度
        let large = [
            &1u32.to_be_bytes()[..],
            b"styp",
            &20u64.to_be_bytes(),
            &[0; 4],
        ]
        .concat();
        assert!(check(large).is_ok());
        // 长度为 0 时到末尾，改写成实际长度
        assert_eq!(
            check([boxed(8, b"styp", 0), boxed(0, b"mdat", 30)].concat()),
            Ok([boxed(8, b"styp", 0), boxed(38, b"mdat", 30)].concat())
        );
        // 第一个 box 认不出
        assert_eq!(
            check(boxed(16, b"mdat", 8)),
            Err(ContentError::Invalid(Integrity::NotFmp4(
                "00 00 00 10 6d 64 61 74 00 00 00 00 00 00 00 00".into()
            )))
        );
        assert!(check(boxed(16, b"html", 8)).is_err());
        assert!(check(b"moof".to_vec()).is_err());
        // 没写完：mdat 截断、末尾多出凑不成 box 头的字节、64 位长度不全
        let incomplete = Err(ContentError::Incomplete);
        assert_eq!(check(segment[..segment.len() - 2].to_vec()), incomplete);
        assert_eq!(check([segment.clone(), vec![0; 3]].concat()), incomplete);
        assert_eq!(
            check([segment.clone(), boxed(1, b"mdat", 4)].concat()),
            incomplete
        );
        // 长度字段小于头长
        assert_eq!(
            check(boxed(7, b"moof", 0)),
            Err(ContentError::Invalid(Integrity::Fmp4BoxSize {
                at: 0,
                size: 7
            }))
        );
    }
}
