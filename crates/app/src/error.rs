//! 业务层的错误。

use crate::TaskId;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 调用方给的值不合要求（如没法存的选轨偏好、不是 UTF-8 的输出路径）
    #[error("参数不合要求：{0}")]
    InvalidInput(String),
    /// 任务存储读写失败（盘满、文件被删、权限）；照原样报出，不可重试与否看包着的原因
    #[error("任务存储出错：{0}")]
    Store(#[from] rusqlite::Error),
    /// 任务存储里的内容认不出：本应用写入的都能读回，读不回即文件被改动过或被别的程序写过
    #[error("任务存储里的内容认不出：{0}")]
    Corrupt(String),
    #[error("没有这个任务：{0:?}")]
    NotFound(TaskId),
    /// 任务正在运行，这个操作要等它停下
    #[error("任务正在运行：{0:?}")]
    Running(TaskId),
    /// 核心库报的错误
    #[error(transparent)]
    Core(#[from] hs_m3u8_core::Error),
    /// 打开任务存储时建目录、设权限失败
    #[error("{action} {path} 失败：{cause}")]
    Io {
        action: &'static str,
        path: std::path::PathBuf,
        cause: std::io::Error,
    },
}
