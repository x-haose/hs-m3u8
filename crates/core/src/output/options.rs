//! 输出配置：输出什么、任务目录在哪里，以及与文件系统无关的路径校验。

use std::path::{Component, Path, PathBuf};

use crate::Error;

/// 输出到哪里、任务目录在哪里；下载（[`crate::JobRequest::output`]）与只合并（[`crate::merge_recorded`]）共用。
/// 用 [`OutputOptions::new`] 取默认值后按需修改字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputOptions {
    pub target: Target,
    /// 任务目录；None 时为 `<MP4 路径>.hsdl`，只输出 HLS 时为 `<HLS 目录>.hsdl`。只能是空目录、不存在的目录或
    /// 本库建立的任务目录
    pub work_dir: Option<PathBuf>,
    /// 输出已存在时替换：MP4 文件直接替换；HLS 目录只在其中全是本库写出的文件时替换，否则仍报
    /// [`Error::OutputExists`]，以免路径给错时删掉别人的文件。为 false 时 HLS 目录须不存在或为空
    pub overwrite: bool,
    /// 成功后保留任务目录
    pub keep_work_dir: bool,
}

/// 输出什么。各路径与任务目录不能相同或互相包含（按字面比较，不跟随符号链接），所在目录不存在时在合并前创建。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// MP4 文件
    Mp4(PathBuf),
    /// 可直接播放的本地 HLS 目录：不经 FFmpeg，原样保留分片（已解密），不受 MP4 合并对编码的限制。
    /// 入口为其中的 `index.m3u8`，布局见 [`crate::Output::hls`]
    Hls(PathBuf),
    Both {
        mp4: PathBuf,
        hls: PathBuf,
    },
}

impl Target {
    pub fn mp4(&self) -> Option<&Path> {
        match self {
            Target::Mp4(path) | Target::Both { mp4: path, .. } => Some(path),
            Target::Hls(_) => None,
        }
    }

    pub fn hls(&self) -> Option<&Path> {
        match self {
            Target::Hls(path) | Target::Both { hls: path, .. } => Some(path),
            Target::Mp4(_) => None,
        }
    }

    /// 默认任务目录所依据的路径：有 MP4 时为它，否则为 HLS 目录。
    fn primary(&self) -> &Path {
        match self {
            Target::Mp4(path) | Target::Hls(path) | Target::Both { mp4: path, .. } => path,
        }
    }
}

impl OutputOptions {
    pub fn new(target: Target) -> Self {
        OutputOptions {
            target,
            work_dir: None,
            overwrite: false,
            keep_work_dir: false,
        }
    }

    /// 实际使用的任务目录：`work_dir`，未指定时见 [`OutputOptions::work_dir`]。
    pub fn resolved_work_dir(&self) -> PathBuf {
        self.work_dir.clone().unwrap_or_else(|| {
            let mut name = self.target.primary().as_os_str().to_owned();
            name.push(".hsdl");
            PathBuf::from(name)
        })
    }

    /// 路径都有文件名、互不相同也不互相包含，输出按 `overwrite` 可以写。
    pub(crate) fn validate(&self) -> Result<(), Error> {
        let work_dir = self.resolved_work_dir();
        let mut paths: Vec<&Path> = vec![&work_dir];
        paths.extend(self.target.mp4());
        paths.extend(self.target.hls());
        let mut absolute = Vec::with_capacity(paths.len());
        for path in &paths {
            if path.file_name().is_none() {
                return Err(Error::InvalidInput(format!(
                    "路径没有文件名：{}",
                    path.display()
                )));
            }
            absolute.push(lexical_absolute(path)?);
        }
        for (i, a) in absolute.iter().enumerate() {
            for (j, b) in absolute.iter().enumerate() {
                if i != j && a.starts_with(b) {
                    return Err(Error::InvalidInput(format!(
                        "输出与任务目录的路径不能相同或互相包含：{} 与 {}",
                        paths[i].display(),
                        paths[j].display()
                    )));
                }
            }
        }
        super::check_targets(self)
    }
}

/// 按字面规整成绝对路径：补上当前目录，去掉 `.`，`..` 退一级；不访问文件系统，不跟随符号链接。
fn lexical_absolute(path: &Path) -> Result<PathBuf, Error> {
    let absolute = std::path::absolute(path).map_err(|cause| Error::Io {
        action: "解析",
        path: path.to_path_buf(),
        cause,
    })?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    Ok(normalized)
}
