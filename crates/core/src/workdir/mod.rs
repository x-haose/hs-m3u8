//! 任务目录：任务记录、独占锁，HLS 任务已完成的分片与 init 段，文件任务的数据文件与已完成的范围。
//!
//! ```text
//! job.json                                                          任务记录，见 JobFile
//! lock                                                              运行期间持有排他锁
//! outputs.json                                                      正在写的输出，见 PendingOutput
//! tracks/<轨道>/<会话>-<起点>-<序号>-<不连续段>-<init>-<时长>-<身份>.seg  HLS：已解密、通过校验的分片
//! tracks/<轨道>/init-<指纹>.mp4                                      HLS：init 段，按内容命名
//! data                                                              文件任务：下载中的文件，按偏移写入
//! ranges.json                                                       文件任务：data 里已落盘的范围，见 RangesFile
//! ```
//!
//! 分片文件名中：会话为第几个录制会话（点播恒为 0），起点为这条轨在这个会话从哪里开始录（见 [`SessionStart`]，
//! 续录据此确定补录的范围），不连续段为会话内的编号，init 为所用 init 段的内容指纹或 `none`，时长为 EXTINF
//! 声明的微秒数，身份见 [`Fingerprint::of_segment`]。合并与续录要用的都在分片自己的文件名里，只凭目录内容
//! 即可合并或续录。
//!
//! 文件先写 `<名字>.part`，fsync 后改名，因此最终文件名存在即内容完整（断电也成立）；`data` 例外，它就地写入，哪些部分
//! 完整由 `ranges.json` 记（data 先落盘、再写它）。请求配置（请求头、Cookie 等）不写入目录：续传时由调用方再次提供。

mod lock;
mod names;
mod outputs;
mod ranges;
mod record;
mod removal;

use std::fs::{self, File};
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(crate) use self::lock::Lock;
use self::lock::acquire;
pub use self::lock::{ForkGuard, LockFiles, close_inherited_locks, prepare_fork};
pub(crate) use self::names::{GroupKey, SegmentName, SessionStart};
use self::names::{init_file_name, parse_init_name, parse_segment_name, segment_file_name};
pub(crate) use self::outputs::{
    OutputKind, PendingOutput, clear_outputs, outputs_file, read_outputs, record_outputs,
};
use self::ranges::RANGES_FILE;
pub(crate) use self::record::{
    FileRecord, HlsRecord, RecordKind, Recorded, SelectionCheck, identity_digest,
    preference_digest, url_digest,
};
use self::record::{decode, encode_file, encode_hls};
pub(crate) use self::removal::remove;
use self::removal::{remove_steps, remove_tracks_of};
use crate::entries::{self, is_canonical_number};
use crate::error::io_error;
use crate::ident::Fingerprint;
use crate::{
    Error, FileSaved, HlsSaved, JobType, Leftover, Saved, WorkDirProblem, blocking, busy, volume,
};

const JOB_FILE: &str = "job.json";
const LOCK_FILE: &str = "lock";
const TRACKS_DIR: &str = "tracks";
const DATA_FILE: &str = "data";

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
    /// 打开时 `job.json` 里的记录：打开时给的（新建、改为当前任务，或与原有的差别无须核对、已直接改记），或与它有须核对
    /// 的差别的原有记录
    record: HlsRecord,
    /// 打开时给的记录与原有的有须核对内容才能改记的差别时，为打开时给的记录与差别
    pending: Option<Pending>,
    /// `pending` 已核对上、已改记（见 [`WorkDir::adopt_record`]）
    adopted: AtomicBool,
    lock: Lock,
}

/// 等核对内容的记录。
struct Pending {
    record: HlsRecord,
    change: SourceChange,
}

