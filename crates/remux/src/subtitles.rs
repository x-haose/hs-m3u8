//! HLS 的字幕轨：逐组读 WebVTT 分片，按 X-TIMESTAMP-MAP 换到媒体时间线，用本组的偏移平移到输出时间线，生成要封进
//! 输出的字幕包（Matroska 为 SubRip，MP4 为 mov_text），并留下对齐后的 cue 供另存为文件。
//!
//! 对齐规则（RFC 8216 3.5，与 hls.js 相同）：cue 的媒体时刻为 MPEGTS + (cue − LOCAL)，没有 X-TIMESTAMP-MAP 时本地 0 即
//! 媒体 0；33 位回绕按离本组最早的呈现时刻最近的一种展开。本组之前的部分截掉：开始早于本组起点的从起点开始，结束不晚于
//! 起点的丢掉。各组的 cue 去重（见 [`hs_m3u8_subtitle::dedup`]）。
//!
//! 简化：结束晚于本组媒体结尾的 cue 不截短，可能延续到下一组的开头（只出在字幕比画面长的组上）；要截时在组写完、知道
//! 结尾之后再生成这一组的字幕包。

use std::collections::VecDeque;
use std::path::Path;

use hs_m3u8_subtitle::{self as subtitle, Cue, TimestampMap};

use crate::{Container, Error, Origin, TrackSegments};

/// 33 位 90 kHz 时钟的回绕周期，微秒（2^33 × 100 / 9，四舍五入）。
const WRAP_US: i128 = ((1i128 << 33) * 100 + 4) / 9;

/// 封进输出的字幕编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    /// Matroska 的 S_TEXT/UTF8：每条 cue 一个包，可以重叠
    Subrip,
    /// MP4 的 tx3g：同一时刻只有一个样本，重叠的切成段；清屏的空样本由封装器补
    MovText,
}

impl Codec {
    pub(crate) fn of(container: Container) -> Codec {
        match container {
            Container::Matroska => Codec::Subrip,
            Container::Mp4 => Codec::MovText,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Codec::Subrip => "subrip",
            Codec::MovText => "mov_text",
        }
    }
}

/// 一个待写的字幕包，时刻为输出时间线上的微秒。
pub(crate) struct Packet {
    pub(crate) start_us: i64,
    pub(crate) duration_us: i64,
    pub(crate) data: Vec<u8>,
}

/// 一条字幕轨在各组间的状态。
pub(crate) struct Lane {
    pub(crate) track: usize,
    /// 正在写的组
    group: usize,
    /// 封进输出时的输出流下标与编码；不封时为 None，只留下 cue
    pub(crate) output: Option<(usize, Codec)>,
    /// 本组待写的包，按时刻排列
    pub(crate) queue: VecDeque<Packet>,
    /// 已排进队列的最后一个包的起点与结束：SubRip 的起点、mov_text 的段都不往回走
    last: Option<(i64, i64)>,
    /// 对齐后的 cue，各组依次相接
    pub(crate) cues: Vec<Cue>,
}

impl Lane {
    pub(crate) fn new(track: usize, output: Option<(usize, Codec)>) -> Lane {
        Lane {
            track,
            group: 0,
            output,
            queue: VecDeque::new(),
            last: None,
            cues: Vec::new(),
        }
    }

    /// 读第 `group` 组这条轨的分片 `segments`，换到输出时间线并排进队列。`reference_us` 为本组各输入里最早的呈现时刻
    /// （输入时间线，已按回绕对齐），`offset_us` 为本组的偏移：输出时间线上本组从 `reference_us + offset_us` 开始。
    pub(crate) fn load_group(
        &mut self,
        group: usize,
        segments: &TrackSegments,
        reference_us: i64,
        offset_us: i64,
    ) -> Result<(), Error> {
        self.group = group;
        let at = self.at();
        let start_us = reference_us + offset_us;
        let mut cues = Vec::new();
        for segment in &segments.segments {
            let parsed = read(&segment.path, at)?;
            for mut cue in parsed.cues {
                let media = |us| {
                    unwrap(
                        TimestampMap::media_us(parsed.map.as_ref(), us),
                        reference_us,
                    )
                };
                cue.start_us = (media(cue.start_us) + offset_us).max(start_us);
                cue.end_us = media(cue.end_us) + offset_us;
                if cue.end_us > cue.start_us {
                    cues.push(cue);
                }
            }
        }
        subtitle::dedup(&mut cues);
        if let Some((_, codec)) = self.output {
            self.queue_packets(&cues, codec, at)?;
        }
        self.cues.extend(cues);
        Ok(())
    }

