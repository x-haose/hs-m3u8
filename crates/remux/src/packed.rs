//! 打包音频（RFC 8216 3.4）的分片：裸的 AAC（ADTS）、MP3、AC-3 或 E-AC-3 帧，前面是 ID3v2 标签，其中
//! owner 为 `com.apple.streaming.transportStreamTimestamp` 的 PRIV 帧给出首个样本的 PTS（90 kHz、33 位，与 TS
//! 同一个时钟）。FFmpeg 的裸音频解复用器不读这个时间戳，只从 0 起按样本数计时。

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// PRIV 帧里时间戳的 owner。
const TIMESTAMP_OWNER: &[u8] = b"com.apple.streaming.transportStreamTimestamp";

/// 打包音频分片开头的信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Head {
    /// ID3 标签之后音频数据的起点，字节
    pub data_start: u64,
    /// 首个样本的 PTS（90 kHz，未展开的 33 位值）；没有时间戳的 PRIV 帧时为 None
    pub timestamp: Option<i64>,
}

/// 没有 init 段的分片是不是打包音频：TS 以同步字节 0x47 开头，打包音频以 ID3 标签或音频帧开头。
pub(crate) fn is_packed(path: &Path) -> io::Result<bool> {
    let mut first = [0u8; 1];
    let read = File::open(path)?.read(&mut first)?;
    Ok(read == 1 && first[0] != 0x47)
}

/// 读出分片开头的 ID3 标签（可有多个），不读音频数据。
pub(crate) fn read_head(path: &Path) -> io::Result<Head> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut head = Head {
        data_start: 0,
        timestamp: None,
    };
    loop {
        let mut header = [0u8; 10];
        file.seek(SeekFrom::Start(head.data_start))?;
        let read = file.by_ref().take(10).read(&mut header)?;
        let Some(tag_len) = id3_len(&header[..read]) else {
            return Ok(head);
        };
        let end = head.data_start + tag_len as u64;
        if end > len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ID3 标签超出文件末尾",
            ));
        }
        if head.timestamp.is_none() {
            let mut tag = vec![0u8; tag_len];
            file.seek(SeekFrom::Start(head.data_start))?;
            file.read_exact(&mut tag)?;
            head.timestamp = timestamp(&tag);
        }
        head.data_start = end;
    }
}

/// `data` 开头的 ID3v2 标签的总长（含头与尾部）；不以 ID3 标签开头、头不完整或长度字段不合法时为 None。
pub fn id3_len(data: &[u8]) -> Option<usize> {
    let header = data.get(..10).filter(|h| h.starts_with(b"ID3"))?;
    let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };
    Some(10 + syncsafe(&header[6..10])? + footer)
}

/// 4 个各 7 位的字节（最高位须为 0）表示的长度。
fn syncsafe(bytes: &[u8]) -> Option<usize> {
    bytes.iter().try_fold(0usize, |size, &b| {
        (b < 0x80).then_some(size << 7 | usize::from(b))
    })
}

/// 一个完整的 ID3v2.3 或 v2.4 标签里时间戳 PRIV 帧的值：8 字节大端数的低 33 位。
fn timestamp(tag: &[u8]) -> Option<i64> {
    let version = tag[3];
    if !matches!(version, 3 | 4) {
        return None;
    }
    // 头之后到尾部之前：扩展头、各帧与填充
    let body = tag.get(..10 + syncsafe(&tag[6..10])?)?;
    let mut at = 10;
    // 扩展头：v2.4 的长度为 syncsafe 且含长度字段本身，v2.3 的为普通整数且不含长度字段
    if tag[5] & 0x40 != 0 {
        let size = body.get(10..14)?;
        at += match version {
            4 => syncsafe(size)?,
            _ => u32::from_be_bytes(size.try_into().ok()?) as usize + 4,
        };
    }
    // 各帧：4 字节 ID、4 字节长度（v2.4 为 syncsafe）、2 字节标志；ID 为 0 即进入填充
    while let Some(header) = body.get(at..at + 10) {
        if header[0] == 0 {
            return None;
        }
        let size = match version {
            4 => syncsafe(&header[4..8])?,
            _ => u32::from_be_bytes(header[4..8].try_into().ok()?) as usize,
        };
        let data = body.get(at + 10..at + 10 + size)?;
        if &header[..4] == b"PRIV"
            && let Some(value) = data
                .strip_prefix(TIMESTAMP_OWNER)
                .and_then(|rest| rest.strip_prefix(&[0]))
        {
            let value: [u8; 8] = value.try_into().ok()?;
            return Some((u64::from_be_bytes(value) & ((1 << 33) - 1)) as i64);
        }
        at += 10 + size;
    }
    None
}

