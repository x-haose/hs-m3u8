//! HLS 的字幕轨：逐组读 WebVTT 分片，按 X-TIMESTAMP-MAP 换到媒体时间线，用本组的偏移平移到输出时间线，生成要封进
//! 输出的字幕包（Matroska 为 SubRip，MP4 为 mov_text），并留下对齐后的 cue 供另存为文件。
//!
//! 对齐规则（RFC 8216 3.5，与 hls.js 相同）：cue 的媒体时刻为 MPEGTS + (cue − LOCAL)，没有 X-TIMESTAMP-MAP 时本地 0 即
//! 媒体 0；33 位回绕按离本组最早的呈现时刻最近的一种展开（同 [`crate::timing`] 对齐各输入的做法）。每组的 cue 截到本组的
//! 时间范围里：从本组起点到起点加上本组的声明时长（音视频各轨 EXTINF 之和里最长的；都为 0 时不截结尾），全在范围外的
//! 丢掉；起止换到字幕流的时间基（1 毫秒，四舍五入），再去重（见 [`hs_m3u8_subtitle::dedup`]）。丢掉的 cue 与解析时跳过
//! 的块计入 [`crate::StreamReport::discarded_cues`]。
//!
//! 封进输出时每路字幕流的起点不往回走：SubRip 后一条的起点不早于前一条的（Matroska 允许重叠），mov_text 的段不早于
//! 前一段的结尾（MP4 同一时刻只有一个样本，起点严格递增）。只有前一组的 cue 延续到下一组的范围里才会推后，推到没有长度
//! 的同样计入丢掉的。另存的文件用截到各组范围、去重之后的 cue，不随封进输出时的推后而变。

use std::collections::VecDeque;
use std::path::Path;

use hs_m3u8_subtitle::{self as subtitle, Cue, TimestampMap};

use crate::timing::wrap_shift_us;
use crate::{Container, Error, Origin, TrackSegments, Unsupported};

/// 字幕流的时间基的一个 tick，微秒：WebVTT 的精度为 1 毫秒，Matroska 的时间基也是；cue 的起止先换到它，写出时不再舍入。
pub(crate) const TICK_US: i64 = 1000;

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

/// 一个待写的字幕包，时刻为输出时间线上的微秒，都是 [`TICK_US`] 的整数倍。
pub(crate) struct Packet {
    pub(crate) start_us: i64,
    /// 大于 0
    pub(crate) duration_us: i64,
    pub(crate) data: Vec<u8>,
}