/// 打开时给的记录与目录里原有的差别中，要核对过内容才能改记的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceChange {
    /// 完整来源地址不同（换了令牌、主机或路径）
    pub url: bool,
    /// 按轨道顺序，所选的轨身份与记录的不同（重新选轨时才会，见 [`crate::JobRequest::reselect`]）
    pub tracks: Vec<bool>,
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
    /// 已有 `job.json` 时其记录须与 `record` 相符（所选的轨按 `check` 比较，见 [`HlsRecord::conflict`]），不符时报那个
    /// 原因；但目录里还没有已完成的分片时，直接改为当前任务：删掉之前的 init 段，不是本库写的文件留下。相符时，身份相同
    /// 的轨沿用记录里的属性；其余差别中须核对内容的见 [`WorkDir::source_change`]，无须核对的（选轨偏好）直接改记。没有
    /// `job.json` 时目录必须不存在或为空（`.part` 残留与锁文件除外），以免把别人的目录当成任务目录（成功后会整个删
    /// 除）。
    pub(crate) async fn open(
        root: PathBuf,
        record: HlsRecord,
        lock: Option<Lock>,
        check: SelectionCheck,
    ) -> Result<WorkDir, Error> {
        blocking(move || open(root, record, lock, check)).await?
    }

    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    /// 生效的记录，即 `job.json` 里的：有须核对的差别（见 [`WorkDir::source_change`]）而还没改记时为目录里原有的，合并
    /// 与写出输出照它描述已有的内容；否则为打开时给的（同一条轨沿用原有的属性）。
    pub(crate) fn record(&self) -> &HlsRecord {
        match &self.pending {
            Some(pending) if self.adopted.load(Ordering::Relaxed) => &pending.record,
            _ => &self.record,
        }
    }

    /// 打开时给的记录与目录里原有的有须核对内容才能改记的差别；没有（新建、改为当前任务、没有差别或差别无须核对）时为
    /// None。核对过这次取到的是同一份内容（点播见 [`crate::vod::download`]，直播见 [`crate::LiveOptions`]）之后用
    /// [`WorkDir::adopt_record`] 改记。
    pub(crate) fn source_change(&self) -> Option<&SourceChange> {
        self.pending.as_ref().map(|pending| &pending.change)
    }

    /// 把 `job.json` 中的记录换成打开时给的，此后它是生效的记录；只在有须核对的差别（见 [`WorkDir::source_change`]）且
    /// 已核对上时调用。
    pub(crate) async fn adopt_record(&self) -> Result<(), Error> {
        let pending = self.pending.as_ref().expect("只在有须核对的差别时改记");
        let path = self.layout.root.join(JOB_FILE);
        let bytes = encode_hls(&pending.record);
        blocking(move || write_atomic(&path, &bytes)).await??;
        self.adopted.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// 直播录制目录中前 `tracks` 条轨已完成的分片与 init 段。
    pub(crate) async fn scan(&self, tracks: usize) -> Result<Stored, Error> {
        let layout = self.layout.clone();
        blocking(move || scan(&layout, tracks)).await?
    }

    /// 删除整个任务目录。只删本库写的文件（`job.json`、`outputs.json`、各轨的分片与 init 段、文件任务的 `data` 与
    /// `ranges.json`，以及它们写到一半的 `.part`）、系统自动生成的元数据文件（见 [`entries::Entry::is_system_file`]）与
    /// 因此变空的目录；其他文件（例如经符号链接或只差大小写的路径写进来的输出）原样留下。有东西留下时 `job.json` 也留
    /// 下：目录仍是可识别的任务目录，下次运行照常使用（没有已完成的分片，按当前任务重新开始）。没删干净时返回留下的原
    /// 因。
    ///
    /// 持锁删掉其余内容，最后删锁文件，再释放锁、删目录：释放锁之后别的任务就能打开这个目录，先删内容保证它看到的
    /// 不会是删到一半的任务；锁文件也在持锁时删，别的任务打开的是新建的锁文件，不会锁上本任务还持有的那个。
    /// std 打开文件时允许删除，Windows 上持有句柄也能删。
    pub(crate) async fn remove(self) -> Option<Leftover> {
        let WorkDir { layout, lock, .. } = self;
        remove_all(layout.root, lock).await
    }
}

/// 持着 `lock` 删除整个任务目录 `root`，不中途停下（见 [`WorkDir::remove`]）。
async fn remove_all(root: PathBuf, lock: Lock) -> Option<Leftover> {
    remove_steps(root, lock, None)
        .await
        .expect("不取消时删除做完才返回")
}

