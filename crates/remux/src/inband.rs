//! 参数随帧携带的输入（TS 与打包音频，没有 MP4 那样的样本描述）里每个包的编码参数。MP4 的一条轨只有一个样本描述，
//! 由开头的包得出；同一路流后面的包参数变了，样本描述就与它们不符，如采样率变了时播放的音调与速度都不对。音频的
//! 参数从帧头读出；视频（H.264、HEVC）的画面宽高取 FFmpeg 解复用器的解析器从参数集得出的值。

use ffmpeg::codec;
use ffmpeg::format::stream::Stream;
use ffmpeg_next as ffmpeg;

use crate::ffi;

/// 一路流在一组里的参数核对：每个包的参数须与第一个包的相同。
#[derive(Clone)]
pub(crate) enum Inband {
    Audio {
        read: fn(&[u8]) -> Option<AudioParams>,
        first: Option<AudioParams>,
    },
    Video {
        /// 画面宽高
        first: Option<(u32, u32)>,
    },
}

impl Inband {
    /// 编码 `id` 的核对；参数读不出的编码为 None。
    pub(crate) fn of(id: codec::Id) -> Option<Inband> {
        let read: fn(&[u8]) -> Option<AudioParams> = match id {
            codec::Id::H264 | codec::Id::HEVC => return Some(Inband::Video { first: None }),
            codec::Id::AAC => adts,
            codec::Id::AC3 | codec::Id::EAC3 => ac3,
            codec::Id::MP3 => mpeg_audio,
            _ => return None,
        };
        Some(Inband::Audio { read, first: None })
    }

    /// 核对 `stream` 的一个包 `data`：参数与第一个包的不同、或音频包读不出参数时为 false。解析器还没读到参数集的
    /// 视频包不核对。
    pub(crate) fn check(&mut self, data: &[u8], stream: &Stream<'_>) -> bool {
        match self {
            Inband::Audio { read, first } => read(data)
                .is_some_and(|params| *first.get_or_insert_with(|| params.clone()) == params),
            Inband::Video { first } => {
                ffi::parsed_size(stream).is_none_or(|size| *first.get_or_insert(size) == size)
            }
        }
    }
}

/// 一个音频包的编码参数；只与同一路流的其他包比较。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AudioParams {
    /// ADTS 头的 profile、采样率下标与声道配置：MP4 的 AudioSpecificConfig 由第一帧的这三项写成
    Adts {
        profile: u32,
        sampling_index: u32,
        channel_config: u32,
    },
    /// 包里各同步帧（E-AC-3 为独立帧及其从属帧）
    Ac3(Vec<Ac3Frame>),
    /// MPEG 音频帧头的版本、层、采样率下标与是否单声道；立体声与联合立体声逐帧切换属正常，不比较
    Mpeg {
        version: u32,
        layer: u32,
        sampling_index: u32,
        mono: bool,
    },
}

/// AC-3 或 E-AC-3 一个同步帧的参数（A/52 的 bsi）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ac3Frame {
    /// E-AC-3 的 strmtyp 与 substreamid；AC-3 为 None
    substream: Option<(u32, u32)>,
    /// 采样率：fscod 与另一项（AC-3 为 bsid 超出 8 的部分，即采样率减半的次数；E-AC-3 在 fscod 为 3 时为 fscod2）
    sample_rate: (u32, u32),
    acmod: u32,
    lfe: bool,
    /// E-AC-3 从属帧的 chanmap
    chanmap: Option<u32>,
}

/// 按位从高到低读字节。
struct Bits<'a> {
    data: &'a [u8],
    /// 已读的位数
    at: usize,
}

impl Bits<'_> {
    fn new(data: &[u8]) -> Bits<'_> {
        Bits { data, at: 0 }
    }

    /// 读 `n`（不超过 32）位；数据不够时为 None。
    fn read(&mut self, n: usize) -> Option<u32> {
        let mut value = 0;
        for _ in 0..n {
            let byte = self.data.get(self.at / 8)?;
            value = value << 1 | u32::from(byte >> (7 - self.at % 8) & 1);
            self.at += 1;
        }
        Some(value)
    }

    /// 读 `n` 位，须等于 `expected`。
    fn expect(&mut self, n: usize, expected: u32) -> Option<()> {
        (self.read(n)? == expected).then_some(())
    }
}

/// ADTS 帧头（ISO/IEC 14496-3 1.A.2.2）。
fn adts(data: &[u8]) -> Option<AudioParams> {
    let mut b = Bits::new(data);
    b.expect(12, 0xFFF)?;
    b.read(1)?; // ID
    b.expect(2, 0)?; // layer
    b.read(1)?; // protection_absent
    let profile = b.read(2)?;
    let sampling_index = b.read(4)?;
    b.read(1)?; // private_bit
    let channel_config = b.read(3)?;
    Some(AudioParams::Adts {
        profile,
        sampling_index,
        channel_config,
    })
}

