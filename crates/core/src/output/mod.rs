//! 输出：合并为 MP4、写出可直接播放的本地 HLS，或两者都要；按选项替换已有的输出，成功后删除任务目录。

mod hls;
mod options;

use std::io;
use std::path::{Path, PathBuf};

use hs_m3u8_remux::{DiscontinuityGroup, Streams, TrackSegments};

pub use self::options::{OutputOptions, Target};

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
///
/// 先合并 MP4：最常见的失败是编码不受支持，此时还没有写出任何东西。再写 HLS，失败时删掉刚写的 MP4，
/// 任务目录保留，等于没有合并过；要求覆盖时，已被替换掉的旧输出不能恢复。
pub(crate) async fn write(
    dir: WorkDir,
    input: Content,
    options: &OutputOptions,
    bytes: u64,
) -> Result<Output, Error> {
    // 下载或录制期间输出路径可能已被别人占用；开始时检查过，这里再查一次
    check_targets(options)?;
    let Content {
        streams,
        groups,
        segments,
        selection,
        live,
    } = input;
    let mp4 = match options.target.mp4() {
        Some(path) => Some(write_mp4(path.to_path_buf(), streams, &groups).await?),
        None => None,
    };
    if let Some(target) = options.target.hls() {
        let (target, root, overwrite) = (
            target.to_path_buf(),
            dir.layout().root().to_path_buf(),
            options.overwrite,
        );
        let written =
            blocking(move || hls::write(&target, &root, &groups, selection.as_ref(), overwrite))
                .await
                .and_then(|r| r);
        if let Err(failure) = written {
            return Err(match &mp4 {
                Some(mp4) => discard(&mp4.path, failure),
                None => failure,
            });
        }
    }
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

async fn write_mp4(
    path: PathBuf,
    streams: Vec<Streams>,
    groups: &[Vec<GroupTrack>],
) -> Result<Mp4Output, Error> {
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
    blocking(move || {
        create_parent(&path)?;
        let report = hs_m3u8_remux::remux(&streams, &groups, &path)?;
        Ok(Mp4Output { path, report })
    })
    .await?
}

/// 所在目录不存在时创建。
fn create_parent(path: &Path) -> Result<(), Error> {
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => std::fs::create_dir_all(parent).map_err(io_error("创建", parent)),
        None => Ok(()),
    }
}

/// 失败后删掉本次写出的文件；删除也失败时把两者一并返回。
fn discard(path: &Path, failure: Error) -> Error {
    match std::fs::remove_file(path) {
        Ok(()) => failure,
        Err(e) if e.kind() == io::ErrorKind::NotFound => failure,
        Err(cause) => Error::Cleanup {
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
