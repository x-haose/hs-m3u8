//! 支持的编码与输出容器。

use std::fmt;

use ffmpeg::codec;
use ffmpeg_next as ffmpeg;

use crate::inband::Inband;
use crate::{Origin, Shape, StreamKind, Unsupported};

/// 输出容器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Container {
    /// MP4，moov 前置
    Mp4,
    /// Matroska（.mkv）
    Matroska,
}

impl Container {
    /// 编码 `codec` 能否放进这种容器。只看编码，不看输入：TS 与裸音频（含打包音频）里另外只收能逐帧读出参数的 H.264、
    /// HEVC、AAC、AC-3、E-AC-3、MP2、MP3（如 TS 里的 Opus 放不进任何容器）；MP4 封装器另有的限制见
    /// [`Unsupported::Mp3SampleRate`]、[`Unsupported::Eac3ShortFrames`] 与 [`Unsupported::Eac3Substreams`]。
    pub fn holds(self, codec: Codec) -> bool {
        match self {
            Container::Mp4 => codec != Codec::Vorbis,
            Container::Matroska => true,
        }
    }

    /// FFmpeg 的封装器名。
    pub(crate) fn muxer(self) -> &'static std::ffi::CStr {
        match self {
            Container::Mp4 => c"mp4",
            Container::Matroska => c"matroska",
        }
    }
}

impl fmt::Display for Container {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Container::Mp4 => "MP4",
            Container::Matroska => "Matroska",
        })
    }
}

/// 本库能原样复制的编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
    Vp9,
    Av1,
    Aac,
    Opus,
    Mp3,
    Mp2,
    Ac3,
    Eac3,
    Flac,
    Alac,
    Vorbis,
}

impl Codec {
    /// 全部编码，先视频后音频。
    pub const ALL: [Codec; 13] = [
        Codec::H264,
        Codec::Hevc,
        Codec::Vp9,
        Codec::Av1,
        Codec::Aac,
        Codec::Opus,
        Codec::Mp3,
        Codec::Mp2,
        Codec::Ac3,
        Codec::Eac3,
        Codec::Flac,
        Codec::Alac,
        Codec::Vorbis,
    ];

    pub fn kind(self) -> StreamKind {
        match self {
            Codec::H264 | Codec::Hevc | Codec::Vp9 | Codec::Av1 => StreamKind::Video,
            _ => StreamKind::Audio,
        }
    }

    /// FFmpeg 的编码 ID。
    fn id(self) -> codec::Id {
        match self {
            Codec::H264 => codec::Id::H264,
            Codec::Hevc => codec::Id::HEVC,
            Codec::Vp9 => codec::Id::VP9,
            Codec::Av1 => codec::Id::AV1,
            Codec::Aac => codec::Id::AAC,
            Codec::Opus => codec::Id::OPUS,
            Codec::Mp3 => codec::Id::MP3,
            Codec::Mp2 => codec::Id::MP2,
            Codec::Ac3 => codec::Id::AC3,
            Codec::Eac3 => codec::Id::EAC3,
            Codec::Flac => codec::Id::FLAC,
            Codec::Alac => codec::Id::ALAC,
            Codec::Vorbis => codec::Id::VORBIS,
        }
    }

    /// FFmpeg 编码 ID 对应的编码；不在 [`Codec::ALL`] 里的为 None。
    fn of(id: codec::Id) -> Option<Codec> {
        Codec::ALL.into_iter().find(|c| c.id() == id)
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Vp9 => "VP9",
            Codec::Av1 => "AV1",
            Codec::Aac => "AAC",
            Codec::Opus => "Opus",
            Codec::Mp3 => "MP3",
            Codec::Mp2 => "MP2",
            Codec::Ac3 => "AC-3",
            Codec::Eac3 => "E-AC-3",
            Codec::Flac => "FLAC",
            Codec::Alac => "ALAC",
            Codec::Vorbis => "Vorbis",
        })
    }
}

/// MP4 封装器收的 MP3 最低采样率，Hz（链接的 FFmpeg 中 movenc 的 mov_init）。
pub(crate) const MP3_MIN_SAMPLE_RATE: u32 = 16_000;

/// 编码 `codec` 从这种输入能否放进 `container`：`standalone` 为参数随帧携带的输入（TS 与裸音频），要逐包核对参数（输出
/// 的一条轨只有一个样本描述），只收其中能逐帧读出参数的（见 [`Inband::of`]）。
fn accepts(codec: Codec, container: Container, standalone: bool) -> bool {
    container.holds(codec) && (!standalone || Inband::readable(codec.id()))
}

/// [`Unsupported::Codec`] 报错时列出 `container` 支持的编码。
pub(crate) fn supported_codecs(container: Container) -> String {
    let list = |standalone: bool| {
        let names = |kind: StreamKind| {
            Codec::ALL
                .iter()
                .filter(|&&c| c.kind() == kind && accepts(c, container, standalone))
                .map(Codec::to_string)
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
        "参数在文件头里的输入（fMP4、MP4、Matroska）支持 {}，TS 与裸音频（含 HLS 的打包音频）支持 {}",
        list(false),
        list(true)
    )
}

/// 编码能否放进 `container`；`standalone` 见 [`accepts`]。编码已知而不收的先报，参数探测不出（含编码也没认出）的再报。
pub(crate) fn check_codec(
    id: codec::Id,
    shape: &Shape,
    standalone: bool,
    container: Container,
    at: Origin,
) -> Result<(), Unsupported> {
    let kind = shape.kind();
    if id != codec::Id::None
        && !Codec::of(id).is_some_and(|c| c.kind() == kind && accepts(c, container, standalone))
    {
        return Err(Unsupported::Codec {
            at,
            kind,
            codec: id.name(),
            container,
        });
    }
    if id == codec::Id::None || !shape.determined() {
        return Err(Unsupported::ParamsUnknown { at, kind });
    }
    match *shape {
        Shape::Audio { sample_rate, .. }
            if container == Container::Mp4
                && id == codec::Id::MP3
                && sample_rate < MP3_MIN_SAMPLE_RATE =>
        {
            Err(Unsupported::Mp3SampleRate { at, sample_rate })
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_codec_is_listed_and_maps_to_its_ffmpeg_id() {
        // 加了编码而没放进 ALL 时，这里的穷举匹配编译不过
        let listed = |codec: Codec| match codec {
            Codec::H264
            | Codec::Hevc
            | Codec::Vp9
            | Codec::Av1
            | Codec::Aac
            | Codec::Opus
            | Codec::Mp3
            | Codec::Mp2
            | Codec::Ac3
            | Codec::Eac3
            | Codec::Flac
            | Codec::Alac
            | Codec::Vorbis => Codec::ALL.contains(&codec),
        };
        for codec in Codec::ALL {
            assert!(listed(codec));
            assert_eq!(Codec::of(codec.id()), Some(codec));
        }
        assert_eq!(Codec::ALL.len(), 13);
    }
}
