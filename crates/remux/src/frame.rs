//! 裸音频帧的帧头：ADTS（AAC）、AC-3 与 E-AC-3、MPEG 音频。读出编码参数（逐包核对用，见 [`crate::inband`]）与整帧的
//! 字节数（打包音频按它切帧，见 [`crate::packed`]）。合法性的判定与链接的 FFmpeg 的帧头解析一致，帧头不合法时为 None；
//! 从字节里认 ADTS 另有要求（见 [`is_adts`]）。另读 MP4 里 AAC 解码配置的编码类型。

/// ADTS 帧头里 MP4 的 AudioSpecificConfig 由之写成的三项（ISO/IEC 14496-3 1.A.2.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Adts {
    pub profile: u32,
    pub sampling_index: u32,
    pub channel_config: u32,
}

/// 一个 ADTS 帧的帧头与整帧的字节数（含帧头）。
pub(crate) fn adts(data: &[u8]) -> Option<(Adts, usize)> {
    adts_with_layer(data).map(|(adts, _, len)| (adts, len))
}

/// 从字节里认格式时 `data` 开头是不是 ADTS 帧头：另要求 layer 为 0（与链接的 FFmpeg 探测 AAC 的 adts_aac_probe 一致），
/// 否则与 MPEG 音频的同步字分不开。
pub(crate) fn is_adts(data: &[u8]) -> bool {
    adts_with_layer(data).is_some_and(|(_, layer, _)| layer == 0)
}

/// [`adts`] 另带帧头里的 layer：FFmpeg 解析帧头时不查它。
fn adts_with_layer(data: &[u8]) -> Option<(Adts, u32, usize)> {
    let mut b = Bits::new(data);
    b.expect(12, 0xFFF)?;
    b.read(1)?; // ID
    let layer = b.read(2)?;
    b.read(1)?; // protection_absent
    let profile = b.read(2)?;
    let sampling_index = b.read(4)?;
    b.read(1)?; // private_bit
    let channel_config = b.read(3)?;
    b.read(4)?; // original_copy、home、copyright_identification_bit 与 _start
    let len = b.read(13)? as usize;
    // 采样率下标 13～15 为保留值；帧长不短于 7 字节的帧头
    (sampling_index <= 12 && len >= 7).then_some((
        Adts {
            profile,
            sampling_index,
            channel_config,
        },
        layer,
        len,
    ))
}

/// AC-3 或 E-AC-3 一个同步帧的参数（A/52 的 bsi）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ac3Frame {
    /// E-AC-3 的 strmtyp 与 substreamid；AC-3 为 None
    pub substream: Option<(u32, u32)>,
    /// 采样率：fscod 与另一项（AC-3 为 bsid 超出 8 的部分，即采样率减半的次数；E-AC-3 在 fscod 为 3 时为 fscod2）
    pub sample_rate: (u32, u32),
    pub acmod: u32,
    pub lfe: bool,
    /// E-AC-3 从属帧的 chanmap
    pub chanmap: Option<u32>,
    /// 每帧的块数（每块 256 个采样点）；AC-3 恒为 6
    pub blocks: u32,
}

/// 一个 AC-3 或 E-AC-3 同步帧的参数与字节数。两者的 bsid 都在第 6 字节的高 5 位：不超过 10 为 AC-3，11～16 为
/// E-AC-3（A/52 附录 E）。
pub(crate) fn ac3(data: &[u8]) -> Option<(Ac3Frame, usize)> {
    let bsid = u32::from(data.get(5)? >> 3);
    let mut b = Bits::new(data);
    b.expect(16, 0x0B77)?;
    match bsid {
        0..=10 => ac3_after_sync(&mut b, bsid),
        11..=16 => eac3_after_sync(&mut b),
        _ => None,
    }
}

/// AC-3 同步帧在同步字之后的部分；`bsid` 为预先读出的（见 [`ac3`]）。
fn ac3_after_sync(b: &mut Bits<'_>, bsid: u32) -> Option<(Ac3Frame, usize)> {
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
        blocks: 6,
    };
    Some((frame, size))
}

