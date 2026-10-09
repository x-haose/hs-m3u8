//! 任务目录：任务描述、独占锁、已完成的分片与 init 段。
//!
//! ```text
//! job.json                                   任务描述，见 JobFile
//! lock                                       运行期间持有排他锁
//! tracks/<轨道>/<序号>-<不连续段>-<init>.seg  已解密、通过校验的分片；<init> 为 init 编号或 none
//! tracks/<轨道>/init-<编号>.mp4               init 段
//! ```
//!
//! 文件先写 `<名字>.part`，fsync 后改名，因此最终文件名存在即内容完整（断电也成立）。
//! 分片的分组信息在文件名里，只凭目录内容即可合并（直播中断后用到）。
//! 请求配置（请求头、Cookie 等）不写入目录：续传时由调用方再次提供。

use std::fs::{self, File, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use hs_m3u8_remux::Streams;
use serde::{Deserialize, Serialize};

use crate::{Error, blocking};

const FORMAT_VERSION: u32 = 2;
const JOB_FILE: &str = "job.json";
const LOCK_FILE: &str = "lock";

/// 任务目录记录的任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JobKind {
    /// 点播：计划摘要相同才能续传
    Vod { plan_digest: String },
    /// 直播：来源摘要相同才能续传；`tracks` 为各轨的取流方式，合并已录到的分片时用
    Live {
        source_digest: String,
        tracks: Vec<Streams>,
    },
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
        source_digest: String,
        /// 每条轨为 "all"、"video" 或 "audio"
        tracks: Vec<String>,
    },
}

/// 先只读版本号：不认识的版本直接拒绝，不按当前格式去解释它。
#[derive(Deserialize)]
struct Version {
    format_version: u32,
}

/// 任务目录中各文件的路径。
#[derive(Debug, Clone)]
pub(crate) struct WorkDir {
    root: PathBuf,
}

/// 任务目录的排他锁，drop 时释放。
pub(crate) struct DirLock {
    _file: File,
}

/// 目录中一个已完成的分片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SegmentFile {
    pub sequence: u64,
    pub discontinuity: u64,
    pub init: Option<usize>,
    pub path: PathBuf,
    /// 字节数
    pub len: u64,
}

