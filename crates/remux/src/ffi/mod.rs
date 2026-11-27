//! FFmpeg 的薄封装，本 crate 的全部 unsafe 都在这个模块里：输入（经 [`std::io::Read`] 与 [`std::io::Seek`] 读的自定义
//! 读写上下文，打开并探测流信息）、输出（装了中断回调的文件）、包、选项字典、流的参数，以及时间换算、错误码与日志
//! 级别。各类型独占所指的 FFmpeg 对象，析构时释放。FFmpeg 返回的错误码原样换成 [`FfmpegError`] 上抛；FFmpeg 分配内存
//! 失败同 Rust 自己分配失败，中止进程（[`std::alloc::handle_alloc_error`]）。

mod dict;
mod input;
mod output;
mod packet;
mod stream;

use std::alloc::Layout;
use std::ffi::{CStr, c_int};

use hs_m3u8_ffmpeg_sys as sys;

pub(crate) use dict::Dictionary;
pub(crate) use input::InputContext;
pub(crate) use output::{OpenFailure, OutputFile};
pub(crate) use packet::Packet;
pub(crate) use stream::{DecodedAac, Stream};

use crate::FfmpegError;

/// FFmpeg 的错误码 `code`（负的 AVERROR）连同链接的 FFmpeg 给的说明；errno 型的说明取 C 运行库的 strerror。
pub(crate) fn error(code: c_int) -> FfmpegError {
    let mut buf = [0; 128];
    // SAFETY: buf 可写 buf.len() 字节；av_strerror 总是写入 NUL 结尾、不超出给定长度的说明（不认识的码写成
    // 「Error number … occurred」并返回负数，说明照样可用）。
    unsafe { sys::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    // SAFETY: 同上，buf 里是 NUL 结尾的字符串。
    let message = unsafe { CStr::from_ptr(buf.as_ptr()) };
    FfmpegError {
        code,
        message: message.to_string_lossy().into_owned(),
    }
}

/// FFmpeg 的日志一律不输出（进程内全局的设置）。
pub(crate) fn quiet_logs() {
    // SAFETY: 只设一个全局的日志级别，不涉及指针。
    unsafe { sys::av_log_set_level(sys::AV_LOG_QUIET) };
}

/// FFmpeg 分配 `bytes` 字节失败：同 Rust 自己分配失败，中止进程。
fn out_of_memory(bytes: usize) -> ! {
    std::alloc::handle_alloc_error(
        Layout::array::<u8>(bytes).expect("分配的字节数不超过 isize::MAX"),
    )
}

/// 时间基等有理数：分子与分母（AVRational）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rational(pub(crate) i32, pub(crate) i32);

impl Rational {
    fn raw(self) -> sys::AVRational {
        sys::AVRational {
            num: self.0,
            den: self.1,
        }
    }

    fn of(raw: sys::AVRational) -> Rational {
        Rational(raw.num, raw.den)
    }
}

/// 时间换算的取整方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rounding {
    /// 向正无穷
    Up,
    /// 向负无穷
    Down,
}

/// 时间戳换时间基（av_rescale_q 系列，中间结果不溢出）。
pub(crate) trait Rescale {
    /// 从时间基 `from` 换到 `to`，四舍五入（恰在中间时远离 0）。
    fn rescale(self, from: Rational, to: Rational) -> i64;
    /// 同 [`Rescale::rescale`]，按 `rounding` 取整。
    fn rescale_with(self, from: Rational, to: Rational, rounding: Rounding) -> i64;
}

impl Rescale for i64 {
    fn rescale(self, from: Rational, to: Rational) -> i64 {
        // SAFETY: 纯计算，不涉及指针。
        unsafe { sys::av_rescale_q(self, from.raw(), to.raw()) }
    }

    fn rescale_with(self, from: Rational, to: Rational, rounding: Rounding) -> i64 {
        let rounding = match rounding {
            Rounding::Up => sys::AV_ROUND_UP,
            Rounding::Down => sys::AV_ROUND_DOWN,
        };
        // SAFETY: 纯计算，不涉及指针。
        unsafe { sys::av_rescale_q_rnd(self, from.raw(), to.raw(), rounding) }
    }
}

/// FFmpeg 的编码 ID（AVCodecID）。按整数保存，不转成枚举：FFmpeg 给出的值可以是任何编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CodecId(sys::AVCodecID);

