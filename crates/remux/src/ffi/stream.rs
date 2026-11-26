//! 输入或输出上下文里的一路流（AVStream）与它的编码参数。

use std::ffi::CStr;

use hs_m3u8_ffmpeg_sys as sys;

use super::{CodecId, Rational, pixel_format_name};
use crate::{Shape, StreamKind};

/// 一路流，借用所属的上下文：上下文只在可变借用（读包、写包等）时改动流，借用期间流的字段不变。
#[derive(Clone, Copy)]
pub(crate) struct Stream<'a> {
    raw: &'a sys::AVStream,
}

/// 上下文 `ctx` 的各路流。
///
/// # Safety
/// `ctx` 为有效的上下文，在返回值的生命周期内不被改动。
pub(super) unsafe fn streams_of(ctx: &sys::AVFormatContext) -> impl Iterator<Item = Stream<'_>> {
    let count = usize::try_from(ctx.nb_streams).expect("流数在 usize 范围内");
    let streams: &[*mut sys::AVStream] = if count == 0 {
        &[]
    } else {
        // SAFETY: 调用方保证上下文有效；streams 指向 nb_streams 个有效的流指针，归上下文所有。
        unsafe { std::slice::from_raw_parts(ctx.streams, count) }
    };
    streams.iter().map(|&stream| Stream {
        // SAFETY: 同上，各流指针非空，在上下文存活且不被改动期间有效。
        raw: unsafe { &*stream },
    })
}

impl<'a> Stream<'a> {
    /// 在所属上下文里的下标。
    pub(crate) fn index(&self) -> usize {
        usize::try_from(self.raw.index).expect("流下标非负")
    }

    pub(crate) fn time_base(&self) -> Rational {
        Rational::of(self.raw.time_base)
    }

    /// 起始时刻（时间基为 [`Stream::time_base`]）：解复用器给出的第一个包的 PTS；不知道时为 None。
    pub(crate) fn start_time(&self) -> Option<i64> {
        Some(self.raw.start_time).filter(|&start| start != sys::AV_NOPTS_VALUE)
    }

    fn params(&self) -> &'a sys::AVCodecParameters {
        // SAFETY: codecpar 由 avformat_new_stream 随流分配、不为空，归流所有。
        unsafe { &*self.raw.codecpar }
    }

    pub(super) fn params_ptr(&self) -> *const sys::AVCodecParameters {
        self.raw.codecpar
    }

    pub(crate) fn codec_id(&self) -> CodecId {
        CodecId(self.params().codec_id)
    }

    /// 视频、音频或字幕；其余种类（数据、附件等）为 None。
    pub(crate) fn kind(&self) -> Option<StreamKind> {
        match self.params().codec_type {
            sys::AVMEDIA_TYPE_VIDEO => Some(StreamKind::Video),
            sys::AVMEDIA_TYPE_AUDIO => Some(StreamKind::Audio),
            sys::AVMEDIA_TYPE_SUBTITLE => Some(StreamKind::Subtitle),
            _ => None,
        }
    }

    /// 流的元数据里的语言（TS 为 PMT 的 ISO 639 描述符，MP4 为轨的语言）；没有时为 None。
    pub(crate) fn language(&self) -> Option<String> {
        // SAFETY: metadata 为空或归流所有的字典；av_dict_get 只读，返回的条目在字典不变期间有效，键与值为 NUL 结尾的
        // 字符串，此处复制出来。
        unsafe {
            let entry =
                sys::av_dict_get(self.raw.metadata, c"language".as_ptr(), std::ptr::null(), 0)
                    .as_ref()?;
            Some(CStr::from_ptr(entry.value).to_string_lossy().into_owned())
        }
    }

    /// 音视频流的编码参数：视频的宽高与像素格式，音频的采样率与声道数；`kind` 为这路流的种类。
    pub(crate) fn shape(&self, kind: StreamKind) -> Shape {
        let p = self.params();
        let codec = self.codec_id().name();
        let unsigned =
            |value: i32| u32::try_from(value).expect("FFmpeg 的宽高、采样率、声道数非负");
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
            StreamKind::Subtitle => unreachable!("字幕不经 FFmpeg 读，只取音视频流的参数"),
        }
    }

    /// 解码配置（codecpar 的 extradata，如 avcC、esds 里的 AudioSpecificConfig）；没有时为空。
    pub(crate) fn extradata(&self) -> Vec<u8> {
        let p = self.params();
        match usize::try_from(p.extradata_size) {
            // SAFETY: extradata 非空时指向 extradata_size 字节，归 codecpar 所有；此处复制出来。
            Ok(len) if len > 0 && !p.extradata.is_null() => {
                unsafe { std::slice::from_raw_parts(p.extradata, len) }.to_vec()
            }
            _ => Vec::new(),
        }
    }

    /// 见 [`DecodedAac`]；没有解出帧时为 None：profile 只在解帧时得出，frame_size 此前是 mov 按 AudioSpecificConfig
    /// 填的、不含 SBR 加倍的帧长。
    pub(crate) fn decoded_aac(&self) -> Option<DecodedAac> {
        let p = self.params();
        (p.profile >= 0).then_some(DecodedAac {
            profile: p.profile,
            frame_size: p.frame_size,
        })
    }

    /// 音频每帧的采样点数（codecpar 的 frame_size）；帧长不固定或不知道时为 None。
    pub(crate) fn frame_samples(&self) -> Option<u32> {
        u32::try_from(self.params().frame_size)
            .ok()
            .filter(|&n| n > 0)
    }

    /// 解复用器给这路流用的解析器最近从参数集得出的画面：宽、高与像素格式值（AVPixelFormat，色度采样与位深）；没有
    /// 解析器或还没读到参数集时为 None。
    pub(crate) fn parsed_picture(&self) -> Option<(u32, u32, i32)> {
        // SAFETY: av_stream_get_parser 只读流的内部字段，返回的解析器为空或归所属输入上下文所有，在借用期间有效；只读。
        let parser = unsafe { sys::av_stream_get_parser(self.raw).as_ref() }?;
        let positive = |value: i32| u32::try_from(value).ok().filter(|&v| v > 0);
        Some((
            positive(parser.width)?,
            positive(parser.height)?,
            parser.format,
        ))
    }
}

/// fMP4 里的 AAC 流（USAC 以外）探测时解码器解出一帧后得出的参数（codecpar 的 profile 与 frame_size）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodedAac {
    /// FFmpeg 的 AAC profile（AV_PROFILE_AAC_LC 等）
    pub profile: i32,
    /// 每帧的采样点数，含 SBR 的加倍
    pub frame_size: i32,
}
