//! 选出一条轨（或一个完整文件）里要取的流，打开 HLS 一组的各轨，再换成读包的输入。

use ffmpeg::{Rational, codec};
use ffmpeg_next as ffmpeg;

use crate::codecs::check_codec;
use crate::compare::{Config, Desc, same_streams};
use crate::inband::Inband;
use crate::input::Input;
use crate::nal::Framing;
use crate::source::Source;
use crate::{
    Cancel, Container, Error, HLS_CONTAINER, Numbered, Origin, StreamKind, Streams, Unsupported,
    ffi, frame,
};

/// 一条轨在某组中（或一个完整文件里）被选中的一路流。
pub(crate) struct Selected {
    pub(crate) index: usize,
    pub(crate) time_base: Rational,
    pub(crate) id: codec::Id,
    pub(crate) desc: Desc,
    /// HEVC 视频：包里 NAL 单元的排法，用于认出参考上一个 GOP 的前导帧（见 [`crate::nal::hevc_rasl`]）；其余流，及 MP4
    /// 的解码配置太短读不出排法时为 None
    pub(crate) hevc: Option<Framing>,
    /// 参数随帧携带的输入（见 [`Input::standalone`]）：从头开始的逐包核对；其余为 None
    pub(crate) inband: Option<Inband>,
    /// 参数在文件头里的 E-AC-3、输出为 MP4：逐包核对 MP4 封装器能否处理帧结构（见 [`crate::inband::eac3_for_mp4`]；
    /// 参数随帧携带的由 `inband` 核对）
    pub(crate) eac3_for_mp4: bool,
    /// 音频每帧的采样点数（FFmpeg 给的 frame_size）；视频，或帧长不固定、不知道时为 None
    pub(crate) frame_samples: Option<u32>,
    /// 视频的重排延迟（FFmpeg 给的 video_delay），帧数：流开头最多这么多个包推不出 DTS（见
    /// [`Source::read_input`]）；音频为 0
    pub(crate) video_delay: usize,
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
        let inband = if input.standalone() {
            Some(Inband::of(id, container).expect("check_codec 只收能逐帧读出参数的编码"))
        } else {
            None
        };
        let (frame_samples, video_delay) = ffi::timing(&stream);
        selected.push(Selected {
            index: stream.index(),
            time_base: stream.time_base(),
            id,
            desc: Desc { shape, config },
            hevc,
            eac3_for_mp4: inband.is_none() && container == Container::Mp4 && id == codec::Id::EAC3,
            inband,
            frame_samples: frame_samples.filter(|_| kind == StreamKind::Audio),
            video_delay,
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

/// 打开第 `group` 组的各轨并选流（见 [`select`]）。`layout` 为第 0 组各轨选中的流（见 [`layout_of`]），其余组的流种类、
/// 参数与解码配置须与它相同（见 [`same_streams`]），各轨都没有确定的不同、而有项只有一边探测出时报
/// [`Unsupported::ParamsUnknown`]；打开第 0 组时为 None。
pub(crate) fn open_group(
    streams: &[Streams],
    (group, tracks): Numbered<'_>,
    layout: Option<&[Vec<Desc>]>,
    cancel: &Cancel,
) -> Result<Vec<OpenTrack>, Error> {
    let mut opened = Vec::new();
    let mut unknown = None;
    for (track, segments) in tracks.tracks.iter().enumerate() {
        let at = Origin::Track { group, track };
        let input = Input::open(segments, at, cancel)?;
        let selected = select(&input, streams[track], HLS_CONTAINER, at)?;
        let found: Vec<&Desc> = selected.iter().map(|s| &s.desc).collect();
        if let Some(layout) = layout
            && let Some(kind) = same_streams(&found, &layout[track], group, track)?
        {
            unknown.get_or_insert((at, kind));
        }
        opened.push(OpenTrack {
            at,
            input,
            selected,
        });
    }
    check_timed(group, &opened)?;
    match unknown {
        Some((at, kind)) => Err(Error::Unsupported(Unsupported::ParamsUnknown { at, kind })),
        None => Ok(opened),
    }
}

/// 第 0 组各轨（或各完整文件）选中的流，下标为轨号；依次排开即各路输出流。同一类流只能来自一条轨，各轨合起来至少
/// 一路。
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
                at: t.at,
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
                track: untimed.at.index(),
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
        for (mapping, inband) in source.map.iter().zip(&source.inband) {
            if let (Some(mapping), Some(inband)) = (mapping, inband) {
                references[mapping.output] = Some(inband.clone());
            }
        }
    }
    references
}