impl CodecId {
    pub(crate) const NONE: CodecId = CodecId(sys::AV_CODEC_ID_NONE);
    pub(crate) const H264: CodecId = CodecId(sys::AV_CODEC_ID_H264);
    pub(crate) const HEVC: CodecId = CodecId(sys::AV_CODEC_ID_HEVC);
    pub(crate) const VP9: CodecId = CodecId(sys::AV_CODEC_ID_VP9);
    pub(crate) const AV1: CodecId = CodecId(sys::AV_CODEC_ID_AV1);
    pub(crate) const AAC: CodecId = CodecId(sys::AV_CODEC_ID_AAC);
    pub(crate) const OPUS: CodecId = CodecId(sys::AV_CODEC_ID_OPUS);
    pub(crate) const MP3: CodecId = CodecId(sys::AV_CODEC_ID_MP3);
    pub(crate) const MP2: CodecId = CodecId(sys::AV_CODEC_ID_MP2);
    pub(crate) const AC3: CodecId = CodecId(sys::AV_CODEC_ID_AC3);
    pub(crate) const EAC3: CodecId = CodecId(sys::AV_CODEC_ID_EAC3);
    pub(crate) const FLAC: CodecId = CodecId(sys::AV_CODEC_ID_FLAC);
    pub(crate) const ALAC: CodecId = CodecId(sys::AV_CODEC_ID_ALAC);
    pub(crate) const VORBIS: CodecId = CodecId(sys::AV_CODEC_ID_VORBIS);
    pub(crate) const SUBRIP: CodecId = CodecId(sys::AV_CODEC_ID_SUBRIP);
    pub(crate) const MOV_TEXT: CodecId = CodecId(sys::AV_CODEC_ID_MOV_TEXT);

    /// FFmpeg 的编码名，如 `h264`；不认识的 ID 为 `unknown_codec`。
    pub(crate) fn name(self) -> &'static str {
        // SAFETY: avcodec_get_name 对任何 ID 都返回静态的 NUL 结尾字符串，不为空。
        let name = unsafe { CStr::from_ptr(sys::avcodec_get_name(self.0)) };
        name.to_str().expect("FFmpeg 的编码名是 ASCII")
    }
}

/// 按文件开头的字节 `head` 探测格式（同 FFmpeg 打开输入时，按各解复用器的探测打分取最高的），返回解复用器名；认不出
/// 时为 None。
pub(crate) fn probe_format(head: &[u8]) -> Option<&'static str> {
    // 探测函数要求数据后面跟 AVPROBE_PADDING_SIZE 个 0
    let padding = usize::try_from(sys::AVPROBE_PADDING_SIZE).expect("补零的字节数为正");
    let mut buf = head.to_vec();
    buf.resize(head.len() + padding, 0);
    let probe = sys::AVProbeData {
        filename: c"".as_ptr(),
        buf: buf.as_mut_ptr(),
        buf_size: c_int::try_from(head.len()).expect("调用方只给开头 1 MiB"),
        mime_type: std::ptr::null(),
    };
    let mut score = 0;
    // SAFETY: probe 的各指针在调用期间有效：文件名为空串，缓冲区有 buf_size 个字节外加要求的补零；只读。返回的格式描述
    // 是 FFmpeg 的静态数据，名称为静态的 NUL 结尾字符串。
    unsafe {
        let format = sys::av_probe_input_format2(&raw const probe, 1, &raw mut score).as_ref()?;
        CStr::from_ptr(format.name).to_str().ok()
    }
}

/// FFmpeg 的像素格式值 `format`（AVPixelFormat）的名称；不认识（含 AV_PIX_FMT_NONE）时为 None。逐个查像素格式描述
/// 表，不按值直接索引。
fn pixel_format_name(format: c_int) -> Option<&'static str> {
    let mut desc = std::ptr::null();
    loop {
        // SAFETY: 传入空指针或上一次返回的描述，返回表里的下一个描述或空指针；描述表是 FFmpeg 的静态数据。
        desc = unsafe { sys::av_pix_fmt_desc_next(desc) };
        // SAFETY: 非空的描述指向静态数据，在进程内一直有效。
        let found = unsafe { desc.as_ref() }?;
        // SAFETY: desc 为表里的描述。
        if unsafe { sys::av_pix_fmt_desc_get_id(desc) } == format {
            // SAFETY: 描述的名称是静态的 NUL 结尾字符串。
            return unsafe { CStr::from_ptr(found.name) }.to_str().ok();
        }
    }
}
