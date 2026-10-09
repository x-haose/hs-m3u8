//! `job.json`：任务目录记录的任务，及其序列化格式（契约）。

use std::path::Path;

use hs_m3u8_hls::Resolution;
use hs_m3u8_remux::Streams;
use serde::{Deserialize, Serialize};

use crate::selection::{RenditionKey, SelectionKey, VariantKey};
use crate::{Error, JobType, WorkDirProblem};

const FORMAT_VERSION: u32 = 2;

/// 任务目录记录的任务；续传、续录时须与当前请求相符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobRecord {
    /// 来源摘要，见 [`crate::ident::source_digest`]
    pub source: String,
    /// 所选的变体与音频；来源本身是媒体播放列表时为 None
    pub selection: Option<SelectionKey>,
    /// 各轨的取流方式，至少一条
    pub streams: Vec<Streams>,
    pub kind: RecordKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// 点播：计划摘要相同才能续传
    Vod { plan: String },
    /// 直播：`url` 为完整来源地址（含查询串）的摘要。地址变了仍可沿用目录，由录制确认窗口与已录内容衔接后
    /// 再用 [`WorkDir::save`] 更新
    Live { url: String },
}

/// 记录与当前请求不符之处。
pub(super) enum Conflict {
    Source,
    Kind { recorded: JobType, current: JobType },
    Tracks,
    Plan,
}

impl JobRecord {
    fn job_type(&self) -> JobType {
        match self.kind {
            RecordKind::Vod { .. } => JobType::Vod,
            RecordKind::Live { .. } => JobType::Live,
        }
    }

