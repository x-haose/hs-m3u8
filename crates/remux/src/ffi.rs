//! 本 crate 的全部 unsafe：直接读写 AVCodecParameters 字段，读解复用器给流用的解析器的状态、输入的节目列表，查像素
//! 格式描述表，打开装了中断回调的输出、读输出的读写错误，ffmpeg-next 没有对应的安全接口。

use std::ffi::{CStr, CString};

use ffmpeg::codec;
use ffmpeg::format;
use ffmpeg::format::stream::{Stream, StreamMut};
use ffmpeg::util::interrupt::{self, InterruptGuard};
use ffmpeg_next as ffmpeg;

use crate::{Cancel, Shape, StreamKind};

pub(crate) fn shape(stream: &Stream<'_>, kind: StreamKind) -> Shape {
    let params = stream.parameters();
    let codec = params.id().name();
    // SAFETY: 指针指向该流的 codecpar，由所属输入上下文分配，在 params 存活期间有效且非空；此处只读。
    let p = unsafe { &*params.as_ptr() };
    let unsigned = |value: i32| u32::try_from(value).expect("FFmpeg 的宽高、采样率、声道数非负");
    match kind {
        StreamKind::Video => Shape::Video {
            codec,
            width: unsigned(p.width),
            height: unsigned(p.height),
            pixel_format: pixel_format_name(p.format),
        },
        StreamKind::Audio => Shape::Audio {
            codec,
            sample_rate: unsigned(p.sample_rate),
            channels: unsigned(p.ch_layout.nb_channels),
        },
    }
}

/// 流的解码配置（codecpar 的 extradata，如 avcC、esds 里的 AudioSpecificConfig）；没有时为空。
pub(crate) fn extradata(stream: &Stream<'_>) -> Vec<u8> {
    let params = stream.parameters();
    // SAFETY: 指针指向该流的 codecpar，在 params 存活期间有效且非空；extradata 为空指针或指向 extradata_size 字节，
    // 此处只读并复制出来。
    let p = unsafe { &*params.as_ptr() };
    match usize::try_from(p.extradata_size) {
        Ok(len) if len > 0 && !p.extradata.is_null() => {
            unsafe { std::slice::from_raw_parts(p.extradata, len) }.to_vec()
        }
        _ => Vec::new(),
    }
}

/// 输入的节目（AVProgram）里列不列出流 `index`：TS 每读到一份 PMT，FFmpeg 先清空节目的流列表再按它重新加入
/// （mpegts.c 的 clear_avprogram），列表即最近一份 PMT 里的流。没有节目的输入（不是 TS）为 None。
pub(crate) fn in_program(ictx: &format::context::Input, index: usize) -> Option<bool> {
    // SAFETY: 输入上下文在 ictx 存活期间有效；programs 指向 nb_programs 个有效的节目指针，各节目的 stream_index 指向
    // nb_stream_indexes 个下标，都归输入上下文所有；此处只读。
    let programs = unsafe {
        let ctx = &*ictx.as_ptr();
        if ctx.nb_programs == 0 || ctx.programs.is_null() {
            return None;
        }
        std::slice::from_raw_parts(ctx.programs, ctx.nb_programs as usize)
    };
    Some(programs.iter().any(|&program| {
        // SAFETY: 同上，节目指针非空且有效。
        let program = unsafe { &*program };
        if program.stream_index.is_null() {
            return false;
        }
        // SAFETY: 同上。
        let indexes = unsafe {
            std::slice::from_raw_parts(program.stream_index, program.nb_stream_indexes as usize)
        };
        indexes.iter().any(|&i| i as usize == index)
    }))
}

/// fMP4 里的 AAC 流（USAC 以外）探测时解码器解出一帧后得出的参数（codecpar 的 profile 与 frame_size）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodedAac {
    /// FFmpeg 的 AAC profile（AV_PROFILE_AAC_LC 等）
    pub profile: i32,
    /// 每帧的采样点数，含 SBR 的加倍
    pub frame_size: i32,
}

/// 见 [`DecodedAac`]；没有解出帧时为 None：profile 只在解帧时得出，frame_size 此前是 mov 按 AudioSpecificConfig 填的、
/// 不含 SBR 加倍的帧长。
pub(crate) fn decoded_aac(stream: &Stream<'_>) -> Option<DecodedAac> {
    let params = stream.parameters();
    // SAFETY: 指针指向该流的 codecpar，在 params 存活期间有效且非空；此处只读。
    let p = unsafe { &*params.as_ptr() };
    (p.profile >= 0).then_some(DecodedAac {
        profile: p.profile,
        frame_size: p.frame_size,
    })
}

/// FFmpeg 的像素格式值 `format`（AVPixelFormat）的名称；不认识（含 AV_PIX_FMT_NONE）时为 None。逐个查像素格式描述
/// 表，不把整数转成枚举：越界的枚举值是未定义行为。
fn pixel_format_name(format: i32) -> Option<&'static str> {
    let mut desc = std::ptr::null();
    loop {
        // SAFETY: 传入空指针或上一次返回的描述，返回表里的下一个描述或空指针；描述表是 FFmpeg 的静态数据。
        desc = unsafe { ffmpeg::ffi::av_pix_fmt_desc_next(desc) };
        // SAFETY: 非空的描述指向静态数据，在进程内一直有效。
        let found = unsafe { desc.as_ref() }?;
        // SAFETY: desc 为表里的描述。
        if unsafe { ffmpeg::ffi::av_pix_fmt_desc_get_id(desc) } as i32 == format {
            // SAFETY: 描述的名称是静态的 NUL 结尾字符串。
            return unsafe { CStr::from_ptr(found.name) }.to_str().ok();
        }
    }
}

