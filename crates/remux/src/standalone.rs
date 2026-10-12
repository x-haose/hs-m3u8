//! 没有 init 段的分片：MPEG-TS，或 RFC 8216 3.4 的打包音频（裸的 AAC（ADTS）、MP3、AC-3 或 E-AC-3 帧，前面可有
//! 多个 ID3v2 标签）。打包音频首个样本的 PTS 写在 owner 为 `com.apple.streaming.transportStreamTimestamp` 的 ID3
//! PRIV 帧里（90 kHz、33 位，与 TS 同一个时钟）。

use std::io::{self, Read, Seek, SeekFrom};

/// 没有 init 段的分片的格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Ts,
    Aac,
    Mp3,
    Ac3,
    Eac3,
}

impl Format {
    /// 全部格式。
    pub const ALL: [Format; 5] = [
        Format::Ts,
        Format::Aac,
        Format::Mp3,
        Format::Ac3,
        Format::Eac3,
    ];
}

/// 分片开头的信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub format: Format,
    /// 开头的 ID3 标签之后数据的起点，字节；TS 为 0
    pub data_start: u64,
    /// 打包音频首个样本的 PTS（90 kHz，未展开的 33 位值），取第一个带它的标签；TS 或没有时为 None
    pub timestamp: Option<i64>,
}

/// PRIV 帧里时间戳的 owner。
const TIMESTAMP_OWNER: &[u8] = b"com.apple.streaming.transportStreamTimestamp";

/// 时间戳 PRIV 帧的内容长度：owner、NUL 与 8 字节的值。
const TIMESTAMP_FRAME_LEN: usize = TIMESTAMP_OWNER.len() + 1 + 8;

/// 识别格式要看的开头字节数：TS 的前两个包。
const PROBE_LEN: usize = 376;

