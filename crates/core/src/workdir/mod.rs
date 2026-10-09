//! 任务目录：任务记录、独占锁、已完成的分片与 init 段。
//!
//! ```text
//! job.json                                                   任务记录，见 JobFile
//! lock                                                       运行期间持有排他锁
//! tracks/<轨道>/<会话>-<序号>-<不连续段>-<init>-<时长>-<身份>.seg  已解密、通过校验的分片
//! tracks/<轨道>/init-<指纹>.mp4                               init 段，按内容命名
//! ```
//!
//! 分片文件名中：会话为第几个录制会话（点播恒为 0），不连续段为会话内的编号，init 为所用 init 段的内容指纹
//! 或 `none`，时长为 EXTINF 声明的微秒数，身份见 [`Fingerprint::of_segment`]。合并要用的分组与时长、
//! 续录要用的身份都在文件名里，只凭目录内容即可合并或续录。
//!
//! 文件先写 `<名字>.part`，fsync 后改名，因此最终文件名存在即内容完整（断电也成立）。
//! 请求配置（请求头、Cookie 等）不写入目录：续传时由调用方再次提供。

mod names;
mod record;

use std::fs::{self, File, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub(crate) use self::names::SegmentName;
use self::names::{init_file_name, parse_init_name, parse_segment_name, segment_file_name};
pub(crate) use self::record::{JobRecord, RecordKind};
use self::record::{decode, encode};
use crate::ident::Fingerprint;
use crate::{Error, WorkDirProblem, blocking};

const JOB_FILE: &str = "job.json";
const LOCK_FILE: &str = "lock";
const TRACKS_DIR: &str = "tracks";

/// 任务目录中各文件的路径。
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    root: PathBuf,
}

/// 目录中一个已完成的分片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SegmentFile {
    pub name: SegmentName,
    pub path: PathBuf,
    /// 字节数
    pub len: u64,
}

/// 目录中已完成的内容。
pub(crate) struct Stored {
    /// 各轨的分片，按（会话, 序号）排列
    pub segments: Vec<Vec<SegmentFile>>,
    /// 各轨 init 段的字节数之和
    pub init_bytes: u64,
}

impl Layout {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn segment(&self, track: usize, name: &SegmentName) -> PathBuf {
        self.track(track).join(segment_file_name(name))
    }

    pub(crate) fn init(&self, track: usize, fingerprint: Fingerprint) -> PathBuf {
        self.track(track).join(init_file_name(fingerprint))
    }

    fn track(&self, track: usize) -> PathBuf {
        self.root.join(TRACKS_DIR).join(track.to_string())
    }
}

/// 已打开并加锁的任务目录；锁在 drop 时释放。
pub(crate) struct WorkDir {
    layout: Layout,
    previous: Option<JobRecord>,
    _lock: File,
}

impl WorkDir {
    /// 打开或新建任务目录并加锁。
    ///
    /// 已有 `job.json` 时其记录须与 `record` 相符（直播的完整地址除外）：来源不同报
    /// [`WorkDirProblem::SourceMismatch`]，点播与直播不同报 [`WorkDirProblem::KindMismatch`]，轨道不同报
    /// [`WorkDirProblem::TracksMismatch`]，点播的计划不同报 [`WorkDirProblem::PlanChanged`]；但目录里还没有已完成的
    /// 分片时，直接改为当前任务（没有可丢的内容）。没有 `job.json` 时目录必须不存在或为空
    /// （`.part` 残留与锁文件除外），以免把别人的目录当成任务目录（成功后会整个删除）。
    pub(crate) async fn open(root: PathBuf, record: JobRecord) -> Result<WorkDir, Error> {
        blocking(move || open(root, record)).await?
    }

    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    /// 目录里原有的记录与 `current` 不同。[`WorkDir::open`] 已排除其他不符，不同之处只可能是直播的完整地址。
    pub(crate) fn url_changed(&self, current: &JobRecord) -> bool {
        self.previous.as_ref().is_some_and(|p| p != current)
    }

    /// 用 `record` 替换 `job.json` 中的记录。
    pub(crate) async fn save(&self, record: &JobRecord) -> Result<(), Error> {
        let path = self.layout.root.join(JOB_FILE);
        let bytes = encode(record);
        blocking(move || write_atomic(&path, &bytes)).await?
    }

    /// 前 `tracks` 条轨已完成的分片与 init 段。
    pub(crate) async fn scan(&self, tracks: usize) -> Result<Stored, Error> {
        let layout = self.layout.clone();
        blocking(move || {
            let mut stored = Stored {
                segments: Vec::with_capacity(tracks),
                init_bytes: 0,
            };
            for track in 0..tracks {
                let dir = layout.track(track);
                let mut files: Vec<SegmentFile> = list(&dir, parse_segment_name)?
                    .into_iter()
                    .map(|(name, path, len)| SegmentFile { name, path, len })
                    .collect();
                files.sort_by_key(|f| (f.name.session, f.name.sequence));
                stored.segments.push(files);
                for (_, _, len) in list(&dir, parse_init_name)? {
                    stored.init_bytes += len;
                }
            }
            Ok(stored)
        })
        .await?
    }

