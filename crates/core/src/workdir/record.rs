//! `job.json`：任务目录记录的任务，及其序列化格式与其中摘要的编码（契约）。

use hs_m3u8_hls::{AudioChoice, AudioKey, Preference, Resolution, VariantChoice, VariantKey};
use hs_m3u8_remux::Streams;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::ident::digest_hex;
use crate::selection::{AudioRecord, SelectionRecord, VariantRecord, track_streams};
use crate::{JobType, WorkDirProblem};

/// 任务目录格式的版本：job.json 与 outputs.json 的字段，分片与 init 段的文件名、其中指纹与摘要的编码，以及下载时对
/// 文件内容的改写（fMP4 长度写 0 的最后一个 box 改为实际长度）。任何一项改变都要升；两个文件都带它，读到别的版本
/// 一律拒绝。
pub(super) const FORMAT_VERSION: u32 = 12;

/// 任务目录记录的任务；续传、续录时须与当前请求相符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobRecord {
    /// 见 [`preference_digest`]；来源是媒体播放列表（`selection` 为 None）时选轨偏好不起作用，不核对
    pub preference_digest: String,
    /// 见 [`url_digest`]。地址变了仍可沿用目录：核对过这次取到的与已下载的是同一份内容之后，用
    /// [`super::WorkDir::adopt_url`] 改记
    pub url_digest: String,
    /// 所选的变体与音频；来源本身是媒体播放列表时为 None
    pub selection: Option<SelectionRecord>,
    pub kind: RecordKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// 点播：计划摘要（见 [`crate::vod::Plan::digest`]）相同才能续传
    Vod {
        plan_digest: String,
    },
    Live,
}

impl JobRecord {
    /// 各轨的取流方式，由选轨决定。
    pub(crate) fn streams(&self) -> Vec<Streams> {
        track_streams(self.selection.as_ref())
    }

    fn job_type(&self) -> JobType {
        match self.kind {
            RecordKind::Vod { .. } => JobType::Vod,
            RecordKind::Live => JobType::Live,
        }
    }

    /// 选轨偏好的摘要为 `current` 的请求能否沿用这个目录：来源是主播放列表（记录了选轨）而偏好不同时为
    /// [`WorkDirProblem::PreferenceChanged`]。
    pub(crate) fn preference_conflict(&self, current: &str) -> Option<WorkDirProblem> {
        (self.selection.is_some() && self.preference_digest != current)
            .then_some(WorkDirProblem::PreferenceChanged)
    }

    /// 记录为 `self` 的目录不能用于当前请求 `current` 的原因；地址不比较（见 `url_digest`）。
    pub(super) fn conflict(&self, current: &JobRecord) -> Option<WorkDirProblem> {
        if let Some(problem) = self.preference_conflict(&current.preference_digest) {
            return Some(problem);
        }
        let (recorded, current_type) = (self.job_type(), current.job_type());
        if recorded != current_type {
            return Some(WorkDirProblem::KindMismatch {
                recorded,
                current: current_type,
            });
        }
        if self.selection.as_ref().map(SelectionRecord::keys)
            != current.selection.as_ref().map(SelectionRecord::keys)
        {
            return Some(WorkDirProblem::TracksMismatch);
        }
        match (&self.kind, &current.kind) {
            (RecordKind::Vod { plan_digest: a }, RecordKind::Vod { plan_digest: b }) if a != b => {
                Some(WorkDirProblem::PlanChanged)
            }
            _ => None,
        }
    }
}

/// 选轨偏好的摘要（SHA-256 十六进制）。语言代码与选轨一样不区分 ASCII 大小写；按身份选的写成 job.json 里身份的写法
/// （JSON，其中换行已转义）。每项一行；语言里不会有换行（由 [`crate::Source`] 的校验保证），编码没有歧义。
pub(crate) fn preference_digest(preference: &Preference) -> String {
    let variant = match &preference.variant {
        VariantChoice::Best => "best".to_owned(),
        VariantChoice::Index(index) => format!("index {index}"),
        VariantChoice::Matching(key) => format!("matching {}", json(&VariantKeyFile::from(key))),
    };
    let audio = match &preference.audio {
        AudioChoice::Default => "default".to_owned(),
        AudioChoice::Language(language) => format!("language {}", language.to_ascii_lowercase()),
        AudioChoice::Index(index) => format!("index {index}"),
        AudioChoice::Matching(key) => format!("matching {}", json(&AudioKeyFile::from(key))),
    };
    digest_hex(format!("variant {variant}\naudio {audio}\n"))
}

