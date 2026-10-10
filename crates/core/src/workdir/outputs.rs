//! `outputs.json`：正在写的输出。写之前记下各输出的临时名与旧输出挪开后的名字，下次运行据此清掉中断留下的
//! 临时输出、放回挪开了的旧输出；写完（成功或失败后清理完）即删除。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::write_atomic;
use crate::error::io_error;
use crate::{Error, WorkDirProblem};

pub(super) const OUTPUTS_FILE: &str = "outputs.json";

/// 正在写的一个输出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingOutput {
    pub target: PathBuf,
    /// 写到这里，备齐后改名为 `target`
    pub temp: PathBuf,
    /// 要求覆盖时，`target` 上的旧输出先挪到这里，换上新的之后再删
    pub aside: PathBuf,
    /// 是目录（本地 HLS）；否则是文件（MP4）
    pub dir: bool,
}

/// 读取记录；没有时为空。
pub(crate) fn read_outputs(root: &Path) -> Result<Vec<PendingOutput>, Error> {
    let path = root.join(OUTPUTS_FILE);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| Error::WorkDir {
            path: root.to_path_buf(),
            problem: WorkDirProblem::Corrupt(format!("{OUTPUTS_FILE} 无法识别：{e}")),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(cause) => Err(io_error("读取", &path)(cause)),
    }
}

/// 写下记录。路径须为 UTF-8（[`crate::OutputOptions`] 的校验保证）。
pub(crate) fn record_outputs(root: &Path, outputs: &[PendingOutput]) -> Result<(), Error> {
    let bytes = serde_json::to_vec(outputs).expect("输出路径校验过是 UTF-8");
    write_atomic(&outputs_file(root), &bytes)
}

/// 删除记录；没有时不算失败。
pub(crate) fn clear_outputs(root: &Path) -> io::Result<()> {
    match fs::remove_file(outputs_file(root)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub(crate) fn outputs_file(root: &Path) -> PathBuf {
    root.join(OUTPUTS_FILE)
}
