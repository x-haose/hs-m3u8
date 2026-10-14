//! MP4 能放的编码。

use ffmpeg::codec;
use ffmpeg_next as ffmpeg;

use crate::inband::Inband;
use crate::{Shape, StreamKind, Unsupported};

/// MP4 能放的编码：FFmpeg 的编码 ID、流种类与报错时用的名称。参数随帧携带的输入（TS 与打包音频）要逐包核对参数
/// （MP4 的一条轨只有一个样本描述），只收其中能逐帧读出参数的（见 [`Inband::of`]）。
const MP4_CODECS: [(codec::Id, StreamKind, &str); 12] = [
    (codec::Id::H264, StreamKind::Video, "H.264"),
    (codec::Id::HEVC, StreamKind::Video, "HEVC"),
    (codec::Id::AV1, StreamKind::Video, "AV1"),
    (codec::Id::VP9, StreamKind::Video, "VP9"),
    (codec::Id::AAC, StreamKind::Audio, "AAC"),
    (codec::Id::AC3, StreamKind::Audio, "AC-3"),
    (codec::Id::EAC3, StreamKind::Audio, "E-AC-3"),
    (codec::Id::MP3, StreamKind::Audio, "MP3"),
    (codec::Id::MP2, StreamKind::Audio, "MP2"),
    (codec::Id::FLAC, StreamKind::Audio, "FLAC"),
    (codec::Id::OPUS, StreamKind::Audio, "Opus"),
    (codec::Id::ALAC, StreamKind::Audio, "ALAC"),
];

/// MP4 封装器收的 MP3 最低采样率，Hz（链接的 FFmpeg 中 movenc 的 mov_init）。
pub(crate) const MP3_MIN_SAMPLE_RATE: u32 = 16_000;

/// 编码 `id` 能否放进 MP4；`standalone` 为输入没有 init 段（TS 与打包音频）。
fn accepts(id: codec::Id, standalone: bool) -> bool {
    MP4_CODECS.iter().any(|&(c, ..)| c == id) && (!standalone || Inband::of(id).is_some())
}

/// [`Unsupported::Codec`] 报错时列出的支持的编码。
pub(crate) fn supported_codecs() -> String {
    let list = |standalone: bool| {
        let names = |kind: StreamKind| {
            MP4_CODECS
                .iter()
                .filter(|&&(id, k, _)| k == kind && accepts(id, standalone))
                .map(|&(.., name)| name)
                .collect::<Vec<_>>()
                .join("、")
        };
        format!(
            "{} 视频与 {} 音频",
            names(StreamKind::Video),
            names(StreamKind::Audio)
        )
    };
    format!(
        "fMP4 里支持 {}，TS 与打包音频里支持 {}",
        list(false),
        list(true)
    )
}

/// 编码能否放进 MP4；`standalone` 为输入没有 init 段。编码已知而不收的先报，参数探测不出（含编码也没认出）的再报。
pub(crate) fn check_codec(
    id: codec::Id,
    shape: &Shape,
    standalone: bool,
    group: usize,
    track: usize,
) -> Result<(), Unsupported> {
    let kind = shape.kind();
    if id != codec::Id::None && !accepts(id, standalone) {
        return Err(Unsupported::Codec {
            group,
            track,
            kind,
            codec: id.name(),
        });
    }
    if id == codec::Id::None || !shape.determined() {
        return Err(Unsupported::ParamsUnknown { group, track, kind });
    }
    match *shape {
        Shape::Audio { sample_rate, .. }
            if id == codec::Id::MP3 && sample_rate < MP3_MIN_SAMPLE_RATE =>
        {
            Err(Unsupported::Mp3SampleRate {
                group,
                track,
                sample_rate,
            })
        }
        _ => Ok(()),
    }
}
