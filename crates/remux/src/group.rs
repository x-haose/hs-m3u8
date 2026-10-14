//! 打开一组各轨的分片、选出要取的流，再换成读包的输入。

use ffmpeg::{Rational, codec};
use ffmpeg_next as ffmpeg;

use crate::codecs::check_codec;
use crate::compare::{Config, Desc, same_streams};
use crate::inband::Inband;
use crate::input::Input;
use crate::source::Source;
use crate::{Error, Numbered, StreamKind, Streams, TrackSegments, Unsupported, ffi, frame};

/// 一条轨在某组中被选中的一路流。
pub(crate) struct Selected {
    pub(crate) index: usize,
    pub(crate) time_base: Rational,
    pub(crate) id: codec::Id,
    pub(crate) desc: Desc,
}

/// 打开一条轨在一组里的分片，按 `wanted` 选出第一路视频与（或）第一路音频，并检查编码是否受支持。
fn open_selected(
    segments: &TrackSegments,
    wanted: Streams,
    group: usize,
    track: usize,
) -> Result<(Input, Vec<Selected>), Error> {
    let input = Input::open(segments, group, track)?;
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
        selected.push(Selected {
            index: stream.index(),
            time_base: stream.time_base(),
            id,
            desc: Desc { shape, config },
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

/// 打开第 `group` 组的各轨并选流（见 [`open_selected`]）。`layout` 为第 0 组各轨选中的流（见 [`crate::compare::layout_of`]），其余组
/// 的流种类、参数与解码配置须与它相同（见 [`same_streams`]），各轨都没有确定的不同、而有项只有一边探测出时报
/// [`Unsupported::ParamsUnknown`]；打开第 0 组时为 None。
pub(crate) fn open_group(
    streams: &[Streams],
    (group, tracks): Numbered<'_>,
    layout: Option<&[Vec<Desc>]>,
) -> Result<Vec<OpenTrack>, Error> {
    let mut opened = Vec::new();
    let mut unknown = None;
    for (track, segments) in tracks.tracks.iter().enumerate() {
        let (input, selected) = open_selected(segments, streams[track], group, track)?;
        if let Some(layout) = layout
            && let Some(kind) = same_streams(&selected, &layout[track], group, track)?
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
