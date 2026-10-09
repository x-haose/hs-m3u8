//! 任务目录：任务描述、独占锁、已完成的分片与 init 段。
//!
//! ```text
//! job.json                                              任务描述，见 JobFile
//! lock                                                  运行期间持有排他锁
//! tracks/<轨道>/<录制>-<序号>-<不连续段>-<init>-<时长>.seg  已解密、通过校验的分片
//! tracks/<轨道>/init-<编号>.mp4                          init 段
//! ```
//!
//! 分片文件名中：录制为第几次录制（点播恒为 0），init 为 init 段编号或 `none`，时长为 EXTINF 声明的微秒数。
//! 分组信息与时长都在文件名里，只凭目录内容即可合并。
//!
//! 文件先写 `<名字>.part`，fsync 后改名，因此最终文件名存在即内容完整（断电也成立）。
//! 请求配置（请求头、Cookie 等）不写入目录：续传时由调用方再次提供。

use std::fs::{self, File, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use hs_m3u8_remux::Streams;
use serde::{Deserialize, Serialize};

use crate::{Error, JobType, WorkDirProblem, blocking};

const FORMAT_VERSION: u32 = 1;
const JOB_FILE: &str = "job.json";
const LOCK_FILE: &str = "lock";
const TRACKS_DIR: &str = "tracks";

/// 任务目录记录的任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JobKind {
    /// 点播：计划摘要相同才能续传
    Vod { plan_digest: String },
    /// 直播：来源摘要相同才能继续录制或合并；`streams` 为各轨的取流方式
    Live {
        request_digest: String,
        streams: Vec<Streams>,
    },
}

impl JobKind {
    fn job_type(&self) -> JobType {
        match self {
            JobKind::Vod { .. } => JobType::Vod,
            JobKind::Live { .. } => JobType::Live,
        }
    }
}

/// `job.json` 的格式（序列化契约）。`format_version` 不等于 [`FORMAT_VERSION`] 时拒绝。
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum JobFile {
    Vod {
        format_version: u32,
        plan_digest: String,
    },
    Live {
        format_version: u32,
        request_digest: String,
        streams: Vec<StreamsName>,
    },
}

/// [`Streams`] 在 job.json 中的写法。
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StreamsName {
    All,
    Video,
    Audio,
}

impl From<Streams> for StreamsName {
    fn from(streams: Streams) -> Self {
        match streams {
            Streams::All => StreamsName::All,
            Streams::Video => StreamsName::Video,
            Streams::Audio => StreamsName::Audio,
        }
    }
}

impl From<StreamsName> for Streams {
    fn from(name: StreamsName) -> Self {
        match name {
            StreamsName::All => Streams::All,
            StreamsName::Video => Streams::Video,
            StreamsName::Audio => Streams::Audio,
        }
    }
}

/// 先只读版本号：不认识的版本直接拒绝，不按当前格式去解释它。
#[derive(Deserialize)]
struct Version {
    format_version: u32,
}

/// 任务目录中各文件的路径。
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    root: PathBuf,
}

/// 分片文件名记录的信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentName {
    pub session: u32,
    pub sequence: u64,
    pub discontinuity: u64,
    pub init: Option<usize>,
    /// EXTINF 声明的时长，微秒
    pub duration_us: u64,
}

/// 目录中一个已完成的分片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SegmentFile {
    pub name: SegmentName,
    pub path: PathBuf,
    /// 字节数
    pub len: u64,
}

/// 目录中一个已完成的 init 段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitFile {
    pub index: usize,
    pub path: PathBuf,
    pub len: u64,
}

impl Layout {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn segment(&self, track: usize, name: &SegmentName) -> PathBuf {
        self.track(track).join(segment_file_name(name))
    }

    pub(crate) fn init(&self, track: usize, index: usize) -> PathBuf {
        self.track(track).join(format!("init-{index}.mp4"))
    }

    fn track(&self, track: usize) -> PathBuf {
        self.root.join(TRACKS_DIR).join(track.to_string())
    }

