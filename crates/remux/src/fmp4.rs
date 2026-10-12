//! fMP4 的 init 段与分片的结构。合并时一条轨的 init 段与分片接成一个输入读（见 [`crate::TrackSegments`]），mov 解复用器
//! 按 box 声明的长度往后读：各文件须由首尾相接的顶层 box 恰好铺满（见 [`boxes`]），最后一个 box 的长度也不能写 0（到
//! 末尾），否则会越过文件边界，把后面文件的字节当成这个 box 的内容。各字段的含义见 ISO/IEC 14496-12。

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;

/// fMP4 文件开头会有的 box。
const LEADING: [&[u8; 4]; 9] = [
    b"ftyp", b"styp", b"moof", b"moov", b"sidx", b"emsg", b"prft", b"free", b"skip",
];

/// 一个顶层 box 的头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxHeader {
    /// 在文件里的起点，字节
    pub at: u64,
    pub kind: [u8; 4],
    /// 整个 box 的字节数，含头
    pub size: u64,
    /// 长度字段写为 0（到文件末尾）；`size` 为到末尾的实际长度
    pub to_end: bool,
}

/// fMP4 文件的结构问题。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("第一个 box 不是 fMP4 文件开头会有的（ftyp、styp、moof 等）")]
    NotFmp4,
    /// 没写完：顶层 box 超出了数据。长度写 0 的 mdat 靠长度看不出，见 [`sample_data_end`]
    #[error("没写完（box 超出了数据）")]
    Incomplete,
    /// 从 `at` 起末尾的 `len` 个字节（不足 8 个）凑不成 box 头
    #[error("第 {at} 字节起末尾的 {len} 个字节凑不成 box 头")]
    TrailingBytes { at: u64, len: u64 },
    /// 从 `at` 起的 box 的长度字段 `size` 小于 box 头的长度（8 字节，64 位长度时 16 字节）
    #[error("第 {at} 字节起的 box 长度 {size} 小于 box 头")]
    BoxTooSmall { at: u64, size: u64 },
}

/// 从 `at` 起的 moof 里的内容不合法：子 box 超出了它，或字段不全。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadMoof {
    pub at: u64,
}

/// `reader`（一个 init 段或分片）的顶层 box，须首尾相接、恰好铺满到末尾；读不了时为外层的 `Err`。只读各 box 的头。
pub fn boxes(reader: &mut (impl Read + Seek)) -> io::Result<Result<Vec<BoxHeader>, Problem>> {
    let len = reader.seek(SeekFrom::End(0))?;
    let mut found = Vec::new();
    let mut at = 0;
    while at < len {
        reader.seek(SeekFrom::Start(at))?;
        let mut header = [0u8; 16];
        let read = read_up_to(reader, &mut header)?;
        if read < 8 {
            return Ok(Err(if at == 0 {
                Problem::NotFmp4
            } else {
                Problem::TrailingBytes { at, len: len - at }
            }));
        }
        let kind: [u8; 4] = header[4..8].try_into().expect("切片长 4");
        if at == 0 && !LEADING.contains(&&kind) {
            return Ok(Err(Problem::NotFmp4));
        }
        let field = u32::from_be_bytes(header[..4].try_into().expect("切片长 4"));
        let (head, size) = match field {
            0 => (8, len - at),
            // 长度为 1：真实长度是头之后的 64 位整数
            1 if read < 16 => return Ok(Err(Problem::Incomplete)),
            1 => (
                16,
                u64::from_be_bytes(header[8..16].try_into().expect("切片长 8")),
            ),
            size => (8, u64::from(size)),
        };
        if size < head {
            return Ok(Err(Problem::BoxTooSmall { at, size }));
        }
        if size > len - at {
            return Ok(Err(Problem::Incomplete));
        }
        found.push(BoxHeader {
            at,
            kind,
            size,
            to_end: field == 0,
        });
        at += size;
    }
    if found.is_empty() {
        return Ok(Err(Problem::NotFmp4));
    }
    Ok(Ok(found))
}

/// init 段 `init` 里各轨（track_ID）的默认样本长度（moov/mvex/trex）；没有的轨不在其中。init 段结构不合法时照能读出的
/// 给出：它在下载时另行核对。
pub fn trex_sample_sizes(init: &[u8]) -> HashMap<u32, u32> {
    let mut sizes = HashMap::new();
    let whole = 0..init.len();
    for moov in children(init, whole)
        .flatten()
        .filter(|c| &c.kind == b"moov")
    {
        for mvex in children(init, moov.body)
            .flatten()
            .filter(|c| &c.kind == b"mvex")
        {
            for trex in children(init, mvex.body)
                .flatten()
                .filter(|c| &c.kind == b"trex")
            {
                // 版本与标志 4 字节，track_ID，默认的样本描述下标、时长、长度
                let field = |at: usize| be_u32(init, trex.body.start + at);
                if let (Some(track), Some(size)) = (field(4), field(16)) {
                    sizes.insert(track, size);
                }
            }
        }
    }
    sizes
}

