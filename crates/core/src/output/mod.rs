//! 输出：合并为 MP4、写出可直接播放的本地 HLS，或两者都要；按选项替换已有的输出，成功后删除任务目录。

mod hls;
mod options;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use hs_m3u8_remux::{DiscontinuityGroup, Report, Streams, TrackSegments};

pub use self::options::{OutputOptions, Target};

use self::options::sibling;
use crate::ident::Fingerprint;
use crate::selection::SelectionKey;
use crate::workdir::WorkDir;
use crate::{Error, LiveReport, Mp4Output, Output, blocking};

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

/// 输出能否写：MP4 不存在或允许覆盖；HLS 目录不存在、为空，或允许覆盖且其中全是本库写出的文件。
pub(crate) fn check_targets(options: &OutputOptions) -> Result<(), Error> {
    if let Some(mp4) = options.target.mp4() {
        let exists = mp4.try_exists().map_err(io_error("检查", mp4))?;
        if exists && !options.overwrite {
            return Err(Error::OutputExists(mp4.to_path_buf()));
        }
    }
    if let Some(dir) = options.target.hls() {
        hls::check(dir, options.overwrite)?;
    }
    Ok(())
}

/// 写出输出，按选项删除任务目录。`bytes` 为任务目录中已完成的分片与 init 段的字节数。
pub(crate) async fn write(
    dir: WorkDir,
    content: Content,
    options: &OutputOptions,
    bytes: u64,
) -> Result<Output, Error> {
    let Content {
        streams,
        groups,
        segments,
        selection,
        live,
    } = content;
    let (work_dir, files) = (dir.layout().root().to_path_buf(), options.clone());
    let mp4 =
        blocking(move || write_files(&work_dir, &streams, &groups, selection.as_ref(), &files))
            .await??;
    let cleanup_error = if options.keep_work_dir {
        None
    } else {
        dir.remove().await.err().map(|e| e.to_string())
    };
    Ok(Output {
        mp4,
        hls: options.target.hls().map(Path::to_path_buf),
        segments,
        bytes,
        cleanup_error,
        live,
    })
}

/// 一个输出与它的临时名。
struct Pending<'a> {
    path: &'a Path,
    /// 与输出同级，名为 `.<输出名>.<任务目录指纹>.part`。同一任务目录同一时间只有一个任务（任务目录的锁），
    /// 以它区分，几个任务输出到同一处时各写各的；上次中断留下的由持锁的本任务清掉
    temp: PathBuf,
}

impl<'a> Pending<'a> {
    fn new(path: &'a Path, work_dir: &Path) -> Self {
        let name = path.file_name().expect("校验过：输出路径都有文件名");
        let fingerprint = Fingerprint::of_content(work_dir.as_os_str().as_encoded_bytes());
        let mut temp = OsString::from(".");
        temp.push(name);
        temp.push(format!(".{fingerprint}"));
        Pending {
            path,
            temp: sibling(path, &temp, ".part"),
        }
    }
}

/// 先把 MP4 合并到临时名、HLS 备齐到准备目录，都成功后再依次改名为输出。最常见的失败（编码不受支持）发生在
/// 合并 MP4 时，此时还没有动到输出；任一步失败都删掉本次写出的，等于没有写过。要求覆盖时，改名替换掉的旧输出
/// 不能恢复。
fn write_files(
    work_dir: &Path,
    streams: &[Streams],
    groups: &[Vec<GroupTrack>],
    selection: Option<&SelectionKey>,
    options: &OutputOptions,
) -> Result<Option<Mp4Output>, Error> {
    check_targets(options)?;
    let mp4 = options
        .target
        .mp4()
        .map(|path| Pending::new(path, work_dir));
    let hls = options
        .target
        .hls()
        .map(|path| Pending::new(path, work_dir));
    let report = match &mp4 {
        Some(mp4) => Some(write_mp4(&mp4.temp, streams, groups)?),
        None => None,
    };
    let mp4_temp = mp4.as_ref().map(|m| m.temp.as_path());
    if let Some(hls) = &hls {
        hls::stage(&hls.temp, work_dir, groups, selection)
            .map_err(|failure| discard(failure, mp4_temp, None))?;
    }
    commit(options, mp4.as_ref(), hls.as_ref())?;
    Ok(mp4.zip(report).map(|(mp4, report)| Mp4Output {
        path: mp4.path.to_path_buf(),
        report,
    }))
}

/// 合并为 MP4 写到 `temp`；失败时合并本身删掉它。
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

/// 临时的输出改名为输出：先 MP4 后 HLS。合并期间输出路径可能已被别人占用，开始时检查过，这里再查一次。
fn commit(
    options: &OutputOptions,
    mp4: Option<&Pending<'_>>,
    hls: Option<&Pending<'_>>,
) -> Result<(), Error> {
    let stage = hls.map(|h| h.temp.as_path());
    let renamed = check_targets(options).and_then(|()| match mp4 {
        Some(mp4) => fs::rename(&mp4.temp, mp4.path).map_err(io_error("重命名", &mp4.temp)),
        None => Ok(()),
    });
    if let Err(failure) = renamed {
        return Err(discard(failure, mp4.map(|m| m.temp.as_path()), stage));
    }
    if let Some(hls) = hls
        && let Err(failure) = hls::replace(&hls.temp, hls.path, options.overwrite)
    {
        return Err(discard(failure, mp4.map(|m| m.path), stage));
    }
    Ok(())
}

/// 失败后删掉本次写出的 MP4 文件 `file` 与 HLS 准备目录 `dir`（已不存在的不算）；删除也失败时把两者一并返回。
fn discard(failure: Error, file: Option<&Path>, dir: Option<&Path>) -> Error {
    let missing_ok = |result: io::Result<()>| match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    };
    let removed = file
        .map_or(Ok(()), |f| {
            missing_ok(fs::remove_file(f)).map_err(|e| (f, e))
        })
        .and_then(|()| {
            dir.map_or(Ok(()), |d| {
                missing_ok(fs::remove_dir_all(d)).map_err(|e| (d, e))
            })
        });
    match removed {
        Ok(()) => failure,
        Err((path, cause)) => Error::Cleanup {
            failure: Box::new(failure),
            path: path.to_path_buf(),
            cause,
        },
    }
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Error {
    let path = path.to_path_buf();
    move |cause| Error::Io {
        action,
        path,
        cause,
    }
}
