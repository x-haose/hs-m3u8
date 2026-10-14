//! `job.json`：任务目录记录的任务，及其序列化格式（契约）。

use std::path::Path;

use hs_m3u8_hls::{AudioKey, Resolution, VariantKey};
use hs_m3u8_remux::Streams;
use serde::{Deserialize, Serialize};

use crate::selection::{SelectionKey, track_streams};
use crate::{Error, JobType, WorkDirProblem};

/// 任务目录格式的版本：job.json 与 outputs.json 的字段，分片与 init 段的文件名、其中指纹与摘要的编码，以及下载时对
/// 文件内容的改写（fMP4 长度写 0 的最后一个 box 改为实际长度）。任何一项改变都要升；两个文件都带它，读到别的版本
/// 一律拒绝。
pub(super) const FORMAT_VERSION: u32 = 8;

/// 任务目录记录的任务；续传、续录时须与当前请求相符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobRecord {
    /// 见 [`crate::ident::source_digest`]
    pub source_digest: String,
    /// 所选的变体与音频；来源本身是媒体播放列表时为 None
    pub selection: Option<SelectionKey>,
    pub kind: RecordKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// 点播：计划摘要（见 [`crate::vod::Plan::digest`]）相同才能续传
    Vod { plan_digest: String },
    /// 直播：完整来源地址（含查询串）的摘要。地址变了仍可沿用目录，由录制确认窗口与已录内容衔接后
    /// 再用 [`super::WorkDir::adopt_url`] 更新
    Live { url_digest: String },
}

/// 记录与当前请求不符之处。
pub(super) enum Conflict {
    Source,
    Kind { recorded: JobType, current: JobType },
    Tracks,
    Plan,
}

impl JobRecord {
    /// 各轨的取流方式，由选轨决定。
    pub(crate) fn streams(&self) -> Vec<Streams> {
        track_streams(self.selection.as_ref())
    }

    /// 直播记录的完整来源地址摘要；点播为 None。
    pub(super) fn url_digest(&self) -> Option<&str> {
        match &self.kind {
            RecordKind::Vod { .. } => None,
            RecordKind::Live { url_digest } => Some(url_digest),
        }
    }

    fn job_type(&self) -> JobType {
        match self.kind {
            RecordKind::Vod { .. } => JobType::Vod,
            RecordKind::Live { .. } => JobType::Live,
        }
    }

    /// 记录为 `self` 的目录能否用于当前请求 `current`；直播的完整地址不比较。
    pub(super) fn conflict(&self, current: &JobRecord) -> Option<Conflict> {
        if self.source_digest != current.source_digest {
            return Some(Conflict::Source);
        }
        let (recorded, current_type) = (self.job_type(), current.job_type());
        if recorded != current_type {
            return Some(Conflict::Kind {
                recorded,
                current: current_type,
            });
        }
        if self.selection != current.selection {
            return Some(Conflict::Tracks);
        }
        match (&self.kind, &current.kind) {
            (RecordKind::Vod { plan_digest: a }, RecordKind::Vod { plan_digest: b }) if a != b => {
                Some(Conflict::Plan)
            }
            _ => None,
        }
    }
}

impl Conflict {
    pub(super) fn into_error(self, root: &Path) -> Error {
        let problem = match self {
            Conflict::Plan => WorkDirProblem::PlanChanged,
            Conflict::Source => WorkDirProblem::SourceMismatch,
            Conflict::Kind { recorded, current } => {
                WorkDirProblem::KindMismatch { recorded, current }
            }
            Conflict::Tracks => WorkDirProblem::TracksMismatch,
        };
        Error::WorkDir {
            path: root.to_path_buf(),
            problem,
        }
    }
}

/// `job.json` 的写法。`format_version` 不等于 [`FORMAT_VERSION`] 时拒绝。
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum JobFile {
    Vod {
        format_version: u32,
        source_digest: String,
        selection: Option<SelectionFile>,
        plan_digest: String,
    },
    Live {
        format_version: u32,
        source_digest: String,
        selection: Option<SelectionFile>,
        url_digest: String,
    },
}

