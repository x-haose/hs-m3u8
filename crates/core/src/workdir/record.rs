//! `job.json`：任务目录记录的任务（HLS 下载、录制或单个文件），及其序列化格式与其中摘要的编码（契约）。

use hs_m3u8_hls::{
    Preference, RenditionChoice, RenditionKey, Renditions, Resolution, VariantChoice, VariantKey,
};
use hs_m3u8_remux::Streams;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::ident::digest_hex;
use crate::selection::{RenditionRecord, SelectionRecord, VariantRecord, track_streams};
use crate::{JobType, WorkDirProblem};

/// job.json 的格式版本，也是任务目录里其余内容的格式版本：job.json 的字段，分片与 init 段的文件名、其中指纹与摘要的
/// 编码，文件任务的数据文件与已完成范围的记录（`ranges.json`），写到一半的文件的 `.part` 后缀，以及下载时对文件内容
/// 的改写（fMP4 长度写 0 的最后一个 box 改为实际长度）。任何一项改变都要升。outputs.json 另有自己的版本（见
/// [`super::outputs::OUTPUTS_FORMAT_VERSION`]）。
///
/// 各版本：13 为 1.0 写的；14 加了文件任务（job.json 的 `file` 一种与 `ranges.json`），HLS 任务的内容与 13 相同；15 的
/// 文件任务记录加了 `identity_digest`（14 的读作没有），其余与 14 相同；16 的选轨记录可有多条音频与字幕
/// （`selection` 加 `muxed_audio` 与 `subtitles`，`audio` 由单项改为列表，各项加 `default` 与 `forced`；15 及以前的读作
/// 音频 0 或 1 条、没有字幕、没有音频时用变体里混着的、标志都为假），任务目录里轨道按变体、各音频、各字幕排。
///
/// 兼容约定（outputs.json 同）：此后各版本的库读得了 1.0 起各发布版本写的任务目录（[`OLDEST_JOB_FORMAT_VERSION`] 起），
/// 按记录的格式版本解释其中的内容；下游已发布的程序按提交号依赖 core 时，那个提交写的格式也算已发布（14 即是）。格式
/// 版本比本库写的新，或比它旧（1.0 之前开发中的版本写的）时报 [`WorkDirProblem::UnsupportedVersion`]。各已发布版本的
/// 样本在 tests/fixtures/workdir 下，发布后不再改动。
pub(super) const JOB_FORMAT_VERSION: u32 = 16;

/// 读得了的最旧的 job.json 格式版本：1.0 写的。
const OLDEST_JOB_FORMAT_VERSION: u32 = 13;

/// 加了文件任务的 job.json 格式版本。
pub(super) const FILE_JOB_FORMAT_VERSION: u32 = 14;

/// 文件任务记录加了内容身份（`identity_digest`）的 job.json 格式版本。
const IDENTITY_JOB_FORMAT_VERSION: u32 = 15;

/// 选轨记录可有多条音频与字幕的 job.json 格式版本。
const TRACKS_JOB_FORMAT_VERSION: u32 = 16;

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
    /// 已下的内容的身份（调用方给的 [`crate::FileRequest::identity`]）的摘要，见 [`identity_digest`]：续传时没给身份的
    /// 沿用记录的；从未给过时为 None
    pub identity_digest: Option<String>,
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
    Vod { plan_digest: String },
    Live {
        /// 尾部没录到的字幕轨：（会话, 轨号），按会话、轨号排列、不重复（见 [`crate::UnrecordedTail`]）。每次录制结束时
        /// 按这个会话的结局改记；续录同一会话录到了的去掉
        tails: Vec<(u32, usize)>,
    },
}

impl HlsRecord {
    /// 各轨的取流方式，由选轨决定。
    pub(crate) fn streams(&self) -> Vec<Streams> {
        track_streams(self.selection.as_ref())
    }

    pub(crate) fn job_type(&self) -> JobType {
        match self.kind {
            RecordKind::Vod { .. } => JobType::Vod,
            RecordKind::Live { .. } => JobType::Live,
        }
    }

    /// 选轨偏好的摘要为 `current` 的请求能否沿用这个目录：来源是主播放列表（记录了选轨）而偏好不同时为
    /// [`WorkDirProblem::PreferenceChanged`]。
    pub(crate) fn preference_conflict(&self, current: &str) -> Option<WorkDirProblem> {
        (self.selection.is_some() && self.preference_digest != current)
            .then_some(WorkDirProblem::PreferenceChanged)
    }

