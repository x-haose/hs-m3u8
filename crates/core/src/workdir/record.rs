//! `job.json`：任务目录记录的任务（HLS 下载、录制或单个文件），及其序列化格式与其中摘要的编码（契约）。

use hs_m3u8_hls::{AudioChoice, AudioKey, Preference, Resolution, VariantChoice, VariantKey};
use hs_m3u8_remux::Streams;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::ident::digest_hex;
use crate::selection::{AudioRecord, SelectionRecord, VariantRecord, track_streams};
use crate::{JobType, WorkDirProblem};

/// job.json 的格式版本，也是任务目录里其余内容的格式版本：job.json 的字段，分片与 init 段的文件名、其中指纹与摘要的
/// 编码，文件任务的数据文件与已完成范围的记录（`ranges.json`），写到一半的文件的 `.part` 后缀，以及下载时对文件内容
/// 的改写（fMP4 长度写 0 的最后一个 box 改为实际长度）。任何一项改变都要升。outputs.json 另有自己的版本（见
/// [`super::outputs::OUTPUTS_FORMAT_VERSION`]）。
///
/// 各版本：13 为 1.0 写的；14 加了文件任务（job.json 的 `file` 一种与 `ranges.json`），HLS 任务的内容与 13 相同。
///
/// 兼容约定（outputs.json 同）：此后各版本的库读得了 1.0 起各发布版本写的任务目录（[`OLDEST_JOB_FORMAT_VERSION`] 起），
/// 按记录的格式版本解释其中的内容。格式版本比本库写的新，或比它旧（1.0 之前开发中的版本写的）时报
/// [`WorkDirProblem::UnsupportedVersion`]。各已发布版本的样本在 tests/fixtures/workdir 下，发布后不再改动。
pub(super) const JOB_FORMAT_VERSION: u32 = 14;

/// 读得了的最旧的 job.json 格式版本：1.0 写的。
const OLDEST_JOB_FORMAT_VERSION: u32 = 13;

/// 加了文件任务的 job.json 格式版本。
pub(super) const FILE_JOB_FORMAT_VERSION: u32 = 14;

/// job.json 里记录的任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Recorded {
    Hls(Box<HlsRecord>),
    File(FileRecord),
}

impl Recorded {
    pub(crate) fn job_type(&self) -> JobType {
        match self {
            Recorded::Hls(record) => record.job_type(),
            Recorded::File(_) => JobType::File,
        }
    }
}

/// 文件任务记录的来源：续传时据此核对是不是同一个文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRecord {
    /// 见 [`url_digest`]
    pub url_digest: String,
    /// 总字节数；不知道时为 None，这时没有已完成范围的记录，不续传
    pub length: Option<u64>,
    /// 强 ETag（带引号，原样）；服务器没给或给的是弱 ETag 时为 None
    pub etag: Option<String>,
    /// Last-Modified 的值，原样
    pub last_modified: Option<String>,
}

/// HLS 任务目录记录的任务；续传、续录时须与当前请求相符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HlsRecord {
    /// 见 [`preference_digest`]；来源是媒体播放列表（`selection` 为 None）时选轨偏好不起作用，不核对
    pub preference_digest: String,
    /// 见 [`url_digest`]。地址变了仍可沿用目录：核对过这次取到的与已下载的是同一份内容之后，用
    /// [`super::WorkDir::adopt_record`] 改记
    pub url_digest: String,
    /// 所选的变体与音频；来源本身是媒体播放列表时为 None
    pub selection: Option<SelectionRecord>,
    pub kind: RecordKind,
}

/// 打开任务目录时怎样比较本次所选的轨与记录的（见 [`super::WorkDir::open`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionCheck {
    /// 选轨偏好与所选轨的身份都须与记录的相同：续传、只合并
    SameTracks,
    /// 只要求轨道结构（各轨的取流方式）相同：按这次的偏好重新选轨（见 [`crate::JobRequest::reselect`]），身份不同的轨核
    /// 对过内容才改记
    SameLayout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// 点播：计划摘要（见 [`crate::vod::Plan::digest`]）相同才能续传
    Vod {
        plan_digest: String,
    },
    Live,
}

impl HlsRecord {
    /// 各轨的取流方式，由选轨决定。
    pub(crate) fn streams(&self) -> Vec<Streams> {
        track_streams(self.selection.as_ref())
    }

