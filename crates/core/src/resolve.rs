//! 解析来源：拉取播放列表；是主播放列表时选轨，再拉取所选变体与音频 rendition 的媒体播放列表。

use std::sync::Arc;

use hs_m3u8_hls::{self as hls, MasterPlaylist, MediaPlaylist, Playlist, Segment, Selection};
use hs_m3u8_remux::Streams;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::hooks::{HookKind, Hooks, Purpose, run_hook};
use crate::http::{Http, Permit};
use crate::request::Source;
use crate::selection::{SelectionRecord, track_streams};
use crate::{Error, Unsupported};

/// 解析得到的一条轨：媒体播放列表的地址与首次拉到的内容。
/// 第 0 条为所选变体，第 1 条（若有）为独立的音频 rendition。
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
    pub selection: Selection,
    /// 记进任务目录的选轨
    pub record: SelectionRecord,
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

/// 拉取来源并选轨。`recorded` 为任务目录记录的选轨：有记录时按它找回同一条轨，不按偏好重新选，记录原样沿用；
/// 主播放列表里找不到它时为 `Ok(None)`。
pub(crate) async fn resolve(
    http: &Http,
    source: &Source,
    recorded: Option<&SelectionRecord>,
    cancel: &CancellationToken,
) -> Result<Option<Resolved>, Error> {
    let hooks = &source.hooks;
    Ok(Some(
        match load_playlist(http, hooks, &source.url, Attempts::Retrying, cancel).await? {
            Playlist::Media(playlist) => Resolved {
                tracks: vec![ResolvedTrack {
                    url: source.url.clone(),
                    playlist,
                }],
                master: None,
            },
            Playlist::Master(master) => {
                let (selection, record) = match recorded {
                    Some(record) => match record.find(&master) {
                        Some(selection) => (selection, record.clone()),
                        None => return Ok(None),
                    },
                    None => {
                        let selection = hls::select(&master, &source.preference)?;
                        let record = SelectionRecord::of(&selection, &master);
                        (selection, record)
                    }
                };
                let url = selection.variant.uri.clone();
                let playlist = fetch_media(http, hooks, &url, Attempts::Retrying, cancel).await?;
                let mut tracks = vec![ResolvedTrack { url, playlist }];
                if let Some(audio) = &selection.audio {
                    let playlist =
                        fetch_media(http, hooks, &audio.uri, Attempts::Retrying, cancel).await?;
                    tracks.push(ResolvedTrack {
                        url: audio.uri.clone(),
                        playlist,
                    });
                }
                Resolved {
                    tracks,
                    master: Some(Chosen {
                        playlist: master,
                        selection,
                        record,
                    }),
                }
            }
        },
    ))
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
    let final_url = response.url;
    let hook_url = final_url.clone();
    let body = response.body;
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
