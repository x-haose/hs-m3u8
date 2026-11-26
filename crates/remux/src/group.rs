//! 选出一条轨（或一个完整文件）里要取的流，打开 HLS 一组的各轨，再换成读包的输入。

use ffmpeg::{Rational, codec};
use ffmpeg_next as ffmpeg;

use crate::codecs::check_codec;
use crate::compare::{Config, Desc, same_streams};
use crate::hls::Numbered;
use crate::inband::{Inband, PacketCheck};
use crate::input::Input;
use crate::nal::Framing;
use crate::source::Source;
use crate::{Cancel, Container, Error, Origin, StreamKind, Streams, Unsupported, ffi, frame};

/// 一条轨在某组中（或一个完整文件里）被选中的一路流。
pub(crate) struct Selected {
    pub(crate) index: usize,
    pub(crate) time_base: Rational,
    pub(crate) id: codec::Id,
    pub(crate) desc: Desc,
    /// HEVC 视频：包里 NAL 单元的排法，用于认出参考上一个 GOP 的前导帧（见 [`crate::nal::hevc_rasl`]）；其余流，及 MP4
    /// 的解码配置太短读不出排法时为 None
    pub(crate) hevc: Option<Framing>,
    /// 从头开始的逐包核对（见 [`PacketCheck`]）；不用核对时为 None
    pub(crate) check: Option<PacketCheck>,
    /// 音频每帧的采样点数（FFmpeg 给的 frame_size，链接的 FFmpeg 里只有解得出帧的 AAC 有）；视频，或帧长不固定、
    /// 不知道时为 None
    pub(crate) frame_samples: Option<u32>,
}

/// 按 `wanted` 选出 `input`（`at` 处的输入）里第一路视频与（或）第一路音频，并检查编码能否放进 `container`。
pub(crate) fn select(
    input: &Input,
    wanted: Streams,
    container: Container,
    at: Origin,
) -> Result<Vec<Selected>, Error> {
    let mut selected = Vec::new();
    for kind in [StreamKind::Video, StreamKind::Audio] {
        if !wanted.wants(kind) {
            continue;
        }
        let Some(stream) = input
            .context()
            .streams()
            .find(|s| s.parameters().medium() == kind.medium())
        else {
            continue;
        };
        let id = stream.parameters().id();
        let shape = ffi::shape(&stream, kind);
        check_codec(id, &shape, input.standalone(), container, at).map_err(Error::Unsupported)?;
        let config = if input.standalone() {
            Config::Inband
        } else {
            let extradata = ffi::extradata(&stream);
            if id == codec::Id::AAC && frame::audio_object_type(&extradata) != Some(frame::AOT_USAC)
            {
                Config::InitAac {
                    config: extradata,
                    decoded: ffi::decoded_aac(&stream),
                }
            } else {
                Config::Init(extradata)
            }
        };
        let hevc = match (&config, id) {
            (Config::Inband, codec::Id::HEVC) => Some(Framing::AnnexB),
            (Config::Init(hvcc), codec::Id::HEVC) => Framing::of_hvcc(hvcc),
            _ => None,
        };
        let check = if input.standalone() {
            let inband = Inband::of(id, container).expect("check_codec 只收能逐帧读出参数的编码");
            Some(PacketCheck::Inband(inband))
        } else if container == Container::Mp4 && id == codec::Id::EAC3 {
            Some(PacketCheck::Eac3ForMp4)
        } else {
            None
        };
        selected.push(Selected {
            index: stream.index(),
            time_base: stream.time_base(),
            id,
            desc: Desc { shape, config },
            hevc,
            check,
            frame_samples: ffi::frame_samples(&stream).filter(|_| kind == StreamKind::Audio),
        });
    }
    Ok(selected)
}

/// 一条轨在一组里（或一个完整文件）打开的输入与选中的流。
pub(crate) struct OpenTrack {
    pub(crate) at: Origin,
    pub(crate) input: Input,
    pub(crate) selected: Vec<Selected>,
}

