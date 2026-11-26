//! 时间戳：同组各输入的回绕对齐、组间的偏移、平移到输出时间线、音频按采样点接续。

use crate::ffi::{Packet, Rational, Rescale, Rounding};
use crate::source::Source;

/// 时间戳在组间换算时使用的公共时间基（微秒）。
pub(crate) const MICROS: Rational = Rational(1, 1_000_000);

/// 同一组的 TS 与打包音频在同一个 33 位的 90 kHz 时钟上，但各输入按自己的第一个时间戳展开回绕（FFmpeg 读每个 TS
/// 输入、本库读打包音频都如此），起点跨在回绕点附近两侧的两个输入会差 2^33（约 26.5 小时）。以第一个读出包的这类
/// 输入为准，其余这类输入的第一个 DTS 离它超过半个回绕周期时，整个输入平移整数个周期：同一组各轨的起点实际相距远
/// 小于半个周期。fMP4 的时间戳不回绕，不参与。返回各输入（已读到第一个包）要平移的量，微秒，顺序同 `sources`。
pub(crate) fn wrap_shifts_us(sources: &[Source]) -> Vec<i64> {
    let firsts: Vec<Option<i64>> = sources
        .iter()
        .map(|s| s.front_input_dts_us().filter(|_| s.input.standalone()))
        .collect();
    let base = firsts.iter().flatten().next().copied();
    firsts
        .into_iter()
        .map(|first| match (first, base) {
            (Some(first), Some(base)) => wrap_shift_us(first - base),
            _ => 0,
        })
        .collect()
}

/// 相距 `distance_us` 的两个时刻要相距不超过半个回绕周期，后者要加的整数个 33 位回绕周期（90 kHz），微秒。
pub(crate) fn wrap_shift_us(distance_us: i64) -> i64 {
    // 周期 2^33 / 90000 秒，即 2^33 × 100 / 9 微秒，按分数算：周期数四舍五入，平移量四舍五入到微秒
    let (num, den) = (i128::from(1i64 << 33) * 100, 9i128);
    let periods = (2 * i128::from(distance_us) * den + num).div_euclid(2 * num);
    let shift = (2 * periods * num + den).div_euclid(2 * den);
    i64::try_from(-shift).expect("i64 的时刻差不到 2^63 微秒，周期数有限")
}

/// 本组的时间偏移（微秒），取两个下限中较大者：
/// - 呈现：本组最早的 PTS 不早于此前所有流的最晚结束时刻（第 0 组为 0）；
/// - 解码：每路流本组首个 DTS 严格大于该流上一组的末个 DTS，多加 `margin_us` 吸收换到输出时间基时的舍入。
///
/// 整组共用这一个偏移，组内各轨的相对时序保持原样；按呈现而非 DTS 对齐，避免 B 帧的解码提前量在每个组边界留下空隙。
/// 有 B 帧的流首个 DTS 早于它的 PTS，第 0 组平移后为负：MP4 封装器把它写成编辑列表的 media_time，呈现仍从 0 开始。
pub(crate) fn group_offset(
    ends_us: &[Option<i64>],
    last_dts_us: &[Option<i64>],
    first_dts_us: &[Option<i64>],
    min_pts_us: i64,
    margin_us: i64,
) -> i64 {
    let presentation = ends_us.iter().flatten().max().copied().unwrap_or(0) - min_pts_us;
    let decode = last_dts_us
        .iter()
        .zip(first_dts_us)
        .filter_map(|(&last, &first)| Some(last? - first? + margin_us))
        .max();
    decode.map_or(presentation, |d| presentation.max(d))
}

/// 平移后的包在输出时间线上的时刻，微秒。
pub(crate) struct Retimed {
    pub(crate) dts_us: i64,
    /// 没有 PTS 时取 DTS
    pub(crate) pts_us: i64,
    /// 呈现结束时刻：max(PTS, DTS) + 时长
    pub(crate) end_us: i64,
}

/// 把包的时间戳平移 `offset_us` 并换到输出时间基。先换成微秒再平移、最后一次换到输出时间基，只经过一次舍入。
pub(crate) fn retime(
    packet: &mut Packet,
    in_tb: Rational,
    out_tb: Rational,
    offset_us: i64,
) -> Retimed {
    let shift = |ts: i64| ts.rescale(in_tb, MICROS) + offset_us;
    let dts_us = shift(packet.dts().expect("写出的包都带 DTS"));
    let pts_us = packet.pts().map(shift);
    let duration_us = packet.duration().rescale(in_tb, MICROS);
    packet.set_dts(Some(dts_us.rescale(MICROS, out_tb)));
    packet.set_pts(pts_us.map(|us| us.rescale(MICROS, out_tb)));
    packet.set_duration(packet.duration().rescale(in_tb, out_tb));
    let pts_us = pts_us.unwrap_or(dts_us);
    Retimed {
        dts_us,
        pts_us,
        end_us: pts_us.max(dts_us) + duration_us,
    }
}

