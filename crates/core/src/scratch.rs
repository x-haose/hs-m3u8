//! 单元测试用的临时目录。

use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// 系统临时目录下的一个空目录，名字带进程号，几个工作区同时跑测试时互不干扰；测试通过后删除，失败时留着查看。要在
/// 用到其中文件的句柄之前声明，从而最后被释放：Windows 上开着的文件删不掉。
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    /// 名为 `name` 的空目录；同名的已存在时先删掉。
    pub(crate) fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("hs-m3u8-{}-{name}", std::process::id()));
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
}