/// [`SelectionKey`] 在 job.json 中的写法。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionFile {
    variant: VariantFile,
    audio: Option<AudioFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VariantFile {
    bandwidth: Option<u64>,
    /// [宽, 高]
    resolution: Option<[u32; 2]>,
    codecs: Vec<String>,
    audio_group: Option<String>,
    occurrence: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioFile {
    group_id: String,
    language: Option<String>,
    name: Option<String>,
    occurrence: usize,
}

/// 先只读版本号：不认识的版本直接拒绝，不按当前格式去解释它。
#[derive(Deserialize)]
struct Version {
    format_version: u32,
}

/// 任务目录里名为 `file` 的 JSON 文件的格式版本是不是 [`FORMAT_VERSION`]：读不出版本号为内容无法识别，是别的
/// 版本为版本不受支持。
pub(super) fn check_version(bytes: &[u8], file: &'static str) -> Result<(), WorkDirProblem> {
    let version: Version = serde_json::from_slice(bytes)
        .map_err(|e| WorkDirProblem::Corrupt(format!("{file} 无法解析：{e}")))?;
    if version.format_version != FORMAT_VERSION {
        return Err(WorkDirProblem::UnsupportedVersion {
            file,
            found: version.format_version,
            supported: FORMAT_VERSION,
        });
    }
    Ok(())
}

pub(super) fn encode(record: &JobRecord) -> Vec<u8> {
    let format_version = FORMAT_VERSION;
    let source_digest = record.source_digest.clone();
    let selection = record.selection.as_ref().map(SelectionFile::from);
    let file = match &record.kind {
        RecordKind::Vod { plan_digest } => JobFile::Vod {
            format_version,
            source_digest,
            selection,
            plan_digest: plan_digest.clone(),
        },
        RecordKind::Live { url_digest } => JobFile::Live {
            format_version,
            source_digest,
            selection,
            url_digest: url_digest.clone(),
        },
    };
    serde_json::to_vec(&file).expect("JobFile 只含字符串、整数、数组与枚举，序列化不会失败")
}

/// 解析 `job.json`。
pub(super) fn decode(bytes: &[u8]) -> Result<JobRecord, WorkDirProblem> {
    let unreadable = |e| WorkDirProblem::Corrupt(format!("job.json 无法解析：{e}"));
    check_version(bytes, "job.json")?;
    let (source_digest, selection, kind) =
        match serde_json::from_slice(bytes).map_err(unreadable)? {
            JobFile::Vod {
                source_digest,
                selection,
                plan_digest,
                ..
            } => (source_digest, selection, RecordKind::Vod { plan_digest }),
            JobFile::Live {
                source_digest,
                selection,
                url_digest,
                ..
            } => (source_digest, selection, RecordKind::Live { url_digest }),
        };
    Ok(JobRecord {
        source_digest,
        selection: selection.map(SelectionKey::from),
        kind,
    })
}

impl From<&SelectionKey> for SelectionFile {
    fn from(key: &SelectionKey) -> Self {
        SelectionFile {
            variant: VariantFile::from(&key.variant),
            audio: key.audio.as_ref().map(AudioFile::from),
        }
    }
}

impl From<SelectionFile> for SelectionKey {
    fn from(file: SelectionFile) -> Self {
        SelectionKey {
            variant: file.variant.into(),
            audio: file.audio.map(AudioKey::from),
        }
    }
}

impl From<&VariantKey> for VariantFile {
    fn from(key: &VariantKey) -> Self {
        VariantFile {
            bandwidth: key.bandwidth,
            resolution: key.resolution.map(|r| [r.width, r.height]),
            codecs: key.codecs.clone(),
            audio_group: key.audio_group.clone(),
            occurrence: key.occurrence,
        }
    }
}

impl From<VariantFile> for VariantKey {
    fn from(file: VariantFile) -> Self {
        VariantKey {
            bandwidth: file.bandwidth,
            resolution: file
                .resolution
                .map(|[width, height]| Resolution { width, height }),
            codecs: file.codecs,
            audio_group: file.audio_group,
            occurrence: file.occurrence,
        }
    }
}

impl From<&AudioKey> for AudioFile {
    fn from(key: &AudioKey) -> Self {
        AudioFile {
            group_id: key.group_id.clone(),
            language: key.language.clone(),
            name: key.name.clone(),
            occurrence: key.occurrence,
        }
    }
}

impl From<AudioFile> for AudioKey {
    fn from(file: AudioFile) -> Self {
        AudioKey {
            group_id: file.group_id,
            language: file.language,
            name: file.name,
            occurrence: file.occurrence,
        }
    }
}

/// 变体身份在 job.json 里的写法（JSON，字段顺序固定），来源摘要也用它编码按身份选的偏好。
pub(crate) fn variant_json(key: &VariantKey) -> String {
    serde_json::to_string(&VariantFile::from(key)).expect("只含数字、字符串与数组，序列化不会失败")
}

/// 音频身份在 job.json 里的写法（JSON，字段顺序固定），来源摘要也用它编码按身份选的偏好。
pub(crate) fn audio_json(key: &AudioKey) -> String {
    serde_json::to_string(&AudioFile::from(key)).expect("只含数字、字符串与数组，序列化不会失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_file_round_trips_and_rejects_other_versions_and_fields() {
        let live = JobRecord {
            source_digest: "ab".into(),
            selection: Some(SelectionKey {
                variant: VariantKey {
                    bandwidth: Some(2000),
                    resolution: Some(Resolution {
                        width: 1280,
                        height: 720,
                    }),
                    codecs: vec!["avc1.640020".into()],
                    audio_group: Some("aud".into()),
                    occurrence: 1,
                },
                audio: Some(AudioKey {
                    group_id: "aud".into(),
                    language: Some("en".into()),
                    name: None,
                    occurrence: 2,
                }),
            }),
            kind: RecordKind::Live {
                url_digest: "cd".into(),
            },
        };
        let bytes = encode(&live);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"kind":"live","format_version":8,"source_digest":"ab","selection":{"variant":{"bandwidth":2000,"resolution":[1280,720],"codecs":["avc1.640020"],"audio_group":"aud","occurrence":1},"audio":{"group_id":"aud","language":"en","name":null,"occurrence":2}},"url_digest":"cd"}"#
        );
        assert_eq!(decode(&bytes), Ok(live));
        let vod = JobRecord {
            source_digest: "ab".into(),
            selection: None,
            kind: RecordKind::Vod {
                plan_digest: "ef".into(),
            },
        };
        assert_eq!(decode(&encode(&vod)), Ok(vod));

        assert_eq!(
            decode(br#"{"kind":"vod","format_version":9}"#),
            Err(WorkDirProblem::UnsupportedVersion {
                file: "job.json",
                found: 9,
                supported: 8
            })
        );
        for rejected in [
            r#"{"kind":"vod","format_version":1,"plan_digest":"cd"}"#,
            r#"{"kind":"vod","format_version":6,"source_digest":"a","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"vod","format_version":8,"source_digest":"a","selection":null,"plan_digest":"p","extra":1}"#,
            r#"{"format_version":8,"source_digest":"a","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"vod","format_version":8,"source_digest":"a","selection":null,"url_digest":"u"}"#,
            r#"{"kind":"live","format_version":8,"source_digest":"a","selection":{"variant":{"bandwidth":1,"resolution":null,"codecs":[],"audio_group":null,"occurrence":0,"uri":"x"},"audio":null},"url_digest":"u"}"#,
        ] {
            assert!(decode(rejected.as_bytes()).is_err(), "{rejected}");
        }
    }
}
