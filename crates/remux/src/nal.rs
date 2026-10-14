//! H.264 与 HEVC 视频包里的 NAL 单元：判断一个包能否作为一组的解码起点、是不是参考上一个 GOP 的前导帧（见
//! [`crate::source::Source`]）。FFmpeg 的关键帧标志不够用：H.264 只给 IDR、带 recovery point SEI 的帧与参考帧不超过
//! 一个的 I 帧打标志（FFmpeg 9.0.2 h264_parser.c），以不带 SEI 的非 IDR I 帧开头的开放 GOP 认不出；HEVC 的前导帧要按
//! NAL 类型分，RASL 参考上一个 GOP，RADL 只参考本 GOP、解得出。

use crate::bits::Bits;

/// 视频包里 NAL 单元的排法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// 以起始码 00 00 01 分隔（TS 里的 Annex B 字节流）
    AnnexB,
    /// 每个 NAL 前是这么多字节的大端长度（MP4 的样本，字节数取自 avcC、hvcC）
    LengthPrefixed(usize),
}

impl Framing {
    /// MP4 的解码配置（avcC 或 hvcC）里 NAL 长度的字节数：avcC 第 4 字节、hvcC 第 21 字节的低 2 位加 1（ISO/IEC
    /// 14496-15 5.3.3.1、8.3.3.1）。配置太短时为 None。
    pub(crate) fn of_config(config: &[u8], hevc: bool) -> Option<Framing> {
        let at = if hevc { 21 } else { 4 };
        Some(Framing::LengthPrefixed(
            usize::from(config.get(at)? & 0b11) + 1,
        ))
    }
}

/// 包 `data` 里的各 NAL 单元（含 NAL 头）；长度前缀超出数据时截到末尾。
fn units(data: &[u8], framing: Framing) -> Vec<&[u8]> {
    match framing {
        Framing::AnnexB => {
            let starts: Vec<usize> = data
                .windows(3)
                .enumerate()
                .filter(|(_, w)| *w == [0, 0, 1])
                .map(|(i, _)| i + 3)
                .collect();
            starts
                .iter()
                .enumerate()
                .map(|(n, &start)| {
                    // 到下一个起始码之前；4 字节起始码前多出的 0 不属于这个 NAL
                    let end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
                    let mut unit = &data[start..end.max(start)];
                    while let [rest @ .., 0] = unit {
                        unit = rest;
                    }
                    unit
                })
                .filter(|unit| !unit.is_empty())
                .collect()
        }
        Framing::LengthPrefixed(size) => {
            let mut units = Vec::new();
            let mut rest = data;
            while rest.len() >= size {
                let (len, tail) = rest.split_at(size);
                let len = len.iter().fold(0usize, |n, &b| n << 8 | usize::from(b));
                let len = len.min(tail.len());
                units.push(&tail[..len]);
                rest = &tail[len..];
            }
            units
        }
    }
}

