//! 解析来源：拉取播放列表；是主播放列表时选轨，再拉取所选变体与音频 rendition 的媒体播放列表。

use std::sync::Arc;

use hs_m3u8_hls::{self as hls, MediaPlaylist, Playlist};
use hs_m3u8_remux::Streams;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::hooks::{HookKind, Hooks, Purpose, run_hook};
use crate::http::{Http, Priority};
use crate::request::JobRequest;
use crate::selection::SelectionKey;
use crate::{Error, Unsupported, WorkDirProblem};

/// 解析得到的一条轨：媒体播放列表的地址、取流方式与首次拉到的内容。
/// 第 0 条为所选变体，第 1 条（若有）为独立的音频 rendition。
pub(crate) struct ResolvedTrack {
    pub url: Url,
    pub streams: Streams,
    pub playlist: MediaPlaylist,
}

/// 解析结果。
pub(crate) struct Resolved {
    pub tracks: Vec<ResolvedTrack>,
    /// 来源是主播放列表时所选的变体与音频
    pub selection: Option<SelectionKey>,
}

impl Resolved {
    /// 有任一条轨的播放列表没有 EXT-X-ENDLIST 即为直播。
    pub(crate) fn is_live(&self) -> bool {
        self.tracks.iter().any(|t| !t.playlist.ended)
    }
}

/// 拉取来源并选轨。`recorded` 为任务目录记录的选轨：有记录时按它找回同一条轨，不按偏好重新选，
/// 找不到时报 [`WorkDirProblem::SelectionGone`]。
pub(crate) async fn resolve(
    http: &Http,
    request: &JobRequest,
    recorded: Option<&SelectionKey>,
    cancel: &CancellationToken,
) -> Result<Resolved, Error> {
    let hooks = &request.hooks;
    Ok(
        match load_playlist(http, hooks, &request.url, cancel).await? {
            Playlist::Media(playlist) => Resolved {
                tracks: vec![ResolvedTrack {
                    url: request.url.clone(),
                    streams: Streams::All,
                    playlist,
                }],
                selection: None,
            },
            Playlist::Master(master) => {
                let selection = match recorded {
                    Some(key) => key.find(&master).ok_or_else(|| Error::WorkDir {
                        path: request.resolved_work_dir(),
                        problem: WorkDirProblem::SelectionGone,
                    })?,
                    None => hls::select(&master, &request.preference)?,
                };
                let key = SelectionKey::of(&selection);
                // 选了独立音频 rendition 时，变体里混着的音频不用，与播放器的行为一致
                let main = match selection.audio {
                    Some(_) => Streams::Video,
                    None => Streams::All,
                };
                let url = selection.variant.uri;
                let playlist = fetch_media(http, hooks, &url, cancel).await?;
                let mut tracks = vec![ResolvedTrack {
                    url,
                    streams: main,
                    playlist,
                }];
                if let Some(audio) = selection.audio {
                    let playlist = fetch_media(http, hooks, &audio.uri, cancel).await?;
                    tracks.push(ResolvedTrack {
                        url: audio.uri,
                        streams: Streams::Audio,
                        playlist,
                    });
                }
                Resolved {
                    tracks,
                    selection: Some(key),
                }
            }
        },
    )
}

/// 拉取并解析媒体播放列表（刷新直播播放列表也用它）。
pub(crate) async fn fetch_media(
    http: &Http,
    hooks: &Arc<dyn Hooks>,
    url: &Url,
    cancel: &CancellationToken,
) -> Result<MediaPlaylist, Error> {
    match load_playlist(http, hooks, url, cancel).await? {
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
    cancel: &CancellationToken,
) -> Result<Playlist, Error> {
    let response = http
        .get(Purpose::Playlist, url, None, Priority::Urgent, cancel)
        .await?;
    let final_url = response.url;
    let hook_url = final_url.clone();
    let body = response.body;
    let body = run_hook(hooks, HookKind::Playlist, move |hooks| {
        hooks.on_playlist(&hook_url, body)
    })
    .await?;
    let not_a_playlist = || Error::Playlist {
        url: Box::new(final_url.clone()),
        cause: Box::new(hls::Error::NotAPlaylist),
    };
    let text = String::from_utf8(body).map_err(|_| not_a_playlist())?;
    hls::parse(&text, &final_url).map_err(|e| playlist_error(final_url, e))
}

/// DRM、SAMPLE-AES 等提升为 [`Error::Unsupported`]，便于调用方直接提示；其余为 [`Error::Playlist`]。
fn playlist_error(url: Url, error: hls::Error) -> Error {
    match error {
        hls::Error::Unsupported { what, .. } => Error::Unsupported(match what {
            hls::Unsupported::Drm { keyformat } => Unsupported::Drm { keyformat },
            hls::Unsupported::SampleAes => Unsupported::SampleAes,
            hls::Unsupported::Method(method) => Unsupported::KeyMethod(method),
        }),
        other => Error::Playlist {
            url: Box::new(url),
            cause: Box::new(other),
        },
    }
}
