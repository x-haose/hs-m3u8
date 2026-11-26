//! 输出配置：输出什么、任务目录在哪里，以及与文件系统无关的路径校验。

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use hs_m3u8_remux::Container;

use crate::Error;
use crate::error::io_error;
use crate::ident::Fingerprint;
use crate::workdir::{OutputKind, PendingOutput};

/// 输出到哪里、任务目录在哪里；下载（[`crate::JobRequest::output`]）与只合并（[`crate::Engine::merge_recorded`]）共用。
/// 用 [`OutputOptions::new`] 取默认值后按需修改字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputOptions {
    pub target: Target,
    /// 任务目录；None 时与输出同级，名为输出的主名加 `.hsdl`：主名为合并文件的文件名去掉扩展名，只输出 HLS 时为
    /// HLS 目录名。`a.mp4`、`a.mkv` 与 HLS 目录 `a` 共用 `a.hsdl`，改变输出种类后接着用已下载的分片。只能是空目录、
    /// 不存在的目录或本库建立的任务目录
    pub work_dir: Option<PathBuf>,
    /// 输出已存在时替换：合并文件与另存的字幕文件直接替换；HLS 目录只在其中全是本库写出的文件时替换。路径被别的东西占用（见
    /// [`Error::OutputOccupied`]）时覆盖也不替换。为 false 时 HLS 目录须不存在或为空（系统自动生成的元数据文件不算），
    /// 否则报 [`Error::OutputExists`]
    pub overwrite: bool,
    /// 成功后保留任务目录
    pub keep_work_dir: bool,
    /// 选了字幕时怎样输出；本地 HLS 里选了的字幕总是作为 SUBTITLES rendition 写进主播放列表（放原 WebVTT 分片）
    pub subtitles: SubtitleOutput,
}

/// 选了字幕（[`hs_m3u8_hls::Preference::subtitles`]）时合并文件怎样带上它们。时间按 X-TIMESTAMP-MAP 对齐到画面
/// （RFC 8216 3.5），字幕分片按不连续段序号对到同号的那段画面上（见 [`crate::MissReason::NoMatchingDiscontinuity`]）。选了
/// 字幕却既不封进也不另存、又没有本地 HLS 输出时，开始任务报参数错误（字幕下了也用不上）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleOutput {
    /// 封进合并文件：Matroska 为 SubRip（保留粗体、斜体、下划线），MP4 为 mov_text（只有文字，同时显示的几条合成一段）
    pub embed: bool,
    /// 另存为文件，放在合并文件旁，时间与合并文件对齐。文件名为 `<合并文件的主名>.<语言>[.forced].<扩展名>`：语言为
    /// LANGUAGE 原样（BCP 47，如 `zh-Hans`；没写或含字母、数字、`-` 以外的字符时为 `und`），同名的从第二个起再加
    /// `.<n>`（n 从 2 起）。只输出本地 HLS 而要另存时报参数错误
    pub files: Option<SubtitleFileFormat>,
}

impl Default for SubtitleOutput {
    /// 封进合并文件，不另存。
    fn default() -> Self {
        SubtitleOutput {
            embed: true,
            files: None,
        }
    }
}

/// 另存的字幕文件的格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleFileFormat {
    /// `.srt`：粗体、斜体、下划线保留，其余样式与位置丢掉
    Srt,
    /// `.vtt`：cue 的设置与样式原样
    WebVtt,
}

impl SubtitleFileFormat {
    fn extension(self) -> &'static str {
        match self {
            SubtitleFileFormat::Srt => "srt",
            SubtitleFileFormat::WebVtt => "vtt",
        }
    }
}

/// 合并文件：各路流原样复制、不重编码，封装为 `container`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedFile {
    pub path: PathBuf,
    /// MP4 收的编码见 [`Container::holds`]，放不进时报 [`crate::Unsupported::Merge`]，可改用 Matroska。MP3、MP2 音轨放进
    /// MP4 后，苹果基于 AVFoundation 的播放器（QuickTime Player、Safari 等）认不出
    pub container: Container,
}

