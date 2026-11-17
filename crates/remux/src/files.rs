//! 合并完整文件：核对输入，打开各文件、选流，各自平移到 0，交错写出，写完核对每路流都有包写出。

use std::path::{Path, PathBuf};

use same_file::Handle;

use crate::chain::ffmpeg_path;
use crate::group::{OpenTrack, layout_of, select, sources};
use crate::input::Input;
use crate::source::Source;
use crate::write::Muxer;
use crate::{
    Cancel, Container, Control, Error, MediaReport, Origin, StreamKind, Streams, Unsupported, init,
    sync,
};

/// 合并完整文件时的一个输入：文件与它贡献的流。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInput {
    pub path: PathBuf,
    /// 要取的流种类，指定的每种都要有（见 [`merge_files`]）
    pub streams: Streams,
}

/// 把几个完整文件的流原样复制进 `output`（`container` 的格式，MP4 时 moov 前置），写完回读核对每路流的包数并落盘。
/// 失败或取消时 `output` 上可能留有写了一半的文件，由调用方删除；已存在的 `output` 会被替换，不能是某个输入（报
/// [`Error::OutputIsInput`]）。
///
/// 与 HLS 的合并（见本 crate 的说明）不同之处：
/// - 输入格式先由 FFmpeg 按开头的内容探测，只收 MP4 一类（含 ftyp、moov、sidx 与各 moof/mdat 连在一个文件里的分段
///   MP4，及 M4A）、Matroska（含 WebM）、TS 与裸 AAC、MP3，认不出或不收的报 [`Error::FileFormat`]；探测流参数读到
///   FFmpeg 的默认上限为止。
/// - 每个输入按 [`FileInput::streams`] 取第一路视频和（或）第一路音频，指定的每种都要有（否则报
///   [`Error::MissingStream`]），且都要有包写出（否则报 [`Error::EmptyInput`]，视频的包都在第一个关键帧之前时报
///   [`Unsupported::NoKeyframe`]）；同一类流只能来自一个输入。
/// - 各输入的时间线互不相干：每个输入各自平移，起始时刻落在输出的 0 处（同 `ffmpeg -i 视频 -i 音频 -c copy` 的默认
///   做法）。起始时刻取所取各路流里 FFmpeg 给的最早的（第一个包的 PTS，已按容器的编辑列表、MP3 的 Info 帧等去掉编码器
///   的起始延迟）；延迟里的包落在 0 之前，由输出的编辑列表（MP4）或 CodecDelay（Matroska）跳过。不核对时间戳的推进，
///   DTS 的递增照样核对。
/// - 音频的时间戳比采样点粗（如 Matroska 的 1 毫秒）而输出以采样率为时间基（MP4）时，按采样点接续，每个样本的时长
///   都是整帧（同 ffmpeg 命令行复制音频时的做法）。
/// - 报错以 [`Origin::Input`] 或 `input` 字段指出是第几个输入；进度的单位为字节（见 [`crate::WriteProgress`]）。
pub fn merge_files(
    inputs: &[FileInput],
    output: &Path,
    container: Container,
    control: &Control,
) -> Result<MediaReport, Error> {
    init()?;
    if inputs.is_empty() {
        return Err(Error::NoInputs);
    }
    let Some(path) = ffmpeg_path(output) else {
        return Err(Error::NonUtf8Path(output.to_path_buf()));
    };
    check_not_input(inputs, output)?;
    let report = write(inputs, container, path, control)?;
    sync(output)?;
    Ok(report)
}

/// 输出 `output` 不是某个输入：写输出时先截断它，读到一半的输入就毁了。输出还不存在时不用比；输入打不开的由打开它时
/// 报出。
fn check_not_input(inputs: &[FileInput], output: &Path) -> Result<(), Error> {
    let target = match Handle::from_path(output) {
        Ok(target) => target,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(cause) => {
            return Err(Error::Io {
                action: "打开",
                path: output.to_path_buf(),
                cause,
            });
        }
    };
    for (input, file) in inputs.iter().enumerate() {
        if Handle::from_path(&file.path).is_ok_and(|handle| handle == target) {
            return Err(Error::OutputIsInput { input });
        }
    }
    Ok(())
}

/// 合并各文件写到 `path`，写完回读核对每路流的包数。
fn write(
    inputs: &[FileInput],
    container: Container,
    path: &str,
    control: &Control,
) -> Result<MediaReport, Error> {
    let cancel = &control.cancel;
    let mut opened = Vec::with_capacity(inputs.len());
    let mut sizes = Vec::with_capacity(inputs.len());
    for (input, file) in inputs.iter().enumerate() {
        let (track, size) = open_file(file, container, input, cancel)?;
        opened.push(track);
        sizes.push(size);
    }
    let layout = layout_of(&opened)?;
    let mut muxer = Muxer::open(path, container, cancel, &opened, &layout)?;
    let mut sources = sources(opened, &layout, &[]);
    for source in &mut sources {
        start(source)?;
    }
    // 各输入已报为写完的字节数（见 WriteProgress）
    let mut written = vec![0u64; inputs.len()];
    muxer.interleave(&mut sources, |source, packet, _, _| {
        if let Ok(position) = u64::try_from(packet.position()) {
            let done = &mut written[source.at.index()];
            control.progress.add(position.saturating_sub(*done));
            *done = (*done).max(position);
        }
        Ok(())
    })?;
    for (size, done) in sizes.iter().zip(written) {
        control.progress.add(size.saturating_sub(done));
    }
    check_written(&muxer, &sources)?;
    muxer.finish_verified(path, cancel)
}

/// 打开第 `input` 个输入 `file`、选流（见 [`select`]），另返回文件打开时的字节数。
fn open_file(
    file: &FileInput,
    container: Container,
    input: usize,
    cancel: &Cancel,
) -> Result<(OpenTrack, u64), Error> {
    let at = Origin::Input(input);
    let (opened, size) = Input::file(&file.path, input, cancel)?;
    let selected = select(&opened, file.streams, container, at)?;
    for kind in [StreamKind::Video, StreamKind::Audio] {
        if file.streams.wants(kind) && !selected.iter().any(|s| s.desc.shape.kind() == kind) {
            return Err(Error::MissingStream { input, kind });
        }
    }
    let track = OpenTrack {
        at,
        input: opened,
        selected,
    };
    Ok((track, size))
}

/// 定下一个输入平移到输出时间线的量：起始时刻（见 [`Source::start_us`]）落在 0。FFmpeg 没给起始时刻时先读一个包；
/// 一个包都读不出时不平移（写完后报 [`Error::EmptyInput`]）。
fn start(source: &mut Source) -> Result<(), Error> {
    if source.start_us().is_none()
        && let Some(item) = source.read_next()?
    {
        source.queue.push_back(item);
    }
    source.offset_us = -source.start_us().unwrap_or(0);
    Ok(())
}

/// 各输入要取的每路流都有包写出：没有时视频的包都在第一个关键帧之前报 [`Unsupported::NoKeyframe`]，其余报
/// [`Error::EmptyInput`]。
fn check_written(muxer: &Muxer, sources: &[Source]) -> Result<(), Error> {
    for source in sources {
        for mapping in source.map.iter().flatten() {
            let (written, dropped) = muxer.outcome(mapping.output);
            if written > 0 {
                continue;
            }
            if mapping.kind == StreamKind::Video && dropped.before_keyframe > 0 {
                return Err(Error::Unsupported(Unsupported::NoKeyframe {
                    at: source.at,
                }));
            }
            return Err(Error::EmptyInput {
                input: source.at.index(),
                kind: mapping.kind,
            });
        }
    }
    Ok(())
}
