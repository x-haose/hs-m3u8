//! 分片内容校验：拦下解密错误、伪装页面、错误响应等不是媒体数据的内容与没写完的内容，不写盘。

use std::collections::HashMap;
use std::io::Cursor;

use hs_m3u8_remux::fmp4::{self, BadMoof, BoxHeader, Problem};
use hs_m3u8_remux::standalone;
use url::Url;

use crate::crypto::DecryptError;
use crate::{Error, Integrity};

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
    /// 没写完：fMP4 的顶层 box 超出了数据（见 [`Fmp4`]），或密文不足整块而整块部分认得出（见 [`decrypt_failure`]）
    Incomplete,
    Invalid(Integrity),
}

impl ContentError {
    /// 取自 `url` 的数据的失败（[`Error::Incomplete`] 或 [`Error::Integrity`]）。
    pub(crate) fn into_error(self, url: &Url) -> Error {
        let url = Box::new(url.clone());
        match self {
            ContentError::Incomplete => Error::Incomplete { url },
            ContentError::Invalid(kind) => Error::Integrity { url, kind },
        }
    }
}

/// 解密失败的分片：密文不足整块、或填充不合法而解出的明文是只差没写完的 fMP4（截在整块处），或密文不足整块而整块部分
/// 解出的明文认得出是 TS 或打包音频时，为没写完；其余密文不足整块的为长度不对（如源站返回的是错误页面），填充不合法
/// 的多为 key 不对。`fmp4` 为分片有 init 段。
pub(crate) fn decrypt_failure(error: DecryptError, fmp4: bool) -> ContentError {
    let (plain, otherwise) = match error {
        DecryptError::Padding { plain } => (plain, Integrity::Padding),
        DecryptError::Unaligned { len, whole } => (whole, Integrity::CipherLength(len)),
    };
    let incomplete = match (fmp4, &otherwise) {
        (true, _) => matches!(Fmp4::of(&plain), Err(ContentError::Incomplete)),
        (false, Integrity::CipherLength(_)) => check_standalone_segment(&plain).is_ok(),
        (false, _) => false,
    };
    if incomplete {
        ContentError::Incomplete
    } else {
        ContentError::Invalid(otherwise)
    }
}

/// fMP4 分片或 init 段的顶层结构（见 [`fmp4::boxes`]）：首尾相接、恰好铺满数据。最后一个 box 的长度写为 0（到数据末尾）
/// 时，合并前要改写成实际长度（见 [`Fmp4::seal`]）；它是 mdat 时靠长度看不出有没有写完，要按前面 moof 里 trun 声明的
/// 样本数据核对（见 [`Fmp4::check_samples`]）。
pub(crate) struct Fmp4 {
    boxes: Vec<BoxHeader>,
}

impl Fmp4 {
    pub(crate) fn of(data: &[u8]) -> Result<Fmp4, ContentError> {
        let boxes = fmp4::boxes(&mut Cursor::new(data))
            .expect("读内存不会出错")
            .map_err(|problem| match problem {
                Problem::NotFmp4 => ContentError::Invalid(Integrity::NotFmp4(head(data))),
                Problem::Incomplete => ContentError::Incomplete,
                Problem::TrailingBytes { at, len } => {
                    ContentError::Invalid(Integrity::Fmp4TrailingBytes { at, len })
                }
                Problem::BoxTooSmall { at, size } => {
                    ContentError::Invalid(Integrity::Fmp4BoxTooSmall { at, size })
                }
            })?;
        Ok(Fmp4 { boxes })
    }

    /// 最后一个 box 是长度写 0 的 mdat，要按 trun 核对有没有写完。
    pub(crate) fn open_mdat(&self) -> bool {
        self.boxes
            .last()
            .is_some_and(|b| b.to_end && &b.kind == b"mdat")
    }