/// 输出什么。各路径与任务目录须以文件名结尾，不能相同或互相包含（按字面比较，不跟随符号链接），所在目录不存在时在任务
/// 开头与写出前创建；相对路径按开始任务时的当前目录补全，之后改变当前目录不影响任务；补全后最后一段变了（Windows
/// 去掉结尾的点与空格）时报参数错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// 合并文件（MP4 或 Matroska）
    File(MergedFile),
    /// 可直接播放的本地 HLS 目录：不经 FFmpeg，原样保留分片（已解密），不受合并对编码的限制。
    /// 入口为其中的 `index.m3u8`，布局见 [`crate::Output::hls`]
    Hls(PathBuf),
    Both {
        file: MergedFile,
        hls: PathBuf,
    },
}

impl Target {
    pub fn file(&self) -> Option<&MergedFile> {
        match self {
            Target::File(file) | Target::Both { file, .. } => Some(file),
            Target::Hls(_) => None,
        }
    }

    pub fn hls(&self) -> Option<&Path> {
        match self {
            Target::Hls(path) | Target::Both { hls: path, .. } => Some(path),
            Target::File(_) => None,
        }
    }

    /// 默认任务目录所依据的输出与它的主名（见 [`OutputOptions::work_dir`]）；路径没有文件名时为 None。
    fn primary(&self) -> (&Path, Option<&OsStr>) {
        match self {
            Target::File(file) | Target::Both { file, .. } => (&file.path, file.path.file_stem()),
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
            subtitles: SubtitleOutput::default(),
        }
    }

    /// 实际使用的任务目录：`work_dir`，未指定时见 [`OutputOptions::work_dir`]；与开始任务时一样补全为绝对路径、校验
    /// （见 [`Target`]），不合要求或未指定 `work_dir` 而输出路径没有文件名时报参数错误。
    pub fn resolved_work_dir(&self) -> Result<PathBuf, Error> {
        let (primary, stem) = self.target.primary();
        absolute_path(&work_dir_as_given(self.work_dir.as_deref(), primary, stem)?)
    }

    /// 校验并补全为绝对路径（见 [`resolve`]）；只输出本地 HLS 而要另存字幕文件时报参数错误。另存的字幕文件要知道选了
    /// 哪些字幕才定得下，见 [`ResolvedOutput::with_subtitle_files`]。
    pub(crate) fn resolve(&self) -> Result<ResolvedOutput, Error> {
        if self.subtitles.files.is_some() && self.target.file().is_none() {
            return Err(Error::InvalidInput(
                "另存字幕文件要有合并文件：字幕的时间按合并文件对齐".to_owned(),
            ));
        }
        let (primary, stem) = self.target.primary();
        let work_dir = work_dir_as_given(self.work_dir.as_deref(), primary, stem)?;
        let mut targets: Vec<(OutputKind, &Path)> = Vec::new();
        if let Some(file) = self.target.file() {
            targets.push((OutputKind::Merged(file.container), &file.path));
        }
        if let Some(hls) = self.target.hls() {
            targets.push((OutputKind::Hls, hls));
        }
        resolve(
            work_dir,
            &targets,
            self.overwrite,
            self.keep_work_dir,
            Some(self.subtitles.clone()),
        )
    }
}

/// 文件任务的输出（[`crate::FileRequest::output`]）：下载的文件与任务目录。用 [`FileOutputOptions::new`] 取默认值后按需
/// 修改字段。路径的规则同 [`Target`]：输出与任务目录须以文件名结尾，不能相同或互相包含，相对路径按开始任务时的当前目录
/// 补全；输出路径不能以路径分隔符结尾，补全后须是 UTF-8。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutputOptions {
    /// 下载的文件；所在目录不存在时在任务开头与装上前创建
    pub path: PathBuf,
    /// 任务目录；None 时与输出同级，名为输出的文件名去掉扩展名加 `.hsdl`（同 [`OutputOptions::work_dir`]）。只能是空
    /// 目录、不存在的目录或本库建立的任务目录
    pub work_dir: Option<PathBuf>,
    /// 输出已存在时替换；路径上是目录等别的东西（见 [`Error::OutputOccupied`]）时也不替换
    pub overwrite: bool,
    /// 成功后保留任务目录（其中的数据文件已装到输出，与它共用内容或是它的副本）
    pub keep_work_dir: bool,
}