/// 读出分片开头的信息：TS 要偏移 0 处、长度够时偏移 188 处都是同步字节 0x47；打包音频要 ID3 标签之后是 ADTS、
/// MPEG 音频或 AC-3 / E-AC-3 的同步字。都不是（含标签不完整、长度字段不合法）时为 None。只读开头与标签里的帧头，
/// 大标签（如带封面图）不整个读进来。
pub fn read_head(reader: &mut (impl Read + Seek)) -> io::Result<Option<Head>> {
    let mut start = 0u64;
    let mut timestamp = None;
    loop {
        reader.seek(SeekFrom::Start(start))?;
        let mut probe = [0u8; PROBE_LEN];
        let read = read_up_to(reader, &mut probe)?;
        let data = &probe[..read];
        if data.starts_with(b"ID3") {
            let Some(tag) = TagHeader::parse(data) else {
                return Ok(None);
            };
            if timestamp.is_none() {
                timestamp = tag.timestamp(reader, start)?;
            }
            start += tag.len();
            continue;
        }
        let format = match data {
            [0x47, ..] if start == 0 => {
                return Ok((data.len() < 2 * 188 || data[188] == 0x47).then_some(Head {
                    format: Format::Ts,
                    data_start: 0,
                    timestamp: None,
                }));
            }
            // ADTS：12 位同步字，layer 为 0
            [0xFF, b, ..] if b & 0xF6 == 0xF0 => Format::Aac,
            // MPEG 音频帧：11 位同步字，layer 不为 0
            [0xFF, b, ..] if b & 0xE0 == 0xE0 && b & 0x06 != 0 => Format::Mp3,
            // AC-3 与 E-AC-3 同步字相同，按第 6 字节高 5 位的 bsid 区分（≤ 10 为 AC-3，11..=16 为 E-AC-3）
            [0x0B, 0x77, _, _, _, b, ..] => match b >> 3 {
                0..=10 => Format::Ac3,
                11..=16 => Format::Eac3,
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        return Ok(Some(Head {
            format,
            data_start: start,
            timestamp,
        }));
    }
}

/// 读满 `buf` 或读到末尾，返回读到的字节数。
fn read_up_to(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// ID3v2 标签头（10 字节）。
struct TagHeader {
    version: u8,
    flags: u8,
    /// 头与尾部之间的字节数：扩展头、各帧与填充
    body: u64,
}

impl TagHeader {
    /// 头不完整或长度字段不合法时为 None。
    fn parse(data: &[u8]) -> Option<TagHeader> {
        let header = data.get(..10).filter(|h| h.starts_with(b"ID3"))?;
        Some(TagHeader {
            version: header[3],
            flags: header[5],
            body: syncsafe(&header[6..10])?,
        })
    }

    /// 标签总长，含头与尾部。
    fn len(&self) -> u64 {
        let footer = if self.flags & 0x10 != 0 { 10 } else { 0 };
        10 + self.body + footer
    }

    /// 从 `start` 开始的这个标签里时间戳 PRIV 帧的值：8 字节大端数的低 33 位。只认 v2.3 与 v2.4；帧长度超出标签等
    /// 写法不合规时为 None。
    fn timestamp(&self, reader: &mut (impl Read + Seek), start: u64) -> io::Result<Option<i64>> {
        if !matches!(self.version, 3 | 4) {
            return Ok(None);
        }
        let end = start + 10 + self.body;
        let mut at = start + 10;
        // 扩展头：v2.4 的长度为 syncsafe 且含长度字段本身，v2.3 的为普通整数且不含长度字段
        if self.flags & 0x40 != 0 {
            let Some(size) = read_at::<4>(reader, at, end)? else {
                return Ok(None);
            };
            let skip = match self.version {
                4 => syncsafe(&size),
                _ => Some(u64::from(u32::from_be_bytes(size)) + 4),
            };
            let Some(skip) = skip else {
                return Ok(None);
            };
            at += skip;
        }
        // 各帧：4 字节 ID、4 字节长度（v2.4 为 syncsafe）、2 字节标志；ID 为 0 即进入填充
        while let Some(header) = read_at::<10>(reader, at, end)? {
            if header[0] == 0 {
                return Ok(None);
            }
            let size = match self.version {
                4 => syncsafe(&header[4..8]),
                _ => Some(u64::from(u32::from_be_bytes(
                    header[4..8].try_into().expect("切片长 4"),
                ))),
            };
            let Some(size) = size.filter(|&size| at + 10 + size <= end) else {
                return Ok(None);
            };
            if &header[..4] == b"PRIV" && size == TIMESTAMP_FRAME_LEN as u64 {
                let Some(data) = read_at::<TIMESTAMP_FRAME_LEN>(reader, at + 10, end)? else {
                    return Ok(None);
                };
                if data.starts_with(TIMESTAMP_OWNER) && data[TIMESTAMP_OWNER.len()] == 0 {
                    let value: [u8; 8] = data[TIMESTAMP_FRAME_LEN - 8..]
                        .try_into()
                        .expect("切片长 8");
                    return Ok(Some((u64::from_be_bytes(value) & ((1 << 33) - 1)) as i64));
                }
            }
            at += 10 + size;
        }
        Ok(None)
    }
}

/// 从 `at` 起读 `N` 字节；超出 `end` 或读到末尾时为 None。
fn read_at<const N: usize>(
    reader: &mut (impl Read + Seek),
    at: u64,
    end: u64,
) -> io::Result<Option<[u8; N]>> {
    if at + N as u64 > end {
        return Ok(None);
    }
    reader.seek(SeekFrom::Start(at))?;
    let mut buf = [0u8; N];
    Ok((read_up_to(reader, &mut buf)? == N).then_some(buf))
}

/// 4 个各 7 位的字节（最高位须为 0）表示的长度。
fn syncsafe(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0u64, |size, &b| {
        (b < 0x80).then_some(size << 7 | u64::from(b))
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn head(data: &[u8]) -> Option<Head> {
        read_head(&mut Cursor::new(data)).unwrap()
    }

    fn format(data: &[u8]) -> Option<Format> {
        head(data).map(|h| h.format)
    }

    fn ts_packets(count: usize) -> Vec<u8> {
        let mut data = vec![0u8; 188 * count];
        for packet in data.chunks_mut(188) {
            packet[0] = 0x47;
        }
        data
    }

    /// v2.4 的 syncsafe 长度。
    fn syncsafe_bytes(size: usize) -> [u8; 4] {
        [size >> 21, size >> 14, size >> 7, size].map(|b| (b & 0x7F) as u8)
    }

    /// v2.4 的帧。
    fn frame(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        [id.as_slice(), &syncsafe_bytes(data.len()), &[0, 0], data].concat()
    }

    /// 时间戳 PRIV 帧的内容。
    fn priv_timestamp(value: u64) -> Vec<u8> {
        [TIMESTAMP_OWNER, &[0], &value.to_be_bytes()].concat()
    }

    /// v2.4 标签：`flags` 为头里的标志，`body` 为扩展头与各帧，后面补 `padding` 个 0。
    fn tag(flags: u8, body: &[u8], padding: usize) -> Vec<u8> {
        let size = body.len() + padding;
        let footer = if flags & 0x10 != 0 {
            [b"3DI\x04\x00".as_slice(), &[flags], &syncsafe_bytes(size)].concat()
        } else {
            Vec::new()
        };
        [
            b"ID3\x04\x00".as_slice(),
            &[flags],
            &syncsafe_bytes(size),
            body,
            &vec![0; padding],
            &footer,
        ]
        .concat()
    }

    const ADTS: [u8; 7] = [0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC];

    #[test]
    fn formats_are_told_apart() {
        use Format::*;
        let mp3 = [0xFF, 0xFB, 0x90, 0x64];
        let ac3 = [0x0B, 0x77, 0, 0, 0, 8 << 3, 0];
        let eac3 = [0x0B, 0x77, 0, 0, 0, 16 << 3, 0];
        assert_eq!(format(&ts_packets(3)), Some(Ts));
        // 不足两个包时只看首字节；第二个包错位不是 TS
        assert_eq!(format(&ts_packets(1)), Some(Ts));
        let mut shifted = ts_packets(3);
        shifted[188] = 0;
        assert_eq!(format(&shifted), None);
        for (data, expected) in [(&ADTS[..], Aac), (&mp3, Mp3), (&ac3, Ac3), (&eac3, Eac3)] {
            assert_eq!(format(data), Some(expected));
            // 前面的 ID3 标签可有多个、可带尾部、可很长
            let first = tag(0, &frame(b"TIT2", b"\x03title"), 0);
            let second = tag(0x10, &[], 70 * 1024);
            let tagged = [first.as_slice(), &second, data].concat();
            assert_eq!(
                head(&tagged),
                Some(Head {
                    format: expected,
                    data_start: (first.len() + second.len()) as u64,
                    timestamp: None,
                })
            );
        }
        // ID3 之后不是音频、标签超出数据、长度字段最高位为 1、bsid 超出 AC-3 与 E-AC-3 的范围、空
        assert_eq!(format(&[tag(0, &[], 0), b"<html>".to_vec()].concat()), None);
        assert_eq!(format(&tag(0, &[], 20)[..15]), None);
        assert_eq!(format(b"ID3\x04\x00\x00\x00\x00\x00\x80\xFF\xF1"), None);
        assert_eq!(format(&[0x0B, 0x77, 0, 0, 0, 20 << 3]), None);
        assert_eq!(format(&[]), None);
    }

    #[test]
    fn the_timestamp_is_the_low_33_bits_of_the_first_priv_frame() {
        let other = frame(b"PRIV", b"com.example\0abc");
        let stamp = frame(b"PRIV", &priv_timestamp((1 << 40) | 0x0dac29));
        let first = tag(0, &[other, stamp].concat(), 4);
        let second = tag(0, &frame(b"PRIV", &priv_timestamp(180_000)), 300);
        let data = [first.as_slice(), &second, &ADTS].concat();
        assert_eq!(head(&data).unwrap().timestamp, Some(0x0dac29));
    }

    #[test]
    fn extended_headers_footers_and_v2_3_sizes_are_understood() {
        let stamp = frame(b"PRIV", &priv_timestamp(900_000));
        // v2.4 扩展头：长度 6（含长度字段），1 字节标志数与 1 字节标志
        let extended = [syncsafe_bytes(6).as_slice(), &[1, 0]].concat();
        let data = [
            tag(0x40 | 0x10, &[extended, stamp].concat(), 0),
            ADTS.to_vec(),
        ]
        .concat();
        assert_eq!(head(&data).unwrap().timestamp, Some(900_000));

        // v2.3：帧长度是普通整数
        let data_v3 = priv_timestamp(900_000);
        let v3_frame = [
            b"PRIV".as_slice(),
            &(data_v3.len() as u32).to_be_bytes(),
            &[0, 0],
            &data_v3,
        ]
        .concat();
        let mut v3 = tag(0, &v3_frame, 0);
        v3[3] = 3;
        let data = [v3, ADTS.to_vec()].concat();
        assert_eq!(head(&data).unwrap().timestamp, Some(900_000));
    }

    #[test]
    fn malformed_timestamps_are_none_but_the_format_is_still_read() {
        let timestamp = |tag: Vec<u8>| {
            let head = head(&[tag, ADTS.to_vec()].concat()).unwrap();
            assert_eq!(head.format, Format::Aac);
            head.timestamp
        };
        assert_eq!(timestamp(tag(0, &frame(b"TIT2", b"\x03title"), 10)), None);
        // owner 对而值不是 8 字节
        let short = [TIMESTAMP_OWNER, &[0], &[1, 2, 3]].concat();
        assert_eq!(timestamp(tag(0, &frame(b"PRIV", &short), 0)), None);
        // 帧长度超出标签
        let mut overrun = tag(0, &frame(b"PRIV", &priv_timestamp(1)), 0);
        overrun[17] = 0x7F;
        assert_eq!(timestamp(overrun), None);
        // v2.2 没有 4 字节的帧 ID
        let mut v2_2 = tag(0, &frame(b"PRIV", &priv_timestamp(1)), 0);
        v2_2[3] = 2;
        assert_eq!(timestamp(v2_2), None);
    }
}
