//! 解析来源：拉取播放列表；是主播放列表时选轨，再拉取所选变体、音频与字幕 rendition 的媒体播放列表。

use std::sync::Arc;

use futures_util::StreamExt;
use hs_m3u8_hls::{
    self as hls, CodecKind, MasterPlaylist, MediaPlaylist, Playlist, Renditions, Segment,
    Selection, renditions,
};
use hs_m3u8_remux::Streams;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::hooks::{HookKind, Hooks, Purpose, run_hook};
use crate::http::{Http, Permit};
use crate::info::{SubtitleFormat, SubtitleProblem, SubtitleSupport};
use crate::request::Source;
use crate::selection::{SelectionRecord, track_streams};
use crate::{Error, Unsupported};

/// 解析得到的一条轨：媒体播放列表的地址与首次拉到的内容。轨号见 [`crate::Selected`]。
pub(crate) struct ResolvedTrack {
    pub url: Url,
    pub playlist: MediaPlaylist,
}

/// 解析结果。
pub(crate) struct Resolved {
    pub tracks: Vec<ResolvedTrack>,
    /// 来源是主播放列表时为它与其中所选的轨；来源本身是媒体播放列表时为 None
    pub master: Option<Chosen>,
}

/// 主播放列表与其中所选的轨。
pub(crate) struct Chosen {
    pub playlist: MasterPlaylist,
    /// 最终所选：自动选到而下不了的字幕已去掉
    pub selection: Selection,
    /// 记进任务目录的选轨
    pub record: SelectionRecord,
    /// 读过媒体播放列表的字幕能不能下，按字幕的位置（口径同 [`hls::RenditionChoice::Index`]）：开始任务时为选到的，探测
    /// 时为所选变体字幕组里的全部
    pub subtitle_support: Vec<(usize, SubtitleSupport)>,
    /// 选到而没选上的字幕的位置，按选的顺序：自动选到而格式不支持的，探测时另有读不了的与点名而格式不支持的
    pub skipped_subtitles: Vec<usize>,
}

impl Resolved {
    /// 记进任务目录的选轨；来源本身是媒体播放列表时为 None。
    pub(crate) fn selection_record(&self) -> Option<SelectionRecord> {
        self.master.as_ref().map(|m| m.record.clone())
    }

    /// 有任一条音视频轨的播放列表没有 EXT-X-ENDLIST 即为直播；字幕播放列表写没写不算。
    pub(crate) fn is_live(&self) -> bool {
        self.tracks
            .iter()
            .zip(self.streams())
            .any(|(t, streams)| streams != Streams::Subtitles && !t.playlist.ended)
    }

    /// 各轨的取流方式。
    pub(crate) fn streams(&self) -> Vec<Streams> {
        track_streams(self.master.as_ref().map(|m| &m.record))
    }
}

/// 读哪些字幕的媒体播放列表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubtitleScope {
    /// 只读选到的（开始任务）
    Selected,
    /// 所选变体字幕组里的全部（探测）
    Group,
}

/// 拉取来源并选轨。`recorded` 为任务目录记录的选轨：有记录时按它找回同一条轨，不按偏好重新选，记录原样沿用；
/// 主播放列表里找不到它时为 `Ok(None)`。
pub(crate) async fn resolve(
    http: &Http,
    source: &Source,
    recorded: Option<&SelectionRecord>,
    scope: SubtitleScope,
    cancel: &CancellationToken,
) -> Result<Option<Resolved>, Error> {
    let first = load_playlist(http, &source.hooks, &source.url, Attempts::Retrying, cancel).await?;
    resolve_playlist(http, source, first, recorded, scope, cancel).await
}