    /// 本组对齐后的 `cues`（按起点排序）生成字幕包排进队列。起点不早于上一个包的起点（MP4 还要严格晚于它）：上一组延续
    /// 过来的 cue 与本组的重叠时，本组的往后推。
    fn queue_packets(&mut self, cues: &[Cue], codec: Codec, at: Origin) -> Result<(), Error> {
        match codec {
            Codec::Subrip => {
                for cue in cues {
                    let start_us = match self.last {
                        Some((last_start, _)) => cue.start_us.max(last_start),
                        None => cue.start_us,
                    };
                    if cue.end_us <= start_us {
                        continue;
                    }
                    self.push(Packet {
                        start_us,
                        duration_us: cue.end_us - start_us,
                        data: subtitle::srt_text(&cue.text).into_bytes(),
                    });
                }
            }
            Codec::MovText => {
                // 空档不写空样本：链接的 FFmpeg 的 movenc 在任何一路的包越过上一个字幕样本的结尾时自己补一个清屏的空样本
                // （mov_write_packet 与 mov_write_trailer），自己再写会多出一个
                for slice in subtitle::slices(cues) {
                    let free_us = self.last.map_or(i64::MIN, |(_, end)| end);
                    let start_us = slice.start_us.max(free_us);
                    if slice.end_us <= start_us || slice.text.is_empty() {
                        continue;
                    }
                    self.push(Packet {
                        start_us,
                        duration_us: slice.end_us - start_us,
                        data: mov_text(at, &slice.text)?,
                    });
                }
            }
        }
        Ok(())
    }

    fn push(&mut self, packet: Packet) {
        self.last = Some((packet.start_us, packet.start_us + packet.duration_us));
        self.queue.push_back(packet);
    }

    /// 正在写的组里的这条轨，报错时用。
    pub(crate) fn at(&self) -> Origin {
        Origin::Track {
            group: self.group,
            track: self.track,
        }
    }

    /// 队首的包在输出时间线上的起点。
    pub(crate) fn front_us(&self) -> Option<i64> {
        self.queue.front().map(|p| p.start_us)
    }
}

fn mov_text(at: Origin, text: &str) -> Result<Vec<u8>, Error> {
    subtitle::mov_text_sample(text).map_err(|cause| Error::SubtitleTooLong { at, cause })
}

/// 读并解析一个字幕分片：文件出错为 [`Error::Io`]，内容不是 WebVTT 为 [`Error::Subtitle`]。
fn read(path: &Path, at: Origin) -> Result<subtitle::WebVtt, Error> {
    let bytes = std::fs::read(path).map_err(|cause| Error::Io {
        action: "读取",
        path: path.to_path_buf(),
        cause,
    })?;
    subtitle::parse(&bytes).map_err(|cause| Error::Subtitle {
        at,
        path: path.to_path_buf(),
        cause,
    })
}

/// 媒体时刻 `media_us`（33 位回绕未展开）加上整数个回绕周期，取离 `reference_us` 最近的。
fn unwrap(media_us: i64, reference_us: i64) -> i64 {
    let distance = i128::from(reference_us) - i128::from(media_us);
    let periods = (2 * distance + WRAP_US).div_euclid(2 * WRAP_US);
    i64::try_from(i128::from(media_us) + periods * WRAP_US).expect("展开后与参照相距不到半个周期")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_times_are_unwrapped_next_to_the_reference() {
        let wrap = i64::try_from(WRAP_US).unwrap();
        assert_eq!(unwrap(10, 20), 10);
        assert_eq!(unwrap(10, wrap + 20), wrap + 10);
        assert_eq!(unwrap(wrap - 10, 5), -10);
        assert_eq!(unwrap(wrap / 2 - 1, 0), wrap / 2 - 1);
    }
}
