//! 输出：合并为 MP4、写出可直接播放的本地 HLS，或两者都要；按选项替换已有的输出，成功后删除任务目录。

mod commit;
mod hls;
mod options;

use std::fs;
use std::path::{Path, PathBuf};

use hs_m3u8_remux::{DiscontinuityGroup, Report, Streams, TrackSegments};

pub(crate) use self::commit::recover_recorded;
pub use self::options::{OutputOptions, Target};
pub(crate) use self::options::{ResolvedOutput, absolute_path};

use self::commit::{Commit, Occupant};
use crate::error::io_error;
use crate::selection::SelectionKey;
use crate::workdir::{Layout, OutputKind, SegmentName, WorkDir};
use crate::{Error, Leftover, LiveReport, Mp4Output, Output, WorkDirProblem, blocking};

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
#[derive(Clone)]
pub(crate) struct GroupTrack {
    /// fMP4 的 init 段；None 即分片自成一体（TS 或打包音频）
    pub init: Option<PathBuf>,
    /// 按播放顺序
    pub segments: Vec<GroupSegment>,
    /// 与前一个分片的媒体序号不相接（中间缺了分片）的分片下标，升序
    pub gaps: Vec<usize>,
}

impl GroupTrack {
    /// 第 `track` 条轨在一组里的分片 `names`（按播放顺序，至少一个）在任务目录 `layout` 里的文件；init 段为第一个
    /// 分片引用的。
    pub(crate) fn of<'a>(
        layout: &Layout,
        track: usize,
        names: impl IntoIterator<Item = &'a SegmentName>,
    ) -> GroupTrack {
        let mut init = None;
        let mut segments = Vec::new();
        let mut gaps = Vec::new();
        let mut previous: Option<u64> = None;
        for name in names {
            init.get_or_insert(name.init);
            if previous.is_some_and(|p| p.checked_add(1) != Some(name.sequence)) {
                gaps.push(segments.len());
            }
            previous = Some(name.sequence);
            segments.push(GroupSegment {
                path: layout.segment(track, name),
                duration_us: name.duration_us,
            });
        }
        GroupTrack {
            init: init.flatten().map(|f| layout.init(track, f)),
            segments,
            gaps,
        }
    }
}

#[derive(Clone)]
pub(crate) struct GroupSegment {
    pub path: PathBuf,
    /// EXTINF 声明的时长，微秒
    pub duration_us: u64,
}

/// 任务开头：收拾上次写输出留下的（见 [`commit::recover`]），检查各输出能否写，再建出各输出所在的目录（见
/// [`create_dirs`]），都不等下载完。上次没能放回的旧输出原处是别的东西时报 [`Error::Cleanup`]（原因为
/// [`Error::OutputOccupied`]），要用户处理。要读写文件系统，在阻塞线程池中调用；任务目录已存在时调用方已加锁。
pub(crate) fn prepare(options: &ResolvedOutput) -> Result<(), Error> {
    let unsettled = commit::recover(&options.work_dir, &options.outputs)?;
    if let Some(first) = unsettled.first() {
        return Err(Error::Cleanup {
            failure: Box::new(Error::OutputOccupied {
                path: first.target().to_path_buf(),
                ancestor: None,
            }),
            leftovers: unsettled.iter().map(commit::unsettled).collect(),
        });
    }
    check_all(options)?;
    create_dirs(options)
}

/// 建出各输出所在的目录，已有的不动：根存在而中间建不出（没有写权限，如 macOS 上外置盘卸载后的 /Volumes）时报出。
/// 任务开头与写出前各调一次：下载期间目录被删了就重建。
fn create_dirs(options: &ResolvedOutput) -> Result<(), Error> {
    for o in &options.outputs {
        let dir = o
            .target()
            .parent()
            .expect("输出路径是以文件名结尾的绝对路径");
        fs::create_dir_all(dir).map_err(io_error("创建", dir))?;
    }
    Ok(())
}

/// 各输出的路径建不建得出来（见 [`commit::check_creatable`]）。任务开头在给任务目录加锁之前查：默认的任务目录与
/// 输出同级，输出的上级是文件时任务目录也建不出来，先查输出才报得出路径被占用。要读文件系统，在阻塞线程池中调用。
pub(crate) fn check_creatable(options: &ResolvedOutput) -> Result<(), Error> {
    options.outputs.iter().try_for_each(commit::check_creatable)
}

