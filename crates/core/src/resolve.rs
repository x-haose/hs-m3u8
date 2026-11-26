//! 解析来源：拉取播放列表；是主播放列表时选轨，再拉取所选变体、音频与字幕 rendition 的媒体播放列表。

use std::sync::Arc;

use hs_m3u8_hls::{
    self as hls, CodecKind, MasterPlaylist, MediaPlaylist, Playlist, Renditions, Segment,
    Selection, renditions,
};
use hs_m3u8_remux::Streams;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::hooks::{HookKind, Hooks, Purpose, run_hook};
use crate::http::{Http, Permit};
use crate::info::{SubtitleFormat, SubtitleProblem};
use crate::request::Source;
use crate::selection::{SelectionRecord, track_streams};
use crate::{Error, HttpFailure, Unsupported};

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
    pub subtitle_support: Vec<(usize, Result<(), SubtitleProblem>)>,
    /// 自动选到、下不了而没选上的字幕的位置，按选的顺序
    pub skipped_subtitles: Vec<usize>,
}

impl Resolved {
    /// 记进任务目录的选轨；来源本身是媒体播放列表时为 None。
    pub(crate) fn selection_record(&self) -> Option<SelectionRecord> {
        self.master.as_ref().map(|m| m.record.clone())
    }

    /// 有任一条轨的播放列表没有 EXT-X-ENDLIST 即为直播。
    pub(crate) fn is_live(&self) -> bool {
        self.tracks.iter().any(|t| !t.playlist.ended)
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
/// 字幕：点名的（[`Renditions::These`]）与记录里的下不了时报错——拉不到报那次请求的错误，格式不支持报
/// [`Unsupported::SubtitleFormat`]；自动选到的（[`Renditions::Default`]、[`Renditions::All`]）下不了的去掉，原因记在
/// [`Chosen::subtitle_support`]。各条字幕的媒体播放列表并发拉取。
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

    // 要读的字幕：选到的在前（按选的顺序），探测时再加上组里其余的
    let mut wanted: Vec<(usize, Url)> = selection
        .subtitles
        .iter()
        .map(|s| (s.index, s.uri.clone()))
        .collect();
    if scope == SubtitleScope::Group
        && let Some(group) = &selection.variant.subtitles
    {
        for (index, rendition) in renditions(&master, hls::RenditionKind::Subtitles) {
            if rendition.group == *group
                && !wanted.iter().any(|(i, _)| *i == index)
                && let Some(uri) = &rendition.uri
            {
                wanted.push((index, uri.clone()));
            }
        }
    }
    let fetched = futures_util::future::join_all(
        wanted
            .iter()
            .map(|(_, url)| fetch_media(http, hooks, url, Attempts::Retrying, cancel)),
    )
    .await;
    let subtitle_codecs: Vec<String> = selection
        .variant
        .codecs
        .iter()
        .filter(|c| CodecKind::of(c) == CodecKind::Subtitles)
        .cloned()
        .collect();
    let selected = selection.subtitles.len();
    let mut kept = Vec::new();
    let mut skipped_subtitles = Vec::new();
    let mut subtitle_support = Vec::new();
    for (n, ((index, url), result)) in wanted.into_iter().zip(fetched).enumerate() {
        let checked =
            result.and_then(
                |playlist| match subtitle_format(&playlist, &subtitle_codecs) {
                    Some(format) => Err(Error::Unsupported(Unsupported::SubtitleFormat {
                        url: Box::new(url.clone()),
                        format,
                    })),
                    None => Ok(playlist),
                },
            );
        let is_selected = n < selected;
        match checked {
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) if is_selected && explicit => return Err(e),
            Err(e) => {
                if is_selected {
                    skipped_subtitles.push(index);
                }
                subtitle_support.push((index, Err(problem(e))));
            }
            Ok(playlist) => {
                subtitle_support.push((index, Ok(())));
                if is_selected {
                    kept.push(index);
                    tracks.push(ResolvedTrack { url, playlist });
                }
            }
        }
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

/// 字幕的媒体播放列表表明它不是分段的 WebVTT 文本时为它的格式：有 EXT-X-MAP 的是 fMP4（wvtt 或 stpp）；分片地址的扩展名
/// 是 TTML 的。`codecs` 为所选变体 CODECS 里的字幕编码。
fn subtitle_format(playlist: &MediaPlaylist, codecs: &[String]) -> Option<SubtitleFormat> {
    if playlist.segments.iter().any(|s| s.init.is_some()) {
        return Some(SubtitleFormat::Fmp4 {
            codecs: codecs.to_vec(),
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

/// 下不了的字幕的原因。
fn problem(error: Error) -> SubtitleProblem {
    match error {
        Error::Unsupported(Unsupported::SubtitleFormat { format, .. }) => {
            SubtitleProblem::Format(format)
        }
        Error::Http {
            kind, retry_after, ..
        } => SubtitleProblem::Fetch(HttpFailure { kind, retry_after }),
        other => SubtitleProblem::Playlist(other.to_string()),
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
