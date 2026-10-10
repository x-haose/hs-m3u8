//! 输出配置：输出什么、任务目录在哪里，以及与文件系统无关的路径校验。

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use crate::Error;
use crate::error::io_error;

/// 输出到哪里、任务目录在哪里；下载（[`crate::JobRequest::output`]）与只合并（[`crate::Engine::merge_recorded`]）共用。
/// 用 [`OutputOptions::new`] 取默认值后按需修改字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputOptions {
    pub target: Target,
    /// 任务目录；None 时与输出同级，名为输出的主名加 `.hsdl`：主名为 MP4 文件名去掉扩展名，只输出 HLS 时为
    /// HLS 目录名。`a.mp4` 与 HLS 目录 `a` 共用 `a.hsdl`，改变输出种类后接着用已下载的分片。只能是空目录、
    /// 不存在的目录或本库建立的任务目录
    pub work_dir: Option<PathBuf>,
    /// 输出已存在时替换：MP4 文件直接替换；HLS 目录只在其中全是本库写出的文件时替换。路径是别的东西（MP4 路径是
    /// 目录、HLS 路径不是目录或里面有别人的文件）时覆盖也不替换，报 [`Error::OutputOccupied`]。为 false 时 HLS
    /// 目录须不存在或为空（系统自动生成的元数据文件不算），否则报 [`Error::OutputExists`]
    pub overwrite: bool,
    /// 成功后保留任务目录
    pub keep_work_dir: bool,
}

/// 输出什么。各路径与任务目录不能相同或互相包含（按字面比较，不跟随符号链接），所在目录不存在时在写出前创建。
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

    /// 默认任务目录所依据的输出与它的主名（见 [`OutputOptions::work_dir`]）；路径没有文件名时为 None。
    fn primary(&self) -> (&Path, Option<&OsStr>) {
        match self {
            Target::Mp4(path) | Target::Both { mp4: path, .. } => (path, path.file_stem()),
            Target::Hls(path) => (path, path.file_name()),
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
    /// 未指定 `work_dir` 而输出路径没有文件名时报参数错误。
    pub fn resolved_work_dir(&self) -> Result<PathBuf, Error> {
        if let Some(dir) = &self.work_dir {
            return Ok(dir.clone());
        }
        match self.target.primary() {
            (path, Some(stem)) => Ok(sibling(path, stem, ".hsdl")),
            (path, None) => Err(Error::InvalidInput(format!(
                "输出路径没有文件名：{}",
                path.display()
            ))),
        }
    }

    /// 输出路径是 UTF-8、MP4 路径不以分隔符结尾，各路径都有文件名、互不相同也不互相包含。只按字面判断，不访问
    /// 文件系统：输出能否写由
    /// [`super::check_targets`] 在任务开头查。
    pub(crate) fn validate(&self) -> Result<(), Error> {
        let invalid = |reason: &str, path: &Path| {
            Err(Error::InvalidInput(format!("{reason}：{}", path.display())))
        };
        for path in self.target.mp4().into_iter().chain(self.target.hls()) {
            // 写到一半的输出记进任务目录，记录的写法要求 UTF-8；Windows 与 macOS 上的路径总是 UTF-8
            if path.to_str().is_none() {
                return invalid("输出路径须为 UTF-8", path);
            }
        }
        if let Some(mp4) = self.target.mp4()
            && mp4
                .as_os_str()
                .to_string_lossy()
                .ends_with(std::path::is_separator)
        {
            return invalid("MP4 路径是文件，不能以路径分隔符结尾", mp4);
        }
        let work_dir = self.resolved_work_dir()?;
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
        Ok(())
    }
}

/// 与 `path` 同级、名为 `name` 加 `suffix` 的路径；`path` 结尾的分隔符不影响结果。
pub(crate) fn sibling(path: &Path, name: &OsStr, suffix: &str) -> PathBuf {
    let mut file = OsString::from(name);
    file.push(suffix);
    path.parent()
        .map_or_else(|| PathBuf::from(&file), |p| p.join(&file))
}

/// 按字面规整成绝对路径：补上当前目录，去掉 `.`，`..` 退一级；不访问文件系统，不跟随符号链接。
pub(super) fn lexical_absolute(path: &Path) -> Result<PathBuf, Error> {
    let absolute = std::path::absolute(path).map_err(io_error("解析", path))?;
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
