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

use std::fs::{self, File, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use hs_m3u8_hls::Resolution;
use hs_m3u8_remux::Streams;
use serde::{Deserialize, Serialize};

use crate::ident::Fingerprint;
use crate::selection::{RenditionKey, SelectionKey, VariantKey};
use crate::{Error, JobType, WorkDirProblem, blocking};

const FORMAT_VERSION: u32 = 2;
const JOB_FILE: &str = "job.json";
const LOCK_FILE: &str = "lock";
const TRACKS_DIR: &str = "tracks";

/// 任务目录记录的任务；续传、续录时须与当前请求相符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobRecord {
    /// 来源摘要，见 [`crate::ident::source_digest`]
    pub source: String,
    /// 所选的变体与音频；来源本身是媒体播放列表时为 None
    pub selection: Option<SelectionKey>,
    /// 各轨的取流方式，至少一条
    pub streams: Vec<Streams>,
    pub kind: RecordKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// 点播：计划摘要相同才能续传
    Vod { plan: String },
    /// 直播：`url` 为完整来源地址（含查询串）的摘要。地址变了仍可沿用目录，由录制确认窗口与已录内容衔接后
    /// 再用 [`WorkDir::save`] 更新
    Live { url: String },
}

/// 记录与当前请求不符之处。
enum Conflict {
    Source,
    Kind { recorded: JobType, current: JobType },
    Tracks,
    Plan,
}

impl JobRecord {
    fn job_type(&self) -> JobType {
        match self.kind {
            RecordKind::Vod { .. } => JobType::Vod,
            RecordKind::Live { .. } => JobType::Live,
        }
    }

    /// 记录为 `self` 的目录能否用于当前请求 `current`；直播的完整地址不比较。
    fn conflict(&self, current: &JobRecord) -> Option<Conflict> {
        if self.source != current.source {
            return Some(Conflict::Source);
        }
        let (recorded, current_type) = (self.job_type(), current.job_type());
        if recorded != current_type {
            return Some(Conflict::Kind {
                recorded,
                current: current_type,
            });
        }
        if self.selection != current.selection || self.streams != current.streams {
            return Some(Conflict::Tracks);
        }
        match (&self.kind, &current.kind) {
            (RecordKind::Vod { plan: a }, RecordKind::Vod { plan: b }) if a != b => {
                Some(Conflict::Plan)
            }
            _ => None,
        }
    }
}

impl Conflict {
    fn into_error(self, root: &Path) -> Error {
        let problem = match self {
            Conflict::Plan => return Error::PlanChanged,
            Conflict::Source => WorkDirProblem::SourceMismatch,
            Conflict::Kind { recorded, current } => {
                WorkDirProblem::KindMismatch { recorded, current }
            }
            Conflict::Tracks => WorkDirProblem::TracksMismatch,
        };
        Error::WorkDir {
            path: root.to_path_buf(),
            problem,
        }
    }
}

/// `job.json` 的格式（序列化契约）。`format_version` 不等于 [`FORMAT_VERSION`] 时拒绝。
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum JobFile {
    Vod {
        format_version: u32,
        source: String,
        selection: Option<SelectionFile>,
        streams: Vec<StreamsName>,
        plan: String,
    },
    Live {
        format_version: u32,
        source: String,
        selection: Option<SelectionFile>,
        streams: Vec<StreamsName>,
        url: String,
    },
}

