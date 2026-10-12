//! 分片内容校验：拦下解密错误、伪装页面、错误响应等不是媒体数据的内容与没写完的内容，不写盘。

use std::collections::HashMap;
use std::io::Cursor;

use hs_m3u8_remux::fmp4::{self, Problem};
use hs_m3u8_remux::standalone;
use url::Url;

use crate::crypto::DecryptError;
use crate::{Error, Integrity, Unsupported};

/// 分片的格式。
pub(crate) enum Container {
    /// fMP4：`trex` 为所用 init 段里各轨的默认样本长度（见 [`fmp4::trex_sample_sizes`]）
    Fmp4 { trex: HashMap<u32, u32> },
    /// 没有 init 段：TS 或打包音频
    Standalone,
}

/// 分片或 init 段内容校验失败的原因。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ContentError {
    /// 没写完（见 [`Error::Incomplete`]）
    Incomplete,
    Invalid(Integrity),
    /// fMP4 从第 `at` 字节起的 moof 用了绝对位置（见 [`Unsupported::Fmp4BaseDataOffset`]）
    BaseDataOffset {
        at: u64,
    },
}

impl ContentError {
    /// 取自 `url` 的数据的失败。
    pub(crate) fn into_error(self, url: &Url) -> Error {
        let url = Box::new(url.clone());
        match self {
            ContentError::Incomplete => Error::Incomplete { url },
            ContentError::Invalid(kind) => Error::Integrity { url, kind },
            ContentError::BaseDataOffset { at } => {
                Error::Unsupported(Unsupported::Fmp4BaseDataOffset { url, at })
            }
        }
    }

    /// fMP4 `data` 的结构问题。
    fn of_fmp4(problem: Problem, data: &[u8]) -> ContentError {
        let invalid = ContentError::Invalid;
        match problem {
            Problem::Incomplete => ContentError::Incomplete,
            Problem::BaseDataOffset { at } => ContentError::BaseDataOffset { at },
            Problem::NotFmp4 => invalid(Integrity::NotFmp4(head(data))),
            Problem::TrailingBytes { at, len } => invalid(Integrity::Fmp4TrailingBytes { at, len }),
            Problem::BoxTooSmall { at, size } => invalid(Integrity::Fmp4BoxTooSmall { at, size }),
            Problem::Malformed { at } => invalid(Integrity::Fmp4Malformed { at }),
            Problem::OpenBoxTooLarge { at, size } => {
                invalid(Integrity::Fmp4OpenBoxTooLarge { at, size })
            }
        }
    }
}

/// 校验 init 段 `data`（按它自己的 trex，见 [`fmp4::check`]），并把长度写 0 的最后一个 box 改写成实际长度（见
/// [`fmp4::seal`]）。
pub(crate) fn check_init(data: &mut [u8]) -> Result<(), ContentError> {
    let trex = fmp4::trex_sample_sizes(data).map_err(|p| ContentError::of_fmp4(p, data))?;
    fmp4::seal(data, &trex).map_err(|p| ContentError::of_fmp4(p, data))
}

/// 校验分片 `data`：fMP4 见 [`fmp4::check`]，并把长度写 0 的最后一个 box 改写成实际长度（见 [`fmp4::seal`]）；没有
/// init 段的须认得出格式（见 [`standalone::read_head`]）。
pub(crate) fn check_segment(data: &mut [u8], container: &Container) -> Result<(), ContentError> {
    match container {
        Container::Fmp4 { trex } => {
            fmp4::seal(data, trex).map_err(|p| ContentError::of_fmp4(p, data))
        }
        Container::Standalone if recognized(data) => Ok(()),
        Container::Standalone => Err(ContentError::Invalid(Integrity::UnrecognizedSegment(head(
            data,
        )))),
    }
}

/// 没有 init 段的分片 `data` 认得出格式（见 [`standalone::read_head`]）。
fn recognized(data: &[u8]) -> bool {
    standalone::read_head(&mut Cursor::new(data))
        .expect("读内存不会出错")
        .is_some()
}

/// 解密失败的分片：密文不足整块、或填充不合法而解出的明文是只差没写完的 fMP4（截在整块处，见 [`fmp4::check`]），
/// 或密文不足整块而整块部分解出的明文认得出是 TS 或打包音频时，为没写完；其余密文不足整块的为长度不对（如源站返回
/// 的是错误页面），填充不合法的多为 key 不对。
pub(crate) fn decrypt_failure(error: DecryptError, container: &Container) -> ContentError {
    let (plain, otherwise) = match error {
        DecryptError::Padding { plain } => (plain, Integrity::Padding),
        DecryptError::Unaligned { len, whole } => (whole, Integrity::CipherLength(len)),
    };
    let incomplete = match (container, &otherwise) {
        (Container::Fmp4 { trex }, _) => fmp4::check(&plain, trex) == Err(Problem::Incomplete),
        (Container::Standalone, Integrity::CipherLength(_)) => recognized(&plain),
        (Container::Standalone, _) => false,
    };
    if incomplete {
        ContentError::Incomplete
    } else {
        ContentError::Invalid(otherwise)
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
        let check = |mut data: Vec<u8>| check_segment(&mut data, &Container::Standalone);
        assert_eq!(check(ts_packets(3)), Ok(()));
        assert_eq!(
            check(vec![0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC]),
            Ok(())
        );

        let mut shifted = ts_packets(3);
        shifted[188] = 0;
        assert!(check(shifted).is_err());
        assert_eq!(
            check(b"<html>".to_vec()),
            Err(ContentError::Invalid(Integrity::UnrecognizedSegment(
                "3c 68 74 6d 6c 3e".into()
            )))
        );
        assert!(check(Vec::new()).is_err());
    }

    fn boxed(size: u32, kind: &[u8; 4], payload: usize) -> Vec<u8> {
        [&size.to_be_bytes()[..], kind, &vec![0; payload]].concat()
    }

    #[test]
    fn failed_decryption_is_incomplete_only_when_the_plaintext_is_recognized() {
        let fmp4 = Container::Fmp4 {
            trex: HashMap::new(),
        };
        let standalone = Container::Standalone;
        let unaligned = |whole: Vec<u8>| DecryptError::Unaligned { len: 1000, whole };
        let unfinished = [boxed(16, b"moof", 8), boxed(40, b"mdat", 0)].concat();
        let page = b"<html>not found</html>".to_vec();
        assert_eq!(
            decrypt_failure(unaligned(ts_packets(3)), &standalone),
            ContentError::Incomplete
        );
        assert_eq!(
            decrypt_failure(unaligned(unfinished.clone()), &fmp4),
            ContentError::Incomplete
        );
        // 错误页面、不足一块
        let invalid = ContentError::Invalid(Integrity::CipherLength(1000));
        assert_eq!(
            decrypt_failure(unaligned(page.clone()), &standalone),
            invalid
        );
        assert_eq!(decrypt_failure(unaligned(page.clone()), &fmp4), invalid);
        assert_eq!(decrypt_failure(unaligned(Vec::new()), &standalone), invalid);
        // 填充不合法：截在整块处的 fMP4 是没写完；key 不对解出的乱码与 TS 都按填充不合法
        let padding = |plain: Vec<u8>| DecryptError::Padding { plain };
        assert_eq!(
            decrypt_failure(padding(unfinished), &fmp4),
            ContentError::Incomplete
        );
        let invalid = ContentError::Invalid(Integrity::Padding);
        assert_eq!(decrypt_failure(padding(page), &fmp4), invalid);
        assert_eq!(
            decrypt_failure(padding(ts_packets(3)), &standalone),
            invalid
        );
    }
}
