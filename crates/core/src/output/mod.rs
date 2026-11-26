//! 输出：合并为 MP4 或 Matroska（另可把字幕另存为文件）、写出可直接播放的本地 HLS，或两者都要；按选项替换已有的输出，成功后删除任务目录。

mod commit;
mod hls;
mod options;

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use hs_m3u8_remux::{
    Cancel, Container, Control, DiscontinuityGroup, MediaReport, Merged, Segment, Streams, Track,
    TrackSegments, WriteProgress,
};
use hs_m3u8_subtitle::Cue;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(crate) use self::commit::recover_recorded;
pub use self::options::{
    FileOutputOptions, MergedFile, OutputOptions, SubtitleFileFormat, SubtitleOutput, Target,
};
pub(crate) use self::options::{ResolvedOutput, absolute_path};

use self::commit::{Commit, Occupant};
use crate::error::io_error;
use crate::selection::{SelectionRecord, track_labels};
use crate::workdir::{FileDir, Layout, OutputKind, PendingOutput, SegmentName, WorkDir};
use crate::{
    Error, FileCount, Leftover, LeftoverKind, LiveReport, MergedOutput, Missed, Output, Progress,
    WorkDirProblem, Writing, blocking,
};

/// 交给输出的内容。
pub(crate) struct Content {
    /// 各轨的取流方式（合并用）
    pub streams: Vec<Streams>,
    /// 各不连续段组，每组各轨一项，轨道顺序与 `streams` 一致；至少一组，每项至少一个分片
    pub groups: Vec<Vec<GroupTrack>>,
    /// 输出的分片数，各轨合计
    pub segments: usize,
    /// 记进任务目录的选轨；来源本身是媒体播放列表时为 None
    pub selection: Option<SelectionRecord>,
    /// 见 [`Output::missed`]
    pub missed: Vec<Missed>,
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
            init.get_or_insert_with(|| layout.segment_init(track, name));
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
            init: init.flatten(),
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
/// [`create_dirs`]），都不等下载完。有留着的旧输出（上次没能放回而原处是别的东西，或没有记录）时报
/// [`Error::Cleanup`]（原因为 [`Error::OutputOccupied`]），要用户处理。要读写文件系统，在阻塞线程池中调用；任务目录
/// 已存在时调用方已加锁。
pub(crate) fn prepare(options: &ResolvedOutput) -> Result<(), Error> {
    let kept = commit::recover(&options.work_dir, &options.outputs)?;
    if let Some(first) = kept.first() {
        let LeftoverKind::Displaced { target } = &first.kind else {
            unreachable!("recover 只报留着的旧输出");
        };
        return Err(Error::Cleanup {
            failure: Box::new(Error::OutputOccupied {
                path: target.clone(),
                ancestor: None,
            }),
            leftovers: kept,
        });
    }
    check_all(options)?;
    create_dirs(options)
}

/// 任务开头之后才定下的输出（另存的字幕文件，见 [`ResolvedOutput::with_subtitle_files`]）在下载之前核对：路径建不建得
/// 出来、能否写，再建出所在的目录（见 [`prepare`]）。已有的输出照样再查一遍。要读写文件系统，在阻塞线程池中调用；
/// 调用方已给任务目录加锁。
pub(crate) fn check_more(options: &ResolvedOutput) -> Result<(), Error> {
    check_creatable(options)?;
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

/// 写出的进度计入任务的进度（见 [`Writing`]）。
#[derive(Clone)]
struct Advance(watch::Sender<Progress>);

impl Advance {
    /// 合并文件又写完 `files` 个输入文件。
    fn merged(&self, files: usize) {
        self.add(files, |w| w.merged.as_mut());
    }

    /// 本地 HLS 又放好 `files` 个文件。
    fn hls(&self, files: usize) {
        self.add(files, |w| w.hls.as_mut());
    }

    fn add(&self, files: usize, count: impl FnOnce(&mut Writing) -> Option<&mut FileCount>) {
        self.0.send_modify(|p| {
            let count = p
                .writing
                .as_mut()
                .and_then(count)
                .expect("写出开始时已给出这种输出的合计");
            count.done += files;
        });
    }
}

/// 写出输出，按选项删除任务目录。`bytes` 为任务目录中已完成的视频与音频分片与 init 段的字节数。`cancel` 在换上输出之前
/// 都生效（见 [`write_files`]）。写出的进度计入 `progress`（见 [`Progress::writing`]）。
pub(crate) async fn write(
    dir: WorkDir,
    content: Content,
    options: &ResolvedOutput,
    bytes: u64,
    cancel: &CancellationToken,
    progress: &watch::Sender<Progress>,
) -> Result<Output, Error> {
    let Content {
        streams,
        groups,
        segments,
        selection,
        missed,
        live,
    } = content;
    let writing = writing(&groups, options);
    progress.send_modify(|p| p.writing = Some(writing));
    let advance = Advance(progress.clone());
    let files = options.clone();
    let cancel = cancel.clone();
    let (merged, mut leftovers) = blocking(move || {
        write_files(
            &streams,
            &groups,
            selection.as_ref(),
            &files,
            &cancel,
            &advance,
        )
    })
    .await??;
    if !options.keep_work_dir {
        leftovers.extend(dir.remove().await);
    }
    Ok(Output {
        merged,
        hls: options
            .get(OutputKind::Hls)
            .map(|o| o.target().to_path_buf()),
        subtitle_files: options
            .subtitle_files()
            .map(|o| o.target().to_path_buf())
            .collect(),
        segments,
        bytes,
        leftovers,
        missed,
        live,
    })
}

/// 文件任务：把下载完的数据文件装到输出（`options` 里唯一的那个，见 [`crate::FileOutputOptions`]），按选项删除任务目录
/// `dir`；返回装好的输出路径与收尾没删掉的东西。先放到输出旁的临时名（见 [`place`]），再换上（见 [`commit_outputs`]）；
/// 任一步失败都撤回，数据文件留在任务目录里。`cancel` 在换上之前生效，复制本身不中断。
pub(crate) async fn install_file(
    dir: FileDir,
    options: &ResolvedOutput,
    cancel: &CancellationToken,
) -> Result<(PathBuf, Vec<Leftover>), Error> {
    let [output] = options.outputs.as_slice() else {
        unreachable!("文件任务的输出配置只有一个输出（见 FileOutputOptions::resolve）");
    };
    let (data, temp) = (dir.data(), output.temp().to_path_buf());
    let target = output.target().to_path_buf();
    let (files, cancel) = (options.clone(), cancel.clone());
    let ((), mut leftovers) =
        blocking(move || commit_outputs(&files, &cancel, || place(&data, &temp).map(drop)))
            .await??;
    if !options.keep_work_dir {
        leftovers.extend(dir.remove().await);
    }
    Ok((target, leftovers))
}

/// 换上输出：检查各输出能否写、建出所在目录、记进任务目录（见 [`Commit::begin`]），由 `stage` 写出各临时输出；成功且没
/// 取消时再查一次各输出（写出期间输出路径可能已被别人占用，按这次的结果换），换上（见 [`commit`]）。任一步失败都撤回，
/// 等于没有写过；取消时以 [`Error::Cancelled`] 撤回。返回 `stage` 的结果与收尾没删掉的东西。要读写文件系统，在阻塞线程
/// 池中调用；换上的几步改名不中断。
fn commit_outputs<T>(
    options: &ResolvedOutput,
    cancel: &CancellationToken,
    stage: impl FnOnce() -> Result<T, Error>,
) -> Result<(T, Vec<Leftover>), Error> {
    check_all(options)?;
    create_dirs(options)?;
    let commit = Commit::begin(&options.work_dir, &options.outputs)?;
    let staged = stage().and_then(|value| {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok((value, check_all(options)?))
    });
    match staged {
        Ok((value, existing)) => Ok((value, commit.swap(&existing)?)),
        Err(failure) => Err(commit.abandon(failure)),
    }
}

/// 把 `src` 放到新文件 `dst`，返回字节数：能硬链接就硬链接，不占额外空间；做不了时（跨文件系统、文件系统不支持）
/// 复制并落盘。`dst` 已存在时报错，不改写已有的文件：它可能是别处文件的硬链接。
fn place(src: &Path, dst: &Path) -> Result<u64, Error> {
    match fs::hard_link(src, dst) {
        Ok(()) => return Ok(fs::metadata(dst).map_err(io_error("读取", dst))?.len()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(io_error("链接", dst)(e));
        }
        Err(_) => {}
    }
    let mut to = File::options()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(io_error("创建", dst))?;
    let len = File::open(src)
        .and_then(|mut from| io::copy(&mut from, &mut to))
        .map_err(io_error("复制", src))?;
    to.sync_all().map_err(io_error("落盘", dst))?;
    Ok(len)
}

/// 写出开始时的进度：合并文件与本地 HLS 都是各组各轨的分片与 init 段合计。
fn writing(groups: &[Vec<GroupTrack>], options: &ResolvedOutput) -> Writing {
    let per_output: usize = groups
        .iter()
        .flatten()
        .map(|t| t.segments.len() + usize::from(t.init.is_some()))
        .sum();
    let count = FileCount {
        done: 0,
        total: per_output,
    };
    Writing {
        merged: options.merged().map(|_| count),
        hls: options.get(OutputKind::Hls).map(|_| count),
    }
}

/// 先把合并文件与另存的字幕文件写到临时名、HLS 备齐到准备目录，都成功后再换上（见 [`commit_outputs`]）。合并失败时还没有
/// 动到输出。成功时另返回收尾没删掉的东西。`cancel` 在合并（见 [`hs_m3u8_remux::Cancel`]）与备齐 HLS 的每个文件之间也
/// 检查。
fn write_files(
    streams: &[Streams],
    groups: &[Vec<GroupTrack>],
    selection: Option<&SelectionRecord>,
    options: &ResolvedOutput,
    cancel: &CancellationToken,
    advance: &Advance,
) -> Result<(Option<MergedOutput>, Vec<Leftover>), Error> {
    if options.get(OutputKind::Hls).is_some() {
        hls::check_content(streams, groups, selection)?;
    }
    let (report, leftovers) = commit_outputs(options, cancel, || {
        write_temps(streams, groups, selection, options, cancel, advance)
    })?;
    let merged = options
        .merged()
        .zip(report)
        .map(|((merged, container), report)| MergedOutput {
            path: merged.target().to_path_buf(),
            container,
            report,
        });
    Ok((merged, leftovers))
}

/// 写出各临时输出，写好一个文件报给 `advance`；失败时可能留有写了一半的，由调用方删除。
fn write_temps(
    streams: &[Streams],
    groups: &[Vec<GroupTrack>],
    selection: Option<&SelectionRecord>,
    options: &ResolvedOutput,
    cancel: &CancellationToken,
    advance: &Advance,
) -> Result<Option<MediaReport>, Error> {
    let report = match options.merged() {
        Some((merged, container)) => {
            let tracks: Vec<Track> = streams
                .iter()
                .zip(track_labels(selection))
                .map(|(&streams, label)| Track { streams, label })
                .collect();
            let written = write_merged(
                merged.temp(),
                container,
                &tracks,
                groups,
                options,
                cancel,
                advance,
            )?;
            write_subtitle_files(options, &written.subtitles)?;
            Some(written.report)
        }
        None => None,
    };
    if let Some(hls) = options.get(OutputKind::Hls) {
        hls::stage(
            hls.temp(),
            &options.work_dir,
            streams,
            groups,
            selection,
            cancel,
            &|files| advance.hls(files),
        )?;
    }
    Ok(report)
}

/// 合并为 `container` 写到 `temp`，字幕按 `options.subtitles` 封进去与否，写完输入文件报给 `advance`（见
/// [`WriteProgress`]）；失败或取消时 `temp` 上可能留有写了一半的文件。
fn write_merged(
    temp: &Path,
    container: Container,
    tracks: &[Track],
    groups: &[Vec<GroupTrack>],
    options: &ResolvedOutput,
    cancel: &CancellationToken,
    advance: &Advance,
) -> Result<Merged, Error> {
    let groups: Vec<DiscontinuityGroup> = groups.iter().map(|g| remux_group(g)).collect();
    let token = cancel.clone();
    let advance = advance.clone();
    let control = Control {
        cancel: Cancel::new(move || token.is_cancelled()),
        // 合并 HLS 时单位为文件，合计不超过分片与 init 段的个数，在 usize 范围内
        progress: WriteProgress::new(move |files| {
            advance.merged(usize::try_from(files).expect("文件个数在 usize 范围内"))
        }),
    };
    let embed = options.subtitles.embed;
    hs_m3u8_remux::remux(tracks, &groups, temp, container, embed, &control)
        .map_err(|e| remux_error(e, &options.work_dir))
}

/// 把对齐后的字幕（按字幕的轨号，见 [`Merged::subtitles`]）写到各另存的字幕文件的临时名。
fn write_subtitle_files(options: &ResolvedOutput, subtitles: &[Vec<Cue>]) -> Result<(), Error> {
    let Some(format) = options.subtitles.files else {
        return Ok(());
    };
    let files: Vec<&PendingOutput> = options.subtitle_files().collect();
    assert_eq!(
        files.len(),
        subtitles.len(),
        "另存的字幕文件按所选的字幕建出，与合并给出的字幕轨一一对应"
    );
    for (file, cues) in files.into_iter().zip(subtitles) {
        let text = match format {
            SubtitleFileFormat::Srt => hs_m3u8_subtitle::srt(cues),
            SubtitleFileFormat::WebVtt => hs_m3u8_subtitle::webvtt(cues),
        };
        hls::write_file(file.temp(), &text)?;
    }
    Ok(())
}

/// 下载完之前核对能否合并进 `container`（见 [`hs_m3u8_remux::check`]），各轨只需开头的分片。`groups` 的各项为合并时的组号
/// 与这一组，第一项为第 0 组；`work_dir` 为分片所在的任务目录。要读文件，在阻塞线程池中调用。
pub(crate) fn check_merge(
    streams: &[Streams],
    groups: &[(usize, Vec<GroupTrack>)],
    container: Container,
    work_dir: &Path,
) -> Result<(), Error> {
    let remux: Vec<(usize, DiscontinuityGroup)> =
        groups.iter().map(|(n, g)| (*n, remux_group(g))).collect();
    let numbered: Vec<(usize, &DiscontinuityGroup)> = remux.iter().map(|(n, g)| (*n, g)).collect();
    hs_m3u8_remux::check(streams, &numbered, container).map_err(|e| remux_error(e, work_dir))
}

/// 任务目录里的分片认不出格式：下载时都核对过，认不出即目录里的文件被改动过。
pub(super) fn unrecognized_segment(work_dir: &Path, path: &Path) -> Error {
    Error::WorkDir {
        path: work_dir.to_path_buf(),
        problem: WorkDirProblem::Corrupt(format!("分片内容无法识别：{}", path.display())),
    }
}

/// 合并或核对任务目录里的分片时 remux 的错误：放不进合并文件的为 [`crate::Unsupported::Merge`]；认不出的分片、结构不对的
/// fMP4 与解析不了的字幕分片为任务目录损坏（下载时都核对过，即目录里的文件被改动过），认不出的与写本地 HLS 时相同（见
/// [`unrecognized_segment`]）；读分片、写出、关闭或回读输出时文件出错为 [`Error::Io`]（如盘满），取消为
/// [`Error::Cancelled`]；其余为 [`Error::Remux`]。
fn remux_error(error: hs_m3u8_remux::Error, work_dir: &Path) -> Error {
    match error {
        hs_m3u8_remux::Error::Io {
            action,
            path,
            cause,
        } => Error::Io {
            action,
            path,
            cause,
        },
        hs_m3u8_remux::Error::Cancelled => Error::Cancelled,
        hs_m3u8_remux::Error::Unsupported(unsupported) => {
            Error::Unsupported(crate::Unsupported::Merge(unsupported))
        }
        hs_m3u8_remux::Error::Unrecognized(path) => unrecognized_segment(work_dir, &path),
        error @ (hs_m3u8_remux::Error::Fmp4 { .. }
        | hs_m3u8_remux::Error::Fmp4OpenEnded { .. }
        | hs_m3u8_remux::Error::Subtitle { .. }) => Error::WorkDir {
            path: work_dir.to_path_buf(),
            problem: WorkDirProblem::Corrupt(error.to_string()),
        },
        other => Error::Remux(Box::new(other)),
    }
}

fn remux_group(tracks: &[GroupTrack]) -> DiscontinuityGroup {
    DiscontinuityGroup {
        tracks: tracks
            .iter()
            .map(|t| TrackSegments {
                init: t.init.clone(),
                segments: t
                    .segments
                    .iter()
                    .map(|s| Segment {
                        path: s.path.clone(),
                        duration_us: s.duration_us,
                    })
                    .collect(),
                gaps: t.gaps.clone(),
            })
            .collect(),
    }
}
