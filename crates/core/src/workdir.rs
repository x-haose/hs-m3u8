//! 任务目录：计划摘要、独占锁、已完成的分片与 init 段。
//!
//! ```text
//! job.json                     {"format_version": 1, "plan_digest": "<SHA-256 十六进制>"}
//! lock                         运行期间持有排他锁
//! tracks/<轨道>/<序号>.seg      已解密、通过校验的分片
//! tracks/<轨道>/init-<编号>.mp4 init 段，编号为该段在本轨 init 列表中的下标
//! ```
//!
//! 文件先写 `<名字>.part`，fsync 后改名，因此最终文件名存在即内容完整（断电也成立）。
//! 请求配置（请求头、Cookie 等）不写入目录：续传时由调用方再次提供。

use std::fs::{self, File, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, blocking};

const FORMAT_VERSION: u32 = 1;
const JOB_FILE: &str = "job.json";
const LOCK_FILE: &str = "lock";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobFile {
    format_version: u32,
    plan_digest: String,
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

impl WorkDir {
    /// 打开或新建任务目录并加锁。
    ///
    /// 已有 `job.json` 时其计划摘要必须等于 `digest`，否则 [`Error::PlanChanged`]；
    /// 没有时目录必须不存在或为空，以免把别人的目录当成任务目录（成功后会整个删除）。
    pub(crate) async fn open(root: PathBuf, digest: String) -> Result<(WorkDir, DirLock), Error> {
        blocking(move || open(root, &digest)).await?
    }

    pub(crate) fn segment(&self, track: usize, sequence: u64) -> PathBuf {
        self.track(track).join(format!("{sequence}.seg"))
    }

    pub(crate) fn init(&self, track: usize, index: usize) -> PathBuf {
        self.track(track).join(format!("init-{index}.mp4"))
    }

    fn track(&self, track: usize) -> PathBuf {
        self.root.join("tracks").join(track.to_string())
    }
}

fn open(root: PathBuf, digest: &str) -> Result<(WorkDir, DirLock), Error> {
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
        Ok(bytes) => {
            let unreadable = |e: serde_json::Error| invalid(format!("job.json 无法解析：{e}"));
            let version: Version = serde_json::from_slice(&bytes).map_err(unreadable)?;
            if version.format_version != FORMAT_VERSION {
                return Err(invalid(format!(
                    "job.json 的格式版本 {} 不受支持（支持 {FORMAT_VERSION}）",
                    version.format_version
                )));
            }
            let job: JobFile = serde_json::from_slice(&bytes).map_err(unreadable)?;
            if job.plan_digest != digest {
                return Err(Error::PlanChanged);
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let job = JobFile {
                format_version: FORMAT_VERSION,
                plan_digest: digest.to_owned(),
            };
            let bytes = serde_json::to_vec(&job).expect("JobFile 只含字符串与整数，序列化不会失败");
            write_atomic(&job_path, &bytes)?;
        }
        Err(source) => return Err(io_error("读取", &job_path)(source)),
    }
    Ok((WorkDir { root }, DirLock { _file: lock }))
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