/// `lock` 为 [`WorkDir::lock_existing`] 已加上的锁时即它；没有（任务开头目录还不存在）时建出目录、加锁，没有 `job.json`
/// 的目录须为空（见 [`ensure_empty`]）。
fn lock_or_create(root: &Path, lock: Option<Lock>) -> Result<Lock, Error> {
    if let Some(lock) = lock {
        return Ok(lock);
    }
    fs::create_dir_all(root).map_err(io_error("创建", root))?;
    if !exists(&root.join(JOB_FILE))? {
        ensure_empty(root)?;
    }
    acquire(root)
}

/// 前 `tracks` 条轨已完成的分片与 init 段。
fn scan(layout: &Layout, tracks: usize) -> Result<Stored, Error> {
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
}

/// 任务目录 `root`（绝对路径）里已完成的内容，不加锁、不改动；目录或 `job.json` 不存在时为 None（见
/// [`crate::Engine::inspect`]）。
pub(crate) async fn inspect(root: PathBuf) -> Result<Option<Saved>, Error> {
    blocking(move || read_saved(&root)).await?
}

fn read_saved(root: &Path) -> Result<Option<Saved>, Error> {
    if !exists(root)? {
        return Ok(None);
    }
    let record = match read_job(root)? {
        None => return Ok(None),
        Some(Recorded::Hls(record)) => record,
        Some(Recorded::File(record)) => {
            let ranges = match record.length {
                Some(length) => read_ranges_within(root, length)?,
                None => Vec::new(),
            };
            return Ok(Some(Saved::File(FileSaved {
                bytes: ranges.iter().map(|r| r.end - r.start).sum(),
                total: record.length,
            })));
        }
    };
    let layout = Layout {
        root: root.to_path_buf(),
    };
    let stored = scan(&layout, record.streams().len())?;
    Ok(Some(Saved::Hls(HlsSaved {
        live: matches!(record.kind, RecordKind::Live),
        segments: stored.segments.iter().map(Vec::len).sum(),
        bytes: stored.bytes(),
        duration: Duration::from_micros(stored.duration_us(0)),
    })))
}

/// 有可续的内容时读取任务目录的记录；目录或 `job.json` 不存在，或还没有已完成的内容（见 [`has_completed`]）时为 None
/// （按当前请求从头开始，见 [`WorkDir::open`]）。用于决定走哪条流程、按什么选轨；目录在任务开头已存在时调用方已加锁，
/// 之后才建立的目录由 [`WorkDir::open`] 加锁后再核对。
pub(crate) async fn read_resumable(root: PathBuf) -> Result<Option<Recorded>, Error> {
    blocking(move || {
        let Some(record) = read_job(&root)? else {
            return Ok(None);
        };
        Ok(has_completed(&root, &record)?.then_some(record))
    })
    .await?
}

fn read_job(root: &Path) -> Result<Option<Recorded>, Error> {
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
    record: HlsRecord,
    lock: Option<Lock>,
    check: SelectionCheck,
) -> Result<WorkDir, Error> {
    let job_path = root.join(JOB_FILE);
    let lock = lock_or_create(&root, lock)?;
    check_tracks(&root)?;
    let layout = Layout { root: root.clone() };
    let conflict = |problem| Error::WorkDir {
        path: root.clone(),
        problem,
    };
    let recorded = read_job(&root)?;
    let previous = match &recorded {
        None => None,
        Some(recorded) if !has_completed(&root, recorded)? => None,
        Some(Recorded::Hls(recorded)) => match recorded.conflict(&record, check) {
            None => Some(*recorded.clone()),
            Some(problem) => return Err(conflict(problem)),
        },
        Some(Recorded::File(_)) => {
            return Err(conflict(WorkDirProblem::KindMismatch {
                recorded: JobType::File,
                current: record.job_type(),
            }));
        }
    };
    let (record, pending) = match previous {
        None => {
            clear_content(&root, recorded.as_ref())?;
            write_atomic(&job_path, &encode_hls(&record))?;
            (record, None)
        }
        Some(previous) => {
            let record = record.keeping_attributes_of(&previous);
            let change = SourceChange {
                url: previous.url_digest != record.url_digest,
                tracks: previous.changed_tracks(&record),
            };
            if change.url || change.tracks.contains(&true) {
                (previous, Some(Pending { record, change }))
            } else {
                if previous != record {
                    write_atomic(&job_path, &encode_hls(&record))?;
                }
                (record, None)
            }
        }
    };
    Ok(WorkDir {
        layout,
        record,
        pending,
        adopted: AtomicBool::new(false),
        lock,
    })
}

