//! 对外描述来源的类型：探测结果与进度里的变体、音轨与各轨概况。不含地址：地址常带令牌，而这些类型常被打进
//! 日志与界面。

use std::time::Duration;

use hs_m3u8_hls::{
    MasterPlaylist, MediaPlaylist, RenditionChoice, RenditionKey, RenditionKind, Resolution,
    VariantChoice, VariantKey, renditions,
};

use crate::FetchFailure;
use crate::resolve::{self, Chosen};

/// 一个变体的属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantInfo {
    /// 主播放列表里第几个变体，也是在 [`MasterInfo::variants`] 里的位置；只对这一份主播放列表有效，开始任务按
    /// [`VariantInfo::choice`] 选
    pub index: usize,
    /// BANDWIDTH，bit/s；来源没写时为 None
    pub bandwidth: Option<u64>,
    pub resolution: Option<Resolution>,
    /// CODECS 拆开后的各项，如 `avc1.640028`、`mp4a.40.2`
    pub codecs: Vec<String>,
    /// 引用的音频组（AUDIO 属性）
    pub audio_group: Option<String>,
    /// 引用的字幕组（SUBTITLES 属性）
    pub subtitle_group: Option<String>,
    /// 身份，开始任务按它选（[`VariantInfo::choice`]）
    pub key: VariantKey,
}

impl VariantInfo {
    /// 按身份选这个变体（[`VariantChoice::Matching`]）：探测之后主播放列表增删重排了变体，开始任务时也选到它。续传
    /// 时给当初得到的那个，见 [`crate::Source::preference`]。
    pub fn choice(&self) -> VariantChoice {
        VariantChoice::Matching(self.key.clone())
    }
}

/// 一个音频 rendition 的属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioInfo {
    /// 主播放列表里第几个音频 rendition（只数音频），也是在 [`MasterInfo::audio`] 里的位置；只对这一份主播放列表
    /// 有效，开始任务按 [`AudioInfo::choice`] 选
    pub index: usize,
    pub group: String,
    pub name: Option<String>,
    pub language: Option<String>,
    /// DEFAULT=YES
    pub default: bool,
    /// CHANNELS 原样，如 `2`、`6`、`16/JOC`
    pub channels: Option<String>,
    /// 有独立的媒体播放列表；为 false 时音频混在变体流里，选它即用变体里的音频
    pub separate: bool,
    /// 身份，开始任务按它选（[`AudioInfo::choice`]）
    pub key: RenditionKey,
}

impl AudioInfo {
    /// 按身份选这个音频 rendition（[`RenditionChoice::Matching`]），同 [`VariantInfo::choice`]。
    pub fn choice(&self) -> RenditionChoice {
        RenditionChoice::Matching(self.key.clone())
    }
}

/// 一个字幕 rendition 的属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleInfo {
    /// 主播放列表里第几个字幕 rendition（只数字幕），也是在 [`MasterInfo::subtitles`] 里的位置；只对这一份主播放列表
    /// 有效，开始任务按 [`SubtitleInfo::choice`] 选
    pub index: usize,
    pub group: String,
    pub name: Option<String>,
    /// LANGUAGE，BCP 47（如 `zh-Hans`）
    pub language: Option<String>,
    /// DEFAULT=YES
    pub default: bool,
    /// AUTOSELECT=YES
    pub autoselect: bool,
    /// FORCED=YES：只含画面里外语对白等必须显示的内容
    pub forced: bool,
    /// 能不能下：要读它的媒体播放列表才知道
    pub support: SubtitleSupport,
    /// 身份，开始任务按它选（[`SubtitleInfo::choice`]）
    pub key: RenditionKey,
}

impl SubtitleInfo {
    /// 按身份选这个字幕 rendition（[`RenditionChoice::Matching`]），同 [`VariantInfo::choice`]。
    pub fn choice(&self) -> RenditionChoice {
        RenditionChoice::Matching(self.key.clone())
    }
}

