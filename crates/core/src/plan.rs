//! 点播计划与摘要（纯计算）：每条轨的分片与不连续段组，下载前确认能够合并。

use std::ops::Range;

use hs_m3u8_hls::{ByteRange, InitSection, Segment};
use hs_m3u8_remux::Streams;
use sha2::{Digest, Sha256};
use url::Url;

use crate::request::JobRequest;
use crate::resolve::ResolvedTrack;
use crate::{Error, Unsupported};

/// 点播计划中的一条轨。
pub(crate) struct Track {
    pub segments: Vec<Segment>,
    /// 本轨用到的 init 段，按 [`init_key`] 去重、按首次出现排序；下标即任务目录中的编号
    pub inits: Vec<InitSection>,
    pub streams: Streams,
}

impl Track {
    fn new(segments: Vec<Segment>, streams: Streams) -> Self {
        let mut inits: Vec<InitSection> = Vec::new();
        for init in segments.iter().filter_map(|s| s.init.as_ref()) {
            if !inits.iter().any(|i| init_key(i) == init_key(init)) {
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
        let key = init_key(segment.init.as_ref()?);
        let index = self.inits.iter().position(|i| init_key(i) == key);
        Some(index.expect("inits 含本轨所有分片的 init 段"))
    }
}

/// init 段的身份：与计划摘要的口径一致（不含查询串），摘要相同即保证编号对应同一个 init 段。
fn init_key(init: &InitSection) -> (String, Option<ByteRange>) {
    (strip_query(&init.uri), init.byte_range)
}

pub(crate) struct Plan {
    pub tracks: Vec<Track>,
    /// 不连续段组，按播放顺序；每组为各轨在组内的分片下标范围（轨道顺序同 `tracks`），范围都不为空
    pub groups: Vec<Vec<Range<usize>>>,
}

impl Plan {
    /// 点播计划；每条轨都必须有分片。
    pub(crate) fn new(tracks: Vec<ResolvedTrack>) -> Result<Self, Error> {
        let tracks = tracks
            .into_iter()
            .map(|t| {
                if t.playlist.segments.is_empty() {
                    return Err(Error::Unsupported(Unsupported::EmptyPlaylist(Box::new(
                        t.url,
                    ))));
                }
                Ok(Track::new(t.playlist.segments, t.streams))
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
    /// 只覆盖分片的身份：轨道、序号、去掉查询串的地址、时长、不连续段序号、字节范围、init 段地址与范围。
    /// 不含 key 地址与 IV：任务目录里存的是已解密的分片；也不含查询串：很多站点的签名或令牌每次会话都不同。
    pub(crate) fn digest(&self) -> String {
        let range = |r: Option<ByteRange>| {
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
                    .map(|i| format!("{} {}", strip_query(&i.uri), range(i.byte_range)))
                    .unwrap_or_default();
                hasher.update(format!(
                    "{} {} {} {} {} {init}\n",
                    s.sequence,
                    strip_query(&s.uri),
                    s.duration_us,
                    s.discontinuity,
                    range(s.byte_range),
                ));
            }
        }
        hex(&hasher.finalize())
    }
}

/// 直播任务目录用的来源摘要（SHA-256 十六进制）：完整的来源地址（含查询串）与选轨偏好。
/// 含查询串：很多直播源靠查询串区分频道；继续录制时须用同一个地址。
pub(crate) fn request_digest(request: &JobRequest) -> String {
    let preference = &request.preference;
    let text = format!(
        "{}\n{:?}\n{:?}",
        request.url, preference.variant, preference.audio_language
    );
    hex(&Sha256::digest(text))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 去掉查询串与片段后的地址。
pub(crate) fn strip_query(url: &Url) -> String {
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
                if track.init_index(&track.segments[range.start]) != track.init_index(s) {
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