/// 包里各个 AC-3 或 E-AC-3 同步帧；有一帧读不出即为 None。
fn ac3(data: &[u8]) -> Option<AudioParams> {
    let mut frames = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (frame, size) = ac3_frame(rest)?;
        frames.push(frame);
        rest = rest.get(size..)?;
    }
    (!frames.is_empty()).then_some(AudioParams::Ac3(frames))
}

/// 一个同步帧的参数与字节数。AC-3 与 E-AC-3 的 bsid 都在第 6 字节的高 5 位：不超过 10 为 AC-3，11～16 为 E-AC-3
/// （A/52 附录 E）。
fn ac3_frame(data: &[u8]) -> Option<(Ac3Frame, usize)> {
    let bsid = u32::from(data.get(5)? >> 3);
    let mut b = Bits::new(data);
    b.expect(16, 0x0B77)?;
    if bsid <= 10 {
        b.read(16)?; // crc1
        let fscod = b.read(2)?;
        let size = ac3_frame_size(fscod, b.read(6)?)?;
        b.read(5 + 3)?; // bsid、bsmod
        let acmod = b.read(3)?;
        if acmod & 1 != 0 && acmod != 1 {
            b.read(2)?; // cmixlev
        }
        if acmod & 4 != 0 {
            b.read(2)?; // surmixlev
        }
        if acmod == 2 {
            b.read(2)?; // dsurmod
        }
        let frame = Ac3Frame {
            substream: None,
            sample_rate: (fscod, bsid.saturating_sub(8)),
            acmod,
            lfe: b.read(1)? == 1,
            chanmap: None,
        };
        return Some((frame, size));
    }
    if bsid > 16 {
        return None;
    }
    let strmtyp = b.read(2)?;
    let substreamid = b.read(3)?;
    let size = (b.read(11)? as usize + 1) * 2;
    let fscod = b.read(2)?;
    // fscod 为 3 时是 fscod2，否则是 numblkscod（每帧的块数，不影响样本描述）
    let next = b.read(2)?;
    let acmod = b.read(3)?;
    let lfe = b.read(1)? == 1;
    b.read(5)?; // bsid
    // dialnorm 与可选的 compr；acmod 为 0（双单声道）时两套
    for _ in 0..if acmod == 0 { 2 } else { 1 } {
        b.read(5)?;
        if b.read(1)? == 1 {
            b.read(8)?;
        }
    }
    let chanmap = match strmtyp {
        1 if b.read(1)? == 1 => Some(b.read(16)?),
        _ => None,
    };
    let frame = Ac3Frame {
        substream: Some((strmtyp, substreamid)),
        sample_rate: (fscod, if fscod == 3 { next } else { 0 }),
        acmod,
        lfe,
        chanmap,
    };
    Some((frame, size))
}

/// AC-3 同步帧的字节数（A/52 表 5.18）：按码率，44.1 kHz 时奇数的 frmsizecod 多一个字（2 字节）。
fn ac3_frame_size(fscod: u32, frmsizecod: u32) -> Option<usize> {
    const KBPS: [usize; 19] = [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
    ];
    let kbps = *KBPS.get(frmsizecod as usize / 2)?;
    let words = match fscod {
        0 => kbps * 2,
        1 => kbps * 320 / 147 + (frmsizecod as usize & 1),
        2 => kbps * 3,
        _ => return None,
    };
    Some(words * 2)
}