/// 一组在输出时间线上的范围，微秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    start_us: i64,
    /// 声明时长都为 0 时为 None：不截结尾
    end_us: Option<i64>,
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
    /// 见 [`crate::StreamReport::discarded_cues`]
    pub(crate) discarded: u64,
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
            discarded: 0,
        }
    }

    /// 读第 `group` 组这条轨的分片 `segments`（可以没有），换到输出时间线并排进队列。`reference_us` 为本组各输入里最早的
    /// 呈现时刻（输入时间线，已按回绕对齐），`offset_us` 为本组的偏移，`declared_us` 为本组的声明时长（见模块说明）。
    pub(crate) fn load_group(
        &mut self,
        group: usize,
        segments: &TrackSegments,
        reference_us: i64,
        offset_us: i64,
        declared_us: i64,
    ) -> Result<(), Error> {
        self.group = group;
        let at = self.at();
        let start_us = reference_us + offset_us;
        let window = Window {
            start_us,
            end_us: (declared_us > 0).then(|| start_us.saturating_add(declared_us)),
        };
        let mut cues = Vec::new();
        for segment in &segments.segments {
            let parsed = read(&segment.path, at)?;
            self.discarded += parsed.skipped as u64;
            for cue in parsed.cues {
                match align(cue, parsed.map.as_ref(), reference_us, offset_us, window) {
                    Some(cue) => cues.push(cue),
                    None => self.discarded += 1,
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

    /// 本组对齐后的 `cues`（按起点排序）生成字幕包排进队列，起点不往回走（见模块说明）。
    fn queue_packets(&mut self, cues: &[Cue], codec: Codec, at: Origin) -> Result<(), Error> {
        match codec {
            Codec::Subrip => {
                for cue in cues {
                    let start_us = self
                        .last
                        .map_or(cue.start_us, |(last_start, _)| cue.start_us.max(last_start));
                    if cue.end_us <= start_us {
                        self.discarded += 1;
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
                    if slice.text.is_empty() {
                        continue;
                    }
                    let start_us = self
                        .last
                        .map_or(slice.start_us, |(_, end)| slice.start_us.max(end));
                    if slice.end_us <= start_us {
                        self.discarded += 1;
                        continue;
                    }
                    let data = subtitle::mov_text_sample(&slice.text).map_err(|too_long| {
                        Error::Unsupported(Unsupported::SubtitleTooLong {
                            at,
                            bytes: too_long.0,
                        })
                    })?;
                    self.push(Packet {
                        start_us,
                        duration_us: slice.end_us - start_us,
                        data,
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

/// 一条 cue 换到输出时间线：媒体时刻（见模块说明）展开回绕、加上 `offset_us`，截到 `window` 里，起止换到 [`TICK_US`]
/// 的整数倍（四舍五入）。全在范围外、截后没有长度或时刻超出 i64 时为 None。
fn align(
    mut cue: Cue,
    map: Option<&TimestampMap>,
    reference_us: i64,
    offset_us: i64,
    window: Window,
) -> Option<Cue> {
    let output = |us| {
        let media = TimestampMap::media_us(map, us)?;
        let unwrapped = media.checked_add(wrap_shift_us(media.checked_sub(reference_us)?))?;
        unwrapped.checked_add(offset_us)
    };
    let start = output(cue.start_us)?.max(window.start_us);
    let mut end = output(cue.end_us)?;
    if let Some(window_end) = window.end_us {
        end = end.min(window_end);
    }
    let (start, end) = (to_tick(start)?, to_tick(end)?);
    if end <= start {
        return None;
    }
    cue.start_us = start;
    cue.end_us = end;
    Some(cue)
}

/// 微秒换成 [`TICK_US`] 的整数倍，四舍五入；超出 i64 时为 None。
fn to_tick(us: i64) -> Option<i64> {
    us.checked_add(TICK_US / 2)?
        .div_euclid(TICK_US)
        .checked_mul(TICK_US)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start_us: i64, end_us: i64) -> Cue {
        Cue {
            start_us,
            end_us,
            id: None,
            settings: String::new(),
            text: "x".into(),
        }
    }

    /// 截到组的范围里并换到毫秒：跨过起点的从起点开始，起点落在毫秒中间时四舍五入，截后不足一个 tick 的丢掉；跨过结尾的
    /// 截到结尾；全在范围外的、换算溢出的丢掉；33 位回绕按参照展开。
    #[test]
    fn cues_are_clipped_to_the_group_and_snapped_to_milliseconds() {
        let window = Window {
            start_us: 9_999_600,
            end_us: Some(20_000_000),
        };
        let aligned = |c: Cue| align(c, None, 0, 0, window).map(|c| (c.start_us, c.end_us));
        assert_eq!(aligned(cue(9_000_000, 10_000_000)), None);
        assert_eq!(aligned(cue(9_000_000, 10_000_400)), None);
        assert_eq!(
            aligned(cue(9_000_000, 10_500_000)),
            Some((10_000_000, 10_500_000))
        );
        assert_eq!(
            aligned(cue(19_000_000, 25_000_000)),
            Some((19_000_000, 20_000_000))
        );
        assert_eq!(aligned(cue(20_000_000, 21_000_000)), None);
        assert_eq!(aligned(cue(i64::MAX - 10, i64::MAX)), None);

        // 2^33 × 100 / 9 微秒，四舍五入
        let wrap = 95_443_717_689;
        let unbounded = Window {
            start_us: 0,
            end_us: None,
        };
        let near_wrap = align(cue(wrap - 1_000, wrap + 2_000), None, 1_000, 0, unbounded);
        assert_eq!(near_wrap.map(|c| (c.start_us, c.end_us)), Some((0, 2_000)));
    }
}
