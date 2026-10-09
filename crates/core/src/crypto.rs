//! AES-128-CBC 解密与分片内容校验。

use aes::Aes128;
use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockModeDecrypt, KeyIvInit};

use crate::Integrity;

/// 原地解密并去掉 PKCS#7 填充。填充不合法几乎只会因为 key 或 IV 不对。
pub(crate) fn decrypt(
    mut data: Vec<u8>,
    key: &[u8; 16],
    iv: &[u8; 16],
) -> Result<Vec<u8>, Integrity> {
    if !data.len().is_multiple_of(16) {
        return Err(Integrity::CipherLength(data.len()));
    }
    let plain_len = cbc::Decryptor::<Aes128>::new(key.into(), iv.into())
        .decrypt_padded::<Pkcs7>(&mut data)
        .map_err(|_| Integrity::Padding)?
        .len();
    data.truncate(plain_len);
    Ok(data)
}

/// 没有 init 段的分片：MPEG-TS（偏移 0 与 188 处为同步字节 0x47）、ADTS 音频，或以 ID3 标签开头的打包音频。
pub(crate) fn check_ts(data: &[u8]) -> Result<(), Integrity> {
    let ts = data.first() == Some(&0x47) && (data.len() < 376 || data[188] == 0x47);
    let adts = data.len() >= 2 && data[0] == 0xFF && data[1] & 0xF0 == 0xF0;
    let id3 = data.starts_with(b"ID3");
    if ts || adts || id3 {
        Ok(())
    } else {
        Err(Integrity::NotTs(head(data)))
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
