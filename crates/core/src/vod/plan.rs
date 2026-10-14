//! 点播计划与摘要（纯计算）：每条轨要下载的分片与 init 段、不连续段组，以及不下载的分片。

use std::ops::Range;

use hs_m3u8_hls::{InitSection, Segment};
use sha2::{Digest, Sha256};

use crate::ident::{Fingerprint, hex};
use crate::resolve::{self, ResolvedTrack};
use crate::{Error, MissReason, Missed, Unsupported};

/// 点播计划中的一条轨。
pub(crate) struct Track {
    /// 要下载的分片，按播放顺序
    pub segments: Vec<Segment>,
    /// 不下载的分片的序号与原因，按播放顺序：来源标了 EXT-X-GAP 的（[`MissReason::Gap`]），所在的不连续段有轨只有
    /// GAP 分片、合并不进输出的（[`MissReason::Unmergeable`]）
    pub skipped: Vec<(u64, MissReason)>,
    /// 要下载的分片用到的 init 段，按首次出现排序、不重复
    pub inits: Vec<InitSection>,
}

impl Track {
    /// 一条轨的分片 `segments` 按所在的段（`runs`，分片下标范围）能否合并（`mergeable`）分拣；另返回各可合并段的分片
    /// 在 `segments` 中的下标范围，都不为空（可合并的段里每条轨都有没标 GAP 的分片）。
    fn new(
        segments: &[Segment],
        runs: &[Range<usize>],
        mergeable: &[bool],
    ) -> (Self, Vec<Range<usize>>) {
        let mut track = Track {
            segments: Vec::new(),
            skipped: Vec::new(),
            inits: Vec::new(),
        };
        let mut ranges = Vec::new();
        for (run, &mergeable) in runs.iter().zip(mergeable) {
            let start = track.segments.len();
            for s in &segments[run.clone()] {
                match (s.gap, mergeable) {
                    (true, _) => track.skipped.push((s.sequence, MissReason::Gap)),
                    (false, false) => track.skipped.push((s.sequence, MissReason::Unmergeable)),
                    (false, true) => track.segments.push(s.clone()),
                }
            }
            if mergeable {
                ranges.push(start..track.segments.len());
            }
        }
        for init in track.segments.iter().filter_map(|s| s.init.as_ref()) {
            if !track.inits.contains(init) {
                track.inits.push(init.clone());
            }
        }
        (track, ranges)
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
    /// 见 [`Plan::digest`]
    digest: String,
}

impl Plan {
    /// 点播计划。各轨的不连续段序列须相同（含标了 EXT-X-GAP 的分片），否则报
    /// [`Unsupported::DiscontinuityMismatch`]；有轨在某段里只有 GAP 分片时这段合并不了，各轨在这段的分片都不下载；
    /// 没有一段合并得了时报 [`Unsupported::NoMergeableGroup`]。要下载的分片取不到 key（见
    /// [`resolve::require_key_uris`]，`custom_key` 为请求给了自定义 key）时报 [`Unsupported::KeyWithoutUri`]。
    pub(crate) fn new(tracks: Vec<ResolvedTrack>, custom_key: bool) -> Result<Self, Error> {
        if let Some(t) = tracks.iter().find(|t| t.playlist.segments.is_empty()) {
            return Err(Error::Unsupported(Unsupported::EmptyPlaylist(Box::new(
                t.url.clone(),
            ))));
        }
        let all: Vec<&[Segment]> = tracks.iter().map(|t| &t.playlist.segments[..]).collect();
        let runs = check_discontinuities(&all)?;
        let mergeable = mergeable(&tracks, &runs)?;
        let mut planned = Vec::with_capacity(tracks.len());
        let mut ranges = Vec::with_capacity(tracks.len());
        for (t, runs) in tracks.iter().zip(&runs) {
            let (track, track_ranges) = Track::new(&t.playlist.segments, runs, &mergeable);
            resolve::require_key_uris(&t.url, &track.segments, custom_key)?;
            planned.push(track);
            ranges.push(track_ranges);
        }
        // 各轨的可合并段数相同（共用 mergeable），转置成按组排
        let groups = (0..ranges[0].len())
            .map(|group| ranges.iter().map(|r| r[group].clone()).collect())
            .collect();
        Ok(Plan {
            digest: digest(&all),
            tracks: planned,
            groups,
        })
    }