/// 分片 `data`（顶层 box 为 `boxes`）里各 moof 的 trun 声明的样本数据的终点中最大的，字节；没有 trun 时为 0。按
/// FFmpeg 9.0.2 mov 解复用器的算法（mov.c 的 mov_read_tfhd、mov_read_trun）：样本数据从基准加 trun 的 data_offset
/// （没写时为 0）起；基准为 tfhd 的 base_data_offset，没写时 default-base-is-moof 取 moof 的起点，否则取上一个 trun
/// 的数据终点（moof 里第一个为 moof 的起点）。样本长度依次取 trun、tfhd、init 段 trex（`trex`，见
/// [`trex_sample_sizes`]）里写的；init 段里没有这条轨的 trex 时 FFmpeg 不读这个 traf，这里也跳过。
pub fn sample_data_end(
    data: &[u8],
    boxes: &[BoxHeader],
    trex: &HashMap<u32, u32>,
) -> Result<u64, BadMoof> {
    let mut end = 0;
    for moof in boxes.iter().filter(|b| &b.kind == b"moof") {
        let bad = || BadMoof { at: moof.at };
        let range = usize::try_from(moof.at)
            .ok()
            .zip(usize::try_from(moof.at + moof.size).ok())
            .map(|(start, end)| start..end)
            .ok_or_else(bad)?;
        // 这个范围里唯一的子 box 就是 moof 自己，由它得出头的长度
        let body = children(data, range)
            .next()
            .and_then(Result::ok)
            .ok_or_else(bad)?
            .body;
        let mut implicit = moof.at;
        for traf in children(data, body) {
            let traf = traf.map_err(|()| bad())?;
            if &traf.kind != b"traf" {
                continue;
            }
            let mut fragment = None;
            for child in children(data, traf.body) {
                let child = child.map_err(|()| bad())?;
                match &child.kind {
                    b"tfhd" => {
                        fragment =
                            tfhd(data, child.body, moof.at, implicit, trex).ok_or_else(bad)?;
                    }
                    b"trun" => {
                        let Some((base, default_size)) = fragment else {
                            continue;
                        };
                        let run_end = trun(data, child.body, base, default_size).ok_or_else(bad)?;
                        implicit = run_end;
                        end = end.max(run_end);
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(end)
}

/// tfhd 给出的样本数据基准与默认样本长度；init 段里没有这条轨的 trex 时为 `Some(None)`；字段不全时为 None。
fn tfhd(
    data: &[u8],
    body: Range<usize>,
    moof_at: u64,
    implicit: u64,
    trex: &HashMap<u32, u32>,
) -> Option<Option<(u64, u32)>> {
    let flags = be_u32(data, body.start)? & 0x00FF_FFFF;
    let track = be_u32(data, body.start + 4)?;
    let Some(&trex_size) = trex.get(&track) else {
        return Some(None);
    };
    let mut at = body.start + 8;
    let base = if flags & 0x01 != 0 {
        let base = be_u64(data, at)?;
        at += 8;
        base
    } else if flags & 0x02_0000 != 0 {
        moof_at
    } else {
        implicit
    };
    // 样本描述下标、默认时长
    at += 4 * (usize::from(flags & 0x02 != 0) + usize::from(flags & 0x08 != 0));
    let size = if flags & 0x10 != 0 {
        let size = be_u32(data, at)?;
        at += 4;
        size
    } else {
        trex_size
    };
    // 默认样本标志
    at += 4 * usize::from(flags & 0x20 != 0);
    (at <= body.end).then_some(Some((base, size)))
}

/// trun 的样本数据终点：从 `base` 加 data_offset 起，各样本长度之和；样本长度没写时用 `default_size`。字段不全、或
/// 算出的位置溢出时为 None。
fn trun(data: &[u8], body: Range<usize>, base: u64, default_size: u32) -> Option<u64> {
    let flags = be_u32(data, body.start)? & 0x00FF_FFFF;
    let count = usize::try_from(be_u32(data, body.start + 4)?).ok()?;
    let mut at = body.start + 8;
    let mut start = base;
    if flags & 0x01 != 0 {
        let offset = be_u32(data, at)? as i32;
        start = start.checked_add_signed(i64::from(offset))?;
        at += 4;
    }
    // first_sample_flags
    at += 4 * usize::from(flags & 0x04 != 0);
    // 每个样本依次为时长、长度、标志、合成时间偏移，各 4 字节，按标志有无
    let fields = [0x100, 0x200, 0x400, 0x800].map(|bit| flags & bit != 0);
    let entry = 4 * fields.iter().filter(|&&present| present).count();
    if at.checked_add(count.checked_mul(entry)?)? > body.end {
        return None;
    }
    let size_at = 4 * usize::from(fields[0]);
    let mut total: u64 = 0;
    for i in 0..count {
        let size = if fields[1] {
            be_u32(data, at + i * entry + size_at)?
        } else {
            default_size
        };
        total = total.checked_add(u64::from(size))?;
    }
    start.checked_add(total)
}

/// 一个子 box：类型与内容的范围（头之后到 box 末尾）。
struct Child {
    kind: [u8; 4],
    body: Range<usize>,
}

/// `data` 里 `range` 范围内首尾相接的子 box；超出范围、头不全时那一项为 `Err`，之后不再给出。
fn children(data: &[u8], range: Range<usize>) -> impl Iterator<Item = Result<Child, ()>> + '_ {
    let mut at = range.start;
    let mut failed = false;
    std::iter::from_fn(move || {
        if failed || at >= range.end {
            return None;
        }
        let child = (|| {
            let field = be_u32(data, at)?;
            let kind: [u8; 4] = data.get(at + 4..at + 8)?.try_into().ok()?;
            let (head, size) = match field {
                0 => (8, range.end - at),
                1 => (16, usize::try_from(be_u64(data, at + 8)?).ok()?),
                size => (8, usize::try_from(size).ok()?),
            };
            let end = at.checked_add(size)?;
            (size >= head && end <= range.end).then_some((
                Child {
                    kind,
                    body: at + head..end,
                },
                end,
            ))
        })();
        match child {
            Some((child, end)) => {
                at = end;
                Some(Ok(child))
            }
            None => {
                failed = true;
                Some(Err(()))
            }
        }
    })
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn be_u64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        data.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        [&((body.len() + 8) as u32).to_be_bytes()[..], kind, body].concat()
    }

    /// 版本为 0 的 full box 的内容：标志与之后的各 32 位字段。
    fn full(flags: u32, fields: &[u32]) -> Vec<u8> {
        std::iter::once(flags)
            .chain(fields.iter().copied())
            .flat_map(u32::to_be_bytes)
            .collect()
    }

    #[test]
    fn trex_gives_the_default_sample_size_of_each_track() {
        let trex = |track| boxed(b"trex", &full(0, &[track, 1, 0, 7 * track, 0]));
        let init = [
            boxed(b"ftyp", b"isom"),
            boxed(b"moov", &boxed(b"mvex", &[trex(1), trex(2)].concat())),
        ]
        .concat();
        assert_eq!(trex_sample_sizes(&init), HashMap::from([(1, 7), (2, 14)]));
    }

    #[test]
    fn sample_data_ends_where_the_truns_say() {
        let trex = HashMap::from([(1, 7)]);
        let end = |segment: &[u8]| {
            let boxes = boxes(&mut io::Cursor::new(segment)).unwrap().unwrap();
            sample_data_end(segment, &boxes, &trex)
        };
        // 基准为 moof 的起点（styp 之后，第 12 字节），trun 的 data_offset 为 100，样本长度取 tfhd 的默认值 10
        let tfhd = boxed(b"tfhd", &full(0x02_0010, &[1, 10]));
        let trun = boxed(b"trun", &full(0x01, &[2, 100]));
        let segment = [
            boxed(b"styp", b"msdh"),
            boxed(b"moof", &boxed(b"traf", &[tfhd, trun].concat())),
        ]
        .concat();
        assert_eq!(end(&segment), Ok(12 + 100 + 20));

        // 隐含的基准：第一个 traf 为 moof 的起点，样本长度取 trex 的 7；第二个接着上一个 trun 的终点，逐个写了长度；
        // init 段里没有 trex 的轨不算
        let traf = |track, trun_flags, fields: &[u32]| {
            let tfhd = boxed(b"tfhd", &full(0, &[track]));
            boxed(
                b"traf",
                &[tfhd, boxed(b"trun", &full(trun_flags, fields))].concat(),
            )
        };
        let moof = [
            traf(1, 0, &[3]),
            traf(1, 0x200, &[2, 5, 6]),
            traf(2, 0x200, &[1, 1000]),
        ]
        .concat();
        assert_eq!(end(&boxed(b"moof", &moof)), Ok(21 + 11));

        // trun 声明 1000 个样本而内容不够
        let short = boxed(b"moof", &traf(1, 0x200, &[1000, 5]));
        assert_eq!(end(&short), Err(BadMoof { at: 0 }));
    }
}
