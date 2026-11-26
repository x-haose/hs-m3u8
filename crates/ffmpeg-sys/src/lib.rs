//! hs-m3u8 用到的那部分 FFmpeg 9.0（libavformat、libavcodec、libavutil）的原始绑定，静态链接。
//!
//! 绑定由 bindgen 按 build.rs 里的清单生成，每个目标一份（`src/bindings/<目标>.rs`），带各结构体大小与字段偏移的编译期
//! 断言：头文件与绑定对不上时编译失败。C 的枚举生成为整数常量（如 `AV_CODEC_ID_H264`），不生成 Rust 的 enum：FFmpeg
//! 返回的值不在 enum 里时是未定义行为。bindgen 生成不了的宏在本文件里手写。
//!
//! 链接的 FFmpeg 由 `FFMPEG_DIR` 指定，见 build.rs。

use std::ffi::c_int;

include!(env!("HS_M3U8_FFMPEG_BINDINGS"));

/// `AV_NOPTS_VALUE`：没有时间戳（`INT64_MIN`）。
pub const AV_NOPTS_VALUE: i64 = i64::MIN;

/// `AVERROR(e)`：errno `e`（正数，取 libc 的值：与 FFmpeg 所用 C 运行库的相同）对应的 FFmpeg 错误码。
pub const fn AVERROR(e: c_int) -> c_int {
    -e
}

/// `FFERRTAG(a, b, c, d)`：FFmpeg 自己的错误码，四个字符按小端拼成的整数取负。
const fn fferrtag(tag: [u8; 4]) -> c_int {
    -i32::from_le_bytes(tag)
}

/// 读到末尾。
pub const AVERROR_EOF: c_int = fferrtag(*b"EOF ");
/// 中断回调要求中止。
pub const AVERROR_EXIT: c_int = fferrtag(*b"EXIT");
/// 处理输入时遇到不合法的数据。
pub const AVERROR_INVALIDDATA: c_int = fferrtag(*b"INDA");

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use super::*;

    /// 链接的 FFmpeg 给错误码 `code` 的说明。
    fn message(code: c_int) -> String {
        let mut buf = [0; 64];
        // SAFETY: buf 可写 buf.len() 字节，av_strerror 写入以 NUL 结尾、不超出给定长度的说明。
        let found = unsafe { av_strerror(code, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(found, 0, "FFmpeg 认得错误码 {code}");
        // SAFETY: 同上，buf 里是 NUL 结尾的字符串。
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn handwritten_error_codes_are_ffmpegs() {
        assert_eq!(message(AVERROR_EOF), "End of file");
        assert_eq!(message(AVERROR_EXIT), "Immediate exit requested");
        assert_eq!(
            message(AVERROR_INVALIDDATA),
            "Invalid data found when processing input"
        );
        assert_eq!(message(AVERROR(libc::EINVAL)), "Invalid argument");
    }
}