/// 删掉任务目录里之前的任务留下的内容，换成当前任务：HLS 的分片与 init 段，与 `job.json` 记着的 `recorded` 是文件任务时
/// 它的数据文件与范围记录（记的不是文件任务时同名的文件不是本库写的）。不是本库写的文件（如删除任务目录时留下的）不影响
/// 读写，删除任务目录时照常指出。
fn clear_content(root: &Path, recorded: Option<&Recorded>) -> Result<(), Error> {
    remove_tracks_of(root)?;
    if !matches!(recorded, Some(Recorded::File(_))) {
        return Ok(());
    }
    // 只删普通文件：同名的符号链接、目录不是本库写的
    for name in [RANGES_FILE, DATA_FILE] {
        let path = root.join(name);
        if entries::kind_of(&path)?.is_some_and(|kind| kind.is_file()) {
            busy::retrying(|| fs::remove_file(&path)).map_err(io_error("删除", &path))?;
        }
    }
    Ok(())
}

/// 任务目录 `root` 里的 `tracks` 与其下各轨的目录（名为规范写法的编号）已有时须是目录，不跟随符号链接；不是时报
/// [`WorkDirProblem::NotEmpty`]：读进去会把别处的文件当成已完成的分片。
fn check_tracks(root: &Path) -> Result<(), Error> {
    let not_ours = || Error::WorkDir {
        path: root.to_path_buf(),
        problem: WorkDirProblem::NotEmpty,
    };
    let tracks = root.join(TRACKS_DIR);
    match entries::kind_of(&tracks)? {
        None => return Ok(()),
        Some(kind) if !kind.is_dir() => return Err(not_ours()),
        Some(_) => {}
    }
    for entry in entries::list(&tracks)? {
        if entry.name.to_str().is_some_and(is_canonical_number) && !entry.kind.is_dir() {
            return Err(not_ours());
        }
    }
    Ok(())
}

/// 没有 `job.json` 的目录只能是空的（是普通文件的 `.part` 残留与锁文件除外），否则不是本库建立的任务目录。
fn ensure_empty(root: &Path) -> Result<(), Error> {
    for entry in entries::list(root)? {
        if entry.is_system_file() {
            continue;
        }
        let name = entry.name.to_string_lossy();
        let partial = name.ends_with(".part") && entry.kind.is_file();
        if name != LOCK_FILE && !partial {
            return Err(Error::WorkDir {
                path: root.to_path_buf(),
                problem: WorkDirProblem::NotEmpty,
            });
        }
    }
    Ok(())
}

/// 记着 `recorded` 的目录里有没有丢了可惜的内容：HLS 任务已完成的分片（只有 init 段时不算：它们随时可以重新拉取），或
/// 文件任务已完成的范围。
fn has_completed(root: &Path, recorded: &Recorded) -> Result<bool, Error> {
    match recorded {
        Recorded::Hls(_) => has_completed_segments(root),
        Recorded::File(_) => Ok(!read_ranges(root)?.is_empty()),
    }
}