    pub(crate) fn job_type(&self) -> JobType {
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

    /// 与 `current` 相比，各轨所选的是否不是同一条，按轨道顺序：第 0 条比所选变体（来源本身是媒体播放列表时为没有，同一
    /// 条见 [`VariantRecord::same_as`]），第 1 条（若有）比独立的音频（见 [`AudioRecord::same_as`]）。两者轨道结构（见
    /// [`HlsRecord::streams`]）相同时才有意义。
    pub(super) fn changed_tracks(&self, current: &HlsRecord) -> Vec<bool> {
        let (recorded, current) = (self.selection.as_ref(), current.selection.as_ref());
        let variant = match (recorded, current) {
            (Some(recorded), Some(current)) => !recorded.variant.same_as(&current.variant),
            (recorded, current) => recorded.is_some() != current.is_some(),
        };
        let mut changed = vec![variant];
        if self.streams().len() > 1 {
            let audio = |s: Option<&SelectionRecord>| s.and_then(|s| s.audio.clone());
            changed.push(match (audio(recorded), audio(current)) {
                (Some(recorded), Some(current)) => !recorded.same_as(&current),
                (recorded, current) => recorded.is_some() != current.is_some(),
            });
        }
        changed
    }

    /// `self` 里与 `recorded` 是同一条（见 [`HlsRecord::changed_tracks`]）的轨，属性换成 `recorded` 里的：同一条轨沿用
    /// 首次记下的属性（见 [`SelectionRecord`]）。
    pub(super) fn keeping_attributes_of(mut self, recorded: &HlsRecord) -> HlsRecord {
        if let (Some(current), Some(recorded)) = (&mut self.selection, &recorded.selection) {
            if recorded.variant.same_as(&current.variant) {
                current.variant = recorded.variant.clone();
            }
            if let (Some(current), Some(recorded)) = (&mut current.audio, &recorded.audio)
                && recorded.same_as(current)
            {
                *current = recorded.clone();
            }
        }
        self
    }

    /// 记录为 `self` 的目录不能用于当前请求 `current` 的原因；地址不比较（见 `url_digest`），所选的轨按 `check` 比较。
    pub(super) fn conflict(
        &self,
        current: &HlsRecord,
        check: SelectionCheck,
    ) -> Option<WorkDirProblem> {
        if check == SelectionCheck::SameTracks
            && let Some(problem) = self.preference_conflict(&current.preference_digest)
        {
            return Some(problem);
        }
        let (recorded, current_type) = (self.job_type(), current.job_type());
        if recorded != current_type {
            return Some(WorkDirProblem::KindMismatch {
                recorded,
                current: current_type,
            });
        }
        let same_tracks = self.streams() == current.streams()
            && (check == SelectionCheck::SameLayout
                || !self.changed_tracks(current).contains(&true));
        if !same_tracks {
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

/// `job.json` 的写法。`format_version` 不等于 [`JOB_FORMAT_VERSION`] 时拒绝。
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
    /// 格式版本 14 起
    File {
        format_version: u32,
        url_digest: String,
        length: Option<u64>,
        etag: Option<String>,
        last_modified: Option<String>,
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
        audio_group: Option<String>,
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

/// 任务目录里名为 `file` 的 JSON 文件的格式版本在不在 `oldest..=supported` 之内，在时返回它：读不出版本号为内容无法
/// 识别，不在为版本不受支持。
pub(super) fn check_version(
    bytes: &[u8],
    file: &'static str,
    oldest: u32,
    supported: u32,
) -> Result<u32, WorkDirProblem> {
    let version: Version = serde_json::from_slice(bytes)
        .map_err(|e| WorkDirProblem::Corrupt(format!("{file} 无法解析：{e}")))?;
    if !(oldest..=supported).contains(&version.format_version) {
        return Err(WorkDirProblem::UnsupportedVersion {
            file,
            found: version.format_version,
            supported,
        });
    }
    Ok(version.format_version)
}

pub(super) fn encode_hls(record: &HlsRecord) -> Vec<u8> {
    let format_version = JOB_FORMAT_VERSION;
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

pub(super) fn encode_file(record: &FileRecord) -> Vec<u8> {
    let file = JobFile::File {
        format_version: JOB_FORMAT_VERSION,
        url_digest: record.url_digest.clone(),
        length: record.length,
        etag: record.etag.clone(),
        last_modified: record.last_modified.clone(),
    };
    serde_json::to_vec(&file).expect("JobFile 只含字符串、整数、数组与枚举，序列化不会失败")
}

/// 解析 `job.json`。
pub(super) fn decode(bytes: &[u8]) -> Result<Recorded, WorkDirProblem> {
    let unreadable = |e| WorkDirProblem::Corrupt(format!("job.json 无法解析：{e}"));
    let version = check_version(
        bytes,
        "job.json",
        OLDEST_JOB_FORMAT_VERSION,
        JOB_FORMAT_VERSION,
    )?;
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
            JobFile::File { .. } if version < FILE_JOB_FORMAT_VERSION => {
                return Err(WorkDirProblem::Corrupt(format!(
                    "job.json 是格式版本 {version}，那时还没有文件任务"
                )));
            }
            JobFile::File {
                url_digest,
                length,
                etag,
                last_modified,
                ..
            } => {
                return Ok(Recorded::File(FileRecord {
                    url_digest,
                    length,
                    etag,
                    last_modified,
                }));
            }
        };
    Ok(Recorded::Hls(Box::new(HlsRecord {
        preference_digest,
        url_digest,
        selection: selection.map(SelectionRecord::from),
        kind,
    })))
}

impl From<&SelectionRecord> for SelectionFile {
    fn from(record: &SelectionRecord) -> Self {
        let variant = &record.variant;
        SelectionFile {
            variant: VariantFile {
                key: VariantKeyFile::from(&variant.key),
                bandwidth: variant.bandwidth,
                resolution: variant.resolution.map(resolution_to_file),
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
                resolution: variant.resolution.map(resolution_from_file),
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
            VariantKey::Stable {
                id,
                audio_group,
                occurrence,
            } => VariantKeyFile::Stable {
                id,
                audio_group,
                occurrence,
            },
            VariantKey::Attributes {
                bandwidth,
                resolution,
                codecs,
                audio_group,
                occurrence,
            } => VariantKeyFile::Attributes {
                bandwidth,
                resolution: resolution.map(resolution_to_file),
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
            VariantKeyFile::Stable {
                id,
                audio_group,
                occurrence,
            } => VariantKey::Stable {
                id,
                audio_group,
                occurrence,
            },
            VariantKeyFile::Attributes {
                bandwidth,
                resolution,
                codecs,
                audio_group,
                occurrence,
            } => VariantKey::Attributes {
                bandwidth,
                resolution: resolution.map(resolution_from_file),
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
fn resolution_to_file(r: Resolution) -> [u32; 2] {
    [r.width, r.height]
}

fn resolution_from_file([width, height]: [u32; 2]) -> Resolution {
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
            audio_group: Some("B".into()),
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
        let live = HlsRecord {
            preference_digest: "ab".into(),
            url_digest: "cd".into(),
            selection: Some(selection(variant_key(), stable_audio_key())),
            kind: RecordKind::Live,
        };
        let bytes = encode_hls(&live);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"kind":"live","format_version":14,"preference_digest":"ab","url_digest":"cd","selection":{"variant":{"key":{"attributes":{"bandwidth":2000,"resolution":[1280,720],"codecs":["avc1.640020","mp4a.40.2"],"audio_group":"aud","occurrence":1}},"bandwidth":2000,"resolution":[1280,720],"codecs":["avc1.640020"]},"audio":{"key":{"stable":{"id":"Audio-37262","occurrence":0}},"name":"Eng\n\"lish\"","language":"en"}}}"#
        );
        assert_eq!(decode(&bytes), Ok(Recorded::Hls(Box::new(live.clone()))));
        let stable = HlsRecord {
            selection: Some(selection(stable_variant_key(), audio_key())),
            ..live
        };
        assert_eq!(
            decode(&encode_hls(&stable)),
            Ok(Recorded::Hls(Box::new(stable)))
        );
        let vod = HlsRecord {
            preference_digest: "ab".into(),
            url_digest: "cd".into(),
            selection: None,
            kind: RecordKind::Vod {
                plan_digest: "ef".into(),
            },
        };
        assert_eq!(decode(&encode_hls(&vod)), Ok(Recorded::Hls(Box::new(vod))));
        let file = FileRecord {
            url_digest: "cd".into(),
            length: Some(1 << 40),
            etag: Some("\"x\"".into()),
            last_modified: None,
        };
        let bytes = encode_file(&file);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"kind":"file","format_version":14,"url_digest":"cd","length":1099511627776,"etag":"\"x\"","last_modified":null}"#
        );
        assert_eq!(decode(&bytes), Ok(Recorded::File(file)));
        // 1.0 写的格式版本 13 照样读
        let v13 = r#"{"kind":"vod","format_version":13,"preference_digest":"a","url_digest":"u","selection":null,"plan_digest":"p"}"#;
        assert!(matches!(decode(v13.as_bytes()), Ok(Recorded::Hls(_))));

        for found in [12, 15] {
            assert_eq!(
                decode(format!(r#"{{"kind":"vod","format_version":{found}}}"#).as_bytes()),
                Err(WorkDirProblem::UnsupportedVersion {
                    file: "job.json",
                    found,
                    supported: 14
                })
            );
        }
        for rejected in [
            r#"{"kind":"vod","format_version":1,"plan_digest":"cd"}"#,
            r#"{"kind":"vod","format_version":9,"address_digest":"a","preference_digest":"b","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"vod","format_version":13,"preference_digest":"a","url_digest":"u","selection":null,"plan_digest":"p","extra":1}"#,
            r#"{"kind":"file","format_version":13,"url_digest":"u","length":1,"etag":null,"last_modified":null}"#,
            r#"{"kind":"file","format_version":14,"url_digest":"u","length":-1,"etag":null,"last_modified":null}"#,
            r#"{"format_version":13,"preference_digest":"a","url_digest":"u","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"vod","format_version":13,"preference_digest":"a","url_digest":"u","selection":null}"#,
            r#"{"kind":"vod","format_version":13,"preference_digest":"a","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"live","format_version":13,"preference_digest":"a","url_digest":"u","selection":{"variant":{"key":{"stable":{"id":"x","audio_group":null,"occurrence":0,"uri":"x"}},"bandwidth":null,"resolution":null,"codecs":[]},"audio":null}}"#,
            r#"{"kind":"live","format_version":13,"preference_digest":"a","url_digest":"u","selection":{"variant":{"key":{"uri":{"id":"x","occurrence":0}},"bandwidth":null,"resolution":null,"codecs":[]},"audio":null}}"#,
        ] {
            assert!(decode(rejected.as_bytes()).is_err(), "{rejected}");
        }
    }

    /// 重新选轨时选轨偏好与所选的轨不比较，只比轨道结构（各轨的取流方式）；不重新选轨时各报各的不同。
    #[test]
    fn reselecting_compares_only_the_track_layout() {
        let recorded = HlsRecord {
            preference_digest: "a".into(),
            url_digest: "u".into(),
            selection: Some(selection(variant_key(), audio_key())),
            kind: RecordKind::Live,
        };
        let other = HlsRecord {
            preference_digest: "b".into(),
            selection: Some(selection(stable_variant_key(), stable_audio_key())),
            ..recorded.clone()
        };
        let other_tracks = HlsRecord {
            preference_digest: "a".into(),
            ..other.clone()
        };
        assert_eq!(
            recorded.conflict(&other, SelectionCheck::SameTracks),
            Some(WorkDirProblem::PreferenceChanged)
        );
        assert_eq!(
            recorded.conflict(&other_tracks, SelectionCheck::SameTracks),
            Some(WorkDirProblem::TracksMismatch)
        );
        assert_eq!(recorded.conflict(&other, SelectionCheck::SameLayout), None);

        let muxed_audio = HlsRecord {
            selection: Some(SelectionRecord {
                audio: None,
                ..selection(variant_key(), audio_key())
            }),
            ..recorded.clone()
        };
        let media = HlsRecord {
            selection: None,
            ..recorded.clone()
        };
        for (a, b) in [(&recorded, &muxed_audio), (&recorded, &media)] {
            assert_eq!(
                a.conflict(b, SelectionCheck::SameLayout),
                Some(WorkDirProblem::TracksMismatch)
            );
        }
        assert_eq!(
            muxed_audio.conflict(&media, SelectionCheck::SameLayout),
            None
        );
        assert_eq!(
            muxed_audio.conflict(&media, SelectionCheck::SameTracks),
            Some(WorkDirProblem::TracksMismatch)
        );
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
            "8a71248b99082cb5d404754aca25f314afbdc29d27c7902a1e4347e70ee38c82"
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

/// 已发布版本的样本（见 [`super::released`]）。
#[cfg(test)]
mod released {
    use super::*;
    use crate::workdir::released::{versions, visible_entries};

    /// 都读得了；当前版本的按当前写法重新写出逐字节相同，改了写法而没升版本时在这里失败。最新的样本须是当前版本的：
    /// 升了版本而没生成新版本的样本时在这里失败。
    #[test]
    fn samples_are_readable() {
        let versions = versions("job");
        for (version, dir) in &versions {
            for sample in visible_entries(dir) {
                let path = sample.join("work/job.json");
                let bytes = std::fs::read(&path).unwrap();
                let written: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(written["format_version"], *version, "{}", path.display());
                let record = decode(&bytes).unwrap_or_else(|e| panic!("{}：{e}", path.display()));
                if *version == JOB_FORMAT_VERSION {
                    let rewritten = match &record {
                        Recorded::Hls(record) => encode_hls(record),
                        Recorded::File(record) => encode_file(record),
                    };
                    assert_eq!(rewritten, bytes, "{}", path.display());
                }
            }
        }
        assert_eq!(
            versions.last().map(|(version, _)| *version),
            Some(JOB_FORMAT_VERSION),
            "缺少当前格式版本的样本，见 samples::write_samples"
        );
    }
}
