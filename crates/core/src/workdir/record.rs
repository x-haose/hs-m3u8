//! `job.json`：任务目录记录的任务，及其序列化格式（契约）。

use std::path::Path;

use hs_m3u8_hls::Resolution;
use hs_m3u8_remux::Streams;
use serde::{Deserialize, Serialize};

use crate::selection::{RenditionKey, SelectionKey, VariantAttributes, VariantKey, track_streams};
use crate::{Error, JobType, WorkDirProblem};

const FORMAT_VERSION: u32 = 3;

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
    /// 再用 [`super::WorkDir::save`] 更新
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

/// `job.json` 的格式（序列化契约）。`format_version` 不等于 [`FORMAT_VERSION`] 时拒绝。
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
    audio: Option<RenditionFile>,
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
struct RenditionFile {
    group_id: String,
    language: Option<String>,
    name: Option<String>,
}

/// 先只读版本号：不认识的版本直接拒绝，不按当前格式去解释它。
#[derive(Deserialize)]
struct Version {
    format_version: u32,
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

/// 解析 `job.json`；失败时返回原因。
pub(super) fn decode(bytes: &[u8]) -> Result<JobRecord, String> {
    let unreadable = |e: serde_json::Error| format!("job.json 无法解析：{e}");
    let version: Version = serde_json::from_slice(bytes).map_err(unreadable)?;
    if version.format_version != FORMAT_VERSION {
        return Err(format!(
            "job.json 的格式版本 {} 不受支持（支持 {FORMAT_VERSION}）",
            version.format_version
        ));
    }
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
        let v = &key.variant.attributes;
        SelectionFile {
            variant: VariantFile {
                bandwidth: v.bandwidth,
                resolution: v.resolution.map(|r| [r.width, r.height]),
                codecs: v.codecs.clone(),
                audio_group: v.audio_group.clone(),
                occurrence: key.variant.occurrence,
            },
            audio: key.audio.as_ref().map(|a| RenditionFile {
                group_id: a.group_id.clone(),
                language: a.language.clone(),
                name: a.name.clone(),
            }),
        }
    }
}

impl From<SelectionFile> for SelectionKey {
    fn from(file: SelectionFile) -> Self {
        let v = file.variant;
        SelectionKey {
            variant: VariantKey {
                attributes: VariantAttributes {
                    bandwidth: v.bandwidth,
                    resolution: v
                        .resolution
                        .map(|[width, height]| Resolution { width, height }),
                    codecs: v.codecs,
                    audio_group: v.audio_group,
                },
                occurrence: v.occurrence,
            },
            audio: file.audio.map(|a| RenditionKey {
                group_id: a.group_id,
                language: a.language,
                name: a.name,
            }),
        }
    }
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
                    attributes: VariantAttributes {
                        bandwidth: Some(2000),
                        resolution: Some(Resolution {
                            width: 1280,
                            height: 720,
                        }),
                        codecs: vec!["avc1.640020".into()],
                        audio_group: Some("aud".into()),
                    },
                    occurrence: 1,
                },
                audio: Some(RenditionKey {
                    group_id: "aud".into(),
                    language: Some("en".into()),
                    name: None,
                }),
            }),
            kind: RecordKind::Live {
                url_digest: "cd".into(),
            },
        };
        let bytes = encode(&live);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"kind":"live","format_version":3,"source_digest":"ab","selection":{"variant":{"bandwidth":2000,"resolution":[1280,720],"codecs":["avc1.640020"],"audio_group":"aud","occurrence":1},"audio":{"group_id":"aud","language":"en","name":null}},"url_digest":"cd"}"#
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

        for rejected in [
            r#"{"kind":"vod","format_version":1,"plan_digest":"cd"}"#,
            r#"{"kind":"vod","format_version":2,"source_digest":"a","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"vod","format_version":3,"source_digest":"a","selection":null,"plan_digest":"p","extra":1}"#,
            r#"{"format_version":3,"source_digest":"a","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"vod","format_version":3,"source_digest":"a","selection":null,"url_digest":"u"}"#,
            r#"{"kind":"live","format_version":3,"source_digest":"a","selection":{"variant":{"bandwidth":1,"resolution":null,"codecs":[],"audio_group":null,"occurrence":0,"uri":"x"},"audio":null},"url_digest":"u"}"#,
        ] {
            assert!(decode(rejected.as_bytes()).is_err(), "{rejected}");
        }
    }
}
