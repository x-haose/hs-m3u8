//! 时间戳：同组各输入的回绕对齐、组间的偏移、平移到输出时间线。

use ffmpeg::{Packet, Rational, Rescale};
use ffmpeg_next as ffmpeg;

use crate::source::Source;

/// 时间戳在组间换算时使用的公共时间基（微秒）。
pub(crate) const MICROS: Rational = Rational(1, 1_000_000);

/// 同一组的 TS 与打包音频在同一个 33 位的 90 kHz 时钟上，但各输入按自己的第一个时间戳展开回绕（FFmpeg 读每个 TS
/// 输入、本库读打包音频都如此），起点跨在回绕点附近两侧的两个输入会差 2^33（约 26.5 小时）。以第一个读出包的这类
/// 输入为准，其余这类输入的第一个 DTS 离它超过半个回绕周期时，整个输入平移整数个周期：同一组各轨的起点实际相距远
/// 小于半个周期。fMP4 的时间戳不回绕，不参与。
pub(crate) fn align_wraps(sources: &mut [Source]) {
    let firsts: Vec<Option<i64>> = sources
        .iter()
        .map(|s| s.front_dts_us().filter(|_| s.input.standalone()))
        .collect();
    let Some(&base) = firsts.iter().flatten().next() else {
        return;
    };
    for (source, first) in sources.iter_mut().zip(firsts) {
        if let Some(first) = first {
            source.wrap_us = wrap_shift_us(first - base);
        }
    }
}

/// 相距 `distance_us` 的两个时刻要相距不超过半个回绕周期，后者要加的整数个 33 位回绕周期（90 kHz），微秒。
fn wrap_shift_us(distance_us: i64) -> i64 {
    // 周期 2^33 / 90000 秒，即 2^33 × 100 / 9 微秒，按分数算：周期数四舍五入，平移量四舍五入到微秒
    let (num, den) = (i128::from(1i64 << 33) * 100, 9i128);
    let periods = (2 * i128::from(distance_us) * den + num).div_euclid(2 * num);
    let shift = (2 * periods * num + den).div_euclid(2 * den);
    i64::try_from(-shift).expect("i64 的时刻差不到 2^63 微秒，周期数有限")
}

/// 本组的时间偏移（微秒），取两个下限中较大者：
/// - 呈现：本组最早的 PTS 不早于此前所有流的最晚结束时刻（第 0 组为 0）；
/// - 解码：每路流本组首个 DTS 严格大于该流上一组的末个 DTS，多加 `margin_us` 吸收换到输出时间基时的舍入；
///   此前没写过包的流，DTS 不小于 0。
///
/// 整组共用这一个偏移，组内各轨的相对时序保持原样；按呈现而非 DTS 对齐，避免 B 帧的解码提前量在每个组边界留下空隙。
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
        .filter_map(|(last, first)| match (last, first) {
            (Some(last), Some(first)) => Some(last - first + margin_us),
            (None, Some(first)) => Some(-first),
            (_, None) => None,
        })
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
}