/// 33 位的 PTS（90 kHz）按 FFmpeg 读 TS 时的规则展开，同组的 TS 轨按同样的规则读出，两边才对得上：以 `first`
/// （本轨在这一组的第一个时间戳）前 60 秒为参照；`first` 不在回绕点前 60 秒内时，比参照小的加 2^33，在时，不比参照
/// 小的减 2^33。
pub(crate) fn unwrap_pts(pts: i64, first: i64) -> i64 {
    const WRAP: i64 = 1 << 33;
    const MARGIN: i64 = 60 * 90_000;
    let reference = first - MARGIN;
    if first < WRAP - MARGIN {
        if pts < reference { pts + WRAP } else { pts }
    } else if pts >= reference {
        pts - WRAP
    } else {
        pts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_timestamp_is_the_low_33_bits_of_its_priv_frame() {
        let other = frame(b"PRIV", b"com.example\0abc");
        let stamp = frame(b"PRIV", &priv_timestamp((1 << 40) | 0x0dac29));
        let data = tag(0, &[other, stamp].concat(), 4);
        assert_eq!(timestamp(&data), Some(0x0dac29));
        assert_eq!(id3_len(&data), Some(data.len()));
    }

    #[test]
    fn extended_headers_footers_and_v2_3_sizes_are_understood() {
        let stamp = frame(b"PRIV", &priv_timestamp(900_000));
        // v2.4 扩展头：长度 6（含长度字段），1 字节标志数与 1 字节标志
        let extended = [syncsafe_bytes(6).as_slice(), &[1, 0]].concat();
        let data = tag(0x40 | 0x10, &[extended, stamp.clone()].concat(), 0);
        assert_eq!(timestamp(&data), Some(900_000));
        assert_eq!(id3_len(&data), Some(data.len()));

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
        assert_eq!(timestamp(&v3), Some(900_000));
    }

    #[test]
    fn tags_without_a_well_formed_timestamp_have_none() {
        assert_eq!(timestamp(&tag(0, &frame(b"TIT2", b"\x03title"), 10)), None);
        // owner 对而值不是 8 字节
        let short = [TIMESTAMP_OWNER, &[0], &[1, 2, 3]].concat();
        assert_eq!(timestamp(&tag(0, &frame(b"PRIV", &short), 0)), None);
        // 帧长度超出标签
        let mut overrun = tag(0, &frame(b"PRIV", &priv_timestamp(1)), 0);
        overrun[17] = 0x7F;
        assert_eq!(timestamp(&overrun), None);
        let mut v2_2 = tag(0, &frame(b"PRIV", &priv_timestamp(1)), 0);
        v2_2[3] = 2;
        assert_eq!(timestamp(&v2_2), None);
    }

    #[test]
    fn heads_skip_every_leading_tag_and_take_the_first_timestamp() {
        let dir = std::env::temp_dir().join(format!("hs-m3u8-packed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seg.aac");
        let first = tag(0, &frame(b"PRIV", &priv_timestamp(90_000)), 0);
        let second = tag(0, &frame(b"PRIV", &priv_timestamp(180_000)), 300);
        let audio = [0xFF, 0xF1, 0x50, 0x80];
        std::fs::write(&path, [first.as_slice(), &second, &audio].concat()).unwrap();
        assert_eq!(
            read_head(&path).unwrap(),
            Head {
                data_start: (first.len() + second.len()) as u64,
                timestamp: Some(90_000),
            }
        );

        std::fs::write(&path, audio).unwrap();
        assert_eq!(
            read_head(&path).unwrap(),
            Head {
                data_start: 0,
                timestamp: None,
            }
        );

        std::fs::write(&path, &first[..first.len() - 1]).unwrap();
        assert_eq!(
            read_head(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn timestamps_unwrap_like_ffmpeg_reads_ts() {
        const WRAP: i64 = 1 << 33;
        const MINUTE: i64 = 60 * 90_000;
        // 一般情况：回绕之后的值加 2^33，参照前后 60 秒内的不动
        let first = 1_000 * 90_000;
        assert_eq!(unwrap_pts(first + 5, first), first + 5);
        assert_eq!(unwrap_pts(first - MINUTE, first), first - MINUTE);
        assert_eq!(
            unwrap_pts(first - MINUTE - 1, first),
            first - MINUTE - 1 + WRAP
        );
        // 开头离回绕点不到 60 秒：回绕之前的值减 2^33，之后的不动
        let late = WRAP - MINUTE / 2;
        assert_eq!(unwrap_pts(late, late), late - WRAP);
        assert_eq!(unwrap_pts(5, late), 5);
        // 开头刚过回绕点：参照为负，都不动
        assert_eq!(unwrap_pts(WRAP - 1, 10), WRAP - 1);
    }
}