    /// 前 `tracks` 条轨已完成的分片，各轨按（录制次数, 序号）排列。
    pub(crate) async fn completed_segments(
        &self,
        tracks: usize,
    ) -> Result<Vec<Vec<SegmentFile>>, Error> {
        let layout = self.clone();
        blocking(move || {
            (0..tracks)
                .map(|t| list(&layout.track(t), parse_segment_name))
                .map(|files| {
                    let mut files: Vec<SegmentFile> = files?
                        .into_iter()
                        .map(|(name, path, len)| SegmentFile { name, path, len })
                        .collect();
                    files.sort_by_key(|f| (f.name.session, f.name.sequence));
                    Ok(files)
                })
                .collect()
        })
        .await?
    }

    /// 前 `tracks` 条轨已完成的 init 段，各轨按编号排列。
    pub(crate) async fn completed_inits(&self, tracks: usize) -> Result<Vec<Vec<InitFile>>, Error> {
        let layout = self.clone();
        blocking(move || {
            (0..tracks)
                .map(|t| list(&layout.track(t), parse_init_name))
                .map(|files| {
                    let mut files: Vec<InitFile> = files?
                        .into_iter()
                        .map(|(index, path, len)| InitFile { index, path, len })
                        .collect();
                    files.sort_by_key(|f| f.index);
                    Ok(files)
                })
                .collect()
        })
        .await?
    }
}

/// 已打开并加锁的任务目录；锁在 drop 时释放。
pub(crate) struct WorkDir {
    layout: Layout,
    _lock: File,
}

impl WorkDir {
    /// 打开或新建任务目录并加锁。
    ///
    /// 已有 `job.json` 时其记录必须等于 `kind`：点播摘要不同报 [`Error::PlanChanged`]，其余不同报
    /// [`Error::WorkDir`]；但目录里还没有已完成的分片与 init 段时，直接改为当前任务（没有可丢的内容）。
    /// 没有 `job.json` 时目录必须不存在或为空（`.part` 残留与锁文件除外），以免把别人的目录当成任务目录
    /// （成功后会整个删除）。
    pub(crate) async fn open(root: PathBuf, kind: JobKind) -> Result<WorkDir, Error> {
        blocking(move || open(root, &kind)).await?
    }

    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    /// 释放锁并删除整个任务目录。
    pub(crate) async fn remove(self) -> Result<(), Error> {
        let WorkDir { layout, _lock } = self;
        // Windows 上打开着的锁文件无法删除，先释放
        drop(_lock);
        blocking(move || fs::remove_dir_all(&layout.root).map_err(io_error("删除", &layout.root)))
            .await?
    }
}

