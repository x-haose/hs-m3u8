//! 任务目录的排他锁，与本进程持有的锁文件的登记（fork 出的子进程据此关掉继承来的锁）。

use std::collections::BTreeMap;
use std::fs::{File, TryLockError};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LockResult, Mutex, MutexGuard};

use super::LOCK_FILE;
use crate::error::io_error;
use crate::{Error, WorkDirProblem};

/// 任务目录的排他锁：同一任务目录同一时间只有一个任务。加了锁的锁文件登记在 [`HELD`]，drop 时取出关闭，即释放。
pub(crate) struct Lock {
    /// 在 [`HELD`] 里的编号
    id: u64,
}

impl Drop for Lock {
    fn drop(&mut self) {
        held().0.remove(&self.id);
    }
}

/// 本进程持有的任务目录锁文件：[`prepare_fork`] 锁住的登记表的内容。
pub struct LockFiles(BTreeMap<u64, File>);

/// 登记表。锁跟着打开的文件走，fork 出的子进程也就继承了它们，见 [`prepare_fork`]。从打开锁文件到登记、从取出到
/// 关闭都拿着它，所以没有已打开而不在表里的锁文件；这些地方持有期间不等别的锁（[`ForkGuard`] 除外）。
static HELD: Mutex<LockFiles> = Mutex::new(LockFiles(BTreeMap::new()));

fn held() -> MutexGuard<'static, LockFiles> {
    HELD.lock().expect("持锁期间不会 panic")
}

/// fork 之前调用：用 `lock` 锁住登记表；fork 之后在子进程里调用返回值的 [`ForkGuard::child`]，在父进程里丢弃它。
///
/// fork 出的子进程继承父进程打开的文件，也就继承了任务目录的锁：父进程的任务结束、关掉自己那份之后锁仍在，直到
/// 子进程退出，期间续传或放弃这个任务都报 [`WorkDirProblem::Locked`]。子进程关掉继承来的锁文件即可，父进程的锁
/// 不受影响。返回的 [`ForkGuard`] 拿着登记表的锁，fork 期间别的线程改不了登记。
///
/// `lock` 为怎么等登记表的锁，通常即 [`Mutex::lock`]。拿着它去等别的锁的，只有另一个正在 fork 的线程（它拿着
/// [`ForkGuard`] 执行 fork 前后的其他处理）：调用方持有那些处理也要的锁时，等待期间要放开。
pub fn prepare_fork(
    lock: impl FnOnce(&'static Mutex<LockFiles>) -> LockResult<MutexGuard<'static, LockFiles>>,
) -> ForkGuard {
    ForkGuard(lock(&HELD).expect("持锁期间不会 panic"))
}

/// fork 出的子进程里、fork 之前没能调用 [`prepare_fork`] 时用（如它在 fork 开始之后才登记为 fork 前的处理）：登记表
/// 此刻没被拿着才关掉继承来的锁文件。被拿着时，拿着它的是父进程里的线程，不在子进程里，这把锁再也放不开：继承来的
/// 锁文件留到子进程退出，子进程里再加锁的任务会一直等着。
pub fn close_inherited_locks() {
    if let Ok(mut held) = HELD.try_lock() {
        held.0.clear();
    }
}

/// 见 [`prepare_fork`]。
pub struct ForkGuard(MutexGuard<'static, LockFiles>);

impl ForkGuard {
    /// fork 出的子进程里调用：关掉从父进程继承来的锁文件。
    pub fn child(mut self) {
        self.0.0.clear();
    }
}

/// 打开锁文件并加排他锁；已被别的任务锁住时报 [`WorkDirProblem::Locked`]。
pub(super) fn acquire(root: &Path) -> Result<Lock, Error> {
    let path = root.join(LOCK_FILE);
    // 打开之前拿住登记表，加锁失败时关掉文件之后才放开（`file` 先于它 drop）
    let mut held = held();
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_error("创建", &path))?;
    match file.try_lock() {
        Ok(()) => {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            held.0.insert(id, file);
            Ok(Lock { id })
        }
        Err(TryLockError::WouldBlock) => Err(Error::WorkDir {
            path: root.to_path_buf(),
            problem: WorkDirProblem::Locked,
        }),
        Err(TryLockError::Error(cause)) => Err(io_error("锁定", &path)(cause)),
    }
}