impl FileOutputOptions {
    pub fn new(path: PathBuf) -> Self {
        FileOutputOptions {
            path,
            work_dir: None,
            overwrite: false,
            keep_work_dir: false,
        }
    }

    /// 实际使用的任务目录，同 [`OutputOptions::resolved_work_dir`]。
    pub fn resolved_work_dir(&self) -> Result<PathBuf, Error> {
        absolute_path(&self.work_dir_as_given()?)
    }

    fn work_dir_as_given(&self) -> Result<PathBuf, Error> {
        work_dir_as_given(self.work_dir.as_deref(), &self.path, self.path.file_stem())
    }

    /// 校验并补全为绝对路径（见 [`resolve`]）。
    pub(crate) fn resolve(&self) -> Result<ResolvedOutput, Error> {
        let targets = [(OutputKind::File, self.path.as_path())];
        resolve(
            self.work_dir_as_given()?,
            &targets,
            self.overwrite,
            self.keep_work_dir,
            None,
        )
    }
}

/// 任务目录，未补全：给了 `work_dir` 即它，否则与 `primary` 同级、名为它的主名 `stem` 加 `.hsdl`；没有主名时报参数错
/// 误。
fn work_dir_as_given(
    work_dir: Option<&Path>,
    primary: &Path,
    stem: Option<&OsStr>,
) -> Result<PathBuf, Error> {
    if let Some(dir) = work_dir {
        return Ok(dir.to_path_buf());
    }
    match stem {
        Some(stem) => Ok(sibling(primary, stem, ".hsdl")),
        None => Err(Error::InvalidInput(format!(
            "输出路径没有文件名：{}",
            primary.display()
        ))),
    }
}

/// 校验并补全为绝对路径：文件（合并文件、另存的字幕文件、文件任务的输出）路径不以分隔符结尾，各路径见 [`absolute_path`]
/// 、互不相同也不互相包含，输出路径补全后是 UTF-8（任务目录记下正在写的输出，记录的写法要求 UTF-8；Unix 上的文件名、
/// Windows 上含孤立代理项的路径可以不是）。只按字面判断，不访问文件系统：输出能否写在任务开头查。
fn resolve(
    work_dir: PathBuf,
    targets: &[(OutputKind, &Path)],
    overwrite: bool,
    keep_work_dir: bool,
    subtitles: Option<SubtitleOutput>,
) -> Result<ResolvedOutput, Error> {
    let invalid = |reason: &str, path: &Path| {
        Err(Error::InvalidInput(format!("{reason}：{}", path.display())))
    };
    for &(kind, path) in targets {
        let is_file = !matches!(kind, OutputKind::Hls);
        if is_file
            && path
                .as_os_str()
                .to_string_lossy()
                .ends_with(std::path::is_separator)
        {
            return invalid("输出的是文件，路径不能以路径分隔符结尾", path);
        }
    }
    let mut paths: Vec<&Path> = vec![&work_dir];
    paths.extend(targets.iter().map(|&(_, path)| path));
    let absolute = paths
        .iter()
        .map(|path| absolute_path(path))
        .collect::<Result<Vec<_>, _>>()?;
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
    for (&(kind, path), target) in targets.iter().zip(absolute) {
        if target.to_str().is_none() {
            return invalid("输出路径须为 UTF-8", path);
        }
        outputs.push(PendingOutput::new(kind, target, fingerprint));
    }
    Ok(ResolvedOutput {
        outputs,
        work_dir,
        overwrite,
        keep_work_dir,
        subtitles,
    })
}

