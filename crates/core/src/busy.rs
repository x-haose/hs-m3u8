//! Windows 上别的进程开着文件（杀毒软件扫描刚写完的文件、搜索索引、播放器）时，改名与删除会失败：访问被拒绝
//! （ERROR_ACCESS_DENIED，5）、共享冲突（ERROR_SHARING_VIOLATION，32）、锁冲突（ERROR_LOCK_VIOLATION，33）。其他平台上
//! 改名与删除不受别的进程开着文件影响。

use std::io;
use std::time::Duration;

/// 退避重试的等待，毫秒，依次用完即放弃：合计 750 毫秒，等得过短暂的扫描，一直开着文件（如播放器）时也不久拖。
const BACKOFF_MS: [u64; 4] = [50, 100, 200, 400];

/// 做 `op`（改名或删除）；Windows 上因别的进程开着文件而失败时，按 [`BACKOFF_MS`] 退避后再做。在阻塞线程中调用。
pub(crate) fn retrying<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    for delay in BACKOFF_MS {
        match op() {
            Err(e) if busy(&e) => std::thread::sleep(Duration::from_millis(delay)),
            result => return result,
        }
    }
    op()
}

/// 别的进程开着文件（Windows）。
fn busy(error: &io::Error) -> bool {
    cfg!(windows) && matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

/// 别的进程一直开着文件（Windows 的共享冲突、锁冲突）：关掉它之后再试可能成功。访问被拒绝不算：退避重试之后仍被
/// 拒绝的多为确实没有权限。
pub(crate) fn held_open(error: &io::Error) -> bool {
    cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33))
}

#[cfg(all(test, windows))]
mod tests {
    use std::fs;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::PathBuf;

    use super::*;

    /// 测试用的目录：测试通过后删除，失败时留着查看。要在用到其中文件的句柄之前声明，从而最后被释放。
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                fs::remove_dir_all(&self.0).unwrap();
            }
        }
    }

    /// 别的句柄不共享地一直开着文件：改名退避重试后仍失败，失败为共享冲突，可重试。
    #[test]
    fn renaming_a_file_held_open_fails_as_held_open() {
        let scratch =
            Scratch(std::env::temp_dir().join(format!("hs-m3u8-busy-{}", std::process::id())));
        let dir = &scratch.0;
        if dir.exists() {
            fs::remove_dir_all(dir).unwrap();
        }
        fs::create_dir_all(dir).unwrap();
        let from = dir.join("a");
        fs::write(&from, b"x").unwrap();
        let _held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&from)
            .unwrap();

        let err = retrying(|| fs::rename(&from, dir.join("b"))).unwrap_err();
        assert!(held_open(&err), "{err:?}");
    }
}
