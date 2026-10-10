//! 输出：合并为 MP4、写出可直接播放的本地 HLS，或两者都要；按选项替换已有的输出，成功后删除任务目录。

mod commit;
mod hls;
mod options;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use hs_m3u8_remux::{DiscontinuityGroup, Report, Streams, TrackSegments};

pub use self::options::{OutputOptions, Target};

use self::options::{lexical_absolute, sibling};
use crate::error::io_error;
use crate::ident::Fingerprint;
use crate::selection::SelectionKey;
use crate::workdir::{self, PendingOutput, WorkDir};
use crate::{Error, Leftover, LeftoverKind, LiveReport, Mp4Output, Output, blocking};

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

/// 输出能否写：MP4 不存在，或是文件且允许覆盖；HLS 目录见 [`hls::check`]。
pub(crate) fn check_targets(options: &OutputOptions) -> Result<(), Error> {
    if let Some(mp4) = options.target.mp4() {
        match fs::symlink_metadata(mp4) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(cause) => return Err(io_error("检查", mp4)(cause)),
            Ok(meta) if meta.is_dir() => return Err(Error::OutputOccupied(mp4.to_path_buf())),
            Ok(_) if !options.overwrite => return Err(Error::OutputExists(mp4.to_path_buf())),
            Ok(_) => {}
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
    let (mp4, mut leftovers) =
        blocking(move || write_files(&work_dir, &streams, &groups, selection.as_ref(), &files))
            .await??;
    if !options.keep_work_dir {
        leftovers.extend(dir.remove().await);
    }
    Ok(Output {
        mp4,
        hls: options.target.hls().map(Path::to_path_buf),
        segments,
        bytes,
        leftovers,
        live,
    })
}

/// 一个输出的临时名与旧输出挪开后的名字：与输出同级，名为 `hsdl-<任务目录指纹>.<种类>.part` 与 `.old`。定长，
/// 不随输出名变长；同一任务目录同一时间只有一个任务（任务目录的锁），以它区分，几个任务输出到同一处时各写各的。
fn pending(target: &Path, fingerprint: Fingerprint, kind: &str, dir: bool) -> PendingOutput {
    let name = |suffix: &str| OsString::from(format!("hsdl-{fingerprint}.{kind}.{suffix}"));
    PendingOutput {
        target: target.to_path_buf(),
        temp: sibling(target, &name("part"), ""),
        aside: sibling(target, &name("old"), ""),
        dir,
    }
}

/// 先把 MP4 合并到临时名、HLS 备齐到准备目录，都成功后再换上（见 [`commit`]）。最常见的失败（编码不受支持）发生在
/// 合并 MP4 时，此时还没有动到输出；任一步失败都删掉本次写出的，等于没有写过。写之前把临时名记进任务目录，
/// 上次中断留下的先按记录清掉。成功时另返回收尾没删掉的东西。
fn write_files(
    work_dir: &Path,
    streams: &[Streams],
    groups: &[Vec<GroupTrack>],
    selection: Option<&SelectionKey>,
    options: &OutputOptions,
) -> Result<(Option<Mp4Output>, Vec<Leftover>), Error> {
    check_targets(options)?;
    if options.target.hls().is_some() {
        hls::check_content(groups, selection)?;
    }
    commit::recover(&workdir::read_outputs(work_dir)?)?;
    let fingerprint =
        Fingerprint::of_content(lexical_absolute(work_dir)?.as_os_str().as_encoded_bytes());
    let mp4 = options
        .target
        .mp4()
        .map(|path| pending(path, fingerprint, "mp4", false));
    let hls = options
        .target
        .hls()
        .map(|path| pending(path, fingerprint, "hls", true));
    let outputs: Vec<PendingOutput> = mp4.iter().chain(&hls).cloned().collect();
    workdir::record_outputs(work_dir, &outputs)?;
    let written = write_temps(
        work_dir,
        streams,
        groups,
        selection,
        mp4.as_ref(),
        hls.as_ref(),
    )
    .and_then(|report| {
        // 写出期间输出路径可能已被别人占用；开始时检查过，换上前再查一次
        check_targets(options)?;
        Ok((report, commit::swap_in(&outputs)?))
    });
    let (report, mut leftovers) = match written {
        Ok(written) => written,
        Err(failure) => return Err(discard(failure, work_dir, &outputs)),
    };
    if let Err(e) = workdir::clear_outputs(work_dir) {
        leftovers.push(Leftover {
            path: workdir::outputs_file(work_dir),
            kind: LeftoverKind::Removable,
            cause: e.to_string(),
        });
    }
    let mp4 = mp4.zip(report).map(|(mp4, report)| Mp4Output {
        path: mp4.target,
        report,
    });
    Ok((mp4, leftovers))
}

fn write_temps(
    work_dir: &Path,
    streams: &[Streams],
    groups: &[Vec<GroupTrack>],
    selection: Option<&SelectionKey>,
    mp4: Option<&PendingOutput>,
    hls: Option<&PendingOutput>,
) -> Result<Option<Report>, Error> {
    let report = match mp4 {
        Some(mp4) => Some(write_mp4(&mp4.temp, streams, groups)?),
        None => None,
    };
    if let Some(hls) = hls {
        hls::stage(&hls.temp, work_dir, groups, selection)?;
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

/// 失败后删掉本次写出的临时输出（已不存在的不算），删完后删掉任务目录里的记录；删除也失败时把两者一并返回，
/// 记录留着，下次运行再清。
fn discard(failure: Error, work_dir: &Path, outputs: &[PendingOutput]) -> Error {
    let removed = outputs
        .iter()
        .try_for_each(|o| commit::remove(&o.temp, o.dir).map_err(|e| (o.temp.clone(), e)));
    let cleared = removed.and_then(|()| {
        workdir::clear_outputs(work_dir).map_err(|e| (workdir::outputs_file(work_dir), e))
    });
    match cleared {
        Ok(()) => failure,
        Err((path, cause)) => Error::Cleanup {
            failure: Box::new(failure),
            leftovers: vec![Leftover {
                path,
                kind: LeftoverKind::Removable,
                cause: cause.to_string(),
            }],
        },
    }
}
