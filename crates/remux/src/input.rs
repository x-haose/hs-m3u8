//! 一条轨在一组里的输入。同一组的各输入读出的时间戳都在来源的同一条时间线上（TS 与打包音频为 33 位的 90 kHz
//! 时钟，回绕另行对齐；fMP4 为媒体时间），整组因此共用一个偏移。

use std::fs::File;
use std::ops::Range;
use std::path::{Path, PathBuf};

use ffmpeg::format::context::StreamIo;
use ffmpeg::{Packet, format};
use ffmpeg_next as ffmpeg;

use crate::chain::{self, SegmentChain};
use crate::packed::Packed;
use crate::{Error, TrackSegments, Unsupported, fmp4, standalone};

pub(crate) enum Input {
    /// TS 与 fMP4：init 段与分片按字节相连，作为一个输入读
    Chain {
        ictx: format::context::Input,
        /// 读分片文件出错时的路径与原因
        failure: chain::Failure,
        /// 有 init 段（fMP4）：编码参数都在 init 段里，不随帧携带
        init: bool,
    },
    Packed(Box<Packed>),
}

impl Input {
    /// 打开一条轨在一组里的分片。没有 init 段的分片须同一格式（见 [`Unsupported::FormatChangedInGroup`]），不是 TS
    /// 的为打包音频。
    pub(crate) fn open(
        segments: &TrackSegments,
        group: usize,
        track: usize,
    ) -> Result<Input, Error> {
        let init = segments.init.is_some();
        if !init {
            let heads = segments
                .segments
                .iter()
                .map(|path| segment_head(path))
                .collect::<Result<Vec<_>, Error>>()?;
            if heads.iter().any(|h| h.format != heads[0].format) {
                return Err(Error::Unsupported(Unsupported::FormatChangedInGroup {
                    group,
                    track,
                }));
            }
            if heads[0].format != standalone::Format::Ts {
                return Packed::open(segments, &heads, group, track)
                    .map(|packed| Input::Packed(Box::new(packed)));
            }
        }
        let mut files = Vec::with_capacity(segments.segments.len() + 1);
        for path in segments.init.iter().chain(&segments.segments) {
            let len = if init {
                fmp4_len(path)?
            } else {
                std::fs::metadata(path).map_err(read_error(path))?.len()
            };
            files.push((path.clone(), 0..len));
        }
        let (chain, failure) = new_chain(files)?;
        let ictx = open_chain(chain, &failure, group, track)?;
        Ok(Input::Chain {
            ictx,
            failure,
            init,
        })
    }

    pub(crate) fn context(&self) -> &format::context::Input {
        match self {
            Input::Chain { ictx, .. } => ictx,
            Input::Packed(packed) => packed.context(),
        }
    }

    /// 读下一个包；读到末尾返回 false。
    pub(crate) fn read(
        &mut self,
        packet: &mut Packet,
        group: usize,
        track: usize,
    ) -> Result<bool, Error> {
        match self {
            Input::Chain { ictx, failure, .. } => read_packet(ictx, failure, packet, group, track),
            Input::Packed(packed) => packed.read(packet, group, track),
        }
    }

    /// 没有 init 段的分片（TS 与打包音频）：编码参数随帧携带，要逐包核对（见 [`crate::inband`]）；时间戳在 33 位的
    /// 90 kHz 时钟上，会回绕。
    pub(crate) fn standalone(&self) -> bool {
        !matches!(self, Input::Chain { init: true, .. })
    }

    /// 打包音频丢弃的损坏字节：音频流的下标与字节数（见 [`Packed`]，读到末尾后才是全部）；其余输入为 None。
    pub(crate) fn discarded(&self) -> Option<(usize, u64)> {
        match self {
            Input::Packed(packed) => packed.discarded(),
            Input::Chain { .. } => None,
        }
    }

    /// 打包音频，而这一组有分片没有时间戳。
    pub(crate) fn untimed(&self) -> bool {
        matches!(self, Input::Packed(packed) if !packed.timed())
    }
}

/// fMP4 的 init 段或分片 `path` 的字节数：顶层 box 须恰好铺满（否则报 [`Error::Fmp4`]）、最后一个的长度不写 0（否则
/// 报 [`Error::Fmp4OpenEnded`]）。
fn fmp4_len(path: &Path) -> Result<u64, Error> {
    let mut file = File::open(path).map_err(read_error(path))?;
    let boxes = fmp4::boxes(&mut file)
        .map_err(read_error(path))?
        .map_err(|problem| Error::Fmp4 {
            path: path.to_path_buf(),
            problem,
        })?;
    if let Some(open) = boxes.iter().find(|b| b.to_end) {
        return Err(Error::Fmp4OpenEnded {
            path: path.to_path_buf(),
            at: open.at,
        });
    }
    Ok(boxes.last().map_or(0, |b| b.at + b.size))
}

/// 读文件 `path` 失败。
fn read_error(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |cause| Error::Io {
        action: "读取",
        path: path.to_path_buf(),
        cause,
    }
}

/// 没有 init 段的分片开头的信息（见 [`standalone::read_head`]）；认不出格式时报 [`Error::Unrecognized`]。
pub(crate) fn segment_head(path: &Path) -> Result<standalone::Head, Error> {
    let head = File::open(path)
        .and_then(|mut file| standalone::read_head(&mut file))
        .map_err(read_error(path))?;
    head.ok_or_else(|| Error::Unrecognized(path.to_path_buf()))
}

/// 把 `pieces`（文件与其中要读的字节区间）串成一个分片链，并返回记录文件错误的位置。
pub(crate) fn new_chain(
    pieces: Vec<(PathBuf, Range<u64>)>,
) -> Result<(SegmentChain, chain::Failure), Error> {
    SegmentChain::new(pieces).map_err(|(path, cause)| Error::Io {
        action: "读取",
        path,
        cause,
    })
}

/// 把分片链当作一个输入交给 FFmpeg 打开，格式由开头的内容识别。
pub(crate) fn open_chain(
    chain: SegmentChain,
    failure: &chain::Failure,
    group: usize,
    track: usize,
) -> Result<format::context::Input, Error> {
    StreamIo::from_read_seek(chain)
        .and_then(|io| format::input_from_stream(io, None, None))
        .map_err(|cause| {
            read_failure(failure).unwrap_or(Error::OpenInput {
                group,
                track,
                cause: cause.into(),
            })
        })
}

/// 读下一个包；读到末尾返回 false。用 `Packet::read` 而不是 ffmpeg-next 的 `packets()` 迭代器：后者把读错误当作读到
/// 末尾，并静默跳过 `InvalidData` 的包。
pub(crate) fn read_packet(
    ictx: &mut format::context::Input,
    failure: &chain::Failure,
    packet: &mut Packet,
    group: usize,
    track: usize,
) -> Result<bool, Error> {
    match packet.read(ictx) {
        Ok(()) => Ok(true),
        Err(ffmpeg::Error::Eof) => Ok(false),
        Err(cause) => Err(read_failure(failure).unwrap_or(Error::Read {
            group,
            track,
            cause: cause.into(),
        })),
    }
}

/// 分片链记下的文件错误；FFmpeg 只拿到错误码，这里换回带路径的原始错误。
fn read_failure(failure: &chain::Failure) -> Option<Error> {
    let (path, cause) = failure.lock().unwrap_or_else(|p| p.into_inner()).take()?;
    Some(Error::Io {
        action: "读取",
        path,
        cause,
    })
}