/// 各输出能否写（见 [`commit::check`]），顺序同各输出。
fn check_all(options: &ResolvedOutput) -> Result<Vec<Occupant>, Error> {
    options
        .outputs
        .iter()
        .map(|o| commit::check(o, options.overwrite))
        .collect()
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
        hls: options
            .get(OutputKind::Hls)
            .map(|o| o.target().to_path_buf()),
        segments,
        bytes,
        leftovers,
        live,
    })
}

/// 先把 MP4 合并到临时名、HLS 备齐到准备目录，都成功后再换上（见 [`commit`]）。合并 MP4 失败时还没有动到输出；
/// 任一步失败都撤回，等于没有写过。成功时另返回收尾没删掉的东西。
fn write_files(
    streams: &[Streams],
    groups: &[Vec<GroupTrack>],
    selection: Option<&SelectionKey>,
    options: &ResolvedOutput,
) -> Result<(Option<Mp4Output>, Vec<Leftover>), Error> {
    check_all(options)?;
    create_dirs(options)?;
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
            path: mp4.target().to_path_buf(),
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
        Some(mp4) => Some(write_mp4(mp4.temp(), streams, groups, &options.work_dir)?),
        None => None,
    };
    if let Some(hls) = options.get(OutputKind::Hls) {
        hls::stage(hls.temp(), &options.work_dir, groups, selection)?;
    }
    Ok(report)
}

/// 合并为 MP4 写到 `temp`；失败时 `temp` 上可能留有写了一半的文件。`work_dir` 为分片所在的任务目录。
fn write_mp4(
    temp: &Path,
    streams: &[Streams],
    groups: &[Vec<GroupTrack>],
    work_dir: &Path,
) -> Result<Report, Error> {
    let groups: Vec<DiscontinuityGroup> = groups.iter().map(|g| remux_group(g)).collect();
    hs_m3u8_remux::remux(streams, &groups, temp).map_err(|e| remux_error(e, work_dir))
}

/// 下载完之前核对能否合并进 MP4（见 [`hs_m3u8_remux::check`]），各轨只需开头的分片。`groups` 的各项为合并时的组号与
/// 这一组，第一项为第 0 组；`work_dir` 为分片所在的任务目录。要读文件，在阻塞线程池中调用。
pub(crate) fn check_mp4(
    streams: &[Streams],
    groups: &[(usize, Vec<GroupTrack>)],
    work_dir: &Path,
) -> Result<(), Error> {
    let remux: Vec<(usize, DiscontinuityGroup)> =
        groups.iter().map(|(n, g)| (*n, remux_group(g))).collect();
    let numbered: Vec<(usize, &DiscontinuityGroup)> = remux.iter().map(|(n, g)| (*n, g)).collect();
    hs_m3u8_remux::check(streams, &numbered).map_err(|e| remux_error(e, work_dir))
}

/// 任务目录里的分片认不出格式：下载时都核对过，认不出即目录里的文件被改动过。
pub(super) fn unrecognized_segment(work_dir: &Path, path: &Path) -> Error {
    Error::WorkDir {
        path: work_dir.to_path_buf(),
        problem: WorkDirProblem::Corrupt(format!("分片内容无法识别：{}", path.display())),
    }
}

/// 合并或核对任务目录里的分片时 remux 的错误：放不进 MP4 的为 [`crate::Unsupported::Mp4`]；认不出的分片为任务目录
/// 损坏，与写本地 HLS 时相同（见 [`unrecognized_segment`]）；其余为 [`Error::Remux`]。
fn remux_error(error: hs_m3u8_remux::Error, work_dir: &Path) -> Error {
    match error {
        hs_m3u8_remux::Error::Unsupported(unsupported) => {
            Error::Unsupported(crate::Unsupported::Mp4(unsupported))
        }
        hs_m3u8_remux::Error::Unrecognized(path) => unrecognized_segment(work_dir, &path),
        other => Error::Remux(Box::new(other)),
    }
}

fn remux_group(tracks: &[GroupTrack]) -> DiscontinuityGroup {
    DiscontinuityGroup {
        tracks: tracks
            .iter()
            .map(|t| TrackSegments {
                init: t.init.clone(),
                segments: t.segments.iter().map(|s| s.path.clone()).collect(),
                gaps: t.gaps.clone(),
            })
            .collect(),
    }
}