impl WorkDir {
    /// 打开或新建任务目录并加锁。
    ///
    /// 已有 `job.json` 时其记录必须等于 `kind`：点播摘要不同报 [`Error::PlanChanged`]，其余不同报
    /// [`Error::WorkDir`]。没有时目录必须不存在或为空，以免把别人的目录当成任务目录（成功后会整个删除）。
    pub(crate) async fn open(root: PathBuf, kind: JobKind) -> Result<(WorkDir, DirLock), Error> {
        blocking(move || open(root, &kind)).await?
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn segment(
        &self,
        track: usize,
        sequence: u64,
        discontinuity: u64,
        init: Option<usize>,
    ) -> PathBuf {
        let init = init.map_or_else(|| "none".to_owned(), |i| i.to_string());
        self.track(track)
            .join(format!("{sequence}-{discontinuity}-{init}.seg"))
    }

    pub(crate) fn init(&self, track: usize, index: usize) -> PathBuf {
        self.track(track).join(format!("init-{index}.mp4"))
    }

    fn track(&self, track: usize) -> PathBuf {
        self.root.join("tracks").join(track.to_string())
    }

    /// 前 `tracks` 条轨已完成的分片，各轨按序号排列。不符合分片命名的文件（如 `.part`）忽略。
    pub(crate) async fn completed_segments(
        &self,
        tracks: usize,
    ) -> Result<Vec<Vec<SegmentFile>>, Error> {
        let dir = self.clone();
        blocking(move || {
            (0..tracks)
                .map(|track| list_segments(&dir.track(track)))
                .collect()
        })
        .await?
    }
}

/// 读取任务目录记录的任务；目录或 `job.json` 不存在时为 None。不加锁，只用于决定走哪条流程。
pub(crate) async fn recorded(root: PathBuf) -> Result<Option<JobKind>, Error> {
    blocking(move || {
        let job_path = root.join(JOB_FILE);
        match fs::read(&job_path) {
            Ok(bytes) => decode(&bytes).map(Some).map_err(|reason| Error::WorkDir {
                path: root.clone(),
                reason,
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(io_error("读取", &job_path)(source)),
        }
    })
    .await?
}

fn open(root: PathBuf, kind: &JobKind) -> Result<(WorkDir, DirLock), Error> {
    let invalid = |reason: String| Error::WorkDir {
        path: root.clone(),
        reason,
    };
    fs::create_dir_all(&root).map_err(io_error("创建", &root))?;
    let job_path = root.join(JOB_FILE);
    if !exists(&job_path)? {
        for entry in fs::read_dir(&root).map_err(io_error("读取", &root))? {
            let entry = entry.map_err(io_error("读取", &root))?;
            if entry.file_name() != LOCK_FILE {
                return Err(invalid("目录不为空，且没有 job.json，不是任务目录".into()));
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
        Err(TryLockError::WouldBlock) => return Err(invalid("正被另一个任务使用".into())),
        Err(TryLockError::Error(source)) => return Err(io_error("锁定", &lock_path)(source)),
    }

    match fs::read(&job_path) {
        Ok(bytes) => match (decode(&bytes).map_err(invalid)?, kind) {
            (found, expected) if found == *expected => {}
            (JobKind::Vod { .. }, JobKind::Vod { .. }) => return Err(Error::PlanChanged),
            (JobKind::Live { .. }, JobKind::Live { .. }) => {
                return Err(invalid("记录的是另一个直播源或另一种选轨".into()));
            }
            (JobKind::Vod { .. }, JobKind::Live { .. }) => {
                return Err(invalid("记录的是点播任务，当前来源是直播".into()));
            }
            (JobKind::Live { .. }, JobKind::Vod { .. }) => {
                return Err(invalid("记录的是直播任务，当前来源是点播".into()));
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => write_atomic(&job_path, &encode(kind))?,
        Err(source) => return Err(io_error("读取", &job_path)(source)),
    }
    Ok((WorkDir { root }, DirLock { _file: lock }))
}

fn encode(kind: &JobKind) -> Vec<u8> {
    let file = match kind {
        JobKind::Vod { plan_digest } => JobFile::Vod {
            format_version: FORMAT_VERSION,
            plan_digest: plan_digest.clone(),
        },
        JobKind::Live {
            source_digest,
            tracks,
        } => JobFile::Live {
            format_version: FORMAT_VERSION,
            source_digest: source_digest.clone(),
            tracks: tracks
                .iter()
                .map(|s| match s {
                    Streams::All => "all",
                    Streams::Video => "video",
                    Streams::Audio => "audio",
                })
                .map(str::to_owned)
                .collect(),
        },
    };
    serde_json::to_vec(&file).expect("JobFile 只含字符串与整数，序列化不会失败")
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
            source_digest,
            tracks,
            ..
        } => JobKind::Live {
            source_digest,
            tracks: tracks
                .iter()
                .map(|t| match t.as_str() {
                    "all" => Ok(Streams::All),
                    "video" => Ok(Streams::Video),
                    "audio" => Ok(Streams::Audio),
                    other => Err(format!("job.json 中轨的取流方式无法识别：{other:?}")),
                })
                .collect::<Result<_, _>>()?,
        },
    })
}

/// 一条轨目录中的分片文件，按序号排列；目录不存在时为空。
fn list_segments(dir: &Path) -> Result<Vec<SegmentFile>, Error> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error("读取", dir)(source)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_error("读取", dir))?;
        let path = entry.path();
        if let Some((sequence, discontinuity, init)) = parse_segment_name(&path) {
            let len = entry.metadata().map_err(io_error("读取", &path))?.len();
            files.push(SegmentFile {
                sequence,
                discontinuity,
                init,
                path,
                len,
            });
        }
    }
    files.sort_by_key(|f| f.sequence);
    Ok(files)
}

/// `<序号>-<不连续段>-<init 编号或 none>.seg` → (序号, 不连续段, init 编号)。
fn parse_segment_name(path: &Path) -> Option<(u64, u64, Option<usize>)> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".seg")?;
    let mut parts = stem.split('-');
    let sequence = parts.next()?.parse().ok()?;
    let discontinuity = parts.next()?.parse().ok()?;
    let init = match parts.next()? {
        "none" => None,
        index => Some(index.parse().ok()?),
    };
    if parts.next().is_some() {
        return None;
    }
    Some((sequence, discontinuity, init))
}

/// 已完成文件的字节数；不存在时为 None。
pub(crate) fn completed_len(path: &Path) -> Result<Option<u64>, Error> {
    match fs::metadata(path) {
        Ok(meta) => Ok(Some(meta.len())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error("读取", path)(source)),
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

/// 释放锁并删除整个任务目录。
pub(crate) async fn remove(dir: WorkDir, lock: DirLock) -> Result<(), Error> {
    // Windows 上打开着的锁文件无法删除，先释放
    drop(lock);
    blocking(move || fs::remove_dir_all(&dir.root).map_err(io_error("删除", &dir.root))).await?
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
    move |source| Error::Io {
        action,
        path,
        source,
    }
}
