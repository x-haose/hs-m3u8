//! 一条轨在一组里的输入。同一组的各输入读出的时间戳都在来源的同一条时间线上（TS 与打包音频为 33 位的 90 kHz
//! 时钟，回绕另行对齐；fMP4 为媒体时间），整组因此共用一个偏移。

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use ffmpeg::format::context::StreamIo;
use ffmpeg::{Packet, format};
use ffmpeg_next as ffmpeg;

use crate::chain::{self, SegmentChain};
use crate::packed::Packed;
use crate::{Error, TrackSegments, standalone};

pub(crate) enum Input {
    /// TS 与 fMP4：init 段与分片按字节相连，作为一个输入读
    Chain {
        ictx: format::context::Input,
        /// 读分片文件出错时的路径与原因
        failure: chain::Failure,
        /// 有 init 段（fMP4）：编码参数都在 init 段里，不随帧携带
        init: bool,
    },
    Packed(Packed),
}

impl Input {
    /// 打开一条轨在一组里的分片：没有 init 段且第一个分片不是 TS 的为打包音频。
    pub(crate) fn open(
        segments: &TrackSegments,
        group: usize,
        track: usize,
    ) -> Result<Input, Error> {
        let init = segments.init.is_some();
        if !init && segment_head(&segments.segments[0])?.format != standalone::Format::Ts {
            return Packed::open(&segments.segments, group, track).map(Input::Packed);
        }
        let files = segments
            .init
            .iter()
            .chain(&segments.segments)
            .map(|path| (path.clone(), 0))
            .collect();
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

    /// 打包音频，而这一组有分片没有时间戳。
    pub(crate) fn untimed(&self) -> bool {
        matches!(self, Input::Packed(packed) if !packed.timed())
    }
}

/// 没有 init 段的分片开头的信息（见 [`standalone::read_head`]）；认不出格式时报 `InvalidData`。
pub(crate) fn segment_head(path: &Path) -> Result<standalone::Head, Error> {
    let failed = |cause| Error::Io {
        action: "读取",
        path: path.to_path_buf(),
        cause,
    };
    let head = File::open(path)
        .and_then(|mut file| standalone::read_head(&mut file))
        .map_err(failed)?;
    head.ok_or_else(|| {
        failed(io::Error::new(
            io::ErrorKind::InvalidData,
            "既不是 TS 也不是可识别的打包音频",
        ))
    })
}

/// 把 `files`（路径与开头不读的字节数）串成一个分片链，并返回记录文件错误的位置。
pub(crate) fn new_chain(
    files: Vec<(PathBuf, u64)>,
) -> Result<(SegmentChain, chain::Failure), Error> {
    SegmentChain::new(files).map_err(|(path, cause)| Error::Io {
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
