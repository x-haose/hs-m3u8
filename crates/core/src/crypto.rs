//! AES-128-CBC 解密。

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
    fn wrong_key_and_bad_length_are_rejected() {
        let (key, iv) = ([7u8; 16], [9u8; 16]);
        let cipher = encrypt(b"0123456789abcdef0123", &key, &iv);
        assert_eq!(decrypt(cipher, &[8u8; 16], &iv), Err(Integrity::Padding));
        assert_eq!(
            decrypt(vec![0; 15], &key, &iv),
            Err(Integrity::CipherLength(15))
        );
    }
}
