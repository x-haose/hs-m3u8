//! 输出配置：输出什么、任务目录在哪里，以及与文件系统无关的路径校验。

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use crate::Error;
use crate::error::io_error;
use crate::ident::Fingerprint;
use crate::workdir::{OutputKind, PendingOutput};

/// 输出到哪里、任务目录在哪里；下载（[`crate::JobRequest::output`]）与只合并（[`crate::Engine::merge_recorded`]）共用。
/// 用 [`OutputOptions::new`] 取默认值后按需修改字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputOptions {
    pub target: Target,
    /// 任务目录；None 时与输出同级，名为输出的主名加 `.hsdl`：主名为 MP4 文件名去掉扩展名，只输出 HLS 时为
    /// HLS 目录名。`a.mp4` 与 HLS 目录 `a` 共用 `a.hsdl`，改变输出种类后接着用已下载的分片。只能是空目录、
    /// 不存在的目录或本库建立的任务目录
    pub work_dir: Option<PathBuf>,
    /// 输出已存在时替换：MP4 文件直接替换；HLS 目录只在其中全是本库写出的文件时替换。路径被别的东西占用（见
    /// [`Error::OutputOccupied`]）时覆盖也不替换。为 false 时 HLS 目录须不存在或为空（系统自动生成的元数据文件不算），
    /// 否则报 [`Error::OutputExists`]
    pub overwrite: bool,
    /// 成功后保留任务目录
    pub keep_work_dir: bool,
}

/// 输出什么。各路径与任务目录不能相同或互相包含（按字面比较，不跟随符号链接），所在目录不存在时在任务开头创建；
/// 相对路径按开始任务时的当前目录补全，之后改变当前目录不影响任务。
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

    /// 实际使用的任务目录：`work_dir`，未指定时见 [`OutputOptions::work_dir`]；相对路径按当前目录补全为绝对路径，与
    /// 开始任务时一样。未指定 `work_dir` 而输出路径没有文件名时报参数错误。
    pub fn resolved_work_dir(&self) -> Result<PathBuf, Error> {
        absolute_path(&self.work_dir_as_given()?)
    }

    /// 任务目录，未补全：`work_dir`，未指定时见 [`OutputOptions::work_dir`]。
    fn work_dir_as_given(&self) -> Result<PathBuf, Error> {
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

    /// 校验并补全为绝对路径：MP4 路径不以分隔符结尾，各路径都以文件名结尾、互不相同也不互相包含，输出路径补全后
    /// 是 UTF-8（任务目录记下正在写的输出，记录的写法要求 UTF-8；Unix 上的文件名、Windows 上含孤立代理项的路径可以
    /// 不是）。只按字面判断，不访问文件系统：输出能否写在任务开头查。
    pub(crate) fn resolve(&self) -> Result<ResolvedOutput, Error> {
        let invalid = |reason: &str, path: &Path| {
            Err(Error::InvalidInput(format!("{reason}：{}", path.display())))
        };
        if let Some(mp4) = self.target.mp4()
            && mp4
                .as_os_str()
                .to_string_lossy()
                .ends_with(std::path::is_separator)
        {
            return invalid("MP4 路径是文件，不能以路径分隔符结尾", mp4);
        }
        let work_dir = self.work_dir_as_given()?;
        let targets = [
            (OutputKind::Mp4, self.target.mp4()),
            (OutputKind::Hls, self.target.hls()),
        ];
        let mut paths: Vec<&Path> = vec![&work_dir];
        paths.extend(targets.iter().filter_map(|&(_, path)| path));
        let mut absolute = Vec::with_capacity(paths.len());
        for path in &paths {
            if !ends_with_file_name(path) {
                return invalid("路径须以文件名结尾", path);
            }
            absolute.push(absolute_path(path)?);
        }
        let literal: Vec<PathBuf> = absolute.iter().map(|p| lexical(p)).collect();
        for (i, a) in literal.iter().enumerate() {
            for (j, b) in literal.iter().enumerate() {
                if i != j && a.starts_with(b) {
                    return Err(Error::InvalidInput(format!(
                        "输出与任务目录的路径不能相同或互相包含：{} 与 {}",
                        paths[i].display(),
                        paths[j].display()
                    )));
                }
            }
        }
        let fingerprint = Fingerprint::of_content(literal[0].as_os_str().as_encoded_bytes());
        let mut absolute = absolute.into_iter();
        let work_dir = absolute.next().expect("第一个是任务目录");
        let mut outputs = Vec::new();
        for ((kind, path), target) in targets
            .into_iter()
            .filter_map(|(kind, path)| Some((kind, path?)))
            .zip(absolute)
        {
            if target.to_str().is_none() {
                return invalid("输出路径须为 UTF-8", path);
            }
            outputs.push(PendingOutput::new(kind, target, fingerprint));
        }
        Ok(ResolvedOutput {
            outputs,
            work_dir,
            overwrite: self.overwrite,
            keep_work_dir: self.keep_work_dir,
        })
    }
}

