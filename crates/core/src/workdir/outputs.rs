//! `outputs.json`：正在写的输出，及其序列化格式（契约）。写临时输出之前记下各输出的临时名与旧输出挪开后的名字，
//! 之后的运行与放弃任务时据此收拾留下的东西；都收拾完即删除。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::record::{FORMAT_VERSION, check_version};
use super::write_atomic;
use crate::error::io_error;
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

/// 正在写的一个输出；各路径都是绝对路径，与当前目录无关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingOutput {
    pub kind: OutputKind,
    pub target: PathBuf,
    /// 写到这里，备齐后装到 `target`
    pub temp: PathBuf,
    /// 要替换 `target` 上已有的东西时，先把它挪到这里，换上新的之后再删
    pub aside: PathBuf,
}

/// 读取记录；没有时（含任务目录不存在）为空。
pub(crate) fn read_outputs(root: &Path) -> Result<Vec<PendingOutput>, Error> {
    let path = outputs_file(root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(cause) => return Err(io_error("读取", &path)(cause)),
    };
    decode(&bytes).map_err(|reason| Error::WorkDir {
        path: root.to_path_buf(),
        problem: WorkDirProblem::Corrupt(reason),
    })
}

/// 写下记录。
pub(crate) fn record_outputs(root: &Path, outputs: &[PendingOutput]) -> Result<(), Error> {
    write_atomic(&outputs_file(root), &encode(outputs))
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

/// `outputs.json` 的写法。`format_version` 不等于 [`FORMAT_VERSION`] 时拒绝。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputsFile {
    format_version: u32,
    outputs: Vec<OutputFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputFile {
    kind: KindFile,
    target: String,
    temp: String,
    aside: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum KindFile {
    Mp4,
    Hls,
}

fn encode(outputs: &[PendingOutput]) -> Vec<u8> {
    let text = |path: &Path| {
        path.to_str()
            .expect("输出路径在开始任务时校验过是 UTF-8")
            .to_owned()
    };
    let file = OutputsFile {
        format_version: FORMAT_VERSION,
        outputs: outputs
            .iter()
            .map(|o| OutputFile {
                kind: match o.kind {
                    OutputKind::Mp4 => KindFile::Mp4,
                    OutputKind::Hls => KindFile::Hls,
                },
                target: text(&o.target),
                temp: text(&o.temp),
                aside: text(&o.aside),
            })
            .collect(),
    };
    serde_json::to_vec(&file).expect("OutputsFile 只含字符串、整数、数组与枚举，序列化不会失败")
}

/// 解析 `outputs.json`；失败时返回原因。
fn decode(bytes: &[u8]) -> Result<Vec<PendingOutput>, String> {
    check_version(bytes, OUTPUTS_FILE)?;
    let file: OutputsFile =
        serde_json::from_slice(bytes).map_err(|e| format!("{OUTPUTS_FILE} 无法解析：{e}"))?;
    Ok(file
        .outputs
        .into_iter()
        .map(|o| PendingOutput {
            kind: match o.kind {
                KindFile::Mp4 => OutputKind::Mp4,
                KindFile::Hls => OutputKind::Hls,
            },
            target: o.target.into(),
            temp: o.temp.into(),
            aside: o.aside.into(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs_file_round_trips_and_rejects_other_versions_and_fields() {
        let outputs = vec![
            PendingOutput {
                kind: OutputKind::Mp4,
                target: "/d/a.mp4".into(),
                temp: "/d/hsdl-0123456789abcdef.mp4.part".into(),
                aside: "/d/hsdl-0123456789abcdef.mp4.old".into(),
            },
            PendingOutput {
                kind: OutputKind::Hls,
                target: "/d/a".into(),
                temp: "/d/hsdl-0123456789abcdef.hls.part".into(),
                aside: "/d/hsdl-0123456789abcdef.hls.old".into(),
            },
        ];
        let bytes = encode(&outputs);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"format_version":6,"outputs":[{"kind":"mp4","target":"/d/a.mp4","temp":"/d/hsdl-0123456789abcdef.mp4.part","aside":"/d/hsdl-0123456789abcdef.mp4.old"},{"kind":"hls","target":"/d/a","temp":"/d/hsdl-0123456789abcdef.hls.part","aside":"/d/hsdl-0123456789abcdef.hls.old"}]}"#
        );
        assert_eq!(decode(&bytes), Ok(outputs));

        for rejected in [
            r#"[{"target":"/d/a.mp4","temp":"/d/t","aside":"/d/o","dir":false}]"#,
            r#"{"format_version":5,"outputs":[]}"#,
            r#"{"format_version":6,"outputs":[],"extra":1}"#,
            r#"{"format_version":6,"outputs":[{"kind":"mkv","target":"/d/a","temp":"/d/t","aside":"/d/o"}]}"#,
            r#"{"format_version":6,"outputs":[{"kind":"mp4","target":"/d/a","temp":"/d/t"}]}"#,
        ] {
            assert!(decode(rejected.as_bytes()).is_err(), "{rejected}");
        }
    }
}