/// 解复用器给这路流用的解析器最近从参数集得出的画面：宽、高与像素格式值（AVPixelFormat，色度采样与位深）；没有
/// 解析器或还没读到参数集时为 None。
pub(crate) fn parsed_picture(stream: &Stream<'_>) -> Option<(u32, u32, i32)> {
    // SAFETY: 流指针在 stream 存活期间有效；av_stream_get_parser 只读流的内部字段，返回的解析器为空或归所属输入上下文
    // 所有，在 stream 存活期间有效；此处只读。
    let parser = unsafe { ffmpeg::ffi::av_stream_get_parser(stream.as_ptr()).as_ref() }?;
    let positive = |value: i32| u32::try_from(value).ok().filter(|&v| v > 0);
    Some((
        positive(parser.width)?,
        positive(parser.height)?,
        parser.format,
    ))
}

/// HEVC 标 `hvc1`（QuickTime/Safari 不播 `hev1`）；其余置 0，由 MP4 封装器按编码选择。
pub(crate) fn set_codec_tag(ost: &mut StreamMut<'_>, id: codec::Id) {
    let tag = if id == codec::Id::HEVC {
        u32::from_le_bytes(*b"hvc1")
    } else {
        0
    };
    // SAFETY: codecpar 由 avformat_new_stream 分配、归输出上下文所有且非空；此时尚未写头，改 codec_tag 不影响其他状态。
    unsafe {
        (*ost.parameters().as_mut_ptr()).codec_tag = tag;
    }
}

/// 装了中断回调的输出。关闭文件时 FFmpeg 刷出缓冲还会调用回调：析构时先关闭文件（见 [`Drop`] 的实现），再释放上下文，
/// 最后释放回调持有的闭包（字段按声明的顺序析构）。从输出流取得的 ffmpeg-next 对象（如流参数）持有上下文的引用，会让
/// 上下文晚于本结构释放，所以文件不能留给上下文的析构去关。
pub(crate) struct OutputFile {
    pub(crate) octx: format::context::Output,
    _interrupt: InterruptGuard,
}

impl Drop for OutputFile {
    fn drop(&mut self) {
        // SAFETY: 上下文在 octx 存活期间有效；pb 为本结构用 avio_open2 打开的读写上下文或空，avio_closep 关闭后置空，
        // 之后上下文析构时的 avio_close(NULL) 什么也不做。关闭时刷出缓冲会调用的中断回调此时仍由 _interrupt 持有。
        unsafe {
            ffmpeg::ffi::avio_closep(&raw mut (*self.octx.as_mut_ptr()).pb);
        }
    }
}

/// 打开输出时失败的一步。
pub(crate) enum OpenFailure {
    /// 建立 MP4 封装器的上下文
    Muxer(ffmpeg::Error),
    /// 创建文件
    File(ffmpeg::Error),
}

/// 以 MP4 格式打开输出 `path`（不含 NUL 的 UTF-8），装上中断回调：FFmpeg 每次读写文件前轮询它（含写 moov 前置时
/// 重新打开输出搬动数据的第二遍），`cancel` 取消后读写以 AVERROR_EXIT 失败。
pub(crate) fn open_output(path: &str, cancel: &Cancel) -> Result<OutputFile, OpenFailure> {
    let c_path = CString::new(path).expect("调用方保证路径不含 NUL");
    let cancel = cancel.clone();
    let interrupt = interrupt::new(Box::new(move || cancel.is_cancelled()));
    let mut ctx = std::ptr::null_mut();
    // SAFETY: 格式名与路径是 NUL 结尾的字符串；成功时 ctx 指向新分配的上下文，失败时为空。
    let created = unsafe {
        ffmpeg::ffi::avformat_alloc_output_context2(
            &raw mut ctx,
            std::ptr::null(),
            c"mp4".as_ptr(),
            c_path.as_ptr(),
        )
    };
    if created < 0 {
        return Err(OpenFailure::Muxer(ffmpeg::Error::from(created)));
    }
    // SAFETY: ctx 非空、归本函数所有；装上的回调的闭包由 interrupt.guard 持有，guard 与上下文一同放进 OutputFile，文件
    // 在回调之前关闭（见 OutputFile）。avio_open2 失败时 pb 仍为空，释放上下文即可。
    unsafe {
        (*ctx).interrupt_callback = interrupt.interrupt;
        let opened = ffmpeg::ffi::avio_open2(
            &raw mut (*ctx).pb,
            c_path.as_ptr(),
            ffmpeg::ffi::AVIO_FLAG_WRITE,
            &raw const (*ctx).interrupt_callback,
            std::ptr::null_mut(),
        );
        if opened < 0 {
            ffmpeg::ffi::avformat_free_context(ctx);
            return Err(OpenFailure::File(ffmpeg::Error::from(opened)));
        }
        Ok(OutputFile {
            octx: format::context::Output::wrap(ctx),
            _interrupt: interrupt.guard,
        })
    }
}

/// 输出的读写上下文记下的错误（负的 AVERROR）：FFmpeg 写文件失败时记在这里，封装器自身的错误不记；没有时为 None。
pub(crate) fn output_io_error(octx: &format::context::Output) -> Option<i32> {
    // SAFETY: 上下文在 octx 存活期间有效；pb 为空或指向打开的读写上下文，此处只读。
    let error = unsafe { (*octx.as_ptr()).pb.as_ref() }?.error;
    (error < 0).then_some(error)
}
