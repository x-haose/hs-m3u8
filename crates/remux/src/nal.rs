//! HEVC 视频包里的 NAL 单元：判断一个包是不是参考上一个 GOP 的前导帧（见 [`crate::source::Source`]）。FFmpeg 的关键帧
//! 标志只给出随机访问点；它之后、呈现在它之前的前导帧要按 NAL 类型分：RASL 参考上一个 GOP，从这个随机访问点开始解码时
//! 解不对；RADL 只参考本 GOP，解得出。

/// HEVC 视频包里 NAL 单元的排法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// 以起始码 00 00 01 分隔（TS 里的 Annex B 字节流）
    AnnexB,
    /// 每个 NAL 前是这么多字节的大端长度（MP4 的样本，字节数取自 hvcC）
    LengthPrefixed(usize),
}

impl Framing {
    /// MP4 的 HEVC 解码配置（hvcC）里 NAL 长度的字节数：下标 21（从 0 起）那一字节的低 2 位加 1（ISO/IEC 14496-15
    /// 8.3.3.1）。配置太短时为 None。
    pub(crate) fn of_hvcc(config: &[u8]) -> Option<Framing> {
        Some(Framing::LengthPrefixed(
            usize::from(config.get(21)? & 0b11) + 1,
        ))
    }
}

/// 包 `data` 里的各 NAL 单元（含 NAL 头，都不为空：长度为 0 的跳过）；长度前缀超出数据时截到末尾。
fn units(data: &[u8], framing: Framing) -> Vec<&[u8]> {
    let units = match framing {
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
    };
    units.into_iter().filter(|unit| !unit.is_empty()).collect()
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

    /// HEVC 的 NAL：NAL 头为 forbidden_zero_bit、6 位类型、6 位层号、3 位时间层号加 1，后跟一字节数据。
    fn nal(kind: u8) -> Vec<u8> {
        vec![kind << 1, 0x01, 0xAF]
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
    fn hevc_leading_pictures_are_told_apart_by_nal_type() {
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
        // 3 字节起始码，NAL 之间有多余的 0
        let short = [
            &[0, 0, 1][..],
            &nal(32),
            &[0, 0][..],
            &[0, 0, 1][..],
            &nal(8),
        ]
        .concat();
        assert!(hevc_rasl(&short, Framing::AnnexB));
    }

    /// 长度写 0 的 NAL、末尾补的 0 不算单元：跳过，后面的照常认。
    #[test]
    fn empty_nal_units_are_skipped() {
        let data = [
            &[0, 0, 0, 0][..],
            &length_prefixed(&[nal(9)]),
            &[0, 0, 0, 0][..],
        ]
        .concat();
        assert!(hevc_rasl(&data, Framing::LengthPrefixed(4)));
        assert!(!hevc_rasl(&[0, 0, 0, 0], Framing::LengthPrefixed(4)));
    }

    #[test]
    fn nal_length_size_comes_from_the_hvcc_configuration() {
        assert_eq!(
            Framing::of_hvcc(&[0xFF; 23]),
            Some(Framing::LengthPrefixed(4))
        );
        let mut two = [0u8; 23];
        two[21] = 0xFD;
        assert_eq!(Framing::of_hvcc(&two), Some(Framing::LengthPrefixed(2)));
        assert_eq!(Framing::of_hvcc(&[1, 2, 3]), None);
    }
}
