//! `outputs.json`：正在写的输出，及其序列化格式（契约）。写临时输出之前记下各输出，全部装上后改记为已换好；之后
//! 的运行与放弃任务时据此收拾留下的东西，都收拾完即删除。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use hs_m3u8_remux::Container;
use serde::{Deserialize, Serialize};

use super::record::check_version;
use super::write_atomic;
use crate::error::io_error;
use crate::ident::Fingerprint;
use crate::{Error, WorkDirProblem, busy, volume};

pub(super) const OUTPUTS_FILE: &str = "outputs.json";

/// outputs.json 的格式版本，与 job.json 的各自升：管它的字段，以及由字段推出的东西——两个保留名的推导规则（见
/// [`PendingOutput::new`]）与 `swapped` 的含义，任何一项改变都要升。兼容约定同 [`super::record::JOB_FORMAT_VERSION`]：
/// 放弃任务与之后的运行要按旧版本的规则找到写到一半的输出与挪开的旧输出，读不懂时什么也不动。
///
/// 各版本：13 为 1.0 写的；14 加了文件任务的输出（`file` 一种）；15 加了 Matroska 合并文件（`mkv`）与另存的字幕文件
/// （`{"subtitles":{"index":<n>}}`，n 为第几个字幕，保留名的种类段为 `sub<n>`）。
pub(super) const OUTPUTS_FORMAT_VERSION: u32 = 15;

/// 读得了的最旧的 outputs.json 格式版本：1.0 写的。
const OLDEST_OUTPUTS_FORMAT_VERSION: u32 = 13;

/// 加了文件任务的输出的 outputs.json 格式版本。
const FILE_OUTPUTS_FORMAT_VERSION: u32 = 14;

/// 加了 Matroska 合并文件与另存的字幕文件的 outputs.json 格式版本。
const TRACKS_OUTPUTS_FORMAT_VERSION: u32 = 15;

/// 输出的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputKind {
    /// 合并文件（MP4 或 Matroska）
    Merged(Container),
    /// 本地 HLS 目录
    Hls,
    /// 文件任务下载的文件
    File,
    /// 另存的第 n 个字幕文件（按所选字幕的顺序，从 0 起）
    Subtitles(usize),
}

/// 正在写的一个输出：输出路径（绝对路径，与当前目录无关），以及由它与任务目录指纹定下的两个与它同级的保留名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingOutput {
    kind: OutputKind,
    target: PathBuf,
    fingerprint: Fingerprint,
    temp: PathBuf,
    aside: PathBuf,
}

impl PendingOutput {
    /// `target` 须是以文件名结尾的绝对路径；`fingerprint` 为任务目录的指纹。两个保留名为
    /// `hsdl-<fingerprint>.<种类>.part` 与 `.old`，种类为 `mp4`、`mkv`、`hls`、`file` 或 `sub<n>`：不随输出名变长；几个任务
    /// （任务目录不同）输出到同一处时各写各的。
    pub(crate) fn new(kind: OutputKind, target: PathBuf, fingerprint: Fingerprint) -> Self {
        let ext = match kind {
            OutputKind::Merged(Container::Mp4) => "mp4".to_owned(),
            OutputKind::Merged(Container::Matroska) => "mkv".to_owned(),
            OutputKind::Hls => "hls".to_owned(),
            OutputKind::File => "file".to_owned(),
            OutputKind::Subtitles(n) => format!("sub{n}"),
        };
        let dir = target
            .parent()
            .expect("输出路径是以文件名结尾的绝对路径")
            .to_path_buf();
        let name = |suffix: &str| dir.join(format!("hsdl-{fingerprint}.{ext}.{suffix}"));
        PendingOutput {
            kind,
            temp: name("part"),
            aside: name("old"),
            target,
            fingerprint,
        }
    }

    pub(crate) fn kind(&self) -> OutputKind {
        self.kind
    }

    pub(crate) fn target(&self) -> &Path {
        &self.target
    }

    /// 写到这里，备齐后装到输出路径
    pub(crate) fn temp(&self) -> &Path {
        &self.temp
    }

    /// 要替换输出路径上已有的东西时，先把它挪到这里，换上新的之后再删
    pub(crate) fn aside(&self) -> &Path {
        &self.aside
    }
}

/// 任务目录里记着的正在写的输出。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct OutputsRecord {
    /// 各输出都已装上：挪开的旧输出只待删除，不再放回
    pub swapped: bool,
    pub outputs: Vec<PendingOutput>,
}

/// 读取记录；没有时（含任务目录不存在）为空，根读不到时报错（见 [`volume::reachable`]）。
pub(crate) fn read_outputs(root: &Path) -> Result<OutputsRecord, Error> {
    let path = outputs_file(root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            volume::reachable(&path)?;
            return Ok(OutputsRecord::default());
        }
        Err(cause) => return Err(io_error("读取", &path)(cause)),
    };
    decode(&bytes).map_err(|problem| Error::WorkDir {
        path: root.to_path_buf(),
        problem,
    })
}