    /// 要下载的分片数，各轨合计。
    pub(crate) fn segment_count(&self) -> usize {
        self.tracks.iter().map(|t| t.segments.len()).sum()
    }

    /// 来源标了 EXT-X-GAP 的分片数，各轨合计。
    pub(crate) fn gap_count(&self) -> usize {
        let gaps = |t: &Track| {
            t.skipped
                .iter()
                .filter(|(_, r)| *r == MissReason::Gap)
                .count()
        };
        self.tracks.iter().map(gaps).sum()
    }

    /// 检查同一组内每条轨的 init 段内容不变（地址可以不同）。`inits[t][i]` 为第 t 条轨
    /// `inits[i]` 的内容指纹。
    pub(crate) fn check_inits(&self, inits: &[Vec<Fingerprint>]) -> Result<(), Error> {
        for group in &self.groups {
            for (index, range) in group.iter().enumerate() {
                let track = &self.tracks[index];
                let content = |s: &Segment| track.init_index(s).map(|i| inits[index][i]);
                let segments = &track.segments[range.clone()];
                let first = content(&segments[0]);
                if segments.iter().any(|s| content(s) != first) {
                    return Err(Error::Unsupported(Unsupported::InitChangesWithinGroup {
                        track: index,
                        discontinuity: segments[0].discontinuity,
                    }));
                }
            }
        }
        Ok(())
    }

    /// 续传校验用的摘要（SHA-256 十六进制），写进 job.json，口径改变须升格式版本。
    ///
    /// 只覆盖播放列表里全部分片的身份：轨道、序号、时长、不连续段序号、分片与 init 段的身份（见
    /// [`Fingerprint::of_segment`]，与直播比对分片用同一口径）。不含 key 地址与 IV：任务目录里存的是已解密的分片；不含
    /// 主机、路径前段与查询串：CDN 常在其中放每次会话不同的令牌；不含 EXT-X-GAP 标记：它只决定这次取哪些，不改已完成
    /// 分片的身份。
    pub(crate) fn digest(&self) -> String {
        self.digest.clone()
    }

