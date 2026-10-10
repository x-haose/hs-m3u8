//! 目录项：列出目录，认出系统自动生成的元数据文件与规范写法的编号；任务目录与本地 HLS 输出共用。

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::Error;
use crate::error::io_error;

/// 目录中的一项；类型不跟随符号链接。
pub(crate) struct Entry {
    pub path: PathBuf,
    pub name: OsString,
    pub kind: fs::FileType,
    /// 见 [`Entry::apple_double_owner`]
    apple_double_owner: Option<PathBuf>,
}

impl Entry {
    /// 系统自动生成的元数据文件：访达的 `.DS_Store`、Windows 资源管理器的 `Thumbs.db` 与 `desktop.ini`，以及
    /// AppleDouble 文件（见 [`Entry::apple_double_owner`]）。不含用户的内容，判断目录是否为空、是否全是本库写的
    /// 文件时不算。
    pub(crate) fn is_system_file(&self) -> bool {
        const SYSTEM_FILES: [&str; 3] = [".DS_Store", "Thumbs.db", "desktop.ini"];
        self.apple_double_owner().is_some()
            || self.kind.is_file()
                && self
                    .name
                    .to_str()
                    .is_some_and(|n| SYSTEM_FILES.contains(&n))
    }

    /// AppleDouble 文件 `._<名字>` 所属的同目录的 `<名字>`；不是 AppleDouble 文件时为 None。macOS 在不能存扩展
    /// 属性的文件系统（exFAT、FAT32、部分网络共享）上为每个带扩展属性的文件与目录生成一个，删除 `<名字>` 时一并删除；
    /// 在别的系统上读这种卷时要自己删。名字之外还核对内容的开头，用户自己起名为 `._<名字>` 的文件不算。
    pub(crate) fn apple_double_owner(&self) -> Option<&Path> {
        self.apple_double_owner.as_deref()
    }
}

/// `path`（名为 `name`、类型为 `kind`）是 AppleDouble 文件时，它所属的同目录的文件或目录。AppleDouble 格式
/// （RFC 1740）以 `00 05 16 07` 开头。
fn apple_double_owner(
    path: &Path,
    name: &OsString,
    kind: fs::FileType,
) -> Result<Option<PathBuf>, Error> {
    const MAGIC: [u8; 4] = [0x00, 0x05, 0x16, 0x07];
    let owner = name
        .to_str()
        .and_then(|n| n.strip_prefix("._"))
        .filter(|o| !o.is_empty());
    let Some(owner) = owner.filter(|_| kind.is_file()) else {
        return Ok(None);
    };
    let mut head = [0u8; 4];
    let read = File::open(path).and_then(|mut file| file.read_exact(&mut head));
    match read {
        Ok(()) => Ok((head == MAGIC).then(|| path.with_file_name(owner))),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(cause) => Err(io_error("读取", path)(cause)),
    }
}

/// `dir` 中的各项。
pub(crate) fn list(dir: &Path) -> Result<Vec<Entry>, Error> {
    let mut all = Vec::new();
    for entry in fs::read_dir(dir).map_err(io_error("读取", dir))? {
        let entry = entry.map_err(io_error("读取", dir))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(io_error("读取", &path))?;
        let name = entry.file_name();
        all.push(Entry {
            apple_double_owner: apple_double_owner(&path, &name, kind)?,
            path,
            name,
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
