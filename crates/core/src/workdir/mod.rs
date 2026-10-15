//! 任务目录：任务记录、独占锁、已完成的分片与 init 段。
//!
//! ```text
//! job.json                                                          任务记录，见 JobFile
//! lock                                                              运行期间持有排他锁
//! outputs.json                                                      正在写的输出，见 PendingOutput
//! tracks/<轨道>/<会话>-<起点>-<序号>-<不连续段>-<init>-<时长>-<身份>.seg  已解密、通过校验的分片
//! tracks/<轨道>/init-<指纹>.mp4                                      init 段，按内容命名
//! ```
//!
//! 分片文件名中：会话为第几个录制会话（点播恒为 0），起点为这条轨在这个会话从哪里开始录（见 [`SessionStart`]，
//! 续录据此确定补录的范围），不连续段为会话内的编号，init 为所用 init 段的内容指纹或 `none`，时长为 EXTINF
//! 声明的微秒数，身份见 [`Fingerprint::of_segment`]。合并与续录要用的都在分片自己的文件名里，只凭目录内容
//! 即可合并或续录。
//!
//! 文件先写 `<名字>.part`，fsync 后改名，因此最终文件名存在即内容完整（断电也成立）。
//! 请求配置（请求头、Cookie 等）不写入目录：续传时由调用方再次提供。

mod lock;
mod names;
mod outputs;
mod record;
mod removal;

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub(crate) use self::lock::Lock;
use self::lock::acquire;
pub use self::lock::{ForkGuard, LockFiles, close_inherited_locks, prepare_fork};
pub(crate) use self::names::{GroupKey, SegmentName, SessionStart};
use self::names::{init_file_name, parse_init_name, parse_segment_name, segment_file_name};
pub(crate) use self::outputs::{
    OutputKind, PendingOutput, clear_outputs, outputs_file, read_outputs, record_outputs,
};
pub(crate) use self::record::{JobRecord, RecordKind, preference_digest, url_digest};
use self::record::{decode, encode};
pub(crate) use self::removal::remove;
use self::removal::{remove_steps, remove_tracks};
use crate::entries;
use crate::error::io_error;
use crate::ident::Fingerprint;
use crate::{Error, Leftover, WorkDirProblem, blocking, busy, volume};

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

impl Stored {
    /// 第 `track` 条轨已完成分片的声明时长之和，微秒。
    pub(crate) fn duration_us(&self, track: usize) -> u64 {
        self.segments[track]
            .iter()
            .map(|f| f.name.duration_us)
            .fold(0u64, u64::saturating_add)
    }

    /// 分片与 init 段的字节数之和。
    pub(crate) fn bytes(&self) -> u64 {
        self.segments.iter().flatten().map(|f| f.len).sum::<u64>() + self.init_bytes
    }
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

    /// 第 `track` 条轨的分片 `name` 所用的 init 段；分片没有 init 段时为 None。
    pub(crate) fn segment_init(&self, track: usize, name: &SegmentName) -> Option<PathBuf> {
        name.init.map(|fingerprint| self.init(track, fingerprint))
    }

    fn track(&self, track: usize) -> PathBuf {
        self.root.join(TRACKS_DIR).join(track.to_string())
    }
}

/// 已打开并加锁的任务目录；锁在 drop 时释放。
pub(crate) struct WorkDir {
    layout: Layout,
    /// 打开时给的记录，即本次任务
    record: JobRecord,
    /// 见 [`WorkDir::unverified`]
    unverified: bool,
    lock: Lock,
}

impl WorkDir {
    /// 任务目录已存在时加锁，不存在时为 None、什么也不建；检查同 [`WorkDir::open`]（没有 `job.json` 时须为空）。
    /// 任务开头用它挡住同时使用这个目录的任务，再收拾上次写输出留下的东西；锁随后交给 [`WorkDir::open`]。
    pub(crate) async fn lock_existing(root: PathBuf) -> Result<Option<Lock>, Error> {
        blocking(move || {
            if !exists(&root)? {
                return Ok(None);
            }
            if !exists(&root.join(JOB_FILE))? {
                ensure_empty(&root)?;
            }
            acquire(&root).map(Some)
        })
        .await?
    }

    /// 打开或新建任务目录并加锁；`lock` 为 [`WorkDir::lock_existing`] 已加上的锁。
    ///
    /// 已有 `job.json` 时其记录须与 `record` 相符（见 [`JobRecord::conflict`]，`reselect` 同
    /// [`crate::JobRequest::reselect`]），不符时报那个原因；但目录里还没有已完成的分片时，直接改为当前任务：删掉之前的
    /// init 段，不是本库写的文件留下。相符而有差别时，须核对内容的见 [`WorkDir::unverified`]，其余的（重新选轨只差选轨
    /// 偏好）直接改记。没有 `job.json` 时目录必须不存在或为空（`.part` 残留与锁文件除外），以免把别人的目录当成任务目录
    /// （成功后会整个删除）。
    pub(crate) async fn open(
        root: PathBuf,
        record: JobRecord,
        lock: Option<Lock>,
        reselect: bool,
    ) -> Result<WorkDir, Error> {
        blocking(move || open(root, record, lock, reselect)).await?
    }

    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    /// 本次任务的记录（打开时给的）。
    pub(crate) fn record(&self) -> &JobRecord {
        &self.record
    }

