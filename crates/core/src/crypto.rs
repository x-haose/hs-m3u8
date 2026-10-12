//! AES-128-CBC 解密。

use aes::Aes128;
use aes::cipher::block_padding::{NoPadding, Pkcs7};
use aes::cipher::{BlockModeDecrypt, KeyIvInit};

/// 解密失败的原因。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DecryptError {
    /// 填充不合法：key 不对时几乎必然如此；IV 只影响第一个 16 字节块，而填充在最后一块，IV 不对发现不了
    Padding,
    /// 密文 `len` 字节，不是 16 的整数倍；`whole` 为整块部分解出的明文（不去填充），供调用方判断是不是没写完
    Unaligned { len: usize, whole: Vec<u8> },
}

/// 原地解密并去掉 PKCS#7 填充。
pub(crate) fn decrypt(
    mut data: Vec<u8>,
    key: &[u8; 16],
    iv: &[u8; 16],
) -> Result<Vec<u8>, DecryptError> {
    let cipher = cbc::Decryptor::<Aes128>::new(key.into(), iv.into());
    let len = data.len();
    if !len.is_multiple_of(16) {
        data.truncate(len - len % 16);
        if !data.is_empty() {
            cipher
                .decrypt_padded::<NoPadding>(&mut data)
                .expect("整块且非空的密文不去填充时不会失败");
        }
        return Err(DecryptError::Unaligned { len, whole: data });
    }
    let plain_len = cipher
        .decrypt_padded::<Pkcs7>(&mut data)
        .map_err(|_| DecryptError::Padding)?
        .len();
    data.truncate(plain_len);
    Ok(data)
}

#[cfg(test)]
mod tests {
    use aes::cipher::BlockModeEncrypt;

    use super::*;

    fn encrypt(plain: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
        let mut buf = plain.to_vec();
        buf.resize(plain.len() + 16, 0);
        let len = cbc::Encryptor::<Aes128>::new(key.into(), iv.into())
            .encrypt_padded::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .len();
        buf.truncate(len);
        buf
    }

    #[test]
    fn decrypts_and_strips_padding() {
        let (key, iv) = ([7u8; 16], [9u8; 16]);
        for len in [0, 1, 15, 16, 17, 188 * 3] {
            let plain: Vec<u8> = (0..len).map(|i| i as u8).collect();
            assert_eq!(decrypt(encrypt(&plain, &key, &iv), &key, &iv), Ok(plain));
        }
    }

    #[test]
    fn wrong_key_and_unaligned_ciphertext_are_rejected() {
        let (key, iv) = ([7u8; 16], [9u8; 16]);
        let cipher = encrypt(b"0123456789abcdef0123", &key, &iv);
        assert_eq!(decrypt(cipher, &[8u8; 16], &iv), Err(DecryptError::Padding));
        // 不足整块时解出整块部分
        let plain: Vec<u8> = (0..40).collect();
        let cipher = encrypt(&plain, &key, &iv);
        assert_eq!(
            decrypt(cipher[..40].to_vec(), &key, &iv),
            Err(DecryptError::Unaligned {
                len: 40,
                whole: plain[..32].to_vec()
            })
        );
        assert_eq!(
            decrypt(vec![0; 15], &key, &iv),
            Err(DecryptError::Unaligned {
                len: 15,
                whole: Vec::new()
            })
        );
    }
}
