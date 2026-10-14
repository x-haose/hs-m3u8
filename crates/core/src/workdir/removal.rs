//! 删除任务目录：只删本库写的文件、系统自动生成的元数据文件与因此变空的目录（见 [`super::WorkDir::remove`]）。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use super::names::{parse_init_name, parse_segment_name};
use super::outputs::OUTPUTS_FILE;
use super::{JOB_FILE, LOCK_FILE, Lock, TRACKS_DIR};
use crate::entries::{self, is_canonical_number};
use crate::error::io_error;
use crate::{Error, Leftover, LeftoverKind, blocking, busy};

/// 持着 `lock` 删除任务目录 `root`，同 [`super::WorkDir::remove`]；用于不打开任务、直接放弃它。分两步：先删各轨的分片与
/// init 段，`cancel` 这时已取消即停下、放开锁，报 [`Error::Cancelled`]（记录还在，再放弃即可接着删）；再删记录、锁文件
/// 与目录本身。
pub(crate) async fn remove(
    root: PathBuf,
    lock: Lock,
    cancel: &CancellationToken,
) -> Result<Option<Leftover>, Error> {
    remove_steps(root, lock, Some(cancel)).await
}

/// 见 [`remove`]；`cancel` 为 None 时不停下。没删干净时为留下的原因。
pub(super) async fn remove_steps(
    root: PathBuf,
    lock: Lock,
    cancel: Option<&CancellationToken>,
) -> Result<Option<Leftover>, Error> {
    let tracks = blocking({
        let root = root.clone();
        move || (remove_tracks_of(&root), lock)
    })
    .await;
    let removed = match tracks {
        Ok((Ok(kept), lock)) => {
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return Err(Error::Cancelled);
            }
            let root = root.clone();
            blocking(move || remove_locked(&root, lock, kept)).await
        }
        Ok((Err(failure), _)) => Err(failure),
        Err(failure) => Err(failure),
    };
    let cause = match removed.and_then(|r| r) {
        Ok(kept) => match kept.first() {
            Some(first) => format!("留有不是本库写的文件：{}", first.display()),
            None => return Ok(None),
        },
        Err(e) => e.to_string(),
    };
    Ok(Some(Leftover {
        path: root,
        kind: LeftoverKind::WorkDir,
        cause,
    }))
}

/// 删掉任务目录 `root` 下各轨的分片与 init 段（见 [`remove_tracks`]），返回认不得、留下的文件。
fn remove_tracks_of(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let tracks = root.join(TRACKS_DIR);
    let mut kept = Vec::new();
    match fs::symlink_metadata(&tracks) {
        Ok(meta) if meta.is_dir() => remove_tracks(&tracks, &mut kept)?,
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_error("读取", &tracks)(e)),
    }
    Ok(kept)
}

/// [`super::WorkDir::remove`] 的第二步：持着 `lock` 删除各轨之外的内容，`kept` 为第一步留下的；返回不是本库写的、留下的
/// 文件。
fn remove_locked(root: &Path, lock: Lock, mut kept: Vec<PathBuf>) -> Result<Vec<PathBuf>, Error> {
    let mut system = Vec::new();
    let mut job = None;
    for entry in entries::list(root)? {
        let (path, kind) = (&entry.path, entry.kind);
        // job.json 写到一半的、正在写的输出的记录（含写到一半的）
        let ours = kind.is_file()
            && entry.name.to_str().is_some_and(|n| {
                n.strip_suffix(".part") == Some(JOB_FILE)
                    || n.strip_suffix(".part").unwrap_or(n) == OUTPUTS_FILE
            });
        match entry.name.to_str() {
            Some(LOCK_FILE) => {}
            Some(JOB_FILE) if kind.is_file() => job = Some(entry.path),
            // 第一步已删过，还在即里面有留下的（已在 kept 中）
            Some(TRACKS_DIR) if kind.is_dir() => {}
            _ if entry.is_system_file() => system.push(entry),
            _ if ours => {
                busy::retrying(|| fs::remove_file(path)).map_err(io_error("删除", path))?
            }
            _ => kept.push(entry.path),
        }
    }
    let mut remaining = kept.clone();
    match job {
        Some(job) if kept.is_empty() => {
            busy::retrying(|| fs::remove_file(&job)).map_err(io_error("删除", &job))?;
        }
        Some(job) => remaining.push(job),
        None => {}
    }
    let lock_path = root.join(LOCK_FILE);
    fs::remove_file(&lock_path).map_err(io_error("删除", &lock_path))?;
    for entry in &system {
        remove_system_file(entry, &remaining)?;
    }
    drop(lock);
    if !kept.is_empty() {
        return Ok(kept);
    }
    match fs::remove_dir(root) {
        Ok(()) => Ok(kept),
        // 释放锁后另一个任务已开始使用这个目录：本任务的内容已删完，目录留给它
        Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(kept),
        Err(e) => Err(io_error("删除", root)(e)),
    }
}

/// 删掉 `tracks/` 下各轨的分片与 init 段（含写到一半的 `.part`）、系统自动生成的元数据文件与变空的目录；认不得的
/// 记进 `kept`。
pub(super) fn remove_tracks(tracks: &Path, kept: &mut Vec<PathBuf>) -> Result<(), Error> {
    let before = kept.len();
    let mut system = Vec::new();
    for track in entries::list(tracks)? {
        let dir = &track.path;
        if track.is_system_file() {
            system.push(track);
            continue;
        }
        if !(track.kind.is_dir() && track.name.to_str().is_some_and(is_canonical_number)) {
            kept.push(track.path);
            continue;
        }
        let track_before = kept.len();
        let mut track_system = Vec::new();
        for entry in entries::list(dir)? {
            let ours = entry.kind.is_file()
                && entry.name.to_str().is_some_and(|n| {
                    let n = n.strip_suffix(".part").unwrap_or(n);
                    parse_segment_name(n).is_some() || parse_init_name(n).is_some()
                });
            if entry.is_system_file() {
                track_system.push(entry);
            } else if ours {
                busy::retrying(|| fs::remove_file(&entry.path))
                    .map_err(io_error("删除", &entry.path))?;
            } else {
                kept.push(entry.path);
            }
        }
        for entry in &track_system {
            remove_system_file(entry, &kept[track_before..])?;
        }
        if kept.len() == track_before {
            fs::remove_dir(dir).map_err(io_error("删除", dir))?;
        }
    }
    for entry in &system {
        remove_system_file(entry, &kept[before..])?;
    }
    if kept.len() == before {
        fs::remove_dir(tracks).map_err(io_error("删除", tracks))?;
    }
    Ok(())
}

/// 删掉系统自动生成的元数据文件 `entry`（见 [`entries::Entry::is_system_file`]）。AppleDouble 文件随它的主文件：
/// 主文件或主目录里的东西在 `remaining` 中（留下）时它也留下；macOS 删除主文件时已一并删掉它，已不存在不算失败。
fn remove_system_file(entry: &entries::Entry, remaining: &[PathBuf]) -> Result<(), Error> {
    if let Some(owner) = entry.apple_double_owner()
        && remaining.iter().any(|path| path.starts_with(owner))
    {
        return Ok(());
    }
    match fs::remove_file(&entry.path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other.map_err(io_error("删除", &entry.path)),
    }
}