    /// 删除整个任务目录。持锁删掉全部内容，最后删锁文件，再释放锁、删目录：释放锁之后别的任务就能打开这个目录，
    /// 先删内容保证它看到的不会是删到一半的任务；锁文件也在持锁时删，别的任务打开的是新建的锁文件，
    /// 不会锁上本任务还持有的那个。std 打开文件时允许删除，Windows 上持有句柄也能删。
    pub(crate) async fn remove(self) -> Result<(), Error> {
        let WorkDir { layout, _lock, .. } = self;
        blocking(move || {
            let root = &layout.root;
            for entry in fs::read_dir(root).map_err(io_error("读取", root))? {
                let entry = entry.map_err(io_error("读取", root))?;
                if entry.file_name() == LOCK_FILE {
                    continue;
                }
                let path = entry.path();
                let is_dir = entry.file_type().map_err(io_error("读取", &path))?.is_dir();
                let removed = if is_dir {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                };
                removed.map_err(io_error("删除", &path))?;
            }
            let lock = root.join(LOCK_FILE);
            fs::remove_file(&lock).map_err(io_error("删除", &lock))?;
            drop(_lock);
            match fs::remove_dir(root) {
                Ok(()) => Ok(()),
                // 释放锁后另一个任务已开始使用这个目录：本任务的内容已删完，目录留给它
                Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
                Err(e) => Err(io_error("删除", root)(e)),
            }
        })
        .await?
    }
}

/// 读取任务目录的记录；目录或 `job.json` 不存在时为 None。不加锁，只用于决定走哪条流程。
pub(crate) async fn read_record(root: PathBuf) -> Result<Option<JobRecord>, Error> {
    blocking(move || read_job(&root)).await?
}

fn read_job(root: &Path) -> Result<Option<JobRecord>, Error> {
    let job_path = root.join(JOB_FILE);
    match fs::read(&job_path) {
        Ok(bytes) => decode(&bytes).map(Some).map_err(|reason| Error::WorkDir {
            path: root.to_path_buf(),
            problem: WorkDirProblem::Corrupt(reason),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(cause) => Err(io_error("读取", &job_path)(cause)),
    }
}

fn open(root: PathBuf, record: JobRecord) -> Result<WorkDir, Error> {
    fs::create_dir_all(&root).map_err(io_error("创建", &root))?;
    let job_path = root.join(JOB_FILE);
    if !exists(&job_path)? {
        ensure_empty(&root)?;
    }
    let lock = lock(&root)?;
    let layout = Layout { root: root.clone() };
    let previous = match read_job(&root)? {
        None => None,
        Some(recorded) => match recorded.conflict(&record) {
            None => Some(recorded),
            Some(_) if !has_completed_segments(&layout)? => None,
            Some(conflict) => return Err(conflict.into_error(&root)),
        },
    };
    if previous.is_none() {
        remove_if_exists(&root.join(TRACKS_DIR))?;
        write_atomic(&job_path, &encode(&record))?;
    }
    Ok(WorkDir {
        layout,
        previous,
        _lock: lock,
    })
}

/// 没有 `job.json` 的目录只能是空的（`.part` 残留与锁文件除外），否则不是本库建立的任务目录。
fn ensure_empty(root: &Path) -> Result<(), Error> {
    for entry in fs::read_dir(root).map_err(io_error("读取", root))? {
        let name = entry.map_err(io_error("读取", root))?.file_name();
        let name = name.to_string_lossy();
        if name != LOCK_FILE && !name.ends_with(".part") {
            return Err(Error::WorkDir {
                path: root.to_path_buf(),
                problem: WorkDirProblem::NotEmpty,
            });
        }
    }
    Ok(())
}

/// 打开锁文件并加排他锁；已被别的任务锁住时报 [`WorkDirProblem::Locked`]。
fn lock(root: &Path) -> Result<File, Error> {
    let path = root.join(LOCK_FILE);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_error("创建", &path))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Error::WorkDir {
            path: root.to_path_buf(),
            problem: WorkDirProblem::Locked,
        }),
        Err(TryLockError::Error(cause)) => Err(io_error("锁定", &path)(cause)),
    }
}

/// 目录里有没有已完成的分片。只有 init 段时不算：它们随时可以重新拉取，没有可丢的内容。
fn has_completed_segments(layout: &Layout) -> Result<bool, Error> {
    let tracks = layout.root.join(TRACKS_DIR);
    let entries = match fs::read_dir(&tracks) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(cause) => return Err(io_error("读取", &tracks)(cause)),
    };
    for entry in entries {
        let entry = entry.map_err(io_error("读取", &tracks))?;
        let dir = entry.path();
        if !entry.file_type().map_err(io_error("读取", &dir))?.is_dir() {
            continue;
        }
        if !list(&dir, parse_segment_name)?.is_empty() {
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
        let meta = entry.metadata().map_err(io_error("读取", &path))?;
        if meta.is_file() {
            files.push((parsed, path, meta.len()));
        }
    }
    Ok(files)
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
        let dir = parent(&path);
        fs::create_dir_all(dir).map_err(io_error("创建", dir))?;
        write_atomic(&path, &data)
    })
    .await?
}

/// 存入第 `track` 条轨的 init 段；`fingerprint` 为 `data` 的内容指纹，决定文件名。
pub(crate) async fn store_init(
    layout: &Layout,
    track: usize,
    fingerprint: Fingerprint,
    data: Vec<u8>,
) -> Result<StoredInit, Error> {
    let path = layout.init(track, fingerprint);
    blocking(move || {
        if completed_len(&path)?.is_some() {
            return Ok(StoredInit::Existing);
        }
        let dir = parent(&path);
        fs::create_dir_all(dir).map_err(io_error("创建", dir))?;
        write_atomic(&path, &data).map(|()| StoredInit::Created)
    })
    .await?
}

/// [`store_init`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoredInit {
    Created,
    /// 内容相同的已经存在，没有重写
    Existing,
}

/// 文件所在目录；任务目录内的文件都有上级目录。
fn parent(path: &Path) -> &Path {
    path.parent().expect("任务目录内的文件都有上级目录")
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
