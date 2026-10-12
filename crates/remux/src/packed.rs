//! 打包音频（RFC 8216 3.4）一条轨在一组里的读取与定时。

use std::io;
use std::ops::Range;
use std::path::PathBuf;

use ffmpeg::{Packet, Rational, Rescale, format, media};
use ffmpeg_next as ffmpeg;

use crate::input::{new_chain, open_chain, read_packet};
use crate::standalone::{self, Format, Head};
use crate::{Error, chain, frame};

/// 打包音频时间戳的时钟（90 kHz）。
const CLOCK_90K: Rational = Rational(1, 90_000);

/// ID3v1 标签的字节数（MP3 文件末尾，以 `TAG` 开头）。
const ID3V1_LEN: usize = 128;

/// 打包音频一条轨在一组里的分片。各片按帧头切出音频帧（见 [`cut_frames`]），只把音频帧相连、作为一个输入读：格式
/// 只在开头识别一次，只有一两帧的分片也读得出，包在输入里的字节位置与分片一一对应。FFmpeg 的裸音频解复用器从 0 起按
/// 样本数计时，所以按字节位置找到每个包所在的分片，平移到该片的 ID3 时间戳处：与同组的其他轨对齐，缺失的分片处留出
/// 空档。有分片没有时间戳时整组按样本数接续，只能单独成轨（见 [`crate::Unsupported::UntimedPackedAudio`]）。
///
/// 简化：按样本数计时用的是 FFmpeg 给的包时长，AC-3 与 E-AC-3 的裸音频解复用器以 1/90000 秒为时间基，44.1 kHz
/// 等采样率下每帧向下取整：有时间戳时每片重新对齐，误差不累积；按样本数接续时每小时约短 0.8 秒。遇到没有时间戳的
/// 这类来源时改为按帧头里的采样点数计时。
pub(crate) struct Packed {
    ictx: format::context::Input,
    /// 读分片文件出错时的路径与原因
    failure: chain::Failure,
    /// 各分片的音频帧在相连之后的输入里的起点，字节；升序，第一项为 0
    starts: Vec<u64>,
    /// 各分片的时间戳（90 kHz，未展开）；整组按样本数接续时为 None
    timestamps: Option<Vec<i64>>,
    /// 音频流的下标；没有音频流时为 None
    audio: Option<usize>,
    /// 当前分片的下标与它的音频包要加的偏移（以音频流的时间基为单位）；读到它第一个带 DTS 的音频包时确定
    current: Option<(usize, i64)>,
    /// 切帧时跳过的损坏处数（见 [`cut_frames`]），各片合计
    damaged: u64,
}