/// 同 [`resolve`]，来源的播放列表已拉到并解析为 `first`。
///
/// 字幕：开始任务时（[`SubtitleScope::Selected`]）选到的拉不到或播放列表不能用，报那次的错误（外部故障如实上抛）；格式不
/// 支持（见 [`subtitle_format`]）时，点名的（[`Renditions::These`]）与记录里的报 [`Unsupported::SubtitleFormat`]，自动选到
/// 的（[`Renditions::Default`]、[`Renditions::All`]）去掉，记在 [`Chosen::skipped_subtitles`]。探测时
/// （[`SubtitleScope::Group`]）字幕的问题都不让探测失败：选到而读不了或格式不支持的（点名的也是）去掉、记下；能不能下都记
/// 在 [`Chosen::subtitle_support`]。
pub(crate) async fn resolve_playlist(
    http: &Http,
    source: &Source,
    first: Playlist,
    recorded: Option<&SelectionRecord>,
    scope: SubtitleScope,
    cancel: &CancellationToken,
) -> Result<Option<Resolved>, Error> {
    let hooks = &source.hooks;
    let master = match first {
        Playlist::Media(playlist) => {
            return Ok(Some(Resolved {
                tracks: vec![ResolvedTrack {
                    url: source.url.clone(),
                    playlist,
                }],
                master: None,
            }));
        }
        Playlist::Master(master) => master,
    };
    let (mut selection, explicit) = match recorded {
        Some(record) => match record.find(&master) {
            Some(selection) => (selection, true),
            None => return Ok(None),
        },
        None => (
            hls::select(&master, &source.preference)?,
            matches!(source.preference.subtitles, Renditions::These(_)),
        ),
    };
    let mut tracks = Vec::new();
    for url in std::iter::once(&selection.variant.uri).chain(selection.audio.iter().map(|a| &a.uri))
    {
        let playlist = fetch_media(http, hooks, url, Attempts::Retrying, cancel).await?;
        tracks.push(ResolvedTrack {
            url: url.clone(),
            playlist,
        });
    }

    let read = read_subtitles(http, hooks, &master, &selection, scope, cancel).await?;
    let mut kept = Vec::new();
    let mut skipped_subtitles = Vec::new();
    let mut subtitle_support = Vec::new();
    for subtitle in read {
        let (support, usable) = classify(subtitle.read, &selection.variant.codecs);
        if subtitle.selected {
            match (usable, &support) {
                (Ok(playlist), _) => {
                    kept.push(subtitle.index);
                    tracks.push(ResolvedTrack {
                        url: subtitle.url.clone(),
                        playlist,
                    });
                }
                (Err(Some(error)), _) if scope == SubtitleScope::Selected => return Err(error),
                (Err(_), SubtitleSupport::Unsupported(format))
                    if explicit && scope == SubtitleScope::Selected =>
                {
                    return Err(Error::Unsupported(Unsupported::SubtitleFormat {
                        url: Box::new(subtitle.url),
                        format: format.clone(),
                    }));
                }
                (Err(_), _) => skipped_subtitles.push(subtitle.index),
            }
        }
        subtitle_support.push((subtitle.index, support));
    }
    selection.subtitles.retain(|s| kept.contains(&s.index));
    let record = match recorded {
        Some(record) => record.clone(),
        None => SelectionRecord::of(&selection, &master),
    };
    Ok(Some(Resolved {
        tracks,
        master: Some(Chosen {
            playlist: master,
            selection,
            record,
            subtitle_support,
            skipped_subtitles,
        }),
    }))
}

/// 同时拉取的字幕媒体播放列表数：字幕组常有几十种语言，一起发会被限流。
const SUBTITLE_PLAYLISTS_AT_ONCE: usize = 4;

/// 读过的一条字幕的媒体播放列表。
struct ReadSubtitle {
    /// 第几个字幕 rendition（口径同 [`hls::RenditionChoice::Index`]）
    index: usize,
    url: Url,
    /// 是选到的（否则是探测时顺带读的组里其余的）
    selected: bool,
    read: Result<MediaPlaylist, Error>,
}