/// 一条字幕能不能下（[`SubtitleInfo::support`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtitleSupport {
    /// 没读它的媒体播放列表：不在所选变体的字幕组里（选不到），或没写 URI（不合 RFC 8216，选到时报错）；开始任务时只读选
    /// 到的，其余的也是这样
    NotRead,
    /// 分段的 WebVTT 文本，下得了
    Supported,
    /// 本库不处理的格式：自动选到的跳过，点名的报 [`crate::Unsupported::SubtitleFormat`]
    Unsupported(SubtitleFormat),
    /// 读不了它的媒体播放列表（只在探测时出现：开始任务时选到的读不了，任务以那次的错误失败；没选到的不读）
    Unreadable(SubtitleProblem),
}

/// 探测时读不了字幕的媒体播放列表的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubtitleProblem {
    /// 拉不到（重试之后）：404/410、临时故障或内容没写完，可否重试见其中
    #[error("拉不到字幕的媒体播放列表：{0}")]
    Fetch(FetchFailure),
    /// 拉到了而用不了：播放列表写坏、用了不支持的特性、回调出错等，值为错误说明
    #[error("字幕的媒体播放列表用不了：{0}")]
    Unusable(String),
}

/// 本库不处理的字幕格式。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubtitleFormat {
    /// 分片是 fMP4（有 EXT-X-MAP），里面是 WebVTT（wvtt）或 TTML（stpp）；`codecs` 为所选变体 CODECS 里的字幕编码，
    /// 没写时为空
    #[error("fMP4 里的字幕（{}）", if codecs.is_empty() { "CODECS 没写".to_owned() } else { codecs.join("、") })]
    Fmp4 { codecs: Vec<String> },
    /// 分片地址的扩展名表明不是 WebVTT 文本（如 `.ttml`、`.xml`、`.dfxp` 的 TTML）
    #[error("扩展名为 {0} 的字幕文件")]
    Extension(String),
}

/// 主播放列表里可选的轨与选中的轨。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterInfo {
    /// 按出现顺序
    pub variants: Vec<VariantInfo>,
    /// 音频 rendition，按出现顺序
    pub audio: Vec<AudioInfo>,
    /// 字幕 rendition，按出现顺序
    pub subtitles: Vec<SubtitleInfo>,
    pub selected: Selected,
}

/// 选中的轨，顺序即任务目录里的轨号：变体之后是各音频、各字幕。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub variant: VariantInfo,
    /// 用变体流里混着的音频
    pub muxed_audio: bool,
    /// 有独立媒体播放列表的音频，按选的顺序
    pub audio: Vec<AudioInfo>,
    /// 字幕，按选的顺序
    pub subtitles: Vec<SubtitleInfo>,
    /// 选到而没选上的字幕，原因见各自的 [`SubtitleInfo::support`]：按 [`hs_m3u8_hls::Renditions::Default`] 或
    /// [`hs_m3u8_hls::Renditions::All`] 自动选到而格式不支持的；探测时另有读不了的与点名而格式不支持的（开始任务时这两种
    /// 使任务失败）
    pub skipped_subtitles: Vec<SubtitleInfo>,
}

/// 一条轨的媒体播放列表概况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    /// 有 EXT-X-ENDLIST：点播，或已结束的直播
    pub ended: bool,
    pub segments: usize,
    /// 来源标了 EXT-X-GAP 的分片数（含在 `segments` 里，下载时不取）
    pub gaps: usize,
    /// 分片声明时长之和；直播为当前窗口的
    pub duration: Duration,
    /// 有 AES-128 加密的分片或 init 段
    pub encrypted: bool,
    /// 有没标 EXT-X-GAP 的分片或它的 init 段加密而所用的 EXT-X-KEY 没写 URI：要给自定义 key
    /// （[`crate::JobRequest::key`]）才下得了
    pub needs_custom_key: bool,
}

