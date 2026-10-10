//! 目录项：列出目录，认出系统自动生成的元数据文件与规范写法的编号；任务目录与本地 HLS 输出共用。

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::error::io_error;

/// 目录中的一项；类型不跟随符号链接。
pub(crate) struct Entry {
    pub path: PathBuf,
    pub name: OsString,
    pub kind: fs::FileType,
}

impl Entry {
    /// 系统自动生成的元数据文件（访达的 `.DS_Store`、Windows 资源管理器的 `Thumbs.db` 与 `desktop.ini`）：不含用户
    /// 的内容，判断目录是否为空、是否全是本库写的文件时不算，删除目录时一起删。
    pub(crate) fn is_system_file(&self) -> bool {
        const SYSTEM_FILES: [&str; 3] = [".DS_Store", "Thumbs.db", "desktop.ini"];
        self.kind.is_file()
            && self
                .name
                .to_str()
                .is_some_and(|n| SYSTEM_FILES.contains(&n))
    }
}

/// `dir` 中的各项。
pub(crate) fn list(dir: &Path) -> Result<Vec<Entry>, Error> {
    let mut all = Vec::new();
    for entry in fs::read_dir(dir).map_err(io_error("读取", dir))? {
        let entry = entry.map_err(io_error("读取", dir))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(io_error("读取", &path))?;
        all.push(Entry {
            path,
            name: entry.file_name(),
            kind,
        });
    }
    Ok(all)
}

/// 目录里除了系统自动生成的元数据文件之外没有别的。
pub(crate) fn is_empty(dir: &Path) -> Result<bool, Error> {
    Ok(list(dir)?.iter().all(Entry::is_system_file))
}

/// 规范写法的十进制非负整数：没有多余的前导零。文件名与目录名里的编号只认这种写法。
pub(crate) fn is_canonical_number(text: &str) -> bool {
    !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_canonical_numbers_are_accepted() {
        assert!(is_canonical_number("0") && is_canonical_number("10"));
        assert!(
            !is_canonical_number("") && !is_canonical_number("00") && !is_canonical_number("-1")
        );
    }
}