/// E-AC-3 同步帧在同步字之后的部分。
fn eac3_after_sync(b: &mut Bits<'_>) -> Option<(Ac3Frame, usize)> {
    let strmtyp = b.read(2)?;
    let substreamid = b.read(3)?;
    let size = (b.read(11)? as usize + 1) * 2;
    let fscod = b.read(2)?;
    // fscod 为 3 时是 fscod2（每帧 6 块），否则是 numblkscod
    let next = b.read(2)?;
    // strmtyp 3 与 fscod2 为 3 是保留值；帧长不短于 7 字节
    if strmtyp == 3 || (fscod == 3 && next == 3) || size < 7 {
        return None;
    }
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
        blocks: if fscod == 3 {
            6
        } else {
            [1, 2, 3, 6][next as usize]
        },
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

/// MPEG 音频帧头的版本、层、采样率下标与是否单声道；立体声与联合立体声逐帧切换属正常，不在其中。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mpeg {
    /// 0 为 MPEG-2.5，2 为 MPEG-2，3 为 MPEG-1
    pub version: u32,
    /// 1 为 Layer III，2 为 Layer II，3 为 Layer I
    pub layer: u32,
    pub sampling_index: u32,
    pub mono: bool,
}

/// 各版本、各层的码率，kbps，下标为 bitrate_index（链接的 FFmpeg 的 ff_mpa_bitrate_tab）：先 MPEG-1，后 MPEG-2 与
/// 2.5；每组依次为 Layer I、II、III。
const MPEG_KBPS: [[[usize; 15]; 3]; 2] = [
    [
        [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ],
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
        ],
        [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ],
    ],
    [
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
        ],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ],
];

/// 一个 MPEG 音频帧（ISO/IEC 11172-3 2.4.1.3，含 MPEG-2 与 2.5）的帧头与整帧的字节数。自由格式（bitrate_index 为 0）
/// 的帧长要找下一个同步字才知道，为 None。
pub(crate) fn mpeg_audio(data: &[u8]) -> Option<(Mpeg, Option<usize>)> {
    let mut b = Bits::new(data);
    b.expect(11, 0x7FF)?;
    let version = b.read(2)?;
    let layer = b.read(2)?;
    b.read(1)?; // protection_bit
    let bitrate_index = b.read(4)? as usize;
    let sampling_index = b.read(2)?;
    let padding = b.read(1)? as usize;
    b.read(1)?; // private_bit
    let mode = b.read(2)?;
    if version == 1 || layer == 0 || bitrate_index == 15 || sampling_index == 3 {
        return None;
    }
    let header = Mpeg {
        version,
        layer,
        sampling_index,
        mono: mode == 3,
    };
    if bitrate_index == 0 {
        return Some((header, None));
    }
    // MPEG-2 与 2.5 采样率减半（2.5 再减半），Layer III 每帧的采样点数也减半
    let lsf = usize::from(version != 3);
    let rate =
        [44_100, 48_000, 32_000][sampling_index as usize] >> (lsf + usize::from(version == 0));
    let kbps = MPEG_KBPS[lsf][3 - layer as usize][bitrate_index];
    let len = match layer {
        3 => (kbps * 12_000 / rate + padding) * 4,
        2 => kbps * 144_000 / rate + padding,
        _ => kbps * 144_000 / (rate << lsf) + padding,
    };
    Some((header, Some(len)))
}

/// 裸音频帧的帧头格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// ADTS（AAC）
    Adts,
    /// AC-3 与 E-AC-3
    Ac3,
    /// MPEG 音频（MP3 等）
    Mpeg,
}

/// 一帧的帧头里比较是否同一路流的参数（打包音频重新同步时用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Header {
    Adts(Adts),
    Ac3(Ac3Frame),
    Mpeg(Mpeg),
}

/// `data` 开头一个 `framing` 格式的帧的帧头与整帧的字节数；帧头不合法时为 None，帧长读不出（自由格式的 MP3）时字节数
/// 为 None。
pub(crate) fn header(framing: Framing, data: &[u8]) -> Option<(Header, Option<usize>)> {
    match framing {
        Framing::Adts => adts(data).map(|(h, len)| (Header::Adts(h), Some(len))),
        Framing::Ac3 => ac3(data).map(|(f, len)| (Header::Ac3(f), Some(len))),
        Framing::Mpeg => mpeg_audio(data).map(|(h, len)| (Header::Mpeg(h), len)),
    }
}

/// audioObjectType 里 USAC 的值（ISO/IEC 14496-3 表 1.1）。
pub(crate) const AOT_USAC: u32 = 42;