impl MasterInfo {
    pub(crate) fn of(chosen: &Chosen) -> Self {
        let master = &chosen.playlist;
        MasterInfo {
            variants: (0..master.variants.len())
                .map(|i| variant_info(master, i))
                .collect(),
            audio: renditions(master, RenditionKind::Audio)
                .map(|(i, _)| audio_info(master, i))
                .collect(),
            subtitles: renditions(master, RenditionKind::Subtitles)
                .map(|(i, _)| subtitle_info(chosen, i))
                .collect(),
            selected: Selected::of(chosen),
        }
    }
}

impl Selected {
    pub(crate) fn of(chosen: &Chosen) -> Self {
        let (master, selection) = (&chosen.playlist, &chosen.selection);
        Selected {
            variant: variant_info(master, selection.variant_index),
            muxed_audio: selection.muxed_audio,
            audio: selection
                .audio
                .iter()
                .map(|a| audio_info(master, a.index))
                .collect(),
            subtitles: selection
                .subtitles
                .iter()
                .map(|s| subtitle_info(chosen, s.index))
                .collect(),
            skipped_subtitles: chosen
                .skipped_subtitles
                .iter()
                .map(|&index| subtitle_info(chosen, index))
                .collect(),
        }
    }
}

impl TrackInfo {
    pub(crate) fn of(playlist: &MediaPlaylist) -> Self {
        TrackInfo {
            ended: playlist.ended,
            segments: playlist.segments.len(),
            gaps: playlist.segments.iter().filter(|s| s.gap).count(),
            duration: Duration::from_micros(
                playlist
                    .segments
                    .iter()
                    .map(|s| s.duration_us)
                    .fold(0u64, u64::saturating_add),
            ),
            encrypted: playlist
                .segments
                .iter()
                .any(|s| s.key.is_some() || s.init.as_ref().is_some_and(|i| i.key.is_some())),
            needs_custom_key: resolve::key_without_uri(&playlist.segments),
        }
    }
}

fn variant_info(master: &MasterPlaylist, index: usize) -> VariantInfo {
    let variant = &master.variants[index];
    VariantInfo {
        index,
        bandwidth: variant.bandwidth,
        resolution: variant.resolution,
        codecs: variant.codecs.clone(),
        audio_group: variant.audio.clone(),
        subtitle_group: variant.subtitles.clone(),
        key: VariantKey::of(master, index).expect("下标在范围内"),
    }
}

/// 第 `index` 个音频 rendition（口径同 [`RenditionChoice::Index`]）。
fn audio_info(master: &MasterPlaylist, index: usize) -> AudioInfo {
    let (_, rendition) = renditions(master, RenditionKind::Audio)
        .nth(index)
        .expect("下标在范围内");
    AudioInfo {
        index,
        group: rendition.group.clone(),
        name: rendition.name.clone(),
        language: rendition.language.clone(),
        default: rendition.default,
        channels: rendition.channels.clone(),
        separate: rendition.uri.is_some(),
        key: RenditionKey::of(master, RenditionKind::Audio, index).expect("下标在范围内"),
    }
}

/// 第 `index` 个字幕 rendition（口径同 [`RenditionChoice::Index`]），能不能下取自 `chosen` 里读过的。
fn subtitle_info(chosen: &Chosen, index: usize) -> SubtitleInfo {
    let master = &chosen.playlist;
    let (_, rendition) = renditions(master, RenditionKind::Subtitles)
        .nth(index)
        .expect("下标在范围内");
    SubtitleInfo {
        index,
        group: rendition.group.clone(),
        name: rendition.name.clone(),
        language: rendition.language.clone(),
        default: rendition.default,
        autoselect: rendition.autoselect,
        forced: rendition.forced,
        support: chosen
            .subtitle_support
            .iter()
            .find(|(i, _)| *i == index)
            .map_or(SubtitleSupport::NotRead, |(_, support)| support.clone()),
        key: RenditionKey::of(master, RenditionKind::Subtitles, index).expect("下标在范围内"),
    }
}