/// 拉取选到的字幕的媒体播放列表（按选的顺序），探测时（[`SubtitleScope::Group`]）再加上所选变体字幕组里其余有 URI 的，
/// 至多 [`SUBTITLE_PLAYLISTS_AT_ONCE`] 个同时。各自的失败随结果返回，只有取消中止。
async fn read_subtitles(
    http: &Http,
    hooks: &Arc<dyn Hooks>,
    master: &MasterPlaylist,
    selection: &Selection,
    scope: SubtitleScope,
    cancel: &CancellationToken,
) -> Result<Vec<ReadSubtitle>, Error> {
    let mut wanted: Vec<(usize, Url, bool)> = selection
        .subtitles
        .iter()
        .map(|s| (s.index, s.uri.clone(), true))
        .collect();
    if scope == SubtitleScope::Group
        && let Some(group) = &selection.variant.subtitles
    {
        for (index, rendition) in renditions(master, hls::RenditionKind::Subtitles) {
            if rendition.group == *group
                && !wanted.iter().any(|(i, _, _)| *i == index)
                && let Some(uri) = &rendition.uri
            {
                wanted.push((index, uri.clone(), false));
            }
        }
    }
    let reads =
        futures_util::stream::iter(wanted.into_iter().map(|(index, url, selected)| async move {
            let read = fetch_media(http, hooks, &url, Attempts::Retrying, cancel).await;
            ReadSubtitle {
                index,
                url,
                selected,
                read,
            }
        }))
        .buffered(SUBTITLE_PLAYLISTS_AT_ONCE)
        .collect::<Vec<_>>()
        .await;
    if reads
        .iter()
        .any(|r| matches!(r.read, Err(Error::Cancelled)))
    {
        return Err(Error::Cancelled);
    }
    Ok(reads)
}

/// 字幕的媒体播放列表表明它不是分段的 WebVTT 文本时为它的格式：有 EXT-X-MAP 的是 fMP4（wvtt 或 stpp）；分片地址的扩展名
/// 是 TTML 的。`codecs` 为所选变体的 CODECS，其中的字幕编码写进格式里。
fn subtitle_format(playlist: &MediaPlaylist, codecs: &[String]) -> Option<SubtitleFormat> {
    if playlist.segments.iter().any(|s| s.init.is_some()) {
        return Some(SubtitleFormat::Fmp4 {
            codecs: codecs
                .iter()
                .filter(|c| CodecKind::of(c) == CodecKind::Subtitles)
                .cloned()
                .collect(),
        });
    }
    const TTML: [&str; 4] = ["ttml", "xml", "dfxp", "ttml2"];
    playlist.segments.iter().find_map(|s| {
        let name = s.uri.path_segments()?.next_back()?;
        let (_, extension) = name.rsplit_once('.')?;
        let extension = extension.to_ascii_lowercase();
        TTML.contains(&extension.as_str())
            .then(|| SubtitleFormat::Extension(format!(".{extension}")))
    })
}

/// 读过的一条字幕能不能下，与能下时它的媒体播放列表；读不了时另带那次的错误（开始任务时照原样上抛），格式不支持时没有。
/// `codecs` 为所选变体的 CODECS。读不了的原因：拉不到的保留可否重试（见 [`crate::FetchFailure`]），其余（播放列表写坏、
/// 用了不支持的特性、回调出错等）为错误说明。
fn classify(
    read: Result<MediaPlaylist, Error>,
    codecs: &[String],
) -> (SubtitleSupport, Result<MediaPlaylist, Option<Error>>) {
    match read {
        Ok(playlist) => match subtitle_format(&playlist, codecs) {
            Some(format) => (SubtitleSupport::Unsupported(format), Err(None)),
            None => (SubtitleSupport::Supported, Ok(playlist)),
        },
        Err(error) => {
            let problem = match error.missable() {
                Some(failure) => SubtitleProblem::Fetch(failure),
                None => SubtitleProblem::Unusable(error.to_string()),
            };
            (SubtitleSupport::Unreadable(problem), Err(Some(error)))
        }
    }
}

/// 拉取播放列表的请求失败时再不再试。
#[derive(Debug, Clone, Copy)]
pub(crate) enum Attempts {
    /// 可重试的失败按重试策略退避后再试（开始任务时）
    Retrying,
    /// 只发一次请求（刷新直播播放列表）：失败交给录制循环，按刷新的节奏再刷新，原因即时放进进度
    Once,
}

