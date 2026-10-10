//! 输出：合并为 MP4、写出可直接播放的本地 HLS，或两者都要；按选项替换已有的输出，成功后删除任务目录。

mod commit;
mod hls;
mod options;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use hs_m3u8_remux::{DiscontinuityGroup, Report, Streams, TrackSegments};

pub(crate) use self::options::ResolvedOutput;
pub use self::options::{OutputOptions, Target};

use self::commit::{Commit, Existing};
use crate::error::io_error;
use crate::selection::SelectionKey;
use crate::workdir::{OutputKind, PendingOutput, WorkDir};
use crate::{Error, Leftover, LiveReport, Mp4Output, Output, blocking};

/// 交给输出的内容。
pub(crate) struct Content {
    /// 各轨的取流方式（合并 MP4 用）
    pub streams: Vec<Streams>,
    /// 各不连续段组，每组各轨一项，轨道顺序与 `streams` 一致；至少一组，每项至少一个分片
    pub groups: Vec<Vec<GroupTrack>>,
    /// 输出的分片数，各轨合计
    pub segments: usize,
    /// 记进任务目录的选轨身份；来源本身是媒体播放列表时为 None
    pub selection: Option<SelectionKey>,
    pub live: Option<LiveReport>,
}

/// 一组里一条轨的内容。
pub(crate) struct GroupTrack {
    /// fMP4 的 init 段；None 即分片自成一体（TS 或打包音频）
    pub init: Option<PathBuf>,
    /// 按播放顺序
    pub segments: Vec<GroupSegment>,
}

pub(crate) struct GroupSegment {
    pub path: PathBuf,
    /// EXTINF 声明的时长，微秒
    pub duration_us: u64,
}

/// 任务开头：收拾上次写输出留下的（见 [`commit::recover`]），再检查各输出能否写。要读写文件系统，在阻塞线程池中
/// 调用；任务目录已存在时调用方已加锁。
pub(crate) fn prepare(options: &ResolvedOutput) -> Result<(), Error> {
    commit::recover(&options.work_dir, &options.outputs)?;
    check_all(options)?;
    Ok(())
}

/// 各输出能否写（见 [`check`]），顺序同各输出。
fn check_all(options: &ResolvedOutput) -> Result<Vec<Existing>, Error> {
    options
        .outputs
        .iter()
        .map(|o| check(o, options.overwrite))
        .collect()
}

/// 输出能否写，能写时返回路径上已有的东西：MP4 不存在，或是文件且允许覆盖；HLS 目录见 [`hls::check`]。
fn check(output: &PendingOutput, overwrite: bool) -> Result<Existing, Error> {
    let target = &output.target;
    match output.kind {
        OutputKind::Mp4 => match fs::symlink_metadata(target) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Existing::Nothing),
            Err(cause) => Err(io_error("检查", target)(cause)),
            Ok(meta) if meta.is_dir() => Err(Error::OutputOccupied(target.clone())),
            Ok(_) if !overwrite => Err(Error::OutputExists(target.clone())),
            Ok(_) => Ok(Existing::Output),
        },
        OutputKind::Hls => hls::check(target, overwrite),
    }
}

/// 写出输出，按选项删除任务目录。`bytes` 为任务目录中已完成的分片与 init 段的字节数。
pub(crate) async fn write(
    dir: WorkDir,
    content: Content,
    options: &ResolvedOutput,
    bytes: u64,
) -> Result<Output, Error> {
    let Content {
        streams,
        groups,
        segments,
        selection,
        live,
    } = content;
    let files = options.clone();
    let (mp4, mut leftovers) =
        blocking(move || write_files(&streams, &groups, selection.as_ref(), &files)).await??;
    if !options.keep_work_dir {
        leftovers.extend(dir.remove().await);
    }
    Ok(Output {
        mp4,
        hls: options.get(OutputKind::Hls).map(|o| o.target.clone()),
        segments,
        bytes,
        leftovers,
        live,
    })
}

/// 先把 MP4 合并到临时名、HLS 备齐到准备目录，都成功后再换上（见 [`commit`]）。最常见的失败（编码不受支持）发生在
/// 合并 MP4 时，此时还没有动到输出；任一步失败都撤回，等于没有写过。成功时另返回收尾没删掉的东西。
fn write_files(
    streams: &[Streams],
    groups: &[Vec<GroupTrack>],
    selection: Option<&SelectionKey>,
    options: &ResolvedOutput,
) -> Result<(Option<Mp4Output>, Vec<Leftover>), Error> {
    check_all(options)?;
    if options.get(OutputKind::Hls).is_some() {
        hls::check_content(groups, selection)?;
    }
    let commit = Commit::begin(&options.work_dir, &options.outputs)?;
    let written = write_temps(streams, groups, selection, options).and_then(|report| {
        // 写出期间输出路径可能已被别人占用；开始时检查过，换上前再查一次，按这次的结果换
        Ok((report, check_all(options)?))
    });
    let (report, existing) = match written {
        Ok(written) => written,
        Err(failure) => return Err(commit.abandon(failure)),
    };
    let leftovers = commit.swap(&existing)?;
    let mp4 = options
        .get(OutputKind::Mp4)
        .zip(report)
        .map(|(mp4, report)| Mp4Output {
            path: mp4.target.clone(),
            report,
        });
    Ok((mp4, leftovers))
}

/// 写出各临时输出；失败时可能留有写了一半的，由调用方删除。
fn write_temps(
    streams: &[Streams],
    groups: &[Vec<GroupTrack>],
    selection: Option<&SelectionKey>,
    options: &ResolvedOutput,
) -> Result<Option<Report>, Error> {
    let report = match options.get(OutputKind::Mp4) {
        Some(mp4) => Some(write_mp4(&mp4.temp, streams, groups)?),
        None => None,
    };
    if let Some(hls) = options.get(OutputKind::Hls) {
        hls::stage(&hls.temp, &options.work_dir, groups, selection)?;
    }
    Ok(report)
}

/// 合并为 MP4 写到 `temp`；失败时 `temp` 上可能留有写了一半的文件。
fn write_mp4(
    temp: &Path,
    streams: &[Streams],
    groups: &[Vec<GroupTrack>],
) -> Result<Report, Error> {
    let groups: Vec<DiscontinuityGroup> = groups
        .iter()
        .map(|tracks| DiscontinuityGroup {
            tracks: tracks
                .iter()
                .map(|t| TrackSegments {
                    init: t.init.clone(),
                    segments: t.segments.iter().map(|s| s.path.clone()).collect(),
                })
                .collect(),
        })
        .collect();
    if let Some(parent) = temp.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(io_error("创建", parent))?;
    }
    Ok(hs_m3u8_remux::remux(streams, &groups, temp)?)
}