/// 文件任务目录 `root` 里已落盘的范围（见 [`ranges`]）；没有记录时为空。不核对总长度，见 [`read_ranges_within`]。
fn read_ranges(root: &Path) -> Result<Vec<Range<u64>>, Error> {
    let path = root.join(RANGES_FILE);
    match fs::read(&path) {
        Ok(bytes) => ranges::decode(&bytes).map_err(|problem| Error::WorkDir {
            path: root.to_path_buf(),
            problem,
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(cause) => Err(io_error("读取", &path)(cause)),
    }
}

/// 文件任务目录 `root` 里已落盘的范围（见 [`read_ranges`]），核对过不超过记录的总长度 `length`：超过时为目录损坏。
fn read_ranges_within(root: &Path, length: u64) -> Result<Vec<Range<u64>>, Error> {
    let ranges = read_ranges(root)?;
    match ranges.last() {
        Some(last) if last.end > length => Err(Error::WorkDir {
            path: root.to_path_buf(),
            problem: WorkDirProblem::Corrupt(format!(
                "{RANGES_FILE} 里的范围到 {}，超过总长 {length}",
                last.end
            )),
        }),
        _ => Ok(ranges),
    }
}

/// 目录里有没有已完成的分片。
fn has_completed_segments(root: &Path) -> Result<bool, Error> {
    let tracks = root.join(TRACKS_DIR);
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

/// 文件任务任务目录里可续的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileResume {
    pub record: FileRecord,
    /// 总长度（记录里有它才可续）
    pub length: u64,
    /// 已落盘的范围，升序、不重叠，至少一个
    pub ranges: Vec<Range<u64>>,
}

/// 文件任务的续传要用的：`job.json` 记的是文件任务、知道总长度、且有已落盘的范围时为它们；目录或 `job.json` 不存在、
/// 没有已完成的内容时为 None（从头开始）。记的是有已完成分片的 HLS 任务时报 [`WorkDirProblem::KindMismatch`]。
/// 目录在任务开头已存在时调用方已加锁。
pub(crate) async fn read_file_resume(root: PathBuf) -> Result<Option<FileResume>, Error> {
    blocking(move || {
        let record = match read_job(&root)? {
            None => return Ok(None),
            Some(Recorded::File(record)) => record,
            Some(hls @ Recorded::Hls(_)) => {
                if !has_completed(&root, &hls)? {
                    return Ok(None);
                }
                return Err(Error::WorkDir {
                    path: root,
                    problem: WorkDirProblem::KindMismatch {
                        recorded: hls.job_type(),
                        current: JobType::File,
                    },
                });
            }
        };
        let Some(length) = record.length else {
            return Ok(None);
        };
        let ranges = read_ranges_within(&root, length)?;
        Ok((!ranges.is_empty()).then_some(FileResume {
            record,
            length,
            ranges,
        }))
    })
    .await?
}

/// 加了锁的文件任务的任务目录；锁在 drop 时释放。
pub(crate) struct FileDir {
    root: PathBuf,
    lock: Lock,
}

impl FileDir {
    /// 打开或新建文件任务的任务目录并加锁，`job.json` 写为 `record`；`lock` 为 [`WorkDir::lock_existing`] 已加上的锁。
    /// `resumed` 为任务开头读到的可续内容（见 [`read_file_resume`]，调用方已核对过来源）：有时保留数据文件与范围记录，
    /// 没有时删掉之前的内容（见 [`clear_content`]），从头开始。没有 `job.json` 时目录须为空，同 [`WorkDir::open`]；
    /// 名为 `data`、`ranges.json` 的项不是本库写的（`job.json` 记的不是文件任务，或它不是普通文件，如符号链接）时报
    /// [`WorkDirProblem::NotEmpty`]，不改记录。任务开头目录还不存在、加锁时已有了别的任务的内容（同时用它建了目录）时，
    /// 是 HLS 任务的报 [`WorkDirProblem::KindMismatch`]，是文件任务的报 [`WorkDirProblem::Locked`]。
    pub(crate) async fn open(
        root: PathBuf,
        lock: Option<Lock>,
        record: FileRecord,
        resumed: bool,
    ) -> Result<FileDir, Error> {
        blocking(move || {
            let created = lock.is_none();
            let lock = lock_or_create(&root, lock)?;
            let recorded = read_job(&root)?;
            let problem = |problem| Error::WorkDir {
                path: root.clone(),
                problem,
            };
            if created
                && let Some(recorded) = &recorded
                && has_completed(&root, recorded)?
            {
                return Err(problem(match recorded {
                    Recorded::Hls(hls) => WorkDirProblem::KindMismatch {
                        recorded: hls.job_type(),
                        current: JobType::File,
                    },
                    Recorded::File(_) => WorkDirProblem::Locked,
                }));
            }
            let file_task = matches!(recorded, Some(Recorded::File(_)));
            for name in [DATA_FILE, RANGES_FILE] {
                match entries::kind_of(&root.join(name))? {
                    None => {}
                    Some(kind) if file_task && kind.is_file() => {}
                    Some(_) => return Err(problem(WorkDirProblem::NotEmpty)),
                }
            }
            if !resumed {
                clear_content(&root, recorded.as_ref())?;
            }
            write_atomic(&root.join(JOB_FILE), &encode_file(&record))?;
            Ok(FileDir { root, lock })
        })
        .await?
    }

    /// 数据文件：下载中的内容按偏移写入
    pub(crate) fn data(&self) -> PathBuf {
        self.root.join(DATA_FILE)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// 记下数据文件里已落盘的范围 `ranges`（升序、不重叠、不相接、不为空）；调用方须已让这些范围落盘。
    pub(crate) async fn record_ranges(&self, ranges: Vec<Range<u64>>) -> Result<(), Error> {
        let path = self.root.join(RANGES_FILE);
        blocking(move || write_atomic(&path, &ranges::encode(&ranges))).await?
    }

    /// 删除整个任务目录，同 [`WorkDir::remove`]。
    pub(crate) async fn remove(self) -> Option<Leftover> {
        remove_all(self.root, self.lock).await
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
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            // 列出之后、读属性之前被删掉（放弃任务、别的进程收拾目录时）：已不在，不算已完成的内容
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(cause) => return Err(io_error("读取", &path)(cause)),
        };
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

/// 写入某条轨的完整文件 `path`（见 [`Layout::segment`]）：先写 `.part` 并 fsync，再改名，所在的轨目录见
/// [`create_track_dir`]。
pub(crate) async fn write(path: PathBuf, data: Vec<u8>) -> Result<(), Error> {
    blocking(move || {
        create_track_dir(parent(&path))?;
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
        create_track_dir(parent(&path))?;
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

/// 建出轨目录 `dir`（`<任务目录>/tracks/<轨>`）与 `tracks`，已有的沿用。已有的不是目录（如符号链接：不跟随，不写到它
/// 指向的地方）时报 [`WorkDirProblem::NotEmpty`]。
fn create_track_dir(dir: &Path) -> Result<(), Error> {
    let tracks = parent(dir);
    let root = parent(tracks);
    for dir in [tracks, dir] {
        match fs::create_dir(dir) {
            Ok(()) => {}
            // 同一条轨的分片并发写入时可能同时建它
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if !entries::kind_of(dir)?.is_some_and(|kind| kind.is_dir()) {
                    return Err(Error::WorkDir {
                        path: root.to_path_buf(),
                        problem: WorkDirProblem::NotEmpty,
                    });
                }
            }
            Err(cause) => return Err(io_error("创建", dir)(cause)),
        }
    }
    Ok(())
}

/// 文件所在目录；任务目录内的文件都有上级目录。
fn parent(path: &Path) -> &Path {
    path.parent().expect("任务目录内的文件都有上级目录")
}

/// 先写 `<path>.part`（之前留下的先删掉，同名的符号链接只删链接本身，不写到它指向的地方）并落盘，再改名为 `path`。
fn write_atomic(path: &Path, data: &[u8]) -> Result<(), Error> {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    match busy::retrying(|| fs::remove_file(&part)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(io_error("删除", &part)(e)),
        _ => {}
    }
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(&part)
        .map_err(io_error("创建", &part))?;
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

/// 已发布版本的样本，在 tests/fixtures/workdir 下按 `<job|outputs>/<格式版本>/` 存放，发布后不再改动：job 的为 core 的端到端
/// 测试 `samples::write_samples` 运行生成的任务目录（`<名字>/work/`），outputs 的为手写的记录（见各自的测试）。
#[cfg(test)]
mod released {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use super::names::{SessionStart, parse_init_name, parse_segment_name};
    use super::ranges::RANGES_FILE;
    use super::{DATA_FILE, JOB_FILE, TRACKS_DIR};
    use crate::Saved;

    /// `kind`（`job` 或 `outputs`）下各格式版本与它的样本目录，按版本排列。
    pub(super) fn versions(kind: &str) -> Vec<(u32, PathBuf)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/workdir")
            .join(kind);
        let mut versions: Vec<(u32, PathBuf)> = visible_entries(&root)
            .into_iter()
            .map(|dir| {
                (
                    dir.file_name().unwrap().to_str().unwrap().parse().unwrap(),
                    dir,
                )
            })
            .collect();
        versions.sort();
        versions
    }

    /// 各版本的样本都能查看：HLS 样本的分片数为各轨里完整（不带 `.part`）的分片文件数，直播样本认作直播；文件任务的样本
    /// 有已落盘的字节与总长度。不存在的目录为 None。
    #[test]
    fn released_samples_can_be_inspected() {
        for (_, dir) in versions("job") {
            for sample in visible_entries(&dir) {
                let work = sample.join("work");
                let name = sample.file_name().unwrap().to_str().unwrap();
                if name.starts_with("file") {
                    let Some(Saved::File(saved)) = super::read_saved(&work).unwrap() else {
                        panic!("{} 应为文件任务", work.display());
                    };
                    let total = std::fs::metadata(work.join(DATA_FILE)).unwrap().len();
                    assert_eq!(saved.total, Some(total), "{}", work.display());
                    assert!(saved.bytes > 0 && saved.bytes < total, "{}", work.display());
                    continue;
                }
                let complete = visible_entries(&work.join(TRACKS_DIR))
                    .iter()
                    .flat_map(|track| visible_entries(track))
                    .filter(|file| {
                        let name = file.file_name().unwrap().to_str().unwrap();
                        parse_segment_name(name).is_some()
                    })
                    .count();
                let Some(Saved::Hls(saved)) = super::read_saved(&work).unwrap() else {
                    panic!("{} 应为 HLS 任务", work.display());
                };
                assert_eq!(saved.segments, complete, "{}", work.display());
                assert!(saved.segments > 0 && saved.bytes > 0, "{}", work.display());
                assert_eq!(saved.live, name.starts_with("live"), "{}", work.display());
            }
        }
        assert_eq!(super::read_saved(&dir_missing()).unwrap(), None);
    }

    fn dir_missing() -> PathBuf {
        std::env::temp_dir().join(format!("hs-m3u8-{}-no-such-work-dir", std::process::id()))
    }

    /// 目录 `dir` 里名字不以 `.` 开头的项（`.DS_Store` 之类不算）。
    pub(super) fn visible_entries(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| !path.file_name().unwrap().to_str().unwrap().starts_with('.'))
            .collect()
    }

    /// 任务目录样本里都是本库写的文件（写到一半的带 `.part`），当前版本的样本覆盖了文件名的各种写法与文件任务的内容。续
    /// 传与合并时认不出的文件被当作别人的而跳过，覆盖不全时，将来改了写法而漏了兼容旧写法测不出来；`SessionStart` 加了
    /// 取值时这里编译不过，要为它生成样本。
    #[test]
    fn sample_file_names_are_recognized_and_cover_every_form() {
        let versions = versions("job");
        let (_, current) = versions.last().unwrap();
        for (_, dir) in &versions {
            let mut forms = BTreeSet::new();
            for work in visible_entries(dir)
                .iter()
                .map(|sample| sample.join("work"))
            {
                for entry in visible_entries(&work) {
                    let name = entry.file_name().unwrap().to_str().unwrap();
                    let known = [JOB_FILE, TRACKS_DIR, DATA_FILE, RANGES_FILE]
                        .contains(&name.strip_suffix(".part").unwrap_or(name));
                    assert!(known, "{}", entry.display());
                    if name.ends_with(".part") {
                        forms.insert("写到一半的 job.json");
                    }
                    if name == RANGES_FILE {
                        forms.insert("文件任务的范围记录");
                    }
                }
                let tracks = work.join(TRACKS_DIR);
                let tracks = if tracks.exists() {
                    visible_entries(&tracks)
                } else {
                    Vec::new()
                };
                for track in tracks {
                    for file in visible_entries(&track) {
                        let name = file.file_name().unwrap().to_str().unwrap();
                        let (stem, partial) = match name.strip_suffix(".part") {
                            Some(stem) => (stem, true),
                            None => (name, false),
                        };
                        if partial {
                            forms.insert("写到一半的分片或 init 段");
                        }
                        if parse_init_name(stem).is_some() {
                            forms.insert("init 段");
                            continue;
                        }
                        let segment = parse_segment_name(stem)
                            .unwrap_or_else(|| panic!("认不出 {}", file.display()));
                        forms.insert(match segment.start {
                            SessionStart::Fresh => "起点 fresh",
                            SessionStart::After(_) => "起点 after",
                        });
                        forms.insert(if segment.init.is_some() {
                            "用 init 段"
                        } else {
                            "不用 init 段"
                        });
                        if segment.session > 0 {
                            forms.insert("会话大于 0");
                        }
                        if segment.discontinuity > 0 {
                            forms.insert("不连续段大于 0");
                        }
                    }
                }
            }
            if dir == current {
                let all = BTreeSet::from([
                    "写到一半的 job.json",
                    "写到一半的分片或 init 段",
                    "init 段",
                    "起点 fresh",
                    "起点 after",
                    "用 init 段",
                    "不用 init 段",
                    "会话大于 0",
                    "不连续段大于 0",
                    "文件任务的范围记录",
                ]);
                assert_eq!(forms, all, "{}", dir.display());
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::scratch::Scratch;

    /// `.part` 是指向别处的符号链接（如之前留下的，或别人放的）：只删链接本身，不写到它指向的文件。
    #[test]
    fn partial_files_never_write_through_symlinks() {
        let dir = Scratch::new("write-atomic-symlink");
        let victim = dir.join("victim.txt");
        fs::write(&victim, "user file").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("job.json.part")).unwrap();

        write_atomic(&dir.join("job.json"), b"{}").unwrap();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "user file");
        let written = fs::symlink_metadata(dir.join("job.json")).unwrap();
        assert!(written.is_file());
        assert_eq!(fs::read(dir.join("job.json")).unwrap(), b"{}");
    }

    /// 没有 job.json 的目录里，是符号链接的 `.part` 不算本库写的残留：报目录不为空，不当成任务目录。
    #[test]
    fn symlinked_partial_files_are_not_ours() {
        let dir = Scratch::new("ensure-empty-symlink");
        fs::write(dir.join("job.json.part"), "{").unwrap();
        ensure_empty(&dir).unwrap();
        fs::remove_file(dir.join("job.json.part")).unwrap();
        std::os::unix::fs::symlink(dir.join("gone"), dir.join("job.json.part")).unwrap();

        assert!(matches!(
            ensure_empty(&dir),
            Err(Error::WorkDir {
                problem: WorkDirProblem::NotEmpty,
                ..
            })
        ));
    }

    fn is_not_empty<T>(result: Result<T, Error>) -> bool {
        matches!(
            result,
            Err(Error::WorkDir {
                problem: WorkDirProblem::NotEmpty,
                ..
            })
        )
    }

    /// `tracks` 或某条轨的目录是指向别处的符号链接：打开时与写分片时都报目录里有不是本库写的文件，不写进链接指向的
    /// 目录；换成当前任务时也不进去删。
    #[test]
    fn track_dirs_never_follow_symlinks() {
        let dir = Scratch::new("track-dir-symlink");
        let elsewhere = dir.join("elsewhere");
        fs::create_dir_all(elsewhere.join("0")).unwrap();
        let victim = elsewhere.join("0/0-fresh-1-0-none-1000-0123456789abcdef.seg");
        assert!(parse_segment_name("0-fresh-1-0-none-1000-0123456789abcdef.seg").is_some());
        fs::write(&victim, "user file").unwrap();
        let root = dir.join("work");
        fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(TRACKS_DIR)).unwrap();

        assert!(is_not_empty(check_tracks(&root)));
        assert!(is_not_empty(create_track_dir(&root.join("tracks/0"))));
        clear_content(&root, None).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "user file");

        fs::remove_file(root.join(TRACKS_DIR)).unwrap();
        fs::create_dir(root.join(TRACKS_DIR)).unwrap();
        check_tracks(&root).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("0"), root.join("tracks/0")).unwrap();
        assert!(is_not_empty(check_tracks(&root)));
        assert!(is_not_empty(create_track_dir(&root.join("tracks/0"))));
        create_track_dir(&root.join("tracks/1")).unwrap();
        assert!(
            fs::symlink_metadata(root.join("tracks/1"))
                .unwrap()
                .is_dir()
        );
    }

    /// 锁文件是悬空的符号链接：报目录里有不是本库写的文件，不在它指向的地方建出文件。
    #[test]
    fn lock_files_never_follow_symlinks() {
        let dir = Scratch::new("lock-symlink");
        let target = dir.join("elsewhere");
        std::os::unix::fs::symlink(&target, dir.join(LOCK_FILE)).unwrap();

        assert!(is_not_empty(acquire(&dir)));
        assert!(!target.exists());
    }
}
