//! 本 crate 的全部 unsafe：直接读写 AVCodecParameters 字段，ffmpeg-next 没有对应的安全接口。

use ffmpeg::codec;
use ffmpeg::format::stream::{Stream, StreamMut};
use ffmpeg_next as ffmpeg;

use crate::StreamKind;

/// 一路流放进 MP4 轨时必须前后一致的编码参数；不同不连续段组的同一条轨之间按此比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Video {
        codec: &'static str,
        width: i32,
        height: i32,
    },
    Audio {
        codec: &'static str,
        sample_rate: i32,
        channels: i32,
    },
}

pub(crate) fn shape(stream: &Stream<'_>, kind: StreamKind) -> Shape {
    let params = stream.parameters();
    let codec = params.id().name();
    // SAFETY: 指针指向该流的 codecpar，由所属输入上下文分配，在 params 存活期间有效且非空；此处只读。
    let p = unsafe { &*params.as_ptr() };
    match kind {
        StreamKind::Video => Shape::Video {
            codec,
            width: p.width,
            height: p.height,
        },
        StreamKind::Audio => Shape::Audio {
            codec,
            sample_rate: p.sample_rate,
            channels: p.ch_layout.nb_channels,
        },
    }
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