/// 校验过、补全为绝对路径的输出配置（见 [`OutputOptions::resolve`]），任务内部只用它。
#[derive(Debug, Clone)]
pub(crate) struct ResolvedOutput {
    /// 各输出，MP4 在前
    pub outputs: Vec<PendingOutput>,
    pub work_dir: PathBuf,
    pub overwrite: bool,
    pub keep_work_dir: bool,
}

impl ResolvedOutput {
    pub(crate) fn get(&self, kind: OutputKind) -> Option<&PendingOutput> {
        self.outputs.iter().find(|o| o.kind() == kind)
    }
}

/// `path` 按当前目录补全的绝对路径，去掉结尾的分隔符：带着它，文件系统调用指的是「这一项当作目录」，这一项是文件
/// 时报「不是目录」，而不是看这一项本身。
pub(crate) fn absolute_path(path: &Path) -> Result<PathBuf, Error> {
    let absolute = std::path::absolute(path).map_err(io_error("解析", path))?;
    Ok(match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => absolute,
    })
}

/// 路径的最后一段（结尾的分隔符不算）就是它的文件名。`Path` 解析时略去结尾的 `.`：`out.mp4/.` 的文件名仍是
/// `out.mp4`，指的却是它本身当作目录。
fn ends_with_file_name(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    let end = bytes
        .iter()
        .rposition(|&b| !std::path::is_separator(char::from(b)))
        .map_or(0, |last| last + 1);
    path.file_name()
        .is_some_and(|name| bytes[..end].ends_with(name.as_encoded_bytes()))
}

/// 与 `path` 同级、名为 `name` 加 `suffix` 的路径；`path` 结尾的分隔符不影响结果。
pub(crate) fn sibling(path: &Path, name: &OsStr, suffix: &str) -> PathBuf {
    let mut file = OsString::from(name);
    file.push(suffix);
    path.parent()
        .map_or_else(|| PathBuf::from(&file), |p| p.join(&file))
}

/// 按字面规整绝对路径 `absolute`：去掉 `.`，`..` 退一级；不访问文件系统，不跟随符号链接。
fn lexical(absolute: &Path) -> PathBuf {
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
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 相对路径按当前目录补全，记进任务目录的临时名与挪开的名字也就与之后的当前目录无关；它们与输出同级。
    #[test]
    fn relative_paths_are_resolved_against_the_current_directory() {
        let options = OutputOptions::new(Target::Both {
            mp4: "a/out.mp4".into(),
            hls: "a/out/".into(),
        });
        let resolved = options.resolve().unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(resolved.work_dir, cwd.join("a/out.hsdl"));
        assert_eq!(options.resolved_work_dir().unwrap(), resolved.work_dir);
        for (o, target) in resolved.outputs.iter().zip(["a/out.mp4", "a/out"]) {
            assert_eq!(o.target(), cwd.join(target));
            for name in [o.temp(), o.aside()] {
                assert_eq!(name.parent(), Some(cwd.join("a").as_path()));
            }
        }
    }
}