impl Packed {
    /// 切出各片的音频帧，相连打开。`segments` 至少有一个，`heads` 为各片开头的信息，格式相同。
    pub(crate) fn open(
        segments: &[PathBuf],
        heads: &[Head],
        group: usize,
        track: usize,
    ) -> Result<Packed, Error> {
        let mut pieces = Vec::new();
        let mut starts = Vec::with_capacity(segments.len());
        let mut total = 0;
        let mut damaged = 0;
        for (path, head) in segments.iter().zip(heads) {
            let read_failed = |cause| Error::Io {
                action: "读取",
                path: path.clone(),
                cause,
            };
            let data = std::fs::read(path).map_err(read_failed)?;
            // 开头的信息是先前读的，文件随后变短时起点会超出
            let data_start = usize::try_from(head.data_start)
                .ok()
                .filter(|&start| start <= data.len())
                .ok_or_else(|| {
                    read_failed(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "文件比开始合并时短（合并期间被改动）",
                    ))
                })?;
            let cut = cut_frames(&data, data_start, head.format);
            damaged += cut.damaged;
            starts.push(total);
            for range in cut.ranges {
                total += range.end - range.start;
                pieces.push((path.clone(), range));
            }
        }
        let (chain, failure) = new_chain(pieces)?;
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
            damaged,
        })
    }

    /// 切帧时跳过的损坏处：音频流的下标与处数；没有音频流时为 None。
    pub(crate) fn damaged(&self) -> Option<(usize, u64)> {
        self.audio.map(|audio| (audio, self.damaged))
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

/// 一个打包音频分片切出的音频帧。
struct Cut {
    /// 音频帧所在的字节区间，相接的合成一段
    ranges: Vec<Range<u64>>,
    /// 跳过的损坏处数
    damaged: u64,
}

/// 一个打包音频分片从 `start`（开头的 ID3 标签之后）起切出音频帧。跳过帧之间的 ID3v2 标签、末尾的 ID3v1 标签，以及
/// MP3 第一帧为 Xing、Info 或 VBRI 标签帧时的这一帧（它不含音频，FFmpeg 读单个文件时同样跳过）。损坏处跳过并计数，与
/// FFmpeg 读裸音频时重新同步一样：认不出的字节（含自由格式的 MP3）、超出分片的帧或标签，跳到下一个同步点（见
/// [`synced`]），没有就到分片末尾。
fn cut_frames(data: &[u8], start: usize, format: Format) -> Cut {
    let mut cut = Cut {
        ranges: Vec::new(),
        damaged: 0,
    };
    let mut at = start;
    let mut first_frame = true;
    while at < data.len() {
        let rest = &data[at..];
        if is_id3v1(rest) {
            break;
        }
        let Some((len, tag)) = item(format, rest).filter(|&(len, _)| len <= rest.len()) else {
            cut.damaged += 1;
            match (at + 1..data.len()).find(|&next| synced(data, next, format)) {
                Some(next) => {
                    at = next;
                    continue;
                }
                None => break,
            }
        };
        let audio = !tag && !(first_frame && format == Format::Mp3 && vbr_tag_frame(rest));
        first_frame &= tag;
        if audio {
            let range = at as u64..(at + len) as u64;
            match cut.ranges.last_mut() {
                Some(last) if last.end == range.start => last.end = range.end,
                _ => cut.ranges.push(range),
            }
        }
        at += len;
    }
    cut
}

/// `data` 是分片末尾的 ID3v1 标签（128 字节，以 `TAG` 开头）。
fn is_id3v1(data: &[u8]) -> bool {
    data.len() == ID3V1_LEN && data.starts_with(b"TAG")
}

/// `data` 开头的 ID3v2 标签或 `format` 的帧：字节数（可能超出 `data`）与是不是标签；都不是时为 None。
fn item(format: Format, data: &[u8]) -> Option<(usize, bool)> {
    match standalone::tag_len(data) {
        Some(len) => Some((usize::try_from(len).ok()?, true)),
        None => frame_len(format, data).map(|len| (len, false)),
    }
}

/// 损坏处之后能从 `at` 重新同步：这里是一个完整的标签或帧，紧接着分片末尾、ID3v1 标签、另一个标签或帧。只看一个帧头
/// 时，损坏的字节里碰巧像帧头的地方会被当成同步点。
fn synced(data: &[u8], at: usize, format: Format) -> bool {
    let Some((len, _)) = item(format, &data[at..]) else {
        return false;
    };
    let next = at + len;
    next == data.len()
        || data
            .get(next..)
            .is_some_and(|rest| is_id3v1(rest) || item(format, rest).is_some())
}

/// `data` 开头一个 `format` 的帧的字节数；帧头不合法或是自由格式的 MP3 时为 None。
fn frame_len(format: Format, data: &[u8]) -> Option<usize> {
    match format {
        Format::Aac => frame::adts(data).map(|(_, len)| len),
        Format::Mp3 => frame::mpeg_audio(data).and_then(|(_, len)| len),
        Format::Ac3 | Format::Eac3 => frame::ac3(data).map(|(_, len)| len),
        Format::Ts => panic!("TS 分片不按打包音频读"),
    }
}

/// MPEG 音频帧是 Xing、Info 或 VBRI 标签帧：只看 Layer III，Xing 与 Info 在帧头之后、按版本与声道数定的偏移处，VBRI
/// 恒在帧头之后 32 字节处（FFmpeg 9.0.2 mp3dec 的 mp3_parse_vbr_tags）。
fn vbr_tag_frame(data: &[u8]) -> bool {
    let Some((header, _)) = frame::mpeg_audio(data) else {
        return false;
    };
    if header.layer != 1 {
        return false;
    }
    let xing = match (header.version == 3, header.mono) {
        (true, false) => 32,
        (true, true) | (false, false) => 17,
        (false, true) => 9,
    };
    let tag = |offset: usize| data.get(4 + offset..8 + offset);
    matches!(tag(xing), Some(b"Xing" | b"Info")) || tag(32) == Some(b"VBRI")
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
    use crate::frame::testing::{ac3_syncframe, adts_frame, mpeg_frame};

    /// [`cut_frames`] 的结果：区间写成（起点, 终点），与跳过的损坏处数。
    fn spans(data: &[u8], start: usize, format: Format) -> (Vec<(u64, u64)>, u64) {
        let cut = cut_frames(data, start, format);
        let ranges = cut.ranges.iter().map(|r| (r.start, r.end)).collect();
        (ranges, cut.damaged)
    }

    /// ID3v2.4 标签，内容为 `body` 字节的 0。
    fn tag(body: usize) -> Vec<u8> {
        let size = [body >> 21, body >> 14, body >> 7, body].map(|b| (b & 0x7F) as u8);
        [b"ID3\x04\x00\x00".as_slice(), &size, &vec![0; body]].concat()
    }

    #[test]
    fn audio_frames_are_cut_out_between_tags() {
        let lead = tag(20);
        let (a, b) = (adts_frame(1, 3, 2, 100), adts_frame(1, 3, 2, 60));
        let inline = tag(30);
        let id3v1 = [b"TAG".as_slice(), &[0; 125]].concat();
        let data = [lead.clone(), a.clone(), b.clone(), inline.clone(), a, id3v1].concat();
        let start = lead.len();
        let second = (start + 160 + inline.len()) as u64;
        assert_eq!(
            spans(&data, start, Format::Aac),
            (
                vec![(start as u64, start as u64 + 160), (second, second + 100)],
                0
            )
        );

        let ac3 = [ac3_syncframe(8, 2, false), ac3_syncframe(8, 2, false)].concat();
        assert_eq!(spans(&ac3, 0, Format::Ac3), (vec![(0, 512)], 0));
    }

    #[test]
    fn damage_is_skipped_up_to_the_next_sync_point_and_counted() {
        let frame = adts_frame(1, 3, 2, 100);
        // 帧之间的杂字节：跳到下一个帧
        let junk = [frame.clone(), b"junk".to_vec(), frame.clone()].concat();
        assert_eq!(
            spans(&junk, 0, Format::Aac),
            (vec![(0, 100), (104, 204)], 1)
        );
        // 末尾不完整的帧与标签：丢到分片末尾
        let truncated = [frame.clone(), frame[..50].to_vec()].concat();
        assert_eq!(spans(&truncated, 0, Format::Aac), (vec![(0, 100)], 1));
        let unfinished_tag = [frame.clone(), tag(30)[..20].to_vec()].concat();
        assert_eq!(spans(&unfinished_tag, 0, Format::Aac), (vec![(0, 100)], 1));
        // AAC 的分片里出现 AC-3 帧；自由格式的 MP3 读不出帧长
        let foreign = [frame.clone(), ac3_syncframe(8, 2, false)].concat();
        assert_eq!(spans(&foreign, 0, Format::Aac), (vec![(0, 100)], 1));
        let free = mpeg_frame(3, 1, 0, 0, 0, 0, 400);
        assert_eq!(spans(&free, 0, Format::Mp3), (vec![], 1));
        // 杂字节里碰巧像帧头、后面接不上帧的地方不当作同步点
        let lookalike = adts_frame(1, 3, 2, 30);
        let data = [
            frame.clone(),
            b"xx".to_vec(),
            lookalike[..10].to_vec(),
            vec![0x55; 40],
            frame,
        ]
        .concat();
        assert_eq!(
            spans(&data, 0, Format::Aac),
            (vec![(0, 100), (152, 252)], 1)
        );
    }

    #[test]
    fn only_a_leading_vbr_tag_frame_is_skipped() {
        // MPEG-1 Layer III 128 kbps 44.1 kHz 立体声：417 字节，Xing 与 Info 在帧头之后 32 字节处
        let audio = mpeg_frame(3, 1, 9, 0, 0, 0, 417);
        let mut info = audio.clone();
        info[36..40].copy_from_slice(b"Info");
        let mut vbri = audio.clone();
        vbri[36..40].copy_from_slice(b"VBRI");
        let lead = tag(10);
        let data = [lead.clone(), info.clone(), audio.clone(), info].concat();
        let start = lead.len() as u64;
        assert_eq!(
            spans(&data, lead.len(), Format::Mp3),
            (vec![(start + 417, start + 3 * 417)], 0)
        );
        let data = [vbri, audio.clone()].concat();
        assert_eq!(spans(&data, 0, Format::Mp3), (vec![(417, 834)], 0));
        // 单声道 MPEG-1 的 Xing 在帧头之后 17 字节处：32 字节处的不算
        let mut mono = mpeg_frame(3, 1, 5, 1, 0, 3, 192);
        mono[36..40].copy_from_slice(b"Xing");
        assert_eq!(spans(&mono, 0, Format::Mp3), (vec![(0, 192)], 0));
        mono[21..25].copy_from_slice(b"Xing");
        assert_eq!(spans(&mono, 0, Format::Mp3), (vec![], 0));
    }

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