/// 音频按采样点接续的时钟（同 FFmpeg 的 av_rescale_delta，ffmpeg 命令行复制音频时用它）。输入时间基比一个采样点
/// 粗（如 Matroska 的 1 毫秒）时，逐包取整的时间戳换到更细的输出时间基（如 MP4 以采样率为时间基）里，样本时长忽长
/// 忽短（48 kHz 的 AAC 一帧 1024 个采样点，取整后为 1008 与 1056 交替），见 [`SampleClock::applies`]。
///
/// 简化：帧长取 FFmpeg 给的每帧采样点数，链接的 FFmpeg 里只有解得出帧的 AAC 有；其余编码（MP3、AC-3、E-AC-3、FLAC
/// 等）照原样换算，样本时长差不过一个输入 tick。要它们也整帧接续时，按帧头读出每包的采样点数。每包的时刻落在输入时间戳的取整误差以内时，取上一包按帧长
/// 接续的值；超出时（时间戳本身有跳变）按输入的时间戳重新开始接续。误差按一个输入 tick 算：写入时四舍五入的差半个
/// tick，截断的差不到一个（FFmpeg 的 av_rescale_delta 只容半个）。
pub(crate) struct SampleClock {
    /// 采样点的时间基，1/采样率
    samples: Rational,
    /// 每帧的采样点数
    frame: i64,
    /// 下一包按接续应在的时刻，采样点
    next: Option<i64>,
}

impl SampleClock {
    /// 采样率为 `rate` 的音频从时间基 `in_tb` 换到 `out_tb` 要不要接续：输入时间基比一个采样点粗、输出时间基比输入
    /// 细时才要；输出与输入一样粗（如都是 Matroska 的 1 毫秒）时原样换算。
    pub(crate) fn applies(in_tb: Rational, out_tb: Rational, rate: u32) -> bool {
        let tick = |tb: Rational| (i64::from(tb.0), i64::from(tb.1));
        let ((in_num, in_den), (out_num, out_den)) = (tick(in_tb), tick(out_tb));
        in_num * i64::from(rate) > in_den && out_num * in_den < in_num * out_den
    }

    /// 采样率为 `rate`、每帧 `frame` 个采样点的音频。
    pub(crate) fn new(rate: u32, frame: u32) -> SampleClock {
        SampleClock {
            samples: Rational(1, i32::try_from(rate).expect("采样率在 i32 范围内")),
            frame: i64::from(frame),
            next: None,
        }
    }

    /// 输出时间线上 DTS 为 `dts_us`（微秒）、输入时间基为 `in_tb` 的包按接续定的时刻，以采样点为单位。
    pub(crate) fn place(&mut self, dts_us: i64, in_tb: Rational) -> i64 {
        let tick_us = 1i64.rescale_with(in_tb, MICROS, Rounding::Up);
        let earliest = (dts_us - tick_us).rescale_with(MICROS, self.samples, Rounding::Down);
        let latest = (dts_us + tick_us).rescale_with(MICROS, self.samples, Rounding::Up);
        let at = match self.next {
            Some(next) if (earliest..=latest).contains(&next) => next,
            _ => dts_us.rescale(MICROS, self.samples),
        };
        self.next = Some(at + self.frame);
        at
    }

    /// 采样点为单位的时刻 `at` 换到时间基 `tb`。
    pub(crate) fn to(&self, at: i64, tb: Rational) -> i64 {
        at.rescale(self.samples, tb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_further_apart_than_half_a_wrap_are_shifted_by_whole_wraps() {
        // 回绕周期 2^33 / 90000 秒，约 95443717688.9 微秒
        let wrap = 95_443_717_689;
        assert_eq!(wrap_shift_us(0), 0);
        assert_eq!(wrap_shift_us(-75_000_000), 0);
        assert_eq!(wrap_shift_us(wrap / 2 - 1), 0);
        // 视频在回绕点前、音频在回绕点后：音频的起点读成了早约一个周期
        assert_eq!(wrap_shift_us(75_000_000 - wrap), wrap);
        assert_eq!(wrap_shift_us(wrap - 75_000_000), -wrap);
        assert_eq!(wrap_shift_us(2 * wrap), -2 * wrap);
    }

    #[test]
    fn audio_in_millisecond_timestamps_continues_sample_by_sample() {
        // 48 kHz、每帧 1024 个采样点（21.333 毫秒）的音频，时间戳截断或四舍五入到毫秒
        let ms = Rational(1, 1000);
        let mp4 = Rational(1, 48_000);
        assert!(SampleClock::applies(ms, mp4, 48_000));
        for rounded in [false, true] {
            let mut clock = SampleClock::new(48_000, 1024);
            let placed: Vec<i64> = (0..6i64)
                .map(|n| {
                    let half = if rounded { 24_000 } else { 0 };
                    clock.place((n * 1024 * 1000 + half) / 48_000 * 1000, ms)
                })
                .collect();
            assert_eq!(
                placed,
                [0, 1024, 2048, 3072, 4096, 5120],
                "四舍五入：{rounded}"
            );
        }
        let mut clock = SampleClock::new(48_000, 1024);
        clock.place(0, ms);
        // 时间戳跳了 1 秒：从输入的时刻重新接续
        assert_eq!(clock.place(1_128_000, ms), 54_144);
        assert_eq!(clock.place(1_149_000, ms), 54_144 + 1024);
        // 输入时间基比采样点细，或输出与输入一样粗时不用接续
        assert!(!SampleClock::applies(Rational(1, 90_000), mp4, 48_000));
        assert!(!SampleClock::applies(mp4, mp4, 48_000));
        assert!(!SampleClock::applies(ms, ms, 48_000));
    }
}
