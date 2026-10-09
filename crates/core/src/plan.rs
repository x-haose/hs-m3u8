//! 解析与计划：拉取播放列表、选轨；点播在此得到每条轨的分片与不连续段组，并在下载前确认能够合并。

use std::ops::Range;

use std::sync::Arc;

use hs_m3u8_hls::{self as hls, InitSection, MediaPlaylist, Playlist, Segment};
use hs_m3u8_remux::Streams;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::http::Http;
use crate::{Error, Hooks, JobRequest, Purpose, Unsupported, run_hook};

/// 解析得到的一条轨：媒体播放列表的地址、取流方式与首次拉到的内容。
/// 第 0 条为所选变体，第 1 条（若有）为独立的音频 rendition。
pub(crate) struct Source {
    pub url: Url,
    pub streams: Streams,
    pub playlist: MediaPlaylist,
}

/// 有任一条轨的播放列表没有 EXT-X-ENDLIST 即为直播。
pub(crate) fn is_live(sources: &[Source]) -> bool {
    sources.iter().any(|s| !s.playlist.ended)
}

/// 点播计划中的一条轨。
pub(crate) struct Track {
    pub segments: Vec<Segment>,
    /// 本轨用到的 init 段，按首次出现的顺序去重；下标即任务目录中的编号
    pub inits: Vec<InitSection>,
    /// 本轨在输出中贡献的流
    pub streams: Streams,
}

impl Track {
    fn new(segments: Vec<Segment>, streams: Streams) -> Self {
        let mut inits: Vec<InitSection> = Vec::new();
        for init in segments.iter().filter_map(|s| s.init.as_ref()) {
            if !inits.contains(init) {
                inits.push(init.clone());
            }
        }
        Track {
            segments,
            inits,
            streams,
        }
    }

    /// 分片所用 init 段在 `inits` 中的下标；无 init 段时为 None。
    pub(crate) fn init_index(&self, segment: &Segment) -> Option<usize> {
        let init = segment.init.as_ref()?;
        let index = self.inits.iter().position(|i| i == init);
        Some(index.expect("inits 含本轨所有分片的 init 段"))
    }
}

pub(crate) struct Plan {
    pub tracks: Vec<Track>,
    /// 不连续段组，按播放顺序；每组为各轨在组内的分片下标范围（轨道顺序同 `tracks`），范围都不为空
    pub groups: Vec<Vec<Range<usize>>>,
}