/// 去掉防竞争字节（00 00 03 里的 03，H.264 7.4.1），只取开头 `limit` 字节：slice 头的前几个字段就在开头。
fn rbsp(unit: &[u8], limit: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(limit);
    let mut zeros = 0;
    for &byte in unit {
        if out.len() >= limit {
            break;
        }
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// H.264 包是一整帧 I（或 SI）slice：至少一个 slice，每个都是。I 帧之后的帧只参考它与它之后的帧时，从它开始解得出；
/// 解码顺序在它之后、呈现在它之前的帧另按前导帧处理。
pub(crate) fn h264_intra(data: &[u8], framing: Framing) -> bool {
    let mut slices = 0;
    for unit in units(data, framing) {
        // nal_unit_type：1 为非 IDR 的 slice，5 为 IDR 的 slice
        if !matches!(unit[0] & 0x1F, 1 | 5) {
            continue;
        }
        slices += 1;
        let header = rbsp(&unit[1..], 16);
        let mut bits = Bits::new(&header);
        let slice_type = bits.read_ue().and_then(|_first_mb| bits.read_ue());
        // slice_type：2、7 为 I，4、9 为 SI（H.264 表 7-6）
        if !matches!(slice_type, Some(2 | 4 | 7 | 9)) {
            return false;
        }
    }
    slices > 0
}

/// HEVC 包的图像是 RASL（NAL 类型 8、9，H.265 表 7-1）：随机访问点之后、参考它之前的图像，从这个随机访问点开始解码时
/// 解不对。看第一个 VCL NAL（类型 0～31）。
pub(crate) fn hevc_rasl(data: &[u8], framing: Framing) -> bool {
    units(data, framing)
        .into_iter()
        .map(|unit| unit[0] >> 1 & 0x3F)
        .find(|&kind| kind < 32)
        .is_some_and(|kind| matches!(kind, 8 | 9))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::testing::bits;

    /// H.264 的 slice NAL：NAL 头 `header`，slice 头开头的 first_mb_in_slice 为 0、slice_type 为 `slice_type`。
    fn h264_slice(header: u8, slice_type: u32) -> Vec<u8> {
        // ue(v)：值 v 写成 (v+1) 的二进制，前面补与它位数减一相同个数的 0
        let ue = |v: u32| {
            let n = 32 - (v + 1).leading_zeros() as usize;
            [(0, n - 1), (v + 1, n)]
        };
        let [zeros, value] = ue(slice_type);
        let mut fields = vec![(1, 1)];
        fields.extend([zeros, value]);
        [vec![header], bits(&fields), vec![0x80]].concat()
    }

    fn annexb(units: &[Vec<u8>]) -> Vec<u8> {
        units
            .iter()
            .flat_map(|u| [[0, 0, 0, 1].as_slice(), u].concat())
            .collect()
    }

    fn length_prefixed(units: &[Vec<u8>]) -> Vec<u8> {
        units
            .iter()
            .flat_map(|u| [(u.len() as u32).to_be_bytes().as_slice(), u].concat())
            .collect()
    }

    #[test]
    fn h264_frames_made_of_intra_slices_are_recognised() {
        let i = h264_slice(0x01, 2);
        let i_all = h264_slice(0x01, 7);
        let p = h264_slice(0x01, 0);
        let idr = h264_slice(0x65, 7);
        // SPS、PPS 与 SEI 不是 slice
        let sps = vec![0x67, 0x64, 0x00, 0x1F];
        let sei = vec![0x06, 0x05, 0x01, 0x00];
        assert!(h264_intra(
            &annexb(&[sps.clone(), sei.clone(), i.clone()]),
            Framing::AnnexB
        ));
        assert!(h264_intra(
            &annexb(&[i_all.clone(), i.clone()]),
            Framing::AnnexB
        ));
        assert!(h264_intra(&annexb(&[idr]), Framing::AnnexB));
        assert!(!h264_intra(
            &annexb(&[i.clone(), p.clone()]),
            Framing::AnnexB
        ));
        assert!(!h264_intra(&annexb(&[sps, sei]), Framing::AnnexB));
        assert!(h264_intra(
            &length_prefixed(std::slice::from_ref(&i)),
            Framing::LengthPrefixed(4)
        ));
        assert!(!h264_intra(
            &length_prefixed(&[p]),
            Framing::LengthPrefixed(4)
        ));
        // 3 字节起始码，NAL 之间有多余的 0
        let short = [&[0, 0, 1][..], &i, &[0, 0][..], &[0, 0, 1][..], &i_all].concat();
        assert!(h264_intra(&short, Framing::AnnexB));
    }

    #[test]
    fn emulation_prevention_bytes_are_removed_before_reading_the_slice_header() {
        assert_eq!(rbsp(&[0, 0, 3, 1, 0, 0, 3, 0], 16), [0, 0, 1, 0, 0, 0]);
        assert_eq!(rbsp(&[5, 0, 0, 3, 0, 9], 3), [5, 0, 0]);
    }

    #[test]
    fn hevc_leading_pictures_are_told_apart_by_nal_type() {
        // HEVC 的 NAL 头：forbidden_zero_bit、6 位类型、6 位层号、3 位时间层号加 1
        let nal = |kind: u8| vec![kind << 1, 0x01, 0xAF];
        let vps = nal(32);
        for (kind, rasl) in [
            (8, true),
            (9, true),
            (6, false),
            (7, false),
            (21, false),
            (1, false),
        ] {
            assert_eq!(
                hevc_rasl(&annexb(&[vps.clone(), nal(kind)]), Framing::AnnexB),
                rasl,
                "NAL 类型 {kind}"
            );
            assert_eq!(
                hevc_rasl(&length_prefixed(&[nal(kind)]), Framing::LengthPrefixed(4)),
                rasl,
                "NAL 类型 {kind}"
            );
        }
        assert_eq!(
            Framing::of_config(&[0; 23].map(|_| 0xFF), true),
            Some(Framing::LengthPrefixed(4))
        );
        assert_eq!(
            Framing::of_config(&[1, 0x64, 0, 0x1F, 0xFD], false),
            Some(Framing::LengthPrefixed(2))
        );
        assert_eq!(Framing::of_config(&[1, 0x64], false), None);
    }
}