/// 校验过、补全为绝对路径的输出配置（见 [`resolve`]），任务内部只用它。
#[derive(Debug, Clone)]
pub(crate) struct ResolvedOutput {
    /// 各输出：合并文件在前，其后为本地 HLS、各另存的字幕文件（按字幕的轨号）；文件任务只有一个
    pub outputs: Vec<PendingOutput>,
    pub work_dir: PathBuf,
    pub overwrite: bool,
    pub keep_work_dir: bool,
    /// 选了字幕时怎样输出；文件任务为 None
    pub subtitles: Option<SubtitleOutput>,
}

impl ResolvedOutput {
    pub(crate) fn get(&self, kind: OutputKind) -> Option<&PendingOutput> {
        self.outputs.iter().find(|o| o.kind() == kind)
    }

    /// 合并文件与它的容器。
    pub(crate) fn merged(&self) -> Option<(&PendingOutput, Container)> {
        self.outputs.iter().find_map(|o| match o.kind() {
            OutputKind::Merged(container) => Some((o, container)),
            _ => None,
        })
    }

    /// 各另存的字幕文件，按字幕的轨号。
    pub(crate) fn subtitle_files(&self) -> impl Iterator<Item = &PendingOutput> {
        self.outputs
            .iter()
            .filter(|o| matches!(o.kind(), OutputKind::Subtitles(_)))
    }

    /// 加上各另存的字幕文件（见 [`SubtitleOutput::files`]）：`subtitles` 为所选各字幕的语言与是否强制，按轨号。不另存或
    /// 没选字幕时原样返回。新加的路径与已有的同样校验（见 [`resolve`]）。
    pub(crate) fn with_subtitle_files(
        &self,
        subtitles: &[(Option<&str>, bool)],
    ) -> Result<ResolvedOutput, Error> {
        let format = self.subtitles.as_ref().and_then(|s| s.files);
        let (Some(format), Some((merged, _))) = (format, self.merged()) else {
            return Ok(self.clone());
        };
        if subtitles.is_empty() {
            return Ok(self.clone());
        }
        let target = merged.target();
        let stem = target
            .file_stem()
            .expect("合并文件的路径以文件名结尾")
            .to_string_lossy()
            .into_owned();
        let names = subtitle_file_names(&stem, subtitles, format.extension());
        let paths: Vec<PathBuf> = names
            .iter()
            .map(|name| target.with_file_name(name))
            .collect();
        let mut targets: Vec<(OutputKind, &Path)> = self
            .outputs
            .iter()
            .map(|o| (o.kind(), o.target()))
            .collect();
        for (n, path) in paths.iter().enumerate() {
            targets.push((OutputKind::Subtitles(n), path));
        }
        resolve(
            self.work_dir.clone(),
            &targets,
            self.overwrite,
            self.keep_work_dir,
            self.subtitles.clone(),
        )
    }
}

/// 另存的字幕文件名（见 [`SubtitleOutput::files`]）：`subtitles` 为各字幕的语言与是否强制。同名按不分 ASCII 大小写比：
/// macOS 与 Windows 上的文件名默认不分大小写，`en` 与 `EN` 是同一个文件。
fn subtitle_file_names(
    stem: &str,
    subtitles: &[(Option<&str>, bool)],
    extension: &str,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::with_capacity(subtitles.len());
    for &(language, forced) in subtitles {
        let language = language
            .filter(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
            .unwrap_or("und");
        let forced = if forced { ".forced" } else { "" };
        let base = format!("{stem}.{language}{forced}");
        let mut name = format!("{base}.{extension}");
        let mut n = 2;
        while names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            name = format!("{base}.{n}.{extension}");
            n += 1;
        }
        names.push(name);
    }
    names
}