/// job.json 里一项身份的 JSON。
fn json(file: &impl Serialize) -> String {
    serde_json::to_string(file).expect("只含数字、字符串与数组，序列化不会失败")
}

/// 完整来源地址（含用户名、密码、查询串与片段）的摘要（SHA-256 十六进制）。
pub(crate) fn url_digest(url: &Url) -> String {
    digest_hex(url.as_str())
}

/// `job.json` 的写法。`format_version` 不等于 [`FORMAT_VERSION`] 时拒绝。
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum JobFile {
    Vod {
        format_version: u32,
        preference_digest: String,
        url_digest: String,
        selection: Option<SelectionFile>,
        plan_digest: String,
    },
    Live {
        format_version: u32,
        preference_digest: String,
        url_digest: String,
        selection: Option<SelectionFile>,
    },
}

/// [`SelectionRecord`] 在 job.json 中的写法。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionFile {
    variant: VariantFile,
    audio: Option<AudioFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VariantFile {
    key: VariantKeyFile,
    bandwidth: Option<u64>,
    /// [宽, 高]
    resolution: Option<[u32; 2]>,
    codecs: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioFile {
    key: AudioKeyFile,
    name: Option<String>,
    language: Option<String>,
}

/// [`VariantKey`] 在 job.json 中的写法。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum VariantKeyFile {
    Stable {
        id: String,
        occurrence: usize,
    },
    Attributes {
        bandwidth: Option<u64>,
        /// [宽, 高]
        resolution: Option<[u32; 2]>,
        codecs: Vec<String>,
        audio_group: Option<String>,
        occurrence: usize,
    },
}

/// [`AudioKey`] 在 job.json 中的写法。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum AudioKeyFile {
    Stable {
        id: String,
        occurrence: usize,
    },
    Attributes {
        group: String,
        name: Option<String>,
        language: Option<String>,
        occurrence: usize,
    },
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
    let preference_digest = record.preference_digest.clone();
    let url_digest = record.url_digest.clone();
    let selection = record.selection.as_ref().map(SelectionFile::from);
    let file = match &record.kind {
        RecordKind::Vod { plan_digest } => JobFile::Vod {
            format_version,
            preference_digest,
            url_digest,
            selection,
            plan_digest: plan_digest.clone(),
        },
        RecordKind::Live => JobFile::Live {
            format_version,
            preference_digest,
            url_digest,
            selection,
        },
    };
    serde_json::to_vec(&file).expect("JobFile 只含字符串、整数、数组与枚举，序列化不会失败")
}

/// 解析 `job.json`。
pub(super) fn decode(bytes: &[u8]) -> Result<JobRecord, WorkDirProblem> {
    let unreadable = |e| WorkDirProblem::Corrupt(format!("job.json 无法解析：{e}"));
    check_version(bytes, "job.json")?;
    let (preference_digest, url_digest, selection, kind) =
        match serde_json::from_slice(bytes).map_err(unreadable)? {
            JobFile::Vod {
                preference_digest,
                url_digest,
                selection,
                plan_digest,
                ..
            } => (
                preference_digest,
                url_digest,
                selection,
                RecordKind::Vod { plan_digest },
            ),
            JobFile::Live {
                preference_digest,
                url_digest,
                selection,
                ..
            } => (preference_digest, url_digest, selection, RecordKind::Live),
        };
    Ok(JobRecord {
        preference_digest,
        url_digest,
        selection: selection.map(SelectionRecord::from),
        kind,
    })
}

impl From<&SelectionRecord> for SelectionFile {
    fn from(record: &SelectionRecord) -> Self {
        let variant = &record.variant;
        SelectionFile {
            variant: VariantFile {
                key: VariantKeyFile::from(&variant.key),
                bandwidth: variant.bandwidth,
                resolution: variant.resolution.map(resolution_file),
                codecs: variant.codecs.clone(),
            },
            audio: record.audio.as_ref().map(|audio| AudioFile {
                key: AudioKeyFile::from(&audio.key),
                name: audio.name.clone(),
                language: audio.language.clone(),
            }),
        }
    }
}

impl From<SelectionFile> for SelectionRecord {
    fn from(file: SelectionFile) -> Self {
        let variant = file.variant;
        SelectionRecord {
            variant: VariantRecord {
                key: variant.key.into(),
                bandwidth: variant.bandwidth,
                resolution: variant.resolution.map(resolution),
                codecs: variant.codecs,
            },
            audio: file.audio.map(|audio| AudioRecord {
                key: audio.key.into(),
                name: audio.name,
                language: audio.language,
            }),
        }
    }
}

