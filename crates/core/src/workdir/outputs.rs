//! `outputs.json`：正在写的输出，及其序列化格式（契约）。写临时输出之前记下各输出，全部装上后改记为已换好；之后
//! 的运行与放弃任务时据此收拾留下的东西，都收拾完即删除。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::record::{FORMAT_VERSION, check_version};
use super::write_atomic;
use crate::error::io_error;
use crate::ident::Fingerprint;
use crate::{Error, WorkDirProblem};

pub(super) const OUTPUTS_FILE: &str = "outputs.json";

/// 输出的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputKind {
    /// MP4 文件
    Mp4,
    /// 本地 HLS 目录
    Hls,
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
    /// `hsdl-<fingerprint>.<mp4|hls>.part` 与 `.old`：定长，不随输出名变长；几个任务（任务目录不同）输出到同一处时
    /// 各写各的。
    pub(crate) fn new(kind: OutputKind, target: PathBuf, fingerprint: Fingerprint) -> Self {
        let ext = match kind {
            OutputKind::Mp4 => "mp4",
            OutputKind::Hls => "hls",
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

/// 读取记录；没有时（含任务目录不存在）为空。
pub(crate) fn read_outputs(root: &Path) -> Result<OutputsRecord, Error> {
    let path = outputs_file(root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(OutputsRecord::default()),
        Err(cause) => return Err(io_error("读取", &path)(cause)),
    };
    decode(&bytes).map_err(|reason| Error::WorkDir {
        path: root.to_path_buf(),
        problem: WorkDirProblem::Corrupt(reason),
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
    match fs::remove_file(outputs_file(root)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub(crate) fn outputs_file(root: &Path) -> PathBuf {
    root.join(OUTPUTS_FILE)
}

/// `outputs.json` 的写法。`format_version` 不等于 [`FORMAT_VERSION`] 时拒绝。保留名不写进去，由输出路径与指纹
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
}

fn encode(outputs: &[PendingOutput], swapped: bool) -> Vec<u8> {
    let file = OutputsFile {
        format_version: FORMAT_VERSION,
        swapped,
        outputs: outputs
            .iter()
            .map(|o| OutputFile {
                kind: match o.kind {
                    OutputKind::Mp4 => KindFile::Mp4,
                    OutputKind::Hls => KindFile::Hls,
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

/// 解析 `outputs.json`；失败时返回原因。
fn decode(bytes: &[u8]) -> Result<OutputsRecord, String> {
    check_version(bytes, OUTPUTS_FILE)?;
    let file: OutputsFile =
        serde_json::from_slice(bytes).map_err(|e| format!("{OUTPUTS_FILE} 无法解析：{e}"))?;
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
                KindFile::Mp4 => OutputKind::Mp4,
                KindFile::Hls => OutputKind::Hls,
            };
            Ok(PendingOutput::new(kind, target, fingerprint))
        })
        .collect::<Result<_, String>>()?;
    Ok(OutputsRecord {
        swapped: file.swapped,
        outputs,
    })
}

/// 路径的写法随平台不同，字面 JSON 在 Unix 上核对；别的平台由端到端测试写入、读回记录。
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn outputs_file_round_trips_and_rejects_other_versions_and_fields() {
        let fingerprint = Fingerprint::parse("0123456789abcdef").unwrap();
        let mp4 = PendingOutput::new(OutputKind::Mp4, "/d/a.mp4".into(), fingerprint);
        let hls = PendingOutput::new(OutputKind::Hls, "/d/a/".into(), fingerprint);
        assert_eq!(
            (mp4.temp(), mp4.aside()),
            (
                Path::new("/d/hsdl-0123456789abcdef.mp4.part"),
                Path::new("/d/hsdl-0123456789abcdef.mp4.old")
            )
        );
        assert_eq!(hls.temp(), Path::new("/d/hsdl-0123456789abcdef.hls.part"));
        let outputs = vec![mp4, hls];
        let bytes = encode(&outputs, true);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"format_version":7,"swapped":true,"outputs":[{"kind":"mp4","target":"/d/a.mp4","fingerprint":"0123456789abcdef"},{"kind":"hls","target":"/d/a/","fingerprint":"0123456789abcdef"}]}"#
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
                r#"{{"format_version":7,"swapped":false,"outputs":[{{"kind":"{kind}","target":"{target}","fingerprint":"{fingerprint}"}}]}}"#
            )
        };
        for rejected in [
            r#"{"format_version":6,"outputs":[]}"#.to_owned(),
            r#"{"format_version":7,"swapped":false,"outputs":[],"extra":1}"#.to_owned(),
            r#"{"format_version":7,"outputs":[]}"#.to_owned(),
            entry("mkv", "/d/a", "0123456789abcdef"),
            entry("mp4", "d/a.mp4", "0123456789abcdef"),
            entry("mp4", "/", "0123456789abcdef"),
            entry("mp4", "/d/a.mp4", "../../x"),
        ] {
            assert!(decode(rejected.as_bytes()).is_err(), "{rejected}");
        }
    }
}