/// `path` 按当前目录补全的绝对路径，去掉结尾的分隔符：带着它，文件系统调用指的是「这一项当作目录」，这一项是文件
/// 时报「不是目录」，而不是看这一项本身。`path` 须以文件名结尾，补全后仍是这个名字，否则报参数错误：Windows 补全时
/// 去掉最后一段结尾的点与空格，`D:\x\...` 补全成 `D:\x`、`a.` 补全成 `a`。
pub(crate) fn absolute_path(path: &Path) -> Result<PathBuf, Error> {
    if !ends_with_file_name(path) {
        return Err(Error::InvalidInput(format!(
            "路径须以文件名结尾：{}",
            path.display()
        )));
    }
    let absolute = std::path::absolute(path).map_err(io_error("解析", path))?;
    let completed = match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => absolute,
    };
    if completed.file_name() != path.file_name() {
        return Err(Error::InvalidInput(format!(
            "路径补全后最后一段变了：{} 补全为 {}",
            path.display(),
            completed.display()
        )));
    }
    Ok(completed)
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

    /// 另存的字幕文件与合并文件同级，名字按语言与是否强制，同名的从第二个起加序号；语言不合文件名时为 und。只输出本地
    /// HLS 而要另存时报参数错误。
    #[test]
    fn subtitle_files_are_named_after_the_merged_file() {
        let mut options = OutputOptions::new(Target::File(MergedFile {
            path: "/v/movie.mkv".into(),
            container: Container::Matroska,
        }));
        options.subtitles.files = Some(SubtitleFileFormat::Srt);
        let resolved = options.resolve().unwrap();
        let subtitles = [
            (Some("zh-Hans"), false),
            (Some("en"), true),
            (Some("zh-Hans"), false),
            (Some("a/b"), false),
            (None, false),
            (Some("EN"), true),
        ];
        let extended = resolved.with_subtitle_files(&subtitles).unwrap();
        let names: Vec<String> = extended
            .subtitle_files()
            .map(|o| {
                o.target()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            [
                "movie.zh-Hans.srt",
                "movie.en.forced.srt",
                "movie.zh-Hans.2.srt",
                "movie.und.srt",
                "movie.und.2.srt",
                "movie.EN.forced.2.srt"
            ]
        );
        assert_eq!(extended.outputs.len(), 7);

        let mut hls_only = OutputOptions::new(Target::Hls("/v/movie".into()));
        hls_only.subtitles.files = Some(SubtitleFileFormat::WebVtt);
        assert!(matches!(hls_only.resolve(), Err(Error::InvalidInput(_))));
    }

    /// 相对路径按当前目录补全，记进任务目录的临时名与挪开的名字也就与之后的当前目录无关；它们与输出同级。
    #[test]
    fn relative_paths_are_resolved_against_the_current_directory() {
        let options = OutputOptions::new(Target::Both {
            file: MergedFile {
                path: "a/out.mp4".into(),
                container: Container::Mp4,
            },
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

    /// Windows 补全时去掉最后一段结尾的点与空格：只有点的变成上一级（`D:\...` 是没有文件名的盘符根），其余的换了名字，
    /// 都报参数错误，任务目录也一样。
    #[cfg(windows)]
    #[test]
    fn paths_whose_name_changes_when_completed_are_rejected() {
        let mp4 = |path: &str| {
            Target::File(MergedFile {
                path: path.into(),
                container: Container::Mp4,
            })
        };
        for path in [r"D:\...", r"D:\x\...", r"D:\x\a.", r"D:\x\a "] {
            let mut options = OutputOptions::new(mp4(path));
            options.work_dir = Some(r"C:\work".into());
            assert!(
                matches!(options.resolve(), Err(Error::InvalidInput(_))),
                "{path}"
            );
            let mut options = OutputOptions::new(mp4(r"C:\out.mp4"));
            options.work_dir = Some(path.into());
            assert!(
                matches!(options.resolved_work_dir(), Err(Error::InvalidInput(_))),
                "{path}"
            );
        }
    }
}