impl From<&VariantKey> for VariantKeyFile {
    fn from(key: &VariantKey) -> Self {
        match key.clone() {
            VariantKey::Stable { id, occurrence } => VariantKeyFile::Stable { id, occurrence },
            VariantKey::Attributes {
                bandwidth,
                resolution: r,
                codecs,
                audio_group,
                occurrence,
            } => VariantKeyFile::Attributes {
                bandwidth,
                resolution: r.map(resolution_file),
                codecs,
                audio_group,
                occurrence,
            },
        }
    }
}

impl From<VariantKeyFile> for VariantKey {
    fn from(file: VariantKeyFile) -> Self {
        match file {
            VariantKeyFile::Stable { id, occurrence } => VariantKey::Stable { id, occurrence },
            VariantKeyFile::Attributes {
                bandwidth,
                resolution: r,
                codecs,
                audio_group,
                occurrence,
            } => VariantKey::Attributes {
                bandwidth,
                resolution: r.map(resolution),
                codecs,
                audio_group,
                occurrence,
            },
        }
    }
}

impl From<&AudioKey> for AudioKeyFile {
    fn from(key: &AudioKey) -> Self {
        match key.clone() {
            AudioKey::Stable { id, occurrence } => AudioKeyFile::Stable { id, occurrence },
            AudioKey::Attributes {
                group,
                name,
                language,
                occurrence,
            } => AudioKeyFile::Attributes {
                group,
                name,
                language,
                occurrence,
            },
        }
    }
}

impl From<AudioKeyFile> for AudioKey {
    fn from(file: AudioKeyFile) -> Self {
        match file {
            AudioKeyFile::Stable { id, occurrence } => AudioKey::Stable { id, occurrence },
            AudioKeyFile::Attributes {
                group,
                name,
                language,
                occurrence,
            } => AudioKey::Attributes {
                group,
                name,
                language,
                occurrence,
            },
        }
    }
}

/// 分辨率在 job.json 中写成 [宽, 高]。
fn resolution_file(r: Resolution) -> [u32; 2] {
    [r.width, r.height]
}