/// 读取任务目录记录的任务；目录或 `job.json` 不存在时为 None。不加锁，只用于决定走哪条流程。
pub(crate) async fn read_kind(root: PathBuf) -> Result<Option<JobKind>, Error> {
    blocking(move || {
        let job_path = root.join(JOB_FILE);
        match fs::read(&job_path) {
            Ok(bytes) => decode(&bytes).map(Some).map_err(|reason| Error::WorkDir {
                path: root.clone(),
                problem: WorkDirProblem::Corrupt(reason),
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(cause) => Err(io_error("读取", &job_path)(cause)),
        }
    })
    .await?
}

fn open(root: PathBuf, kind: &JobKind) -> Result<WorkDir, Error> {
    let problem = |problem| Error::WorkDir {
        path: root.clone(),
        problem,
    };
    fs::create_dir_all(&root).map_err(io_error("创建", &root))?;
    let job_path = root.join(JOB_FILE);
    if !exists(&job_path)? {
        for entry in fs::read_dir(&root).map_err(io_error("读取", &root))? {
            let name = entry.map_err(io_error("读取", &root))?.file_name();
            let name = name.to_string_lossy();
            if name != LOCK_FILE && !name.ends_with(".part") {
                return Err(problem(WorkDirProblem::NotEmpty));
            }
        }
    }

    let lock_path = root.join(LOCK_FILE);
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(io_error("创建", &lock_path))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(problem(WorkDirProblem::Locked)),
        Err(TryLockError::Error(cause)) => return Err(io_error("锁定", &lock_path)(cause)),
    }

    let recorded = match fs::read(&job_path) {
        Ok(bytes) => Some(decode(&bytes).map_err(|r| problem(WorkDirProblem::Corrupt(r)))?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(cause) => return Err(io_error("读取", &job_path)(cause)),
    };
    let layout = Layout { root: root.clone() };
    match recorded {
        Some(recorded) if recorded == *kind => {}
        Some(recorded) if has_completed_files(&layout)? => {
            return Err(match (recorded, kind) {
                (JobKind::Vod { .. }, JobKind::Vod { .. }) => Error::PlanChanged,
                (
                    JobKind::Live {
                        request_digest: a, ..
                    },
                    JobKind::Live {
                        request_digest: b, ..
                    },
                ) if a != *b => problem(WorkDirProblem::SourceMismatch),
                (JobKind::Live { .. }, JobKind::Live { .. }) => {
                    problem(WorkDirProblem::StreamsMismatch)
                }
                (recorded, current) => problem(WorkDirProblem::KindMismatch {
                    recorded: recorded.job_type(),
                    current: current.job_type(),
                }),
            });
        }
        Some(_) | None => {
            remove_if_exists(&root.join(TRACKS_DIR))?;
            write_atomic(&job_path, &encode(kind))?;
        }
    }
    Ok(WorkDir {
        layout,
        _lock: lock,
    })
}

/// 目录里有没有已完成的分片或 init 段。
fn has_completed_files(layout: &Layout) -> Result<bool, Error> {
    let tracks = layout.root.join(TRACKS_DIR);
    let entries = match fs::read_dir(&tracks) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(cause) => return Err(io_error("读取", &tracks)(cause)),
    };
    for entry in entries {
        let dir = entry.map_err(io_error("读取", &tracks))?.path();
        let segments = list(&dir, parse_segment_name)?;
        let inits = list(&dir, parse_init_name)?;
        if !segments.is_empty() || !inits.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn remove_if_exists(dir: &Path) -> Result<(), Error> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(cause) => Err(io_error("删除", dir)(cause)),
    }
}

fn encode(kind: &JobKind) -> Vec<u8> {
    let file = match kind {
        JobKind::Vod { plan_digest } => JobFile::Vod {
            format_version: FORMAT_VERSION,
            plan_digest: plan_digest.clone(),
        },
        JobKind::Live {
            request_digest,
            streams,
        } => JobFile::Live {
            format_version: FORMAT_VERSION,
            request_digest: request_digest.clone(),
            streams: streams.iter().map(|&s| s.into()).collect(),
        },
    };
    serde_json::to_vec(&file).expect("JobFile 只含字符串、整数与枚举，序列化不会失败")
}

/// 解析 `job.json`；失败时返回原因。
fn decode(bytes: &[u8]) -> Result<JobKind, String> {
    let unreadable = |e: serde_json::Error| format!("job.json 无法解析：{e}");
    let version: Version = serde_json::from_slice(bytes).map_err(unreadable)?;
    if version.format_version != FORMAT_VERSION {
        return Err(format!(
            "job.json 的格式版本 {} 不受支持（支持 {FORMAT_VERSION}）",
            version.format_version
        ));
    }
    Ok(match serde_json::from_slice(bytes).map_err(unreadable)? {
        JobFile::Vod { plan_digest, .. } => JobKind::Vod { plan_digest },
        JobFile::Live {
            request_digest,
            streams,
            ..
        } if !streams.is_empty() => JobKind::Live {
            request_digest,
            streams: streams.into_iter().map(Streams::from).collect(),
        },
        JobFile::Live { .. } => return Err("job.json 中直播的 streams 为空".into()),
    })
}

/// 一条轨目录中按 `parse` 能识别的文件；目录不存在时为空。
fn list<T>(dir: &Path, parse: fn(&str) -> Option<T>) -> Result<Vec<(T, PathBuf, u64)>, Error> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(cause) => return Err(io_error("读取", dir)(cause)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_error("读取", dir))?;
        let path = entry.path();
        let Some(parsed) = path.file_name().and_then(|n| n.to_str()).and_then(parse) else {
            continue;
        };
        let len = entry.metadata().map_err(io_error("读取", &path))?.len();
        files.push((parsed, path, len));
    }
    Ok(files)
}

/// `<录制>-<序号>-<不连续段>-<init 编号|none>-<时长>.seg`。
fn segment_file_name(name: &SegmentName) -> String {
    let init = name
        .init
        .map_or_else(|| "none".to_owned(), |i| i.to_string());
    format!(
        "{}-{}-{}-{init}-{}.seg",
        name.session, name.sequence, name.discontinuity, name.duration_us
    )
}

