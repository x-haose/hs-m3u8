//! 合并完整文件：核对输入，打开各文件、选流，各自平移到 0，交错写出到临时文件，核对每路流都有包写出，换上输出。

use std::io;
use std::path::{Path, PathBuf};

use crate::chain::ffmpeg_path;
use crate::group::{OpenTrack, add_unused, layout_of, sources};
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
/// 先写到同一目录下的临时文件 `<output 的文件名>.part`（已有的同名文件或符号链接先删掉，不顺着链接写到别处），成功
/// 后改名换上 `output`（已有的替换掉）；失败或取消时 `output` 不变、临时文件删掉，进程中途退出时临时文件留着，下次
/// 合并到同一输出时删掉。`output` 与临时文件都不能是某个输入（报 [`Error::OutputIsInput`]）。
///
/// 与 HLS 的合并（见本 crate 的说明）不同之处：
/// - 输入须是普通文件，格式先由 FFmpeg 按开头的内容探测，只收 MP4 一类（含 ftyp、moov、sidx 与各 moof/mdat 连在一个
///   文件里的分段 MP4，及 M4A）、Matroska（含 WebM）、TS 与裸 AAC、MP3，认不出或不收的报 [`Error::FileFormat`]；
///   探测流参数读到 FFmpeg 的默认上限为止。
/// - 每个输入按 [`FileInput::streams`] 取第一路视频和（或）第一路音频，指定的每种都要有（否则报
///   [`Error::MissingStream`]），且都要有包写出（否则报 [`Error::EmptyInput`]，视频的包都在第一个关键帧之前时报
///   [`Unsupported::NoKeyframe`]；只取一路流的输入在写出别的内容之前就报）；视频只能来自一个输入，音频可以来自几个。
/// - 各输入的时间线互不相干：每个输入各自平移，起始时刻落在输出的 0 处（同 `ffmpeg -i 视频 -i 音频 -c copy` 的默认
///   做法）。起始时刻取所取各路流里 FFmpeg 给的最早的（第一个包的 PTS，已按容器的编辑列表、MP3 的 Info 帧等去掉编码器
///   的起始延迟）；延迟里的包落在 0 之前，由输出的编辑列表（MP4）或 CodecDelay（Matroska）跳过。不核对时间戳的推进，
///   DTS 的递增照样核对。
/// - 报错以 [`Origin::Input`] 指出是第几个输入；进度的单位为字节（见 [`crate::WriteProgress`]）。
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
    let temp = temp_of(output)?;
    let Some(path) = ffmpeg_path(&temp) else {
        return Err(Error::NonUtf8Path(output.to_path_buf()));
    };
    check_not_input(inputs, &[output, &temp])?;
    clear_temp(&temp)?;
    let written = write(inputs, container, path, control)
        .and_then(|report| sync(&temp).map(|()| report))
        .and_then(|report| {
            std::fs::rename(&temp, output)
                .map(|()| report)
                .map_err(|cause| Error::Io {
                    action: "换上",
                    path: output.to_path_buf(),
                    cause,
                })
        });
    if written.is_err() {
        // 已经在报失败的原因；临时文件删不掉时留着，下次合并到同一输出时删掉
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// 输出 `output` 的临时文件：同一目录下的 `<文件名>.part`。
fn temp_of(output: &Path) -> Result<PathBuf, Error> {
    let Some(name) = output.file_name() else {
        return Err(Error::Io {
            action: "创建",
            path: output.to_path_buf(),
            cause: io::Error::new(io::ErrorKind::InvalidInput, "输出路径不以文件名结尾"),
        });
    };
    let mut temp = name.to_os_string();
    temp.push(".part");
    Ok(output.with_file_name(temp))
}

/// 删掉上次留下的临时文件 `temp`：普通文件与符号链接（不跟随）都删；是目录时报错，不动它。
fn clear_temp(temp: &Path) -> Result<(), Error> {
    let failed = |cause| Error::Io {
        action: "删除",
        path: temp.to_path_buf(),
        cause,
    };
    match std::fs::symlink_metadata(temp) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(cause) => Err(failed(cause)),
        Ok(meta) if meta.is_dir() => Err(failed(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "临时文件的位置上是一个目录",
        ))),
        Ok(_) => std::fs::remove_file(temp).map_err(failed),
    }
}

