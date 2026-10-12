//! 本 crate 的全部 unsafe：直接读写 AVCodecParameters 字段，读解复用器给流用的解析器的状态、输入的节目列表，查像素
//! 格式描述表，ffmpeg-next 没有对应的安全接口。

use std::ffi::CStr;

use ffmpeg::codec;
use ffmpeg::format;
use ffmpeg::format::stream::{Stream, StreamMut};
use ffmpeg_next as ffmpeg;

use crate::{Shape, StreamKind};

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

/// 流的 profile 与每帧的采样点数（codecpar 的 profile 与 frame_size，探测时由解码器得出；不知道时分别为负数与 0）。
pub(crate) fn profile_and_frame_size(stream: &Stream<'_>) -> (i32, i32) {
    let params = stream.parameters();
    // SAFETY: 指针指向该流的 codecpar，在 params 存活期间有效且非空；此处只读。
    let p = unsafe { &*params.as_ptr() };
    (p.profile, p.frame_size)
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