impl Plan {
    /// 点播计划；每条轨都必须有分片。
    pub(crate) fn new(sources: Vec<Source>) -> Result<Self, Error> {
        let tracks = sources
            .into_iter()
            .map(|s| {
                if s.playlist.segments.is_empty() {
                    return Err(Error::Unsupported(Unsupported::EmptyPlaylist(Box::new(
                        s.url,
                    ))));
                }
                Ok(Track::new(s.playlist.segments, s.streams))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let groups = groups(&tracks)?;
        Ok(Plan { tracks, groups })
    }

    pub(crate) fn segment_count(&self) -> usize {
        self.tracks.iter().map(|t| t.segments.len()).sum()
    }

    /// 续传校验用的摘要（SHA-256 十六进制）。
    ///
    /// 只覆盖分片的身份：轨道、序号、去掉查询串与片段的地址、时长、不连续段序号、字节范围、init 段地址与范围。
    /// 不含 key 地址与 IV：任务目录里存的是已解密的分片；也不含查询串：很多站点的签名或令牌每次会话都不同。
    pub(crate) fn digest(&self) -> String {
        let range = |r: Option<hls::ByteRange>| {
            r.map(|r| format!("{}@{}", r.length, r.offset))
                .unwrap_or_default()
        };
        let mut hasher = Sha256::new();
        for (index, track) in self.tracks.iter().enumerate() {
            hasher.update(format!("track {index} {}\n", track.segments.len()));
            for s in &track.segments {
                let init = s
                    .init
                    .as_ref()
                    .map(|i| format!("{} {}", identity(&i.uri), range(i.byte_range)))
                    .unwrap_or_default();
                hasher.update(format!(
                    "{} {} {} {} {} {init}\n",
                    s.sequence,
                    identity(&s.uri),
                    s.duration_us,
                    s.discontinuity,
                    range(s.byte_range),
                ));
            }
        }
        hex(&hasher.finalize())
    }
}

/// 直播任务目录用的来源摘要（SHA-256 十六进制）：去掉查询串的来源地址与选轨偏好。
pub(crate) fn source_digest(request: &JobRequest) -> String {
    let preference = &request.preference;
    let text = format!(
        "{}\n{:?}\n{:?}",
        identity(&request.url),
        preference.variant,
        preference.audio_language
    );
    hex(&Sha256::digest(text))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 去掉查询串与片段后的地址。
pub(crate) fn identity(url: &Url) -> String {
    let mut url = url.clone();
    url.set_query(None);
    url.set_fragment(None);
    url.into()
}

/// 按不连续段序号切分各轨，并检查能否合并：各轨的不连续段序列相同，同一组内每条轨的 init 段不变。
fn groups(tracks: &[Track]) -> Result<Vec<Vec<Range<usize>>>, Error> {
    let runs = tracks
        .iter()
        .enumerate()
        .map(|(index, track)| runs(index, track))
        .collect::<Result<Vec<_>, _>>()?;
    let numbers = |runs: &[(u64, Range<usize>)]| runs.iter().map(|(d, _)| *d).collect::<Vec<_>>();
    let first = numbers(&runs[0]);
    for (track, track_runs) in runs.iter().enumerate().skip(1) {
        let found = numbers(track_runs);
        if found != first {
            return Err(Error::Unsupported(Unsupported::DiscontinuityMismatch {
                track,
                first,
                found,
            }));
        }
    }
    Ok((0..first.len())
        .map(|group| runs.iter().map(|r| r[group].1.clone()).collect())
        .collect())
}

/// 一条轨按不连续段序号切成的连续区间。播放列表中的不连续段序号只增不减，所以各区间的序号互不相同。
fn runs(index: usize, track: &Track) -> Result<Vec<(u64, Range<usize>)>, Error> {
    let mut runs: Vec<(u64, Range<usize>)> = Vec::new();
    for (i, s) in track.segments.iter().enumerate() {
        match runs.last_mut() {
            Some((discontinuity, range)) if *discontinuity == s.discontinuity => {
                if track.segments[range.start].init != s.init {
                    return Err(Error::Unsupported(Unsupported::InitChangesWithinGroup {
                        track: index,
                        discontinuity: s.discontinuity,
                    }));
                }
                range.end = i + 1;
            }
            _ => runs.push((s.discontinuity, i..i + 1)),
        }
    }
    Ok(runs)
}

/// 拉取来源播放列表；是主播放列表时选轨，再拉取所选变体与音频 rendition 的媒体播放列表。
pub(crate) async fn resolve(
    http: &Http,
    request: &JobRequest,
    cancel: &CancellationToken,
) -> Result<Vec<Source>, Error> {
    let hooks = &request.hooks;
    Ok(match fetch(http, hooks, &request.url, cancel).await? {
        Playlist::Media(playlist) => vec![Source {
            url: request.url.clone(),
            streams: Streams::All,
            playlist,
        }],
        Playlist::Master(master) => {
            let selection = hls::select(&master, &request.preference)?;
            // 选了独立音频 rendition 时，变体里混着的音频不用，与播放器的行为一致
            let main = match selection.audio {
                Some(_) => Streams::Video,
                None => Streams::All,
            };
            let url = selection.variant.uri;
            let playlist = fetch_media(http, hooks, &url, cancel).await?;
            let mut sources = vec![Source {
                url,
                streams: main,
                playlist,
            }];
            if let Some(audio) = selection.audio {
                let url = audio.uri;
                let playlist = fetch_media(http, hooks, &url, cancel).await?;
                sources.push(Source {
                    url,
                    streams: Streams::Audio,
                    playlist,
                });
            }
            sources
        }
    })
}

/// 拉取并解析媒体播放列表（刷新直播播放列表也用它）。
pub(crate) async fn fetch_media(
    http: &Http,
    hooks: &Arc<dyn Hooks>,
    url: &Url,
    cancel: &CancellationToken,
) -> Result<MediaPlaylist, Error> {
    match fetch(http, hooks, url, cancel).await? {
        Playlist::Media(media) => Ok(media),
        Playlist::Master(_) => Err(Error::InvalidInput(format!(
            "变体 {url} 指向的是主播放列表，不是媒体播放列表"
        ))),
    }
}

/// 拉取并解析播放列表；相对地址按重定向之后的最终地址解析。
async fn fetch(
    http: &Http,
    hooks: &Arc<dyn Hooks>,
    url: &Url,
    cancel: &CancellationToken,
) -> Result<Playlist, Error> {
    let fetched = http.get(Purpose::Playlist, url, None, cancel).await?;
    let final_url = fetched.url;
    let text = String::from_utf8(fetched.body).map_err(|_| Error::Playlist {
        url: Box::new(final_url.clone()),
        source: Box::new(hls::Error::NotAPlaylist),
    })?;
    let hook_url = final_url.clone();
    let text = run_hook(hooks, Purpose::Playlist, move |hooks| {
        hooks.on_playlist(&hook_url, text)
    })
    .await?;
    hls::parse(&text, &final_url).map_err(|source| Error::Playlist {
        url: Box::new(final_url),
        source: Box::new(source),
    })
}