    /// 各 trun 声明的样本数据都在 `data` 里，否则为没写完（见 [`fmp4::sample_data_end`]）；`trex` 为分片所用的 init
    /// 段里各轨的默认样本长度。
    pub(crate) fn check_samples(
        &self,
        data: &[u8],
        trex: &HashMap<u32, u32>,
    ) -> Result<(), ContentError> {
        match fmp4::sample_data_end(data, &self.boxes, trex) {
            Ok(end) if end > data.len() as u64 => Err(ContentError::Incomplete),
            Ok(_) => Ok(()),
            Err(BadMoof { at }) => Err(ContentError::Invalid(Integrity::Fmp4BadMoof { at })),
        }
    }

    /// 把长度写 0 的最后一个 box 的长度字段改写成实际长度：合并时一条轨的 init 段与分片接成一个输入读，到末尾就成了到
    /// 所有分片的末尾。64 位的长度要多 8 字节的头，原地放不下，超过 4 GiB 时报错。
    pub(crate) fn seal(&self, data: &mut [u8]) -> Result<(), ContentError> {
        let Some(open) = self.boxes.last().filter(|b| b.to_end) else {
            return Ok(());
        };
        let (at, size) = (open.at, open.size);
        let actual = u32::try_from(size)
            .map_err(|_| ContentError::Invalid(Integrity::Fmp4OpenBoxTooLarge { at, size }))?;
        let at = at as usize;
        data[at..at + 4].copy_from_slice(&actual.to_be_bytes());
        Ok(())
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

    fn boxed(size: u32, kind: &[u8; 4], payload: usize) -> Vec<u8> {
        [&size.to_be_bytes()[..], kind, &vec![0; payload]].concat()
    }

    #[test]
    fn failed_decryption_is_incomplete_only_when_the_plaintext_is_recognized() {
        let unaligned = |whole: Vec<u8>| DecryptError::Unaligned { len: 1000, whole };
        let unfinished = [boxed(16, b"moof", 8), boxed(40, b"mdat", 0)].concat();
        let page = b"<html>not found</html>".to_vec();
        assert_eq!(
            decrypt_failure(unaligned(ts_packets(3)), false),
            ContentError::Incomplete
        );
        assert_eq!(
            decrypt_failure(unaligned(unfinished.clone()), true),
            ContentError::Incomplete
        );
        // 错误页面、不足一块
        let invalid = ContentError::Invalid(Integrity::CipherLength(1000));
        assert_eq!(decrypt_failure(unaligned(page.clone()), false), invalid);
        assert_eq!(decrypt_failure(unaligned(page.clone()), true), invalid);
        assert_eq!(decrypt_failure(unaligned(Vec::new()), false), invalid);
        // 填充不合法：截在整块处的 fMP4 是没写完；key 不对解出的乱码与 TS 都按填充不合法
        let padding = |plain: Vec<u8>| DecryptError::Padding { plain };
        assert_eq!(
            decrypt_failure(padding(unfinished), true),
            ContentError::Incomplete
        );
        let invalid = ContentError::Invalid(Integrity::Padding);
        assert_eq!(decrypt_failure(padding(page), true), invalid);
        assert_eq!(decrypt_failure(padding(ts_packets(3)), false), invalid);
    }

    #[test]
    fn fmp4_is_tiled_by_complete_top_level_boxes() {
        let check = |mut data: Vec<u8>| {
            let fmp4 = Fmp4::of(&data)?;
            fmp4.seal(&mut data).map(|()| data)
        };
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
        // 没写完：mdat 截断、64 位长度不全
        let incomplete = Err(ContentError::Incomplete);
        assert_eq!(check(segment[..segment.len() - 2].to_vec()), incomplete);
        assert_eq!(
            check([segment.clone(), boxed(1, b"mdat", 4)].concat()),
            incomplete
        );
        // 末尾多出凑不成 box 头的字节；长度字段小于头长
        assert_eq!(
            check([segment.clone(), b"\r\n".to_vec()].concat()),
            Err(ContentError::Invalid(Integrity::Fmp4TrailingBytes {
                at: 28,
                len: 2
            }))
        );
        assert_eq!(
            check(boxed(7, b"moof", 0)),
            Err(ContentError::Invalid(Integrity::Fmp4BoxTooSmall {
                at: 0,
                size: 7
            }))
        );
    }
}