/// [`segment_file_name`] 的逆；只认它写出的规范写法。
fn parse_segment_name(file_name: &str) -> Option<SegmentName> {
    let stem = file_name.strip_suffix(".seg")?;
    let mut parts = stem.split('-');
    let name = SegmentName {
        session: parts.next()?.parse().ok()?,
        sequence: parts.next()?.parse().ok()?,
        discontinuity: parts.next()?.parse().ok()?,
        init: match parts.next()? {
            "none" => None,
            index => Some(index.parse().ok()?),
        },
        duration_us: parts.next()?.parse().ok()?,
    };
    (parts.next().is_none() && segment_file_name(&name) == file_name).then_some(name)
}

/// `init-<编号>.mp4`；只认规范写法。
fn parse_init_name(file_name: &str) -> Option<usize> {
    let index: usize = file_name
        .strip_prefix("init-")?
        .strip_suffix(".mp4")?
        .parse()
        .ok()?;
    (format!("init-{index}.mp4") == file_name).then_some(index)
}

/// 已完成文件的字节数；不存在时为 None。
pub(crate) fn completed_len(path: &Path) -> Result<Option<u64>, Error> {
    match fs::metadata(path) {
        Ok(meta) => Ok(Some(meta.len())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(cause) => Err(io_error("读取", path)(cause)),
    }
}

/// 写入完整文件：先写 `.part` 并 fsync，再改名，所在目录不存在时创建。
pub(crate) async fn write(path: PathBuf, data: Vec<u8>) -> Result<(), Error> {
    blocking(move || {
        let parent = path.parent().expect("任务目录内的文件都有上级目录");
        fs::create_dir_all(parent).map_err(io_error("创建", parent))?;
        write_atomic(&path, &data)
    })
    .await?
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<(), Error> {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let mut file = File::create(&part).map_err(io_error("创建", &part))?;
    file.write_all(data).map_err(io_error("写入", &part))?;
    file.sync_all().map_err(io_error("落盘", &part))?;
    drop(file);
    fs::rename(&part, path).map_err(io_error("重命名", &part))
}

fn exists(path: &Path) -> Result<bool, Error> {
    path.try_exists().map_err(io_error("检查", path))
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Error {
    let path = path.to_path_buf();
    move |cause| Error::Io {
        action,
        path,
        cause,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_file_round_trips_and_rejects_other_versions_and_fields() {
        let live = JobKind::Live {
            request_digest: "ab".into(),
            streams: vec![Streams::Video, Streams::Audio],
        };
        let bytes = encode(&live);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"kind":"live","format_version":1,"request_digest":"ab","streams":["video","audio"]}"#
        );
        assert_eq!(decode(&bytes), Ok(live));
        let vod = JobKind::Vod {
            plan_digest: "cd".into(),
        };
        assert_eq!(decode(&encode(&vod)), Ok(vod));

        for rejected in [
            r#"{"kind":"vod","format_version":2,"plan_digest":"cd"}"#,
            r#"{"kind":"vod","format_version":1,"plan_digest":"cd","extra":1}"#,
            r#"{"format_version":1,"plan_digest":"cd"}"#,
            r#"{"kind":"live","format_version":1,"request_digest":"ab","streams":[]}"#,
            r#"{"kind":"live","format_version":1,"request_digest":"ab","streams":["both"]}"#,
        ] {
            assert!(decode(rejected.as_bytes()).is_err(), "{rejected}");
        }
    }

    #[test]
    fn segment_names_round_trip_and_reject_non_canonical_forms() {
        let name = SegmentName {
            session: 2,
            sequence: u64::MAX,
            discontinuity: 7,
            init: Some(1),
            duration_us: 6_006_000,
        };
        assert_eq!(parse_segment_name(&segment_file_name(&name)), Some(name));
        let plain = SegmentName { init: None, ..name };
        assert_eq!(parse_segment_name(&segment_file_name(&plain)), Some(plain));

        for other in [
            "0-1-0-none-1000.seg.part",
            "0-01-0-none-1000.seg",
            "0-+1-0-none-1000.seg",
            "0-1-0-none.seg",
            "0-1-0-none-1000-9.seg",
            "init-0.mp4",
        ] {
            assert_eq!(parse_segment_name(other), None, "{other}");
        }
        assert_eq!(parse_init_name("init-3.mp4"), Some(3));
        assert_eq!(parse_init_name("init-03.mp4"), None);
        assert_eq!(parse_init_name("init-3.mp4.part"), None);
    }
}