/// `targets`（输出与它的临时文件）都不是某个输入：写临时文件时会截断它，换上输出时会替换它。不存在的不用比；输入
/// 读不到身份的由打开它时报出。
fn check_not_input(inputs: &[FileInput], targets: &[&Path]) -> Result<(), Error> {
    for target in targets {
        let Some(target_id) = identity(target).map_err(|cause| Error::Io {
            action: "读取",
            path: target.to_path_buf(),
            cause,
        })?
        else {
            continue;
        };
        for (input, file) in inputs.iter().enumerate() {
            if identity(&file.path).is_ok_and(|id| id.as_ref() == Some(&target_id)) {
                return Err(Error::OutputIsInput {
                    at: Origin::Input(input),
                });
            }
        }
    }
    Ok(())
}

/// 文件的身份，跟随符号链接，不存在时为 None。Unix 上为设备号与 inode，只读属性、不打开文件（FIFO 打开时会阻塞）；
/// Windows 上为卷序列号与文件索引，以不要求读写权限的方式打开取得。
#[cfg(unix)]
fn identity(path: &Path) -> io::Result<Option<(u64, u64)>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some((meta.dev(), meta.ino()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(windows)]
fn identity(path: &Path) -> io::Result<Option<same_file::Handle>> {
    match same_file::Handle::from_path(path) {
        Ok(handle) => Ok(Some(handle)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// 合并各文件写到 `path`（临时文件），写完回读核对每路流的包数。
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
    let layout = layout_of(&opened, inputs.len())?;
    let mut unused = Vec::new();
    add_unused(&mut unused, &opened);
    let mut muxer = Muxer::open(path, container, cancel, &opened, &layout, None, &[])?;
    let mut sources = sources(opened, &layout, &[]);
    for source in &mut sources {
        start(source)?;
    }
    // 各输入已报为写完的字节数（见 WriteProgress）
    let mut written = vec![0u64; inputs.len()];
    muxer.interleave(&mut sources, &mut [], |source, packet, _, _| {
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
    muxer.finish_verified(path, cancel, unused)
}

/// 打开第 `input` 个输入 `file`、选流（见 [`OpenTrack::new`]），另返回文件打开时的字节数。
fn open_file(
    file: &FileInput,
    container: Container,
    input: usize,
    cancel: &Cancel,
) -> Result<(OpenTrack, u64), Error> {
    let at = Origin::Input(input);
    let (opened, size) = Input::file(&file.path, input, cancel)?;
    let track = OpenTrack::new(opened, file.streams, container, at)?;
    for kind in [StreamKind::Video, StreamKind::Audio] {
        if file.streams.wants(kind) && !track.selected.iter().any(|s| s.desc.shape.kind() == kind) {
            return Err(Error::MissingStream { at, kind });
        }
    }
    Ok((track, size))
}

/// 读出一个输入的第一个包，定下它平移到输出时间线的量：起始时刻（见 [`Source::start_us`]）落在 0。读完了这个输入、
/// 而有要取的流一个包都没有时就报错（见 [`empty_stream`]），不先写出别的输入；几路流来自同一个输入时，其余的要写完
/// 才知道（见 [`check_written`]）。
fn start(source: &mut Source) -> Result<(), Error> {
    if let Some(item) = source.read_next()? {
        source.queue.push_back(item);
    }
    if source.finished {
        for (index, mapping) in source.map.iter().enumerate() {
            let Some(mapping) = mapping else { continue };
            if !source
                .queue
                .iter()
                .any(|(_, queued)| queued.output == mapping.output)
            {
                let before_keyframe = source.dropped[index].before_keyframe;
                return Err(empty_stream(source.at, mapping.kind, before_keyframe));
            }
        }
    }
    source.offset_us = -source.start_us().unwrap_or(0);
    Ok(())
}

/// 各输入要取的每路流都有包写出（见 [`empty_stream`]）。
fn check_written(muxer: &Muxer, sources: &[Source]) -> Result<(), Error> {
    for source in sources {
        for mapping in source.map.iter().flatten() {
            let (written, dropped) = muxer.outcome(mapping.output);
            if written == 0 {
                return Err(empty_stream(
                    source.at,
                    mapping.kind,
                    dropped.before_keyframe,
                ));
            }
        }
    }
    Ok(())
}

/// `at` 处的输入里 `kind` 流没有包写出：视频有 `before_keyframe` 个包在第一个关键帧之前时报
/// [`Unsupported::NoKeyframe`]，其余报 [`Error::EmptyInput`]。
fn empty_stream(at: Origin, kind: StreamKind, before_keyframe: u64) -> Error {
    if kind == StreamKind::Video && before_keyframe > 0 {
        Error::Unsupported(Unsupported::NoKeyframe { at })
    } else {
        Error::EmptyInput { at, kind }
    }
}
