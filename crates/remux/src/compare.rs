//! 后续组与第 0 组的流布局、编码参数与解码配置的比较：MP4 的样本描述由第 0 组的写成，其余组须相同。

use crate::group::{OpenTrack, Selected};
use crate::{Error, Shape, StreamKind, Unsupported, ffi};

impl Shape {
    /// 与第 0 组同一路流的编码参数比较；视频的像素格式可能探测不出，其余各项在打开时已确认探测得出。
    fn compare(&self, first: &Shape) -> Compared {
        match (*self, *first) {
            (
                Shape::Video {
                    codec,
                    width,
                    height,
                    pixel_format,
                },
                Shape::Video {
                    codec: first_codec,
                    width: first_width,
                    height: first_height,
                    pixel_format: first_pixel_format,
                },
            ) if (codec, width, height) == (first_codec, first_width, first_height) => {
                Compared::of(pixel_format, first_pixel_format)
            }
            (found, first) => Compared::of(Some(found), Some(first)),
        }
    }
}

/// 一路流的编码参数与解码配置；MP4 的样本描述由第 0 组的写成，其余组须相同（见 [`same_streams`]）。
#[derive(Clone)]
pub(crate) struct Desc {
    pub(crate) shape: Shape,
    pub(crate) config: Config,
}

impl Desc {
    /// 与第 0 组同一路流比较，依次为编码参数与解码配置的结果。fMP4 的 AAC：配置字节相同则都相同；不同时比解出一帧后
    /// 的参数（隐式 SBR、PS 的采样率与声道数也要解出帧才知道）；有一边没解出帧时解码配置比不了，采样率与声道数按两边
    /// 可能的取值比（见 [`aac_outputs`]），一种都对不上才算不同。
    fn compare(&self, first: &Desc) -> (Compared, Compared) {
        match (&self.config, &first.config) {
            (
                Config::InitAac { config, decoded },
                Config::InitAac {
                    config: first_config,
                    decoded: first_decoded,
                },
            ) => {
                if config == first_config {
                    return (Compared::Same, Compared::Same);
                }
                match (decoded, first_decoded) {
                    (Some(decoded), Some(first_decoded)) => (
                        self.shape.compare(&first.shape),
                        Compared::of(Some(decoded), Some(first_decoded)),
                    ),
                    _ => {
                        let found = aac_outputs(&self.shape, decoded.is_some());
                        let first = aac_outputs(&first.shape, first_decoded.is_some());
                        let shape = if found.iter().any(|output| first.contains(output)) {
                            Compared::Unknown
                        } else {
                            Compared::Differs
                        };
                        (shape, Compared::Unknown)
                    }
                }
            }
            (config, first_config) => (
                self.shape.compare(&first.shape),
                Compared::of(Some(config), Some(first_config)),
            ),
        }
    }
}

/// fMP4 里 AAC 流（`shape`）解码后可能的采样率与声道数：解出了帧（`decoded`）的就是探测到的；没解出的按配置得出，
/// 隐式 SBR 解出后采样率加倍、PS 解出后单声道变双声道，都有可能。
fn aac_outputs(shape: &Shape, decoded: bool) -> Vec<(u32, u32)> {
    let Shape::Audio {
        sample_rate,
        channels,
        ..
    } = *shape
    else {
        unreachable!("解码配置为 AAC 的是音频流");
    };
    if decoded {
        return vec![(sample_rate, channels)];
    }
    let channels: &[u32] = if channels == 1 { &[1, 2] } else { &[channels] };
    [sample_rate, sample_rate * 2]
        .into_iter()
        .flat_map(|rate| channels.iter().map(move |&channels| (rate, channels)))
        .collect()
}

/// 一路流的解码配置。
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Config {
    /// 有 init 段（fMP4）：init 段里的解码配置（codecpar 的 extradata，如 avcC），样本里不带，按字节比较：解码器只按
    /// 样本描述配置，FLAC 与 ALAC 的配置里的帧长上限还用来分配缓冲区。USAC 的配置带有 profile 与帧长之外的解码参数，
    /// 也按字节比较
    Init(Vec<u8>),
    /// fMP4 里 USAC 以外的 AAC：init 段里的 AudioSpecificConfig，与探测时解出一帧后得出的参数（没有解出帧、如开头的
    /// 分片里没有样本时为 None）。配置有等价的不同写法（如显式写出没有 SBR，或省略），见 [`Desc::compare`]
    InitAac {
        config: Vec<u8>,
        decoded: Option<ffi::DecodedAac>,
    },
    /// 参数随帧携带（TS 与打包音频）：音频以第 0 组第一个包的帧头为准逐包核对（见 [`crate::inband::Inband`]）；视频的参数集在关键帧
    /// 前重复、解码器按带内的解码，只核对编码参数（宽高与像素格式，见 [`Shape`]）
    Inband,
}

/// 后续组的一项参数与第 0 组比较的结果。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Compared {
    Same,
    /// 比不了：只有一边探测出，或 fMP4 里配置不同的 AAC 有一边没解出帧（见 [`Desc::compare`]）；其余两边都探测不出的
    /// 项不比，算相同
    Unknown,
    Differs,
}

impl Compared {
    /// 两边各自探测不出时为 None 的一项。
    fn of<T: PartialEq>(found: Option<T>, first: Option<T>) -> Compared {
        match (found, first) {
            (Some(found), Some(first)) if found != first => Compared::Differs,
            (Some(_), None) | (None, Some(_)) => Compared::Unknown,
            _ => Compared::Same,
        }
    }
}

/// 一条轨这一组选中的流与第 0 组的 `first` 比较：种类、编码参数或解码配置不同时报错；其余各项相同、有项只有一边
/// 探测出时返回第一路这样的流的种类。
pub(crate) fn same_streams(
    selected: &[Selected],
    first: &[Desc],
    group: usize,
    track: usize,
) -> Result<Option<StreamKind>, Error> {
    if selected.len() != first.len()
        || selected
            .iter()
            .zip(first)
            .any(|(s, f)| s.desc.shape.kind() != f.shape.kind())
    {
        return Err(Error::Unsupported(Unsupported::LayoutChanged {
            group,
            track,
        }));
    }
    let mut unknown = None;
    for (s, f) in selected.iter().zip(first) {
        let kind = f.shape.kind();
        let (shape, config) = s.desc.compare(f);
        if shape == Compared::Differs {
            return Err(Error::Unsupported(Unsupported::ParamsChanged {
                group,
                track,
                kind,
                first: f.shape,
                found: s.desc.shape,
            }));
        }
        if config == Compared::Differs {
            return Err(Error::Unsupported(Unsupported::ConfigChanged {
                group,
                track,
                kind,
            }));
        }
        if shape == Compared::Unknown || config == Compared::Unknown {
            unknown.get_or_insert(kind);
        }
    }
    Ok(unknown)
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
