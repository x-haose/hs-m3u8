//! 打开一组各轨的分片、选出要取的流，再换成读包的输入。

use ffmpeg::{Rational, codec};
use ffmpeg_next as ffmpeg;

use crate::codecs::check_codec;
use crate::compare::{Config, Desc, same_streams};
use crate::inband::Inband;
use crate::input::Input;
use crate::nal::Framing;
use crate::source::Source;
use crate::{Cancel, Error, Numbered, StreamKind, Streams, TrackSegments, Unsupported, ffi, frame};

/// 一条轨在某组中被选中的一路流。
pub(crate) struct Selected {
    pub(crate) index: usize,
    pub(crate) time_base: Rational,
    pub(crate) id: codec::Id,
    pub(crate) desc: Desc,
    /// HEVC 视频：包里 NAL 单元的排法，用于认出参考上一个 GOP 的前导帧（见 [`crate::nal::hevc_rasl`]）；其余流，及 MP4
    /// 的解码配置太短读不出排法时为 None
    pub(crate) hevc: Option<Framing>,
}

/// 打开一条轨在一组里的分片，按 `wanted` 选出第一路视频与（或）第一路音频，并检查编码是否受支持。
fn open_selected(
    segments: &TrackSegments,
    wanted: Streams,
    group: usize,
    track: usize,
    cancel: &Cancel,
) -> Result<(Input, Vec<Selected>), Error> {
    let input = Input::open(segments, group, track, cancel)?;
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
        check_codec(id, &shape, input.standalone(), group, track).map_err(Error::Unsupported)?;
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
        selected.push(Selected {
            index: stream.index(),
            time_base: stream.time_base(),
            id,
            desc: Desc { shape, config },
            hevc,
        });
    }
    Ok((input, selected))
}

/// 一条轨在一组里打开的输入与选中的流。
pub(crate) struct OpenTrack {
    pub(crate) track: usize,
    pub(crate) input: Input,
    pub(crate) selected: Vec<Selected>,
}

/// 打开第 `group` 组的各轨并选流（见 [`open_selected`]）。`layout` 为第 0 组各轨选中的流（见
/// [`layout_of`]），其余组的流种类、参数与解码配置须与它相同（见 [`same_streams`]），各轨都没有确定的
/// 不同、而有项只有一边探测出时报 [`Unsupported::ParamsUnknown`]；打开第 0 组时为 None。
pub(crate) fn open_group(
    streams: &[Streams],
    (group, tracks): Numbered<'_>,
    layout: Option<&[Vec<Desc>]>,
    cancel: &Cancel,
) -> Result<Vec<OpenTrack>, Error> {
    let mut opened = Vec::new();
    let mut unknown = None;
    for (track, segments) in tracks.tracks.iter().enumerate() {
        let (input, selected) = open_selected(segments, streams[track], group, track, cancel)?;
        let found: Vec<&Desc> = selected.iter().map(|s| &s.desc).collect();
        if let Some(layout) = layout
            && let Some(kind) = same_streams(&found, &layout[track], group, track)?
        {
            unknown.get_or_insert((track, kind));
        }
        opened.push(OpenTrack {
            track,
            input,
            selected,
        });
    }
    check_timed(group, &opened)?;
    match unknown {
        Some((track, kind)) => Err(Error::Unsupported(Unsupported::ParamsUnknown {
            group,
            track,
            kind,
        })),
        None => Ok(opened),
    }
}

/// 第 0 组各轨选中的流，下标为轨道号；依次排开即各路输出流。同一类流只能来自一条轨，各轨合起来至少一路。
pub(crate) fn layout_of(first: &[OpenTrack]) -> Result<Vec<Vec<Desc>>, Error> {
    let mut layout: Vec<Vec<Desc>> = Vec::new();
    for t in first {
        let duplicate = t.selected.iter().find(|s| {
            layout
                .iter()
                .flatten()
                .any(|d| d.shape.kind() == s.desc.shape.kind())
        });
        if let Some(s) = duplicate {
            return Err(Error::DuplicateKind {
                track: t.track,
                kind: s.desc.shape.kind(),
            });
        }
        layout.push(t.selected.iter().map(|s| s.desc.clone()).collect());
    }
    if layout.iter().all(Vec::is_empty) {
        return Err(Error::Unsupported(Unsupported::NoStreams));
    }
    Ok(layout)
}

/// 一组里有打包音频的轨没有时间戳时，只有它一条轨贡献流才能定时。
fn check_timed(group: usize, opened: &[OpenTrack]) -> Result<(), Error> {
    let contributing: Vec<&OpenTrack> = opened.iter().filter(|t| !t.selected.is_empty()).collect();
    match contributing.iter().find(|t| t.input.untimed()) {
        Some(untimed) if contributing.len() > 1 => {
            Err(Error::Unsupported(Unsupported::UntimedPackedAudio {
                group,
                track: untimed.track,
            }))
        }
        _ => Ok(()),
    }
}

/// 一组打开的各轨换成读包的输入，各自读到每路流的第一个包（见 [`Source::prime`]）：各轨选中的流按 `layout` 依次
/// 对应输出流；没有要取的流的轨不贡献，不读它的包。`references` 见 [`references_of`]，第 0 组为空。
pub(crate) fn primed(
    group: usize,
    opened: Vec<OpenTrack>,
    layout: &[Vec<Desc>],
    references: &[Option<Inband>],
) -> Result<Vec<Source>, Error> {
    let mut next = 0;
    let mut sources = Vec::new();
    for t in opened {
        let outputs: Vec<usize> = (next..next + t.selected.len()).collect();
        next += layout[t.track].len();
        if !t.selected.is_empty() {
            let mut source =
                Source::new(group, t.track, t.input, &t.selected, &outputs, references);
            source.prime()?;
            sources.push(source);
        }
    }
    Ok(sources)
}

/// 第 0 组读到第一个包之后各路输出流的逐包核对，下标为输出流；其余组的核对从它开始（见 [`Inband::for_next_group`]），
/// 第一个包就与第 0 组的比较。
pub(crate) fn references_of(sources: &[Source], outputs: usize) -> Vec<Option<Inband>> {
    let mut references = vec![None; outputs];
    for source in sources {
        for (mapping, inband) in source.map.iter().zip(&source.inband) {
            if let (Some(mapping), Some(inband)) = (mapping, inband) {
                references[mapping.output] = Some(inband.clone());
            }
        }
    }
    references
}
