//! 打包音频（RFC 8216 3.4）一条轨在一组里的读取与定时。

use std::path::PathBuf;

use ffmpeg::{Packet, Rational, Rescale, format, media};
use ffmpeg_next as ffmpeg;

use crate::Error;
use crate::chain;
use crate::input::{new_chain, open_chain, read_packet};
use crate::standalone::Head;

/// 打包音频时间戳的时钟（90 kHz）。
const CLOCK_90K: Rational = Rational(1, 90_000);

/// 打包音频一条轨在一组里的分片。各片跳过开头的 ID3 标签后按字节相连、作为一个输入读：格式只在开头识别一次，只有
/// 一两帧的分片也读得出。FFmpeg 的裸音频解复用器从 0 起按样本数计时，所以按字节位置找到每个包所在的分片，平移到该片
/// 的 ID3 时间戳处：与同组的其他轨对齐，缺失的分片处留出空档。有分片没有时间戳时整组按样本数接续，只能单独成轨（见
/// [`crate::Unsupported::UntimedPackedAudio`]）。
///
/// 简化：按样本数计时用的是 FFmpeg 给的包时长，AC-3 与 E-AC-3 的裸音频解复用器以 1/90000 秒为时间基，44.1 kHz
/// 等采样率下每帧向下取整：有时间戳时每片重新对齐，误差不累积；按样本数接续时每小时约短 0.8 秒。遇到没有时间戳的
/// 这类来源时改为按帧头里的采样点数计时。
pub(crate) struct Packed {
    ictx: format::context::Input,
    /// 读分片文件出错时的路径与原因
    failure: chain::Failure,
    /// 各分片的音频数据在相连之后的输入里的起点，字节；升序
    starts: Vec<u64>,
    /// 各分片的时间戳（90 kHz，未展开）；整组按样本数接续时为 None
    timestamps: Option<Vec<i64>>,
    /// 音频流的下标；没有音频流时为 None
    audio: Option<usize>,
    /// 当前分片的下标与它的音频包要加的偏移（以音频流的时间基为单位）；读到它第一个带 DTS 的音频包时确定
    current: Option<(usize, i64)>,
}

impl Packed {
    /// 把各片开头的 ID3 标签之后的音频数据相连打开。`segments` 至少有一个，`heads` 为各片开头的信息，格式相同。
    pub(crate) fn open(
        segments: &[PathBuf],
        heads: &[Head],
        group: usize,
        track: usize,
    ) -> Result<Packed, Error> {
        let files = segments
            .iter()
            .cloned()
            .zip(heads.iter().map(|h| h.data_start))
            .collect();
        let (chain, failure) = new_chain(files)?;
        let starts = chain.starts().to_vec();
        let ictx = open_chain(chain, &failure, group, track)?;
        let audio = ictx
            .streams()
            .find(|s| s.parameters().medium() == media::Type::Audio)
            .map(|s| s.index());
        Ok(Packed {
            ictx,
            failure,
            starts,
            timestamps: heads.iter().map(|h| h.timestamp).collect(),
            audio,
            current: None,
        })
    }

    pub(crate) fn context(&self) -> &format::context::Input {
        &self.ictx
    }

    /// 各分片都有时间戳。
    pub(crate) fn timed(&self) -> bool {
        self.timestamps.is_some()
    }

    /// 读下一个包，音频包平移到所在分片的时间戳处；读到末尾返回 false。
    pub(crate) fn read(
        &mut self,
        packet: &mut Packet,
        group: usize,
        track: usize,
    ) -> Result<bool, Error> {
        if !read_packet(&mut self.ictx, &self.failure, packet, group, track)? {
            return Ok(false);
        }
        let Some(timestamps) = &self.timestamps else {
            return Ok(true);
        };
        let Some(dts) = packet.dts().filter(|_| Some(packet.stream()) == self.audio) else {
            return Ok(true);
        };
        let position =
            u64::try_from(packet.position()).map_err(|_| Error::UnplacedPacket { group, track })?;
        // 第一个分片从 0 起，位置总能归到某一片
        let segment = self.starts.partition_point(|&start| start <= position) - 1;
        let shift = match self.current {
            Some((current, shift)) if current == segment => shift,
            _ => {
                let time_base = self
                    .ictx
                    .stream(packet.stream())
                    .expect("包所属的流存在")
                    .time_base();
                let start =
                    unwrap_pts(timestamps[segment], timestamps[0]).rescale(CLOCK_90K, time_base);
                self.current = Some((segment, start - dts));
                start - dts
            }
        };
        packet.set_dts(Some(dts + shift));
        packet.set_pts(packet.pts().map(|pts| pts + shift));
        Ok(true)
    }
}

/// 33 位时间戳的回绕周期（90 kHz 时约 26.5 小时）。
const WRAP: i64 = 1 << 33;

/// 一条轨的 33 位时间戳按这条轨的起点 `start` 展开：取与它模 2^33 同余、落在 [start − 60 秒, start − 60 秒 + 2^33)
/// 里的值。FFmpeg 读 TS 时同样以第一个时间戳前 60 秒为界展开（demux.c 的 update_wrap_reference）。
fn unwrap_pts(pts: i64, start: i64) -> i64 {
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