/// MPEG 音频帧头（ISO/IEC 11172-3 2.4.1.3，含 MPEG-2 与 2.5 的版本号）。
fn mpeg_audio(data: &[u8]) -> Option<AudioParams> {
    let mut b = Bits::new(data);
    b.expect(11, 0x7FF)?;
    let version = b.read(2)?;
    let layer = b.read(2)?;
    b.read(1 + 4)?; // protection_bit、bitrate_index
    let sampling_index = b.read(2)?;
    b.read(1 + 1)?; // padding_bit、private_bit
    let mode = b.read(2)?;
    (version != 1 && layer != 0 && sampling_index != 3).then_some(AudioParams::Mpeg {
        version,
        layer,
        sampling_index,
        mono: mode == 3,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把各段（值, 位数）按顺序拼成字节，末尾补 0 到整字节。
    fn bits(fields: &[(u32, usize)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut at = 0;
        for &(value, n) in fields {
            for i in (0..n).rev() {
                if at % 8 == 0 {
                    out.push(0);
                }
                let last = out.last_mut().expect("刚补过字节");
                *last |= u8::from(value >> i & 1 == 1) << (7 - at % 8);
                at += 1;
            }
        }
        out
    }

    fn adts_header(profile: u32, sampling_index: u32, channel_config: u32) -> Vec<u8> {
        bits(&[
            (0xFFF, 12),
            (0, 1),
            (0, 2),
            (1, 1),
            (profile, 2),
            (sampling_index, 4),
            (0, 1),
            (channel_config, 3),
            (0, 28),
        ])
    }

    #[test]
    fn adts_headers_give_profile_rate_and_channels() {
        assert_eq!(
            adts(&adts_header(1, 3, 2)),
            Some(AudioParams::Adts {
                profile: 1,
                sampling_index: 3,
                channel_config: 2
            })
        );
        assert_ne!(adts(&adts_header(1, 3, 2)), adts(&adts_header(1, 4, 2)));
        let mut layer = adts_header(1, 3, 2);
        layer[1] |= 0b10;
        assert_eq!(adts(&layer), None);
        assert_eq!(adts(&[0xFF, 0xF1, 0x50]), None);
    }

    /// AC-3 同步帧：48 kHz、frmsizecod 8（64 kbps，256 字节）。
    fn ac3_syncframe(bsid: u32, acmod: u32, lfe: bool) -> Vec<u8> {
        let mut fields = vec![(0x0B77, 16), (0, 16), (0, 2), (8, 6), (bsid, 5), (0, 3)];
        fields.push((acmod, 3));
        if acmod & 1 != 0 && acmod != 1 {
            fields.push((0, 2));
        }
        if acmod & 4 != 0 {
            fields.push((0, 2));
        }
        if acmod == 2 {
            fields.push((0, 2));
        }
        fields.push((u32::from(lfe), 1));
        let mut frame = bits(&fields);
        frame.resize(256, 0);
        frame
    }

    /// E-AC-3 同步帧，`words` 个 16 位字。
    fn eac3_syncframe(strmtyp: u32, acmod: u32, chanmap: Option<u32>, words: usize) -> Vec<u8> {
        let mut fields = vec![
            (0x0B77, 16),
            (strmtyp, 2),
            (0, 3),
            (words as u32 - 1, 11),
            (0, 2),
            (3, 2),
            (acmod, 3),
            (1, 1),
            (16, 5),
            (27, 5),
            (1, 1),
            (0x55, 8),
        ];
        if let Some(map) = chanmap {
            fields.extend([(1, 1), (map, 16)]);
        }
        let mut frame = bits(&fields);
        frame.resize(words * 2, 0);
        frame
    }

    #[test]
    fn ac3_frames_give_rate_and_channel_layout() {
        let stereo = ac3(&ac3_syncframe(8, 2, false)).unwrap();
        let five_one = ac3(&ac3_syncframe(8, 7, true)).unwrap();
        assert_ne!(stereo, five_one);
        // 第 6 字节的 bsid 为 6（另一种语法）与 8 时参数相同；为 9 时采样率减半
        assert_eq!(ac3(&ac3_syncframe(6, 2, false)), Some(stereo.clone()));
        assert_ne!(ac3(&ac3_syncframe(9, 2, false)), Some(stereo));
        // 帧长对不上包长
        assert_eq!(ac3(&ac3_syncframe(8, 2, false)[..200]), None);
    }

    #[test]
    fn eac3_packets_list_every_substream_frame() {
        let independent = eac3_syncframe(0, 7, None, 384);
        let dependent = eac3_syncframe(1, 2, Some(0x0A00), 128);
        let five_one = ac3(&independent).unwrap();
        let seven_one = ac3(&[independent.clone(), dependent.clone()].concat()).unwrap();
        assert_ne!(five_one, seven_one);
        let other_map = eac3_syncframe(1, 2, Some(0x0500), 128);
        assert_ne!(
            ac3(&[independent.clone(), other_map].concat()),
            Some(seven_one)
        );
        // 每帧块数（numblkscod）不同不算参数变化
        let mut fewer_blocks = independent.clone();
        fewer_blocks[4] &= !0b0011_0000;
        assert_eq!(ac3(&fewer_blocks), Some(five_one));
        // AC-3 帧之后跟 E-AC-3 从属帧
        let core = ac3_syncframe(8, 7, true);
        assert!(matches!(
            ac3(&[core, dependent].concat()),
            Some(AudioParams::Ac3(frames)) if frames.len() == 2
        ));
    }

    #[test]
    fn ac3_frame_sizes_follow_the_bitrate_table() {
        assert_eq!(ac3_frame_size(0, 0), Some(128));
        assert_eq!(ac3_frame_size(1, 0), Some(138));
        assert_eq!(ac3_frame_size(1, 1), Some(140));
        assert_eq!(ac3_frame_size(1, 37), Some(2788));
        assert_eq!(ac3_frame_size(2, 37), Some(3840));
        assert_eq!(ac3_frame_size(0, 38), None);
        assert_eq!(ac3_frame_size(3, 0), None);
    }

    #[test]
    fn mpeg_audio_headers_ignore_stereo_mode_switches() {
        let header = |mode: u32| {
            bits(&[
                (0x7FF, 11),
                (3, 2),
                (1, 2),
                (1, 1),
                (9, 4),
                (0, 2),
                (0, 2),
                (mode, 2),
                (0, 6),
            ])
        };
        assert_eq!(mpeg_audio(&header(0)), mpeg_audio(&header(1)));
        assert_ne!(mpeg_audio(&header(0)), mpeg_audio(&header(3)));
        assert!(mpeg_audio(&header(0)).is_some());
        let mut reserved_rate = header(0);
        reserved_rate[2] |= 0b1100;
        assert_eq!(mpeg_audio(&reserved_rate), None);
    }
}