    /// 沿用了目录里原有的记录，而打开时给的与它的差别要核对过内容才能改记：完整来源地址不同（换了令牌、主机或路径），
    /// 或重新选轨选出的轨与记录的身份不同（见 [`crate::JobRequest::reselect`]）。核对过这次取到的是同一份内容（点播见
    /// [`crate::vod::download`]，直播见 [`crate::LiveOptions`]）之后用 [`WorkDir::adopt_record`] 改记。新建或改为当前
    /// 任务时为 false。
    pub(crate) fn unverified(&self) -> bool {
        self.unverified
    }

    /// 把 `job.json` 中的记录换成本次的（见 [`WorkDir::unverified`]）。
    pub(crate) async fn adopt_record(&self) -> Result<(), Error> {
        let path = self.layout.root.join(JOB_FILE);
        let bytes = encode(&self.record);
        blocking(move || write_atomic(&path, &bytes)).await?
    }

    /// 直播录制目录中前 `tracks` 条轨已完成的分片与 init 段。
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

    /// 删除整个任务目录。只删本库写的文件（`job.json`、`outputs.json`、各轨的分片与 init 段，以及它们写到一半的
    /// `.part`）、系统自动生成的元数据文件（见 [`entries::Entry::is_system_file`]）与因此变空的目录；其他文件（例如经符
    /// 号链接或只差大小写的路径写进来的输出）原样留下。有东西留下时 `job.json` 也留下：目录仍是可识别的任务目录，下次运
    /// 行照常使用（没有已完成的分片，按当前任务重新开始）。没删干净时返回留下的原因。
    ///
    /// 持锁删掉其余内容，最后删锁文件，再释放锁、删目录：释放锁之后别的任务就能打开这个目录，先删内容保证它看到的
    /// 不会是删到一半的任务；锁文件也在持锁时删，别的任务打开的是新建的锁文件，不会锁上本任务还持有的那个。
    /// std 打开文件时允许删除，Windows 上持有句柄也能删。
    pub(crate) async fn remove(self) -> Option<Leftover> {
        let WorkDir { layout, lock, .. } = self;
        remove_steps(layout.root, lock, None)
            .await
            .expect("不取消时删除做完才返回")
    }
}

/// 有可续的内容时读取任务目录的记录；目录或 `job.json` 不存在，或还没有已完成的分片时为 None（按当前请求
/// 从头开始，见 [`WorkDir::open`]）。用于决定走哪条流程、按什么选轨；目录在任务开头已存在时调用方已加锁，
/// 之后才建立的目录由 [`WorkDir::open`] 加锁后再核对。
pub(crate) async fn read_resumable(root: PathBuf) -> Result<Option<JobRecord>, Error> {
    blocking(move || {
        let Some(record) = read_job(&root)? else {
            return Ok(None);
        };
        Ok(has_completed_segments(&Layout { root })?.then_some(record))
    })
    .await?
}

fn read_job(root: &Path) -> Result<Option<JobRecord>, Error> {
    let job_path = root.join(JOB_FILE);
    match fs::read(&job_path) {
        Ok(bytes) => decode(&bytes).map(Some).map_err(|problem| Error::WorkDir {
            path: root.to_path_buf(),
            problem,
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(cause) => Err(io_error("读取", &job_path)(cause)),
    }
}

fn open(
    root: PathBuf,
    record: JobRecord,
    lock: Option<Lock>,
    reselect: bool,
) -> Result<WorkDir, Error> {
    let job_path = root.join(JOB_FILE);
    let lock = match lock {
        Some(lock) => lock,
        None => {
            fs::create_dir_all(&root).map_err(io_error("创建", &root))?;
            if !exists(&job_path)? {
                ensure_empty(&root)?;
            }
            acquire(&root)?
        }
    };
    let layout = Layout { root: root.clone() };
    let previous = match read_job(&root)? {
        None => None,
        Some(recorded) => match recorded.conflict(&record, reselect) {
            None => Some(recorded),
            Some(_) if !has_completed_segments(&layout)? => None,
            Some(problem) => {
                return Err(Error::WorkDir {
                    path: root,
                    problem,
                });
            }
        },
    };
    if previous.is_none() {
        // 不是本库写的文件（如删除任务目录时留下的）不影响读写，删除任务目录时照常指出
        let tracks = root.join(TRACKS_DIR);
        if exists(&tracks)? {
            remove_tracks(&tracks, &mut Vec::new())?;
        }
        write_atomic(&job_path, &encode(&record))?;
    }
    let unverified = previous.as_ref().is_some_and(|p| {
        p.url_digest != record.url_digest || p.selection_keys() != record.selection_keys()
    });
    if previous.as_ref().is_some_and(|p| *p != record) && !unverified {
        write_atomic(&job_path, &encode(&record))?;
    }
    Ok(WorkDir {
        layout,
        record,
        unverified,
        lock,
    })
}

/// 没有 `job.json` 的目录只能是空的（`.part` 残留与锁文件除外），否则不是本库建立的任务目录。
fn ensure_empty(root: &Path) -> Result<(), Error> {
    for entry in entries::list(root)? {
        if entry.is_system_file() {
            continue;
        }
        let name = entry.name.to_string_lossy();
        if name != LOCK_FILE && !name.ends_with(".part") {
            return Err(Error::WorkDir {
                path: root.to_path_buf(),
                problem: WorkDirProblem::NotEmpty,
            });
        }
    }
    Ok(())
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
    busy::retrying(|| fs::rename(&part, path)).map_err(io_error("重命名", &part))
}

/// 跟随符号链接；不存在时根读不到即报错（见 [`volume::reachable`]）。
fn exists(path: &Path) -> Result<bool, Error> {
    match path.try_exists() {
        Ok(true) => Ok(true),
        Ok(false) => volume::reachable(path).map(|()| false),
        Err(cause) => Err(io_error("检查", path)(cause)),
    }
}