/// 写下记录。
pub(crate) fn record_outputs(
    root: &Path,
    outputs: &[PendingOutput],
    swapped: bool,
) -> Result<(), Error> {
    write_atomic(&outputs_file(root), &encode(outputs, swapped))
}

/// 删除记录；没有时不算失败。失败时调用方把它记为残留（路径见 [`outputs_file`]）。
pub(crate) fn clear_outputs(root: &Path) -> io::Result<()> {
    match busy::retrying(|| fs::remove_file(outputs_file(root))) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub(crate) fn outputs_file(root: &Path) -> PathBuf {
    root.join(OUTPUTS_FILE)
}

/// `outputs.json` 的写法。`format_version` 不等于 [`OUTPUTS_FORMAT_VERSION`] 时拒绝。保留名不写进去，由输出路径与指纹
/// 算出：记录只能指向输出旁的保留名，碰不到别的路径。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputsFile {
    format_version: u32,
    swapped: bool,
    outputs: Vec<OutputFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputFile {
    kind: KindFile,
    /// 以文件名结尾的绝对路径
    target: String,
    fingerprint: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum KindFile {
    Mp4,
    Hls,
    /// 格式版本 14 起
    File,
    /// 格式版本 15 起
    Mkv,
    /// 格式版本 15 起
    Subtitles {
        index: usize,
    },
}

fn encode(outputs: &[PendingOutput], swapped: bool) -> Vec<u8> {
    let file = OutputsFile {
        format_version: OUTPUTS_FORMAT_VERSION,
        swapped,
        outputs: outputs
            .iter()
            .map(|o| OutputFile {
                kind: match o.kind {
                    OutputKind::Merged(Container::Mp4) => KindFile::Mp4,
                    OutputKind::Merged(Container::Matroska) => KindFile::Mkv,
                    OutputKind::Hls => KindFile::Hls,
                    OutputKind::File => KindFile::File,
                    OutputKind::Subtitles(index) => KindFile::Subtitles { index },
                },
                target: o
                    .target
                    .to_str()
                    .expect("输出路径在开始任务时校验过是 UTF-8")
                    .to_owned(),
                fingerprint: o.fingerprint.to_string(),
            })
            .collect(),
    };
    serde_json::to_vec(&file)
        .expect("OutputsFile 只含字符串、整数、布尔、数组与枚举，序列化不会失败")
}

/// 解析 `outputs.json`。
fn decode(bytes: &[u8]) -> Result<OutputsRecord, WorkDirProblem> {
    let version = check_version(
        bytes,
        OUTPUTS_FILE,
        OLDEST_OUTPUTS_FORMAT_VERSION,
        OUTPUTS_FORMAT_VERSION,
    )?;
    let file: OutputsFile = serde_json::from_slice(bytes)
        .map_err(|e| WorkDirProblem::Corrupt(format!("{OUTPUTS_FILE} 无法解析：{e}")))?;
    let outputs = file
        .outputs
        .into_iter()
        .map(|o| {
            let target = PathBuf::from(o.target);
            if !target.is_absolute() || target.file_name().is_none() {
                return Err(format!(
                    "{OUTPUTS_FILE} 里的输出路径不是以文件名结尾的绝对路径：{}",
                    target.display()
                ));
            }
            let fingerprint = Fingerprint::parse(&o.fingerprint)
                .ok_or_else(|| format!("{OUTPUTS_FILE} 里的指纹无法识别：{}", o.fingerprint))?;
            let kind = match o.kind {
                KindFile::Mp4 => OutputKind::Merged(Container::Mp4),
                KindFile::Hls => OutputKind::Hls,
                KindFile::File if version < FILE_OUTPUTS_FORMAT_VERSION => {
                    return Err(format!(
                        "{OUTPUTS_FILE} 是格式版本 {version}，那时还没有文件任务的输出"
                    ));
                }
                KindFile::File => OutputKind::File,
                KindFile::Mkv | KindFile::Subtitles { .. }
                    if version < TRACKS_OUTPUTS_FORMAT_VERSION =>
                {
                    return Err(format!(
                        "{OUTPUTS_FILE} 是格式版本 {version}，那时还没有 Matroska 与字幕文件的输出"
                    ));
                }
                KindFile::Mkv => OutputKind::Merged(Container::Matroska),
                KindFile::Subtitles { index } => OutputKind::Subtitles(index),
            };
            Ok(PendingOutput::new(kind, target, fingerprint))
        })
        .collect::<Result<_, String>>()
        .map_err(WorkDirProblem::Corrupt)?;
    Ok(OutputsRecord {
        swapped: file.swapped,
        outputs,
    })
}

/// 已发布版本的样本（见 [`super::released`]），每个版本手写：`<平台>/outputs.json` 为记录，`<平台>/reserved.json` 为按那个
/// 版本的规则由它推出的各输出的两个保留名（临时输出、挪开的旧输出）。路径的写法随平台不同，各平台读自己的。
#[cfg(test)]
mod released {
    use super::*;
    use crate::workdir::released::versions;

    /// 都读得了、推出的保留名与当时的相同；当前版本的按当前写法重新写出逐字节相同，改了写法而没升版本时在这里失败。
    /// 最新的样本须是当前版本的。
    #[test]
    fn samples_are_readable() {
        let platform = if cfg!(windows) { "windows" } else { "unix" };
        let versions = versions("outputs");
        for (version, dir) in &versions {
            let path = dir.join(platform).join(OUTPUTS_FILE);
            let bytes = fs::read(&path).unwrap();
            let written: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(written["format_version"], *version, "{}", path.display());
            let record = decode(&bytes).unwrap_or_else(|e| panic!("{}：{e}", path.display()));
            let reserved: Vec<[PathBuf; 2]> = serde_json::from_slice(
                &fs::read(dir.join(platform).join("reserved.json")).unwrap(),
            )
            .unwrap();
            let derived: Vec<[PathBuf; 2]> = record
                .outputs
                .iter()
                .map(|o| [o.temp().to_path_buf(), o.aside().to_path_buf()])
                .collect();
            assert_eq!(derived, reserved, "{}", path.display());
            if *version == OUTPUTS_FORMAT_VERSION {
                let rewritten = encode(&record.outputs, record.swapped);
                assert_eq!(rewritten, bytes, "{}", path.display());
            }
        }
        assert_eq!(
            versions.last().map(|(version, _)| *version),
            Some(OUTPUTS_FORMAT_VERSION),
            "缺少当前格式版本的样本"
        );
    }
}

/// 路径的写法随平台不同，字面 JSON 在 Unix 上核对；别的平台由上面已发布的样本与端到端测试核对。
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn outputs_file_round_trips_and_rejects_other_versions_and_fields() {
        let fingerprint = Fingerprint::parse("0123456789abcdef").unwrap();
        let mp4 = PendingOutput::new(
            OutputKind::Merged(Container::Mp4),
            "/d/a.mp4".into(),
            fingerprint,
        );
        let mkv = PendingOutput::new(
            OutputKind::Merged(Container::Matroska),
            "/d/a.mkv".into(),
            fingerprint,
        );
        let srt = PendingOutput::new(OutputKind::Subtitles(1), "/d/a.en.srt".into(), fingerprint);
        assert_eq!(mkv.temp(), Path::new("/d/hsdl-0123456789abcdef.mkv.part"));
        assert_eq!(srt.aside(), Path::new("/d/hsdl-0123456789abcdef.sub1.old"));
        let hls = PendingOutput::new(OutputKind::Hls, "/d/a/".into(), fingerprint);
        let file = PendingOutput::new(OutputKind::File, "/d/a.zip".into(), fingerprint);
        assert_eq!(file.aside(), Path::new("/d/hsdl-0123456789abcdef.file.old"));
        assert_eq!(
            (mp4.temp(), mp4.aside()),
            (
                Path::new("/d/hsdl-0123456789abcdef.mp4.part"),
                Path::new("/d/hsdl-0123456789abcdef.mp4.old")
            )
        );
        assert_eq!(hls.temp(), Path::new("/d/hsdl-0123456789abcdef.hls.part"));
        let outputs = vec![mp4, hls, file, mkv, srt];
        let bytes = encode(&outputs, true);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"format_version":15,"swapped":true,"outputs":[{"kind":"mp4","target":"/d/a.mp4","fingerprint":"0123456789abcdef"},{"kind":"hls","target":"/d/a/","fingerprint":"0123456789abcdef"},{"kind":"file","target":"/d/a.zip","fingerprint":"0123456789abcdef"},{"kind":"mkv","target":"/d/a.mkv","fingerprint":"0123456789abcdef"},{"kind":{"subtitles":{"index":1}},"target":"/d/a.en.srt","fingerprint":"0123456789abcdef"}]}"#
        );
        assert_eq!(
            decode(&bytes),
            Ok(OutputsRecord {
                swapped: true,
                outputs
            })
        );

        let entry = |kind: &str, target: &str, fingerprint: &str| {
            format!(
                r#"{{"format_version":15,"swapped":false,"outputs":[{{"kind":"{kind}","target":"{target}","fingerprint":"{fingerprint}"}}]}}"#
            )
        };
        for rejected in [
            r#"{"format_version":12,"outputs":[]}"#.to_owned(),
            r#"{"format_version":13,"swapped":false,"outputs":[],"extra":1}"#.to_owned(),
            r#"{"format_version":16,"swapped":false,"outputs":[]}"#.to_owned(),
            r#"{"format_version":14,"swapped":false,"outputs":[{"kind":"mkv","target":"/d/a.mkv","fingerprint":"0123456789abcdef"}]}"#.to_owned(),
            r#"{"format_version":13,"swapped":false,"outputs":[{"kind":"file","target":"/d/a","fingerprint":"0123456789abcdef"}]}"#.to_owned(),
            r#"{"format_version":13,"outputs":[]}"#.to_owned(),
            entry("webm", "/d/a", "0123456789abcdef"),
            entry("mp4", "d/a.mp4", "0123456789abcdef"),
            entry("mp4", "/", "0123456789abcdef"),
            entry("mp4", "/d/a.mp4", "../../x"),
        ] {
            assert!(decode(rejected.as_bytes()).is_err(), "{rejected}");
        }
    }
}