    /// 不下载的分片，按轨道、序号排列，相接且原因相同的合成一个区间（会话为 0）。
    pub(crate) fn missed(&self) -> Vec<Missed> {
        let mut missed: Vec<Missed> = Vec::new();
        for (track, t) in self.tracks.iter().enumerate() {
            for (sequence, reason) in &t.skipped {
                match missed.last_mut() {
                    Some(m)
                        if m.track == track
                            && m.reason == *reason
                            && m.last.checked_add(1) == Some(*sequence) =>
                    {
                        m.last = *sequence;
                    }
                    _ => missed.push(Missed {
                        session: 0,
                        track,
                        first: *sequence,
                        last: *sequence,
                        reason: reason.clone(),
                    }),
                }
            }
        }
        missed
    }
}

/// 各段（`runs` 为各轨各段的分片下标范围）合并得了：每条轨在这段里都有没标 EXT-X-GAP 的分片。没有一段合并得了时
/// 报 [`Unsupported::NoMergeableGroup`]，带第一段里只有 GAP 的那条轨的地址。
fn mergeable(tracks: &[ResolvedTrack], runs: &[Vec<Range<usize>>]) -> Result<Vec<bool>, Error> {
    let has_content = |track: usize, group: usize| {
        tracks[track].playlist.segments[runs[track][group].clone()]
            .iter()
            .any(|s| !s.gap)
    };
    let mergeable: Vec<bool> = (0..runs[0].len())
        .map(|group| (0..tracks.len()).all(|track| has_content(track, group)))
        .collect();
    if mergeable.contains(&true) {
        return Ok(mergeable);
    }
    let track = (0..tracks.len())
        .find(|&track| !has_content(track, 0))
        .expect("第一段合并不了，即有轨在其中只有 GAP 分片");
    Err(Error::Unsupported(Unsupported::NoMergeableGroup(Box::new(
        tracks[track].url.clone(),
    ))))
}

/// 见 [`Plan::digest`]：`tracks` 为各轨播放列表里的全部分片。
fn digest(tracks: &[&[Segment]]) -> String {
    let mut hasher = Sha256::new();
    for (index, segments) in tracks.iter().enumerate() {
        hasher.update(format!("track {index} {}\n", segments.len()));
        for s in *segments {
            let init = s
                .init
                .as_ref()
                .map(|i| Fingerprint::of_segment(&i.uri, i.byte_range).to_string())
                .unwrap_or_default();
            hasher.update(format!(
                "{} {} {} {} {init}\n",
                s.sequence,
                Fingerprint::of_segment(&s.uri, s.byte_range),
                s.duration_us,
                s.discontinuity,
            ));
        }
    }
    hex(&hasher.finalize())
}

/// 各轨按不连续段序号切成的连续区间（分片下标）；各轨的不连续段序列须相同。
fn check_discontinuities(tracks: &[&[Segment]]) -> Result<Vec<Vec<Range<usize>>>, Error> {
    let runs: Vec<Vec<(u64, Range<usize>)>> = tracks.iter().map(|s| runs(s)).collect();
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
    Ok(runs
        .into_iter()
        .map(|runs| runs.into_iter().map(|(_, range)| range).collect())
        .collect())
}

/// 一条轨按不连续段序号切成的连续区间。播放列表中的不连续段序号只增不减，所以各区间的序号互不相同。
fn runs(segments: &[Segment]) -> Vec<(u64, Range<usize>)> {
    let mut runs: Vec<(u64, Range<usize>)> = Vec::new();
    for (i, s) in segments.iter().enumerate() {
        match runs.last_mut() {
            Some((discontinuity, range)) if *discontinuity == s.discontinuity => range.end = i + 1,
            _ => runs.push((s.discontinuity, i..i + 1)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use hs_m3u8_hls::{Playlist, Url, parse};

    use super::*;

    /// 计划摘要写在 job.json 里，值变了即任务目录格式变了，须升格式版本。期望值是按 [`Plan::digest`] 文档
    /// 所述的编码，用另一个 SHA-256 实现算出的。
    #[test]
    fn digest_is_fixed() {
        let url = Url::parse("https://cdn.example/v/index.m3u8?token=1").unwrap();
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n\
                    #EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"100@0\"\n\
                    #EXT-X-BYTERANGE:500@100\n#EXTINF:10,\nseg0.m4s\n\
                    #EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init2.mp4\"\n\
                    #EXTINF:9.5,\nhttps://cdn2.example/x/seg1.m4s?sig=1\n#EXT-X-ENDLIST\n";
        let Ok(Playlist::Media(playlist)) = parse(text, &url) else {
            panic!("应解析为媒体播放列表");
        };
        let plan = Plan::new(
            vec![ResolvedTrack {
                url: url.clone(),
                playlist,
            }],
            false,
        )
        .unwrap();
        assert_eq!(
            plan.digest(),
            "9709cf9cb7943aae405f447022a77b9764cd6b8dca71c76e459631f92ccb162f"
        );

        // EXT-X-GAP 标记不算：标了与没标的同一份播放列表摘要相同
        let digest_of = |text: &str| {
            let Ok(Playlist::Media(playlist)) = parse(text, &url) else {
                panic!("应解析为媒体播放列表");
            };
            let track = ResolvedTrack {
                url: url.clone(),
                playlist,
            };
            Plan::new(vec![track], false).unwrap().digest()
        };
        let plain =
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\nseg0.ts\n#EXTINF:10,\nseg1.ts\n";
        assert_eq!(
            digest_of(plain),
            digest_of(&plain.replace("#EXTINF:10,\nseg1", "#EXT-X-GAP\n#EXTINF:10,\nseg1"))
        );
    }
}