/// 打开第 `group` 组的各音视频轨并选流（见 [`select`]），字幕轨不打开。`layout` 为第 0 组各轨选中的流（见 [`layout_of`]），其余组的流种类、
/// 参数与解码配置须与它相同（见 [`same_streams`]），各轨都没有确定的不同、而有项只有一边探测出时报
/// [`Unsupported::ParamsUnknown`]；打开第 0 组时为 None。
pub(crate) fn open_group(
    streams: &[Streams],
    (group, tracks): Numbered<'_>,
    layout: Option<&[Vec<Desc>]>,
    container: Container,
    cancel: &Cancel,
) -> Result<Vec<OpenTrack>, Error> {
    let mut opened = Vec::new();
    let mut unknown = None;
    for (track, segments) in tracks.tracks.iter().enumerate() {
        if streams[track] == Streams::Subtitles {
            continue;
        }
        let at = Origin::Track { group, track };
        let input = Input::open(segments, at, cancel)?;
        let selected = select(&input, streams[track], container, at)?;
        let found: Vec<&Desc> = selected.iter().map(|s| &s.desc).collect();
        if let Some(layout) = layout
            && let Some(kind) = same_streams(&found, &layout[track], at)?
        {
            unknown.get_or_insert((at, kind));
        }
        opened.push(OpenTrack {
            at,
            input,
            selected,
        });
    }
    check_timed(&opened)?;
    match unknown {
        Some((at, kind)) => Err(Error::Unsupported(Unsupported::ParamsUnknown { at, kind })),
        None => Ok(opened),
    }
}

/// 第 0 组各轨（或各完整文件）选中的流，下标为轨号（共 `tracks` 条；没打开的字幕轨为空）；依次排开即各路音视频输出流。
/// 视频只能来自一条轨，各轨合起来至少一路。
pub(crate) fn layout_of(first: &[OpenTrack], tracks: usize) -> Result<Vec<Vec<Desc>>, Error> {
    let mut layout: Vec<Vec<Desc>> = vec![Vec::new(); tracks];
    let mut video = false;
    for t in first {
        for s in &t.selected {
            if s.desc.shape.kind() == StreamKind::Video {
                if video {
                    return Err(Error::DuplicateVideo { at: t.at });
                }
                video = true;
            }
        }
        layout[t.at.index()] = t.selected.iter().map(|s| s.desc.clone()).collect();
    }
    if layout.iter().all(Vec::is_empty) {
        return Err(Error::Unsupported(Unsupported::NoStreams));
    }
    Ok(layout)
}

/// 一组里有打包音频的轨没有时间戳时，只有它一条轨贡献流才能定时。
fn check_timed(opened: &[OpenTrack]) -> Result<(), Error> {
    let contributing: Vec<&OpenTrack> = opened.iter().filter(|t| !t.selected.is_empty()).collect();
    match contributing.iter().find(|t| t.input.untimed()) {
        Some(untimed) if contributing.len() > 1 => {
            Err(Error::Unsupported(Unsupported::UntimedPackedAudio {
                at: untimed.at,
            }))
        }
        _ => Ok(()),
    }
}

/// 打开的各轨（或各完整文件）换成读包的输入：各轨选中的流按 `layout` 依次对应输出流；没有要取的流的轨不贡献。
/// `references` 见 [`references_of`]，第 0 组与完整文件为空。
pub(crate) fn sources(
    opened: Vec<OpenTrack>,
    layout: &[Vec<Desc>],
    references: &[Option<Inband>],
) -> Vec<Source> {
    let mut next = 0;
    let mut sources = Vec::new();
    for t in opened {
        let outputs: Vec<usize> = (next..next + t.selected.len()).collect();
        next += layout[t.at.index()].len();
        if !t.selected.is_empty() {
            sources.push(Source::new(
                t.at,
                t.input,
                &t.selected,
                &outputs,
                references,
            ));
        }
    }
    sources
}

/// HLS 一组打开的各轨换成读包的输入（见 [`sources`]），各自读到每路流的第一个包（见 [`Source::prime`]）。
pub(crate) fn primed(
    opened: Vec<OpenTrack>,
    layout: &[Vec<Desc>],
    references: &[Option<Inband>],
) -> Result<Vec<Source>, Error> {
    let mut sources = sources(opened, layout, references);
    for source in &mut sources {
        source.prime()?;
    }
    Ok(sources)
}

/// 第 0 组读到第一个包之后各路输出流的逐包核对，下标为输出流；其余组的核对从它开始（见 [`Inband::for_next_group`]），
/// 第一个包就与第 0 组的比较。
pub(crate) fn references_of(sources: &[Source], outputs: usize) -> Vec<Option<Inband>> {
    let mut references = vec![None; outputs];
    for source in sources {
        for (mapping, check) in source.map.iter().zip(&source.checks) {
            if let (Some(mapping), Some(PacketCheck::Inband(inband))) = (mapping, check) {
                references[mapping.output] = Some(inband.clone());
            }
        }
    }
    references
}