    /// 与 `current` 相比，各轨所选的是否不是同一条，按轨道顺序：第 0 条比所选变体（来源本身是媒体播放列表时为没有，同一
    /// 条见 [`VariantRecord::same_as`]），其后各条依次比音频、字幕（见 [`RenditionRecord::same_as`]）。两者轨道结构（见
    /// [`HlsRecord::streams`]）相同时才有意义。
    pub(super) fn changed_tracks(&self, current: &HlsRecord) -> Vec<bool> {
        match (self.selection.as_ref(), current.selection.as_ref()) {
            (Some(recorded), Some(current)) => {
                std::iter::once(!recorded.variant.same_as(&current.variant))
                    .chain(
                        recorded
                            .renditions()
                            .zip(current.renditions())
                            .map(|(a, b)| !a.same_as(b)),
                    )
                    .collect()
            }
            (recorded, current) => vec![recorded.is_some() != current.is_some()],
        }
    }

    /// `self` 里与 `recorded` 是同一条（见 [`HlsRecord::changed_tracks`]）的轨，属性换成 `recorded` 里的：同一条轨沿用
    /// 首次记下的属性（见 [`SelectionRecord`]）；直播尾部没录到的字幕轨照 `recorded` 的（它描述已录到的内容）。两者轨道
    /// 结构相同时才调用。
    pub(super) fn keeping_attributes_of(mut self, recorded: &HlsRecord) -> HlsRecord {
        if let (Some(current), Some(recorded)) = (&mut self.selection, &recorded.selection) {
            if recorded.variant.same_as(&current.variant) {
                current.variant = recorded.variant.clone();
            }
            let pairs = current
                .audio
                .iter_mut()
                .zip(&recorded.audio)
                .chain(current.subtitles.iter_mut().zip(&recorded.subtitles));
            for (current, recorded) in pairs {
                if recorded.same_as(current) {
                    *current = recorded.clone();
                }
            }
        }
        if let (RecordKind::Live { tails }, RecordKind::Live { tails: recorded }) =
            (&mut self.kind, &recorded.kind)
        {
            tails.clone_from(recorded);
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
/// （JSON，其中换行已转义）。每项一行；语言里不会有换行（由 [`crate::Source`] 的校验保证），编码没有歧义：
/// - `variant <变体>`，再是音频：默认为 `audio default`，指定一条为 `audio <那一条>`（同 15 及以前只能选一条音频时的写法，
///   旧任务目录的摘要不变），其余为 `audio none`、`audio all`，或 `audio these <条数>` 后各条一行 `audio <那一条>`；
/// - 字幕不为不要时另起几行，写法同音频、前缀为 `subtitles`（只选一条也写 `subtitles these 1`）。
pub(crate) fn preference_digest(preference: &Preference) -> String {
    let variant = match &preference.variant {
        VariantChoice::Best => "best".to_owned(),
        VariantChoice::Index(index) => format!("index {index}"),
        VariantChoice::Matching(key) => format!("matching {}", json(&VariantKeyFile::from(key))),
    };
    let choice = |choice: &RenditionChoice| match choice {
        RenditionChoice::Language(language) => {
            format!("language {}", language.to_ascii_lowercase())
        }
        RenditionChoice::Index(index) => format!("index {index}"),
        RenditionChoice::Matching(key) => {
            format!("matching {}", json(&RenditionKeyFile::from(key)))
        }
    };
    let renditions = |prefix: &str, renditions: &Renditions, single: bool| match renditions {
        Renditions::None => format!("{prefix} none\n"),
        Renditions::Default => format!("{prefix} default\n"),
        Renditions::All => format!("{prefix} all\n"),
        Renditions::These(choices) if single && choices.len() == 1 => {
            format!("{prefix} {}\n", choice(&choices[0]))
        }
        Renditions::These(choices) => {
            let mut lines = format!("{prefix} these {}\n", choices.len());
            for c in choices {
                lines.push_str(&format!("{prefix} {}\n", choice(c)));
            }
            lines
        }
    };
    let mut text = format!("variant {variant}\n");
    text.push_str(&renditions("audio", &preference.audio, true));
    if preference.subtitles != Renditions::None {
        text.push_str(&renditions("subtitles", &preference.subtitles, false));
    }
    digest_hex(text)
}

/// job.json 里一项身份的 JSON。
fn json(file: &impl Serialize) -> String {
    serde_json::to_string(file).expect("只含数字、字符串与数组，序列化不会失败")
}

/// 完整来源地址（含用户名、密码、查询串与片段）的摘要（SHA-256 十六进制）。
pub(crate) fn url_digest(url: &Url) -> String {
    digest_hex(url.as_str())
}

/// 调用方给的内容身份（UTF-8 字节原样）的摘要（SHA-256 十六进制）：记录里只存摘要，不存调用方的原文。
pub(crate) fn identity_digest(identity: &str) -> String {
    digest_hex(identity)
}

/// `job.json` 的写法。`format_version` 不在 [`OLDEST_JOB_FORMAT_VERSION`]`..=`[`JOB_FORMAT_VERSION`] 之内时拒绝，见
/// [`check_version`]。
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum JobFile {
    Vod {
        format_version: u32,
        preference_digest: String,
        url_digest: String,
        selection: Option<AnySelectionFile>,
        plan_digest: String,
    },
    Live {
        format_version: u32,
        preference_digest: String,
        url_digest: String,
        selection: Option<AnySelectionFile>,
        /// 格式版本 16 起；没有时为空，空时不写
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unrecorded_tails: Vec<TailFile>,
    },
    /// 格式版本 14 起
    File {
        format_version: u32,
        url_digest: String,
        length: Option<u64>,
        etag: Option<String>,
        last_modified: Option<String>,
        /// 格式版本 15 起；14 的没有，读作 None
        #[serde(default)]
        identity_digest: Option<String>,
    },
}

/// 尾部没录到的一条字幕轨在 job.json 中的写法（见 [`RecordKind::Live`]）。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TailFile {
    session: u32,
    track: usize,
}

/// 选轨记录在 job.json 中的写法：格式版本 16 起为 [`SelectionFile`]，之前为 [`SelectionFileV13`]，按版本核对是哪一种。
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum AnySelectionFile {
    Current(SelectionFile),
    V13(SelectionFileV13),
}

/// [`SelectionRecord`] 在 job.json 中的写法（格式版本 16 起）。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionFile {
    variant: VariantFile,
    muxed_audio: bool,
    audio: Vec<RenditionFile>,
    subtitles: Vec<RenditionFile>,
}

/// 格式版本 13～15 的选轨记录：至多一条独立音频，没有时用变体里混着的。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionFileV13 {
    variant: VariantFile,
    audio: Option<AudioFileV13>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioFileV13 {
    key: RenditionKeyFile,
    name: Option<String>,
    language: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenditionFile {
    key: RenditionKeyFile,
    name: Option<String>,
    language: Option<String>,
    default: bool,
    forced: bool,
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

/// [`RenditionKey`] 在 job.json 中的写法（音频与字幕相同）。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum RenditionKeyFile {
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
    let selection = record
        .selection
        .as_ref()
        .map(|s| AnySelectionFile::Current(SelectionFile::from(s)));
    let file = match &record.kind {
        RecordKind::Vod { plan_digest } => JobFile::Vod {
            format_version,
            preference_digest,
            url_digest,
            selection,
            plan_digest: plan_digest.clone(),
        },
        RecordKind::Live { tails } => JobFile::Live {
            format_version,
            preference_digest,
            url_digest,
            selection,
            unrecorded_tails: tails
                .iter()
                .map(|&(session, track)| TailFile { session, track })
                .collect(),
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
        identity_digest: record.identity_digest.clone(),
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
                unrecorded_tails, ..
            } if version < TRACKS_JOB_FORMAT_VERSION && !unrecorded_tails.is_empty() => {
                return Err(WorkDirProblem::Corrupt(format!(
                    "job.json 是格式版本 {version}，那时还没有字幕轨"
                )));
            }
            JobFile::Live {
                preference_digest,
                url_digest,
                selection,
                unrecorded_tails,
                ..
            } => {
                let tails: Vec<(u32, usize)> = unrecorded_tails
                    .iter()
                    .map(|t| (t.session, t.track))
                    .collect();
                if !tails.is_sorted_by(|a, b| a < b) {
                    return Err(WorkDirProblem::Corrupt(
                        "job.json 的 unrecorded_tails 没有按会话、轨号排列或有重复".to_owned(),
                    ));
                }
                (
                    preference_digest,
                    url_digest,
                    selection,
                    RecordKind::Live { tails },
                )
            }
            JobFile::File { .. } if version < FILE_JOB_FORMAT_VERSION => {
                return Err(WorkDirProblem::Corrupt(format!(
                    "job.json 是格式版本 {version}，那时还没有文件任务"
                )));
            }
            JobFile::File {
                identity_digest: Some(_),
                ..
            } if version < IDENTITY_JOB_FORMAT_VERSION => {
                return Err(WorkDirProblem::Corrupt(format!(
                    "job.json 是格式版本 {version}，那时文件任务还不记内容身份"
                )));
            }
            JobFile::File {
                url_digest,
                length,
                etag,
                last_modified,
                identity_digest,
                ..
            } => {
                return Ok(Recorded::File(FileRecord {
                    url_digest,
                    length,
                    etag,
                    last_modified,
                    identity_digest,
                }));
            }
        };
    let selection = match selection {
        None => None,
        Some(AnySelectionFile::Current(file)) if version >= TRACKS_JOB_FORMAT_VERSION => {
            Some(file.into())
        }
        Some(AnySelectionFile::V13(file)) if version < TRACKS_JOB_FORMAT_VERSION => {
            Some(file.into())
        }
        Some(_) => {
            return Err(WorkDirProblem::Corrupt(format!(
                "job.json 是格式版本 {version}，选轨记录的写法与它不符"
            )));
        }
    };
    Ok(Recorded::Hls(Box::new(HlsRecord {
        preference_digest,
        url_digest,
        selection,
        kind,
    })))
}

impl From<&SelectionRecord> for SelectionFile {
    fn from(record: &SelectionRecord) -> Self {
        let variant = &record.variant;
        let renditions = |records: &[RenditionRecord]| {
            records
                .iter()
                .map(|r| RenditionFile {
                    key: RenditionKeyFile::from(&r.key),
                    name: r.name.clone(),
                    language: r.language.clone(),
                    default: r.default,
                    forced: r.forced,
                })
                .collect()
        };
        SelectionFile {
            variant: VariantFile::from(variant),
            muxed_audio: record.muxed_audio,
            audio: renditions(&record.audio),
            subtitles: renditions(&record.subtitles),
        }
    }
}

impl From<&VariantRecord> for VariantFile {
    fn from(variant: &VariantRecord) -> Self {
        VariantFile {
            key: VariantKeyFile::from(&variant.key),
            bandwidth: variant.bandwidth,
            resolution: variant.resolution.map(resolution_to_file),
            codecs: variant.codecs.clone(),
        }
    }
}

impl From<VariantFile> for VariantRecord {
    fn from(variant: VariantFile) -> Self {
        VariantRecord {
            key: variant.key.into(),
            bandwidth: variant.bandwidth,
            resolution: variant.resolution.map(resolution_from_file),
            codecs: variant.codecs,
        }
    }
}

impl From<SelectionFile> for SelectionRecord {
    fn from(file: SelectionFile) -> Self {
        let renditions = |files: Vec<RenditionFile>| {
            files
                .into_iter()
                .map(|r| RenditionRecord {
                    key: r.key.into(),
                    name: r.name,
                    language: r.language,
                    default: r.default,
                    forced: r.forced,
                })
                .collect()
        };
        SelectionRecord {
            variant: file.variant.into(),
            muxed_audio: file.muxed_audio,
            audio: renditions(file.audio),
            subtitles: renditions(file.subtitles),
        }
    }
}

impl From<SelectionFileV13> for SelectionRecord {
    fn from(file: SelectionFileV13) -> Self {
        SelectionRecord {
            variant: file.variant.into(),
            muxed_audio: file.audio.is_none(),
            audio: file
                .audio
                .into_iter()
                .map(|a| RenditionRecord {
                    key: a.key.into(),
                    name: a.name,
                    language: a.language,
                    default: false,
                    forced: false,
                })
                .collect(),
            subtitles: Vec::new(),
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

impl From<&RenditionKey> for RenditionKeyFile {
    fn from(key: &RenditionKey) -> Self {
        match key.clone() {
            RenditionKey::Stable { id, occurrence } => RenditionKeyFile::Stable { id, occurrence },
            RenditionKey::Attributes {
                group,
                name,
                language,
                occurrence,
            } => RenditionKeyFile::Attributes {
                group,
                name,
                language,
                occurrence,
            },
        }
    }
}

impl From<RenditionKeyFile> for RenditionKey {
    fn from(file: RenditionKeyFile) -> Self {
        match file {
            RenditionKeyFile::Stable { id, occurrence } => RenditionKey::Stable { id, occurrence },
            RenditionKeyFile::Attributes {
                group,
                name,
                language,
                occurrence,
            } => RenditionKey::Attributes {
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

    fn audio_key() -> RenditionKey {
        RenditionKey::Attributes {
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

    fn stable_audio_key() -> RenditionKey {
        RenditionKey::Stable {
            id: "Audio-37262".into(),
            occurrence: 0,
        }
    }

    fn selection(variant: VariantKey, audio: RenditionKey) -> SelectionRecord {
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
            muxed_audio: false,
            audio: vec![RenditionRecord {
                key: audio,
                name: Some("Eng\n\"lish\"".into()),
                language: Some("en".into()),
                default: true,
                forced: false,
            }],
            subtitles: vec![RenditionRecord {
                key: RenditionKey::Stable {
                    id: "sub-zh".into(),
                    occurrence: 0,
                },
                name: None,
                language: Some("zh-Hans".into()),
                default: false,
                forced: true,
            }],
        }
    }

    #[test]
    fn job_file_round_trips_and_rejects_other_versions_and_fields() {
        let live = HlsRecord {
            preference_digest: "ab".into(),
            url_digest: "cd".into(),
            selection: Some(selection(variant_key(), stable_audio_key())),
            kind: RecordKind::Live { tails: Vec::new() },
        };
        let bytes = encode_hls(&live);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"kind":"live","format_version":16,"preference_digest":"ab","url_digest":"cd","selection":{"variant":{"key":{"attributes":{"bandwidth":2000,"resolution":[1280,720],"codecs":["avc1.640020","mp4a.40.2"],"audio_group":"aud","occurrence":1}},"bandwidth":2000,"resolution":[1280,720],"codecs":["avc1.640020"]},"muxed_audio":false,"audio":[{"key":{"stable":{"id":"Audio-37262","occurrence":0}},"name":"Eng\n\"lish\"","language":"en","default":true,"forced":false}],"subtitles":[{"key":{"stable":{"id":"sub-zh","occurrence":0}},"name":null,"language":"zh-Hans","default":false,"forced":true}]}}"#
        );
        assert_eq!(decode(&bytes), Ok(Recorded::Hls(Box::new(live.clone()))));
        // 尾部没录到的字幕轨有才写
        let tailed = HlsRecord {
            kind: RecordKind::Live {
                tails: vec![(0, 2), (1, 2)],
            },
            ..live.clone()
        };
        let bytes = encode_hls(&tailed);
        assert!(String::from_utf8(bytes.clone()).unwrap().ends_with(
            r#","unrecorded_tails":[{"session":0,"track":2},{"session":1,"track":2}]}"#
        ));
        assert_eq!(decode(&bytes), Ok(Recorded::Hls(Box::new(tailed))));
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
            identity_digest: Some("ef".into()),
        };
        let bytes = encode_file(&file);
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            r#"{"kind":"file","format_version":16,"url_digest":"cd","length":1099511627776,"etag":"\"x\"","last_modified":null,"identity_digest":"ef"}"#
        );
        assert_eq!(decode(&bytes), Ok(Recorded::File(file)));
        // 14 的文件任务记录没有内容身份
        let v14 = r#"{"kind":"file","format_version":14,"url_digest":"u","length":1,"etag":null,"last_modified":null}"#;
        assert!(matches!(
            decode(v14.as_bytes()),
            Ok(Recorded::File(FileRecord {
                identity_digest: None,
                ..
            }))
        ));
        // 1.0 写的格式版本 13 照样读
        let v13 = r#"{"kind":"vod","format_version":13,"preference_digest":"a","url_digest":"u","selection":null,"plan_digest":"p"}"#;
        assert!(matches!(decode(v13.as_bytes()), Ok(Recorded::Hls(_))));
        // 15 及以前的选轨记录：至多一条音频，没有时用变体里混着的
        let old = |audio: &str| {
            format!(
                r#"{{"kind":"live","format_version":15,"preference_digest":"a","url_digest":"u","selection":{{"variant":{{"key":{{"stable":{{"id":"v","audio_group":null,"occurrence":0}}}},"bandwidth":null,"resolution":null,"codecs":[]}},"audio":{audio}}}}}"#
            )
        };
        let read = |text: String| match decode(text.as_bytes()) {
            Ok(Recorded::Hls(record)) => record.selection.unwrap(),
            other => panic!("{other:?}"),
        };
        let muxed = read(old("null"));
        assert!(muxed.muxed_audio && muxed.audio.is_empty() && muxed.subtitles.is_empty());
        let separate = read(old(
            r#"{"key":{"stable":{"id":"a","occurrence":0}},"name":"n","language":"en"}"#,
        ));
        assert!(!separate.muxed_audio);
        assert_eq!(separate.audio.len(), 1);
        assert_eq!(separate.audio[0].language.as_deref(), Some("en"));

        for found in [12, 17] {
            assert_eq!(
                decode(format!(r#"{{"kind":"vod","format_version":{found}}}"#).as_bytes()),
                Err(WorkDirProblem::UnsupportedVersion {
                    file: "job.json",
                    found,
                    supported: 16
                })
            );
        }
        for rejected in [
            r#"{"kind":"vod","format_version":1,"plan_digest":"cd"}"#,
            r#"{"kind":"vod","format_version":9,"address_digest":"a","preference_digest":"b","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"vod","format_version":13,"preference_digest":"a","url_digest":"u","selection":null,"plan_digest":"p","extra":1}"#,
            r#"{"kind":"file","format_version":13,"url_digest":"u","length":1,"etag":null,"last_modified":null}"#,
            r#"{"kind":"file","format_version":14,"url_digest":"u","length":-1,"etag":null,"last_modified":null}"#,
            r#"{"kind":"file","format_version":14,"url_digest":"u","length":1,"etag":null,"last_modified":null,"identity_digest":"ab"}"#,
            r#"{"format_version":13,"preference_digest":"a","url_digest":"u","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"vod","format_version":13,"preference_digest":"a","url_digest":"u","selection":null}"#,
            r#"{"kind":"vod","format_version":13,"preference_digest":"a","selection":null,"plan_digest":"p"}"#,
            r#"{"kind":"live","format_version":13,"preference_digest":"a","url_digest":"u","selection":{"variant":{"key":{"stable":{"id":"x","audio_group":null,"occurrence":0,"uri":"x"}},"bandwidth":null,"resolution":null,"codecs":[]},"audio":null}}"#,
            r#"{"kind":"live","format_version":13,"preference_digest":"a","url_digest":"u","selection":{"variant":{"key":{"uri":{"id":"x","occurrence":0}},"bandwidth":null,"resolution":null,"codecs":[]},"audio":null}}"#,
            // 版本与选轨记录的写法不符
            r#"{"kind":"live","format_version":16,"preference_digest":"a","url_digest":"u","selection":{"variant":{"key":{"stable":{"id":"x","audio_group":null,"occurrence":0}},"bandwidth":null,"resolution":null,"codecs":[]},"audio":null}}"#,
            r#"{"kind":"live","format_version":15,"preference_digest":"a","url_digest":"u","selection":{"variant":{"key":{"stable":{"id":"x","audio_group":null,"occurrence":0}},"bandwidth":null,"resolution":null,"codecs":[]},"muxed_audio":true,"audio":[],"subtitles":[]}}"#,
            // 尾部记录：15 还没有字幕轨；没排序或有重复
            r#"{"kind":"live","format_version":15,"preference_digest":"a","url_digest":"u","selection":null,"unrecorded_tails":[{"session":0,"track":1}]}"#,
            r#"{"kind":"live","format_version":16,"preference_digest":"a","url_digest":"u","selection":null,"unrecorded_tails":[{"session":1,"track":1},{"session":0,"track":1}]}"#,
            r#"{"kind":"live","format_version":16,"preference_digest":"a","url_digest":"u","selection":null,"unrecorded_tails":[{"session":0,"track":1},{"session":0,"track":1}]}"#,
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
            kind: RecordKind::Live { tails: Vec::new() },
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
                muxed_audio: true,
                audio: Vec::new(),
                subtitles: Vec::new(),
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
            audio: Renditions::These(vec![RenditionChoice::Language("EN".into())]),
            subtitles: Renditions::None,
        };
        assert_eq!(
            preference_digest(&best),
            "aa17c2345a7d79ea2ddd8272b3b49122c3ec2f442df4591cac5b68ba9a56501c"
        );
        let index = Preference {
            variant: VariantChoice::Index(2),
            audio: Renditions::Default,
            subtitles: Renditions::None,
        };
        assert_eq!(
            preference_digest(&index),
            "9f08ffbf33ebf4575933b5c13a4429b0263dbb652e42f3396f8ffcd2d6a10c6e"
        );
        let matching = Preference {
            variant: VariantChoice::Matching(variant_key()),
            audio: Renditions::These(vec![RenditionChoice::Matching(audio_key())]),
            subtitles: Renditions::None,
        };
        assert_eq!(
            preference_digest(&matching),
            "8afe2f1e8436a3017dc5bcfe0f341f700ec1dc74d3572588a4ac0f5752644798"
        );
        let stable = Preference {
            variant: VariantChoice::Matching(stable_variant_key()),
            audio: Renditions::These(vec![RenditionChoice::Matching(stable_audio_key())]),
            subtitles: Renditions::None,
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
        assert_eq!(
            identity_digest("pikpak:VNabc:0123ABCD"),
            "5ed30cc633b640f77bbd303622d2dd733c832d3dca0a774003e1e4993f3a82c4"
        );
    }

    #[test]
    fn preference_digest_ignores_language_case_only() {
        let one = |c| Renditions::These(vec![c]);
        let digest = |variant, audio| {
            preference_digest(&Preference {
                variant,
                audio,
                subtitles: Renditions::None,
            })
        };
        let en = |l: &str| one(RenditionChoice::Language(l.into()));
        let base = digest(VariantChoice::Best, en("EN"));
        assert_eq!(base, digest(VariantChoice::Best, en("en")));
        for other in [
            digest(VariantChoice::Index(0), en("en")),
            digest(VariantChoice::Matching(variant_key()), en("en")),
            digest(VariantChoice::Best, Renditions::Default),
            digest(VariantChoice::Best, one(RenditionChoice::Index(0))),
            digest(
                VariantChoice::Best,
                one(RenditionChoice::Matching(audio_key())),
            ),
            digest(VariantChoice::Best, Renditions::None),
            digest(VariantChoice::Best, Renditions::All),
            digest(
                VariantChoice::Best,
                Renditions::These(vec![
                    RenditionChoice::Language("en".into()),
                    RenditionChoice::Index(0),
                ]),
            ),
        ] {
            assert_ne!(base, other);
        }
    }

    /// 多条音频与字幕的偏好按文档的写法编码；期望值用另一个 SHA-256 实现按那段文字算出。选了字幕的与没选的不同。
    #[test]
    fn digests_of_several_renditions_are_fixed() {
        let preference = Preference {
            variant: VariantChoice::Best,
            audio: Renditions::These(vec![
                RenditionChoice::Language("DE".into()),
                RenditionChoice::Index(1),
            ]),
            subtitles: Renditions::These(vec![RenditionChoice::Language("zh-Hans".into())]),
        };
        // "variant best\naudio these 2\naudio language de\naudio index 1\nsubtitles these 1\nsubtitles language zh-hans\n"
        assert_eq!(
            preference_digest(&preference),
            "d5530def81f9637b8eb00ec1b2bc7f5c115e0d481c1bfd9955329aa2e0bf91f1"
        );
        let all = Preference {
            subtitles: Renditions::All,
            ..Preference::default()
        };
        // "variant best\naudio default\nsubtitles all\n"
        assert_eq!(
            preference_digest(&all),
            "d4a4b713d923808efce6617c0012c0b8fb5e73be0b8fbce5c36a81c64617b895"
        );
        assert_ne!(
            preference_digest(&all),
            preference_digest(&Preference::default())
        );
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