/// [`SelectionKey`] 在 job.json 中的写法。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionFile {
    variant: VariantFile,
    audio: Option<RenditionFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VariantFile {
    bandwidth: Option<u64>,
    /// [宽, 高]
    resolution: Option<[u32; 2]>,
    codecs: Vec<String>,
    audio_group: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenditionFile {
    group_id: String,
    language: Option<String>,
    name: Option<String>,
}

/// [`Streams`] 在 job.json 中的写法。
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StreamsName {
    All,
    Video,
    Audio,
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
    /// 会话内的不连续段编号
    pub discontinuity: u64,
    /// 所用 init 段的内容指纹
    pub init: Option<Fingerprint>,
    /// EXTINF 声明的时长，微秒
    pub duration_us: u64,
    /// 见 [`Fingerprint::of_segment`]
    pub id: Fingerprint,
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
    /// [`WorkDirProblem::TracksMismatch`]，点播的计划不同报 [`Error::PlanChanged`]；但目录里还没有已完成的
    /// 分片与 init 段时，直接改为当前任务（没有可丢的内容）。没有 `job.json` 时目录必须不存在或为空
    /// （`.part` 残留与锁文件除外），以免把别人的目录当成任务目录（成功后会整个删除）。
    pub(crate) async fn open(root: PathBuf, record: JobRecord) -> Result<WorkDir, Error> {
        blocking(move || open(root, record)).await?
    }

    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    /// 打开前目录里已有、与当前请求相符的记录；新建或改为当前任务的目录为 None。
    pub(crate) fn previous(&self) -> Option<&JobRecord> {
        self.previous.as_ref()
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

    /// 删除整个任务目录。持锁删掉锁文件以外的内容，再释放锁、删锁文件与目录：释放锁之后别的任务
    /// 就能打开这个目录，先删内容保证它看到的不会是删到一半的任务。
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
            // Windows 上打开着的文件无法删除，先释放
            drop(_lock);
            let lock = root.join(LOCK_FILE);
            match fs::remove_file(&lock) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(io_error("删除", &lock)(e));
                }
                _ => {}
            }
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
        for entry in fs::read_dir(&root).map_err(io_error("读取", &root))? {
            let name = entry.map_err(io_error("读取", &root))?.file_name();
            let name = name.to_string_lossy();
            if name != LOCK_FILE && !name.ends_with(".part") {
                return Err(Error::WorkDir {
                    path: root,
                    problem: WorkDirProblem::NotEmpty,
                });
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
        Err(TryLockError::WouldBlock) => {
            return Err(Error::WorkDir {
                path: root,
                problem: WorkDirProblem::Locked,
            });
        }
        Err(TryLockError::Error(cause)) => return Err(io_error("锁定", &lock_path)(cause)),
    }

    let layout = Layout { root: root.clone() };
    let previous = match read_job(&root)? {
        None => None,
        Some(recorded) => match recorded.conflict(&record) {
            None => Some(recorded),
            Some(_) if !has_completed_files(&layout)? => None,
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

/// 目录里有没有已完成的分片或 init 段。
fn has_completed_files(layout: &Layout) -> Result<bool, Error> {
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
        if !list(&dir, parse_segment_name)?.is_empty() || !list(&dir, parse_init_name)?.is_empty() {
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

fn encode(record: &JobRecord) -> Vec<u8> {
    let format_version = FORMAT_VERSION;
    let source = record.source.clone();
    let selection = record.selection.as_ref().map(SelectionFile::from);
    let streams = record.streams.iter().map(|&s| s.into()).collect();
    let file = match &record.kind {
        RecordKind::Vod { plan } => JobFile::Vod {
            format_version,
            source,
            selection,
            streams,
            plan: plan.clone(),
        },
        RecordKind::Live { url } => JobFile::Live {
            format_version,
            source,
            selection,
            streams,
            url: url.clone(),
        },
    };
    serde_json::to_vec(&file).expect("JobFile 只含字符串、整数、数组与枚举，序列化不会失败")
}

/// 解析 `job.json`；失败时返回原因。
fn decode(bytes: &[u8]) -> Result<JobRecord, String> {
    let unreadable = |e: serde_json::Error| format!("job.json 无法解析：{e}");
    let version: Version = serde_json::from_slice(bytes).map_err(unreadable)?;
    if version.format_version != FORMAT_VERSION {
        return Err(format!(
            "job.json 的格式版本 {} 不受支持（支持 {FORMAT_VERSION}）",
            version.format_version
        ));
    }
    let (source, selection, streams, kind) =
        match serde_json::from_slice(bytes).map_err(unreadable)? {
            JobFile::Vod {
                source,
                selection,
                streams,
                plan,
                ..
            } => (source, selection, streams, RecordKind::Vod { plan }),
            JobFile::Live {
                source,
                selection,
                streams,
                url,
                ..
            } => (source, selection, streams, RecordKind::Live { url }),
        };
    if streams.is_empty() {
        return Err("job.json 中的 streams 为空".into());
    }
    Ok(JobRecord {
        source,
        selection: selection.map(SelectionKey::from),
        streams: streams.into_iter().map(Streams::from).collect(),
        kind,
    })
}

impl From<&SelectionKey> for SelectionFile {
    fn from(key: &SelectionKey) -> Self {
        let v = &key.variant;
        SelectionFile {
            variant: VariantFile {
                bandwidth: v.bandwidth,
                resolution: v.resolution.map(|r| [r.width, r.height]),
                codecs: v.codecs.clone(),
                audio_group: v.audio_group.clone(),
            },
            audio: key.audio.as_ref().map(|a| RenditionFile {
                group_id: a.group_id.clone(),
                language: a.language.clone(),
                name: a.name.clone(),
            }),
        }
    }
}

impl From<SelectionFile> for SelectionKey {
    fn from(file: SelectionFile) -> Self {
        let v = file.variant;
        SelectionKey {
            variant: VariantKey {
                bandwidth: v.bandwidth,
                resolution: v
                    .resolution
                    .map(|[width, height]| Resolution { width, height }),
                codecs: v.codecs,
                audio_group: v.audio_group,
            },
            audio: file.audio.map(|a| RenditionKey {
                group_id: a.group_id,
                language: a.language,
                name: a.name,
            }),
        }
    }
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

/// `<会话>-<序号>-<不连续段>-<init 指纹|none>-<时长>-<身份>.seg`。
fn segment_file_name(name: &SegmentName) -> String {
    let init = name
        .init
        .map_or_else(|| "none".to_owned(), |f| f.to_string());
    format!(
        "{}-{}-{}-{init}-{}-{}.seg",
        name.session, name.sequence, name.discontinuity, name.duration_us, name.id
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
            text => Some(Fingerprint::parse(text)?),
        },
        duration_us: parts.next()?.parse().ok()?,
        id: Fingerprint::parse(parts.next()?)?,
    };
    (parts.next().is_none() && segment_file_name(&name) == file_name).then_some(name)
}

/// `init-<指纹>.mp4`。
fn init_file_name(fingerprint: Fingerprint) -> String {
    format!("init-{fingerprint}.mp4")
}

/// [`init_file_name`] 的逆。
fn parse_init_name(file_name: &str) -> Option<Fingerprint> {
    Fingerprint::parse(file_name.strip_prefix("init-")?.strip_suffix(".mp4")?)
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

/// 存入第 `track` 条轨的 init 段，返回其内容指纹，以及这次是否新写了文件（同内容的已存在时不重写）。
pub(crate) async fn store_init(
    layout: &Layout,
    track: usize,
    data: Vec<u8>,
) -> Result<(Fingerprint, bool), Error> {
    let fingerprint = Fingerprint::of_content(&data);
    let path = layout.init(track, fingerprint);
    let created = blocking(move || {
        if completed_len(&path)?.is_some() {
            return Ok(false);
        }
        let dir = parent(&path);
        fs::create_dir_all(dir).map_err(io_error("创建", dir))?;
        write_atomic(&path, &data).map(|()| true)
    })
    .await??;
    Ok((fingerprint, created))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(text: &str) -> Fingerprint {
        Fingerprint::parse(text).unwrap()
    }

    #[test]
    fn job_file_round_trips_and_rejects_other_versions_and_fields() {
        let live = JobRecord {
            source: "ab".into(),
            selection: Some(SelectionKey {
                variant: VariantKey {
                    bandwidth: Some(2000),
                    resolution: Some(Resolution {
                        width: 1280,
                        height: 720,
                    }),
                    codecs: vec!["avc1.640020".into()],
                    audio_group: Some("aud".into()),
                },
                audio: Some(RenditionKey {
                    group_id: "aud".into(),
                    language: Some("en".into()),
                    name: None,
                }),
            }),
            streams: vec![Streams::Video, Streams::Audio],
            kind: RecordKind::Live { url: "cd".into() },
        };
        let bytes = encode(&live);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"kind":"live","format_version":2,"source":"ab","selection":{"variant":{"bandwidth":2000,"resolution":[1280,720],"codecs":["avc1.640020"],"audio_group":"aud"},"audio":{"group_id":"aud","language":"en","name":null}},"streams":["video","audio"],"url":"cd"}"#
        );
        assert_eq!(decode(&bytes), Ok(live));
        let vod = JobRecord {
            source: "ab".into(),
            selection: None,
            streams: vec![Streams::All],
            kind: RecordKind::Vod { plan: "ef".into() },
        };
        assert_eq!(decode(&encode(&vod)), Ok(vod));

        for rejected in [
            r#"{"kind":"vod","format_version":1,"plan_digest":"cd"}"#,
            r#"{"kind":"vod","format_version":2,"source":"a","selection":null,"streams":["all"],"plan":"p","extra":1}"#,
            r#"{"format_version":2,"source":"a","selection":null,"streams":["all"],"plan":"p"}"#,
            r#"{"kind":"vod","format_version":2,"source":"a","selection":null,"streams":[],"plan":"p"}"#,
            r#"{"kind":"vod","format_version":2,"source":"a","selection":null,"streams":["both"],"plan":"p"}"#,
            r#"{"kind":"live","format_version":2,"source":"a","selection":{"variant":{"bandwidth":1,"resolution":null,"codecs":[],"audio_group":null,"uri":"x"},"audio":null},"streams":["all"],"url":"u"}"#,
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
            init: Some(fingerprint("00000000000000ff")),
            duration_us: 6_006_000,
            id: fingerprint("0123456789abcdef"),
        };
        assert_eq!(parse_segment_name(&segment_file_name(&name)), Some(name));
        let plain = SegmentName { init: None, ..name };
        assert_eq!(parse_segment_name(&segment_file_name(&plain)), Some(plain));

        let id = "0123456789abcdef";
        for other in [
            format!("0-1-0-none-1000-{id}.seg.part"),
            format!("0-01-0-none-1000-{id}.seg"),
            format!("0-+1-0-none-1000-{id}.seg"),
            "0-1-0-none-1000.seg".to_owned(),
            format!("0-1-0-none-1000-{id}-9.seg"),
            format!("0-1-0-3-1000-{id}.seg"),
            "init-0123456789abcdef.mp4".to_owned(),
        ] {
            assert_eq!(parse_segment_name(&other), None, "{other}");
        }
        assert_eq!(
            parse_init_name("init-0123456789abcdef.mp4"),
            Some(fingerprint(id))
        );
        for other in ["init-3.mp4", "init-0123456789abcdef.mp4.part"] {
            assert_eq!(parse_init_name(other), None, "{other}");
        }
    }
}