/// 拉取并解析媒体播放列表（刷新直播播放列表也用它）。
pub(crate) async fn fetch_media(
    http: &Http,
    hooks: &Arc<dyn Hooks>,
    url: &Url,
    attempts: Attempts,
    cancel: &CancellationToken,
) -> Result<MediaPlaylist, Error> {
    match load_playlist(http, hooks, url, attempts, cancel).await? {
        Playlist::Media(media) => Ok(media),
        Playlist::Master(_) => Err(Error::NotMediaPlaylist {
            url: Box::new(url.clone()),
        }),
    }
}

/// 拉取播放列表，经 `on_playlist` 回调后解析；相对地址按重定向之后的最终地址解析。
async fn load_playlist(
    http: &Http,
    hooks: &Arc<dyn Hooks>,
    url: &Url,
    attempts: Attempts,
    cancel: &CancellationToken,
) -> Result<Playlist, Error> {
    let (purpose, permit) = (Purpose::Playlist, Permit::Exempt);
    let response = match attempts {
        Attempts::Retrying => http.get(purpose, url, None, permit, cancel).await?,
        Attempts::Once => http.get_once(purpose, url, None, permit, cancel).await?,
    };
    parse_playlist(hooks, response.url, response.body).await
}

/// 解析拉到的播放列表 `body`，先经 `on_playlist` 回调；`final_url` 为重定向之后的地址，相对地址按它解析。
pub(crate) async fn parse_playlist(
    hooks: &Arc<dyn Hooks>,
    final_url: Url,
    body: Vec<u8>,
) -> Result<Playlist, Error> {
    let hook_url = final_url.clone();
    let (body, rewritten) = run_hook(hooks, HookKind::Playlist, move |hooks| {
        let rewritten = hooks.on_playlist(&hook_url, &body)?;
        Ok((body, rewritten))
    })
    .await?;
    let malformed = |cause| Error::Playlist {
        url: Box::new(final_url.clone()),
        cause: Box::new(cause),
    };
    let parsed = match &rewritten {
        Some(text) => hls::parse(text, &final_url),
        None => hls::parse_bytes(&body, &final_url),
    };
    parsed.map_err(|e| match e {
        hls::Error::Malformed(cause) => malformed(cause),
        hls::Error::Truncated => Error::Incomplete {
            url: Box::new(final_url.clone()),
        },
        hls::Error::Unsupported { line, what } => {
            let url = Box::new(final_url.clone());
            Error::Unsupported(match what {
                hls::Feature::Encryption(what) => Unsupported::Encryption { url, line, what },
                hls::Feature::IFramesOnly => Unsupported::IFramesOnly { url, line },
                hls::Feature::DeltaUpdate => Unsupported::DeltaUpdate { url, line },
            })
        }
    })
}

/// `segments` 里有没标 EXT-X-GAP 的分片或它的 init 段加密而所用的 EXT-X-KEY 没写 URI：没有自定义 key 就取不到 key。
pub(crate) fn key_without_uri<'a>(segments: impl IntoIterator<Item = &'a Segment>) -> bool {
    segments.into_iter().filter(|s| !s.gap).any(|s| {
        s.key.as_ref().is_some_and(|k| k.uri.is_none())
            || s.init
                .as_ref()
                .and_then(|i| i.key.as_ref())
                .is_some_and(|k| k.uri.is_none())
    })
}

/// 收下播放列表 `url`（开始任务、录制中刷新）时核对它的分片 `segments` 取得到 key（见 [`key_without_uri`]），
/// `custom_key` 为请求给了自定义 key；取不到时报 [`Unsupported::KeyWithoutUri`]。
pub(crate) fn require_key_uris<'a>(
    url: &Url,
    segments: impl IntoIterator<Item = &'a Segment>,
    custom_key: bool,
) -> Result<(), Error> {
    if !custom_key && key_without_uri(segments) {
        return Err(Error::Unsupported(Unsupported::KeyWithoutUri(Box::new(
            url.clone(),
        ))));
    }
    Ok(())
}