/// MP4 里 AAC 的解码配置（AudioSpecificConfig）开头的 audioObjectType（ISO/IEC 14496-3 1.6.2.1）：5 位，为 31 时
/// 取其后 6 位加 32。数据不够时为 None。
pub(crate) fn audio_object_type(config: &[u8]) -> Option<u32> {
    let mut b = Bits::new(config);
    match b.read(5)? {
        31 => Some(32 + b.read(6)?),
        aot => Some(aot),
    }
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

/// 拼测试用的帧，供本 crate 各模块的测试共用。
#[cfg(test)]
pub(crate) mod testing {
    /// 把各段（值, 位数）按顺序拼成字节，末尾补 0 到整字节。
    pub(crate) fn bits(fields: &[(u32, usize)]) -> Vec<u8> {
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

    /// `len` 字节的 ADTS 帧（没有 CRC），帧头之后补 0。
    pub(crate) fn adts_frame(
        profile: u32,
        sampling_index: u32,
        channel_config: u32,
        len: usize,
    ) -> Vec<u8> {
        let mut frame = bits(&[
            (0xFFF, 12),
            (0, 1),
            (0, 2),
            (1, 1),
            (profile, 2),
            (sampling_index, 4),
            (0, 1),
            (channel_config, 3),
            (0, 4),
            (len as u32, 13),
            (0x7FF, 11),
            (0, 2),
        ]);
        frame.resize(len, 0);
        frame
    }

    /// AC-3 同步帧：48 kHz、frmsizecod 8（64 kbps，256 字节）。
    pub(crate) fn ac3_syncframe(bsid: u32, acmod: u32, lfe: bool) -> Vec<u8> {
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

    /// E-AC-3 同步帧，48 kHz、每帧 6 块，`words` 个 16 位字。
    pub(crate) fn eac3_syncframe(
        strmtyp: u32,
        acmod: u32,
        chanmap: Option<u32>,
        words: usize,
    ) -> Vec<u8> {
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

    /// MPEG 音频帧：版本、层、码率下标、采样率下标、padding 与声道模式，帧头之后补 0 到 `len` 字节。
    pub(crate) fn mpeg_frame(
        version: u32,
        layer: u32,
        bitrate_index: u32,
        sampling_index: u32,
        padding: u32,
        mode: u32,
        len: usize,
    ) -> Vec<u8> {
        let mut frame = bits(&[
            (0x7FF, 11),
            (version, 2),
            (layer, 2),
            (1, 1),
            (bitrate_index, 4),
            (sampling_index, 2),
            (padding, 1),
            (0, 1),
            (mode, 2),
            (0, 6),
        ]);
        frame.resize(len, 0);
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn adts_headers_give_the_config_and_frame_length() {
        assert_eq!(
            adts(&adts_frame(1, 3, 2, 300)),
            Some((
                Adts {
                    profile: 1,
                    sampling_index: 3,
                    channel_config: 2
                },
                300
            ))
        );
        assert!(is_adts(&adts_frame(1, 3, 2, 300)));
        // 采样率下标 13 为保留值；帧头不完整；帧长短于帧头；layer 不为 0 时照读，认格式时不认
        assert_eq!(adts(&adts_frame(1, 13, 2, 300)), None);
        assert_eq!(adts(&adts_frame(1, 3, 2, 300)[..5]), None);
        let mut short = adts_frame(1, 3, 2, 300);
        short[3] &= 0xFC;
        short[4] = 0;
        short[5] &= 0x1F;
        assert_eq!(adts(&short), None);
        let mut layer = adts_frame(1, 3, 2, 300);
        layer[1] |= 0b10;
        assert_eq!(adts(&layer).map(|(_, len)| len), Some(300));
        assert!(!is_adts(&layer));
    }

    #[test]
    fn ac3_frames_give_rate_layout_and_size() {
        let (stereo, size) = ac3(&ac3_syncframe(8, 2, false)).unwrap();
        assert_eq!(size, 256);
        let (five_one, _) = ac3(&ac3_syncframe(8, 7, true)).unwrap();
        assert_ne!(stereo, five_one);
        // 第 6 字节的 bsid 为 6（另一种语法）与 8 时参数相同；为 9 时采样率减半
        assert_eq!(ac3(&ac3_syncframe(6, 2, false)).unwrap().0, stereo);
        assert_ne!(ac3(&ac3_syncframe(9, 2, false)).unwrap().0, stereo);
        // bsid 超出 16
        assert_eq!(ac3(&ac3_syncframe(17, 2, false)), None);
    }

    #[test]
    fn eac3_frames_give_substream_layout_blocks_and_size() {
        let (independent, size) = ac3(&eac3_syncframe(0, 7, None, 384)).unwrap();
        assert_eq!(size, 768);
        assert_eq!(independent.substream, Some((0, 0)));
        assert_eq!(independent.blocks, 6);
        let (dependent, _) = ac3(&eac3_syncframe(1, 2, Some(0x0A00), 128)).unwrap();
        assert_eq!(dependent.chanmap, Some(0x0A00));
        // numblkscod 为 2：每帧 3 块
        let mut three_blocks = eac3_syncframe(0, 7, None, 384);
        three_blocks[4] &= !0b0001_0000;
        assert_eq!(ac3(&three_blocks).unwrap().0.blocks, 3);
        // strmtyp 3 与 fscod2 为 3 是保留值
        let mut reserved_type = eac3_syncframe(0, 7, None, 384);
        reserved_type[2] |= 0b1100_0000;
        assert_eq!(ac3(&reserved_type), None);
        let mut reserved_rate = eac3_syncframe(0, 7, None, 384);
        reserved_rate[4] |= 0b1111_0000;
        assert_eq!(ac3(&reserved_rate), None);
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
    fn mpeg_audio_frame_lengths_follow_version_layer_and_rate() {
        let len = |version, layer, bitrate, rate, padding, mode| {
            mpeg_audio(&mpeg_frame(version, layer, bitrate, rate, padding, mode, 4)).map(|h| h.1)
        };
        // MPEG-1 Layer III 128 kbps 44.1 kHz，有无 padding
        assert_eq!(len(3, 1, 9, 0, 0, 0), Some(Some(417)));
        assert_eq!(len(3, 1, 9, 0, 1, 0), Some(Some(418)));
        // MPEG-1 Layer III 64 kbps 48 kHz；MPEG-2 Layer III 64 kbps 24 kHz（每帧采样点数减半）
        assert_eq!(len(3, 1, 5, 1, 0, 3), Some(Some(192)));
        assert_eq!(len(2, 1, 8, 1, 0, 3), Some(Some(192)));
        // MPEG-2.5 Layer III 8 kbps 8 kHz；MPEG-1 Layer II 192 kbps 48 kHz；Layer I 32 kbps 32 kHz
        assert_eq!(len(0, 1, 1, 2, 0, 3), Some(Some(72)));
        assert_eq!(len(3, 2, 10, 1, 0, 0), Some(Some(576)));
        assert_eq!(len(3, 3, 1, 2, 0, 0), Some(Some(48)));
        // 自由格式的帧头合法、帧长不知道；码率下标 15、版本 1、层 0、采样率下标 3 不合法
        assert_eq!(len(3, 1, 0, 0, 0, 0), Some(None));
        assert_eq!(len(3, 1, 15, 0, 0, 0), None);
        assert_eq!(len(1, 1, 9, 0, 0, 0), None);
        assert_eq!(len(3, 0, 9, 0, 0, 0), None);
        assert_eq!(len(3, 1, 9, 3, 0, 0), None);
    }

    #[test]
    fn mpeg_audio_headers_ignore_stereo_mode_switches() {
        let header = |mode| mpeg_audio(&mpeg_frame(3, 1, 9, 0, 0, mode, 4)).unwrap().0;
        assert_eq!(header(0), header(1));
        assert_ne!(header(0), header(3));
    }

    #[test]
    fn audio_object_types_read_the_escape() {
        // LC（2）、显式 SBR（5）；31 之后 6 位为 10，即 USAC；转义的 6 位不完整
        assert_eq!(audio_object_type(&bits(&[(2, 5), (3, 4)])), Some(2));
        assert_eq!(audio_object_type(&bits(&[(5, 5), (6, 4)])), Some(5));
        assert_eq!(
            audio_object_type(&bits(&[(31, 5), (10, 6)])),
            Some(AOT_USAC)
        );
        assert_eq!(audio_object_type(&[0xF8]), None);
    }
}