    /// 记录为 `self` 的目录能否用于当前请求 `current`；直播的完整地址不比较。
    pub(super) fn conflict(&self, current: &JobRecord) -> Option<Conflict> {
        if self.source != current.source {
            return Some(Conflict::Source);
        }
        let (recorded, current_type) = (self.job_type(), current.job_type());
        if recorded != current_type {
            return Some(Conflict::Kind {
                recorded,
                current: current_type,
            });
        }
        if self.selection != current.selection || self.streams != current.streams {
            return Some(Conflict::Tracks);
        }
        match (&self.kind, &current.kind) {
            (RecordKind::Vod { plan: a }, RecordKind::Vod { plan: b }) if a != b => {
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
        source: String,
        selection: Option<SelectionFile>,
        streams: Vec<StreamsName>,
        plan: String,
    },
    Live {
        format_version: u32,
        source: String,
        selection: Option<SelectionFile>,
        streams: Vec<StreamsName>,
        url: String,
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
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenditionFile {
    group_id: String,
    language: Option<String>,
    name: Option<String>,
}

/// [`Streams`] 在 job.json 中的写法。
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StreamsName {
    All,
    Video,
    Audio,
}

/// 先只读版本号：不认识的版本直接拒绝，不按当前格式去解释它。
#[derive(Deserialize)]
struct Version {
    format_version: u32,
}

pub(super) fn encode(record: &JobRecord) -> Vec<u8> {
    let format_version = FORMAT_VERSION;
    let source = record.source.clone();
    let selection = record.selection.as_ref().map(SelectionFile::from);
    let streams = record.streams.iter().map(|&s| s.into()).collect();
    let file = match &record.kind {
        RecordKind::Vod { plan } => JobFile::Vod {
            format_version,
            source,
            selection,
            streams,
            plan: plan.clone(),
        },
        RecordKind::Live { url } => JobFile::Live {
            format_version,
            source,
            selection,
            streams,
            url: url.clone(),
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
    let (source, selection, streams, kind) =
        match serde_json::from_slice(bytes).map_err(unreadable)? {
            JobFile::Vod {
                source,
                selection,
                streams,
                plan,
                ..
            } => (source, selection, streams, RecordKind::Vod { plan }),
            JobFile::Live {
                source,
                selection,
                streams,
                url,
                ..
            } => (source, selection, streams, RecordKind::Live { url }),
        };
    if streams.is_empty() {
        return Err("job.json 中的 streams 为空".into());
    }
    Ok(JobRecord {
        source,
        selection: selection.map(SelectionKey::from),
        streams: streams.into_iter().map(Streams::from).collect(),
        kind,
    })
}

impl From<&SelectionKey> for SelectionFile {
    fn from(key: &SelectionKey) -> Self {
        let v = &key.variant;
        SelectionFile {
            variant: VariantFile {
                bandwidth: v.bandwidth,
                resolution: v.resolution.map(|r| [r.width, r.height]),
                codecs: v.codecs.clone(),
                audio_group: v.audio_group.clone(),
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
                bandwidth: v.bandwidth,
                resolution: v
                    .resolution
                    .map(|[width, height]| Resolution { width, height }),
                codecs: v.codecs,
                audio_group: v.audio_group,
            },
            audio: file.audio.map(|a| RenditionKey {
                group_id: a.group_id,
                language: a.language,
                name: a.name,
            }),
        }
    }
}

impl From<Streams> for StreamsName {
    fn from(streams: Streams) -> Self {
        match streams {
            Streams::All => StreamsName::All,
            Streams::Video => StreamsName::Video,
            Streams::Audio => StreamsName::Audio,
        }
    }
}

impl From<StreamsName> for Streams {
    fn from(name: StreamsName) -> Self {
        match name {
            StreamsName::All => Streams::All,
            StreamsName::Video => Streams::Video,
            StreamsName::Audio => Streams::Audio,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_file_round_trips_and_rejects_other_versions_and_fields() {
        let live = JobRecord {
            source: "ab".into(),
            selection: Some(SelectionKey {
                variant: VariantKey {
                    bandwidth: Some(2000),
                    resolution: Some(Resolution {
                        width: 1280,
                        height: 720,
                    }),
                    codecs: vec!["avc1.640020".into()],
                    audio_group: Some("aud".into()),
                },
                audio: Some(RenditionKey {
                    group_id: "aud".into(),
                    language: Some("en".into()),
                    name: None,
                }),
            }),
            streams: vec![Streams::Video, Streams::Audio],
            kind: RecordKind::Live { url: "cd".into() },
        };
        let bytes = encode(&live);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"kind":"live","format_version":2,"source":"ab","selection":{"variant":{"bandwidth":2000,"resolution":[1280,720],"codecs":["avc1.640020"],"audio_group":"aud"},"audio":{"group_id":"aud","language":"en","name":null}},"streams":["video","audio"],"url":"cd"}"#
        );
        assert_eq!(decode(&bytes), Ok(live));
        let vod = JobRecord {
            source: "ab".into(),
            selection: None,
            streams: vec![Streams::All],
            kind: RecordKind::Vod { plan: "ef".into() },
        };
        assert_eq!(decode(&encode(&vod)), Ok(vod));

        for rejected in [
            r#"{"kind":"vod","format_version":1,"plan_digest":"cd"}"#,
            r#"{"kind":"vod","format_version":2,"source":"a","selection":null,"streams":["all"],"plan":"p","extra":1}"#,
            r#"{"format_version":2,"source":"a","selection":null,"streams":["all"],"plan":"p"}"#,
            r#"{"kind":"vod","format_version":2,"source":"a","selection":null,"streams":[],"plan":"p"}"#,
            r#"{"kind":"vod","format_version":2,"source":"a","selection":null,"streams":["both"],"plan":"p"}"#,
            r#"{"kind":"live","format_version":2,"source":"a","selection":{"variant":{"bandwidth":1,"resolution":null,"codecs":[],"audio_group":null,"uri":"x"},"audio":null},"streams":["all"],"url":"u"}"#,
        ] {
            assert!(decode(rejected.as_bytes()).is_err(), "{rejected}");
        }
    }
}
