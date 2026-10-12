//! 打包音频的定时：各分片的 ID3 时间戳（见 [`crate::standalone`]）只有 33 位，按这条轨的起点展开。

/// 33 位时间戳的回绕周期（90 kHz 时约 26.5 小时）。
const WRAP: i64 = 1 << 33;

/// 一条轨的 33 位时间戳按这条轨的起点 `start` 展开：取与它模 2^33 同余、落在 [start − 60 秒, start − 60 秒 + 2^33)
/// 里的值。FFmpeg 读 TS 时同样以第一个时间戳前 60 秒为界展开（demux.c 的 update_wrap_reference）。
pub(crate) fn unwrap_pts(pts: i64, start: i64) -> i64 {
    let low = start - 60 * 90_000;
    low + (pts - low).rem_euclid(WRAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_unwrap_from_the_start_of_the_track() {
        const MINUTE: i64 = 60 * 90_000;
        let start = 1_000 * 90_000;
        assert_eq!(unwrap_pts(start + 5, start), start + 5);
        // 比起点早 60 秒以内的不动，再早的当作回绕之后
        assert_eq!(unwrap_pts(start - MINUTE, start), start - MINUTE);
        assert_eq!(
            unwrap_pts(start - MINUTE - 1, start),
            start - MINUTE - 1 + WRAP
        );
        // 起点在回绕点前：回绕之后的值接着往后排
        let late = WRAP - MINUTE / 2;
        assert_eq!(unwrap_pts(5, late), WRAP + 5);
    }
}