fn resolution([width, height]: [u32; 2]) -> Resolution {
    Resolution { width, height }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant_key() -> VariantKey {
        VariantKey::Attributes {
            bandwidth: Some(2000),
            resolution: Some(Resolution {
                width: 1280,
                height: 720,
            }),
            codecs: vec!["avc1.640020".into(), "mp4a.40.2".into()],
            audio_group: Some("aud".into()),
            occurrence: 1,
        }
    }

    fn audio_key() -> AudioKey {
        AudioKey::Attributes {
            group: "aud".into(),
            name: Some("Eng\n\"lish\"".into()),
            language: Some("en".into()),
            occurrence: 0,
        }
    }

    fn stable_variant_key() -> VariantKey {
        VariantKey::Stable {
            id: "Video-768".into(),
            occurrence: 1,
        }
    }

    fn stable_audio_key() -> AudioKey {
        AudioKey::Stable {
            id: "Audio-37262".into(),
            occurrence: 0,
        }
    }

    fn selection(variant: VariantKey, audio: AudioKey) -> SelectionRecord {
        SelectionRecord {
            variant: VariantRecord {
                key: variant,
                bandwidth: Some(2000),
                resolution: Some(Resolution {
                    width: 1280,
                    height: 720,
                }),
                codecs: vec!["avc1.640020".into()],
            },
            audio: Some(AudioRecord {
                key: audio,
                name: Some("Eng\n\"lish\"".into()),
                language: Some("en".into()),
            }),
        }
    }

    #[test]
    fn job_file_round_trips_and_rejects_other_versions_and_fields() {
        let live = JobRecord {
            preference_digest: "ab".into(),
            url_digest: "cd".into(),
            selection: Some(selection(variant_key(), stable_audio_key())),
            kind: RecordKind::Live,
        };
        let bytes = encode(&live);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"kind":"live","format_version":12,"preference_digest":"ab","url_digest":"cd","selection":{"variant":{"key":{"attributes":{"bandwidth":2000,"resolution":[1280,720],"codecs":["avc1.640020","mp4a.40.2"],"audio_group":"aud","occurrence":1}},"bandwidth":2000,"resolution":[1280,720],"codecs":["avc1.640020"]},"audio":{"key":{"stable":{"id":"Audio-37262","occurrence":0}},"name":"Eng\n\"lish\"","language":"en"}}}"#
        );
        assert_eq!(decode(&bytes), Ok(live));
        let stable = JobRecord {
            selection: Some(selection(stable_variant_key(), audio_key())),
            ..decode(&bytes).unwrap()
        };
        assert_eq!(decode(&encode(&stable)), Ok(stable));
        let vod = JobRecord {
            preference_digest: "ab".into(),
            url_digest: "cd".into(),
            selection: None,
            kind: RecordKind::Vod {
                plan_digest: "ef".into(),
            },
        };
        assert_eq!(decode(&encode(&vod)), Ok(vod));

        assert_eq!(
            decode(br#"{"kind":"vod","format_version":11}"#),
            Err(WorkDirProblem::UnsupportedVersion {
                file: "job.json",
                found: 11,
                supported: 12
            })
        );
        for rejected in [
            r#"{"kind":"vod","format_version":1,"plan_digest":"cd"}"#,
            r#"{"kind":"vod","format_version":9,"address_digest":"a","preference_digest":"b","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"vod","format_version":12,"preference_digest":"a","url_digest":"u","selection":null,"plan_digest":"p","extra":1}"#,
            r#"{"format_version":12,"preference_digest":"a","url_digest":"u","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"vod","format_version":12,"preference_digest":"a","url_digest":"u","selection":null}"#,
            r#"{"kind":"vod","format_version":12,"preference_digest":"a","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"live","format_version":12,"preference_digest":"a","url_digest":"u","selection":{"variant":{"key":{"stable":{"id":"x","occurrence":0,"uri":"x"}},"bandwidth":null,"resolution":null,"codecs":[]},"audio":null}}"#,
            r#"{"kind":"live","format_version":12,"preference_digest":"a","url_digest":"u","selection":{"variant":{"key":{"uri":{"id":"x","occurrence":0}},"bandwidth":null,"resolution":null,"codecs":[]},"audio":null}}"#,
        ] {
            assert!(decode(rejected.as_bytes()).is_err(), "{rejected}");
        }
    }

    /// 摘要写在 job.json 里，值变了即任务目录格式变了，须升格式版本。期望值是按各函数文档所述的编码，用另一个
    /// SHA-256 实现算出的。
    #[test]
    fn digests_are_fixed() {
        let best = Preference {
            variant: VariantChoice::Best,
            audio: AudioChoice::Language("EN".into()),
        };
        assert_eq!(
            preference_digest(&best),
            "aa17c2345a7d79ea2ddd8272b3b49122c3ec2f442df4591cac5b68ba9a56501c"
        );
        let index = Preference {
            variant: VariantChoice::Index(2),
            audio: AudioChoice::Default,
        };
        assert_eq!(
            preference_digest(&index),
            "9f08ffbf33ebf4575933b5c13a4429b0263dbb652e42f3396f8ffcd2d6a10c6e"
        );
        let matching = Preference {
            variant: VariantChoice::Matching(variant_key()),
            audio: AudioChoice::Matching(audio_key()),
        };
        assert_eq!(
            preference_digest(&matching),
            "8afe2f1e8436a3017dc5bcfe0f341f700ec1dc74d3572588a4ac0f5752644798"
        );
        let stable = Preference {
            variant: VariantChoice::Matching(stable_variant_key()),
            audio: AudioChoice::Matching(stable_audio_key()),
        };
        assert_eq!(
            preference_digest(&stable),
            "02b624dd66508e2ffd0183f2de17849dd9faae72a2e8f7c2b18810cec796b03f"
        );

        let url = Url::parse("https://a.example:8443/live.m3u8?token=1").unwrap();
        assert_eq!(
            url_digest(&url),
            "afd3a584486c3d8f1240bf03a547070da7f147f4a8dd68e49a419956c1b2e743"
        );
    }

    #[test]
    fn preference_digest_ignores_language_case_only() {
        let digest = |variant, audio| preference_digest(&Preference { variant, audio });
        let base = digest(VariantChoice::Best, AudioChoice::Language("EN".into()));
        assert_eq!(
            base,
            digest(VariantChoice::Best, AudioChoice::Language("en".into()))
        );
        for other in [
            digest(VariantChoice::Index(0), AudioChoice::Language("en".into())),
            digest(
                VariantChoice::Matching(variant_key()),
                AudioChoice::Language("en".into()),
            ),
            digest(VariantChoice::Best, AudioChoice::Default),
            digest(VariantChoice::Best, AudioChoice::Index(0)),
            digest(VariantChoice::Best, AudioChoice::Matching(audio_key())),
        ] {
            assert_ne!(base, other);
        }
    }
}
