//! fMP4 的 init 段与分片的结构。合并时一条轨的 init 段与分片接成一个输入读（见 [`crate::TrackSegments`]），mov 解复用器
//! 按 box 声明的长度与 trun 声明的样本位置往后读：各文件须通过 [`check`]，否则会越过文件边界，把后面文件的字节当成
//! 这个文件的内容。各字段的含义见 ISO/IEC 14496-12。

use std::collections::HashMap;
use std::io::{self, Cursor, Read, Seek, SeekFrom};

/// fMP4 文件开头会有的 box。
const LEADING: [&[u8; 4]; 9] = [
    b"ftyp", b"styp", b"moof", b"moov", b"sidx", b"emsg", b"prft", b"free", b"skip",
];

/// 其余认得出类型的顶层 box。
const OTHERS: [&[u8; 4]; 5] = [b"mdat", b"mfra", b"ssix", b"meta", b"uuid"];

/// 一个顶层 box 的头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxHeader {
    /// 在文件里的起点，字节
    pub at: u64,
    pub kind: [u8; 4],
    /// 头的字节数：8，64 位长度时 16
    pub head: u64,
    /// 整个 box 的字节数，含头
    pub size: u64,
    /// 长度字段写为 0（到文件末尾）；`size` 为到末尾的实际长度
    pub to_end: bool,
}

impl BoxHeader {
    /// 在 `data`（这个 box 所在的文件）里的内容：头之后到 box 末尾。
    fn body<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[(self.at + self.head) as usize..(self.at + self.size) as usize]
    }
}

/// fMP4 文件的结构问题。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("第一个 box 不是 fMP4 文件开头会有的（ftyp、styp、moof 等）")]
    NotFmp4,
    /// 没写完：认得出类型的顶层 box 超出了数据，或 trun 声明的样本数据超出了数据
    #[error("没写完（box 或样本数据超出了数据）")]
    Incomplete,
    /// 从 `at` 起末尾的 `len` 个字节不是完整的 box：不足 8 个凑不成 box 头，或类型认不出而长度写 0 或超出了数据
    #[error("第 {at} 字节起末尾的 {len} 个字节不是完整的 box")]
    TrailingBytes { at: u64, len: u64 },
    /// 从 `at` 起的 box 的长度字段 `size` 小于 box 头的长度（8 字节，64 位长度时 16 字节）
    #[error("第 {at} 字节起的 box 长度 {size} 小于 box 头")]
    BoxTooSmall { at: u64, size: u64 },
    /// 从 `at` 起的顶层 box（moof 或 moov）里的内容不合法：子 box 超出了它、字段不全，或算出的样本位置溢出
    #[error("第 {at} 字节起的 box 内容不合法")]
    Malformed { at: u64 },
    /// 从 `at` 起的 moof 里 tfhd 写了 base_data_offset（文件里的绝对位置）：RFC 8216 3.3 要求 fMP4 分片按 moof 相对
    /// 寻址，接成一个输入读时绝对位置也对不上
    #[error("第 {at} 字节起的 moof 用了绝对位置（tfhd 的 base_data_offset）")]
    BaseDataOffset { at: u64 },
    /// 最后一个顶层 box 从 `at` 起长度写为 0，实际的 `size` 字节写不进 32 位的长度字段（见 [`seal`]）
    #[error(
        "第 {at} 字节起的最后一个 box 长度写为 0，实际 {size} 字节超过 4 GiB，改写不了长度字段"
    )]
    OpenBoxTooLarge { at: u64, size: u64 },
}

/// `reader`（一个 init 段或分片）的顶层 box，须首尾相接、恰好铺满到末尾（见 [`check`]）；读不了时为外层的 `Err`。
/// 只读各 box 的头，不核对 box 里的内容。
pub fn boxes(reader: &mut (impl Read + Seek)) -> io::Result<Result<Vec<BoxHeader>, Problem>> {
    let walk = walk(reader)?;
    Ok(match walk.rest {
        None => Ok(walk.boxes),
        Some(problem) => Err(problem),
    })
}

/// 核对一个 init 段或分片 `data`：
/// - 顶层 box 首尾相接、恰好铺满。认得出类型的 box 超出了数据为没写完；末尾不足 8 个字节、或类型认不出而长度写 0
///   或超出了数据的为多出的字节。
/// - 各 moof 的 trun 声明的样本数据都在 `data` 里，否则为没写完；这时末尾多出的字节也按没写完（截断落在 box 头里）。
///   样本数据的位置按 FFmpeg 9.0.2 mov 解复用器的算法（mov.c 的 mov_read_tfhd、mov_read_trun）：从基准加 trun 的
///   data_offset（没写时为 0）起；基准在 tfhd 写了 default-base-is-moof 时为 moof 的起点，否则为上一个 trun 的数据
///   终点（moof 里第一个为 moof 的起点）；tfhd 写了 base_data_offset 的报 [`Problem::BaseDataOffset`]。样本长度依次
///   取 trun、tfhd、`trex` 里写的；`trex` 里没有的轨 FFmpeg 不读，这里也跳过。
///
/// `trex` 为所用 init 段里各轨的默认样本长度（见 [`trex_sample_sizes`]），核对 init 段本身时取它自己的。
pub fn check(data: &[u8], trex: &HashMap<u32, u32>) -> Result<(), Problem> {
    checked(data, trex).map(drop)
}

/// 核对 `data`（见 [`check`]），并把长度写 0 的最后一个 box 的长度字段改写成实际长度：接成一个输入读时，到末尾就
/// 成了到所有分片的末尾。64 位的长度要多 8 字节的头，原地放不下，超过 4 GiB 时报 [`Problem::OpenBoxTooLarge`]。
pub fn seal(data: &mut [u8], trex: &HashMap<u32, u32>) -> Result<(), Problem> {
    let boxes = checked(data, trex)?;
    let Some(open) = boxes.last().filter(|b| b.to_end) else {
        return Ok(());
    };
    let size = u32::try_from(open.size).map_err(|_| Problem::OpenBoxTooLarge {
        at: open.at,
        size: open.size,
    })?;
    let at = open.at as usize;
    data[at..at + 4].copy_from_slice(&size.to_be_bytes());
    Ok(())
}

/// init 段 `init` 里各轨（track_ID）的默认样本长度（moov/mvex/trex），字节；没有 trex 的轨不在其中。只看完整的顶层
/// moov，顶层 box 是否铺满由 [`check`] 核对；moov 里的内容不合法时报 [`Problem::Malformed`]。
pub fn trex_sample_sizes(init: &[u8]) -> Result<HashMap<u32, u32>, Problem> {
    let walk = walk(&mut Cursor::new(init)).expect("读内存不会出错");
    let mut sizes = HashMap::new();
    for moov in walk.boxes.iter().filter(|b| &b.kind == b"moov") {
        let malformed = || Problem::Malformed { at: moov.at };
        for mvex in of_kind(children(moov.body(init)).ok_or_else(malformed)?, b"mvex") {
            for trex in of_kind(children(mvex).ok_or_else(malformed)?, b"trex") {
                let mut fields = Fields::new(trex, moov.at);
                // 版本与标志，track_ID，默认的样本描述下标、时长、长度
                fields.take(4)?;
                let track = fields.u32()?;
                fields.take(8)?;
                sizes.insert(track, fields.u32()?);
            }
        }
    }
    Ok(sizes)
}

/// 核对 `data`（见 [`check`]），返回顶层 box。
fn checked(data: &[u8], trex: &HashMap<u32, u32>) -> Result<Vec<BoxHeader>, Problem> {
    let Walk { boxes, rest } = walk(&mut Cursor::new(data)).expect("读内存不会出错");
    let samples_end = sample_data_end(data, &boxes, trex)?;
    match rest {
        None | Some(Problem::TrailingBytes { .. }) if samples_end > data.len() as u64 => {
            Err(Problem::Incomplete)
        }
        Some(problem) => Err(problem),
        None => Ok(boxes),
    }
}

/// 顶层 box 的遍历：从头起首尾相接的完整的 box，与之后凑不成完整 box 的原因（恰好铺满到末尾时为 None）。
struct Walk {
    boxes: Vec<BoxHeader>,
    rest: Option<Problem>,
}

fn walk(reader: &mut (impl Read + Seek)) -> io::Result<Walk> {
    let len = reader.seek(SeekFrom::End(0))?;
    let mut boxes = Vec::new();
    let mut at = 0;
    while at < len {
        reader.seek(SeekFrom::Start(at))?;
        let mut bytes = Vec::with_capacity(16);
        reader.by_ref().take(16).read_to_end(&mut bytes)?;
        match top_level(&bytes, at, len - at) {
            Ok(found) => {
                at += found.size;
                boxes.push(found);
            }
            Err(problem) => {
                return Ok(Walk {
                    boxes,
                    rest: Some(problem),
                });
            }
        }
    }
    // 空的数据
    let rest = boxes.is_empty().then_some(Problem::NotFmp4);
    Ok(Walk { boxes, rest })
}

/// 从 `at` 起的顶层 box，`bytes` 为从 `at` 起至多 16 个字节，`room` 为从 `at` 到末尾的字节数（分类见 [`check`]）。
fn top_level(bytes: &[u8], at: u64, room: u64) -> Result<BoxHeader, Problem> {
    let kind: Option<[u8; 4]> = bytes
        .get(4..8)
        .map(|kind| kind.try_into().expect("切片长 4"));
    if at == 0 && !kind.is_some_and(|kind| LEADING.contains(&&kind)) {
        return Err(Problem::NotFmp4);
    }
    let known = kind.is_some_and(|kind| LEADING.contains(&&kind) || OTHERS.contains(&&kind));
    let trailing = Problem::TrailingBytes { at, len: room };
    let Some(header) = header(bytes, room) else {
        // 不足 8 个字节，或 64 位长度不全
        return Err(if known { Problem::Incomplete } else { trailing });
    };
    if header.size < header.head {
        return Err(Problem::BoxTooSmall {
            at,
            size: header.size,
        });
    }
    if !known && (header.to_end || header.size > room) {
        return Err(trailing);
    }
    if header.size > room {
        return Err(Problem::Incomplete);
    }
    Ok(BoxHeader {
        at,
        kind: header.kind,
        head: header.head,
        size: header.size,
        to_end: header.to_end,
    })
}

/// box 头里的类型与长度。
struct Header {
    kind: [u8; 4],
    /// 头的字节数：8，64 位长度时 16
    head: u64,
    /// 整个 box 的字节数；长度写 0 时为到所在范围末尾的字节数
    size: u64,
    to_end: bool,
}

/// `bytes`（从 box 起点起的字节）里的 box 头，`room` 为从起点到所在范围末尾的字节数；字节不够读出类型与长度时为
/// None。长度为 1 时真实长度是类型之后的 64 位整数。
fn header(bytes: &[u8], room: u64) -> Option<Header> {
    let field = u32::from_be_bytes(*bytes.first_chunk()?);
    let kind = *bytes.get(4..)?.first_chunk()?;
    let (head, size) = match field {
        0 => (8, room),
        1 => (16, u64::from_be_bytes(*bytes.get(8..)?.first_chunk()?)),
        size => (8, u64::from(size)),
    };
    Some(Header {
        kind,
        head,
        size,
        to_end: field == 0,
    })
}

/// `body`（一个 box 的内容）里首尾相接、恰好铺满的子 box 的类型与内容；没铺满时为 None。
fn children(mut body: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    let mut found = Vec::new();
    while !body.is_empty() {
        let header = header(body, body.len() as u64)?;
        if header.size < header.head || header.size > body.len() as u64 {
            return None;
        }
        let (child, rest) = body.split_at(header.size as usize);
        found.push((header.kind, &child[header.head as usize..]));
        body = rest;
    }
    Some(found)
}

/// `children` 里类型为 `kind` 的子 box 的内容。
fn of_kind<'a>(
    children: Vec<([u8; 4], &'a [u8])>,
    kind: &'a [u8; 4],
) -> impl Iterator<Item = &'a [u8]> {
    children
        .into_iter()
        .filter_map(move |(found, body)| (&found == kind).then_some(body))
}

/// 一个 traf 的样本数据基准与默认样本长度。
struct Fragment {
    base: u64,
    default_size: u32,
}

/// 各 moof 的 trun 声明的样本数据的终点中最大的，字节；没有 trun 时为 0（规则见 [`check`]）。
fn sample_data_end(
    data: &[u8],
    boxes: &[BoxHeader],
    trex: &HashMap<u32, u32>,
) -> Result<u64, Problem> {
    let mut end = 0;
    for moof in boxes.iter().filter(|b| &b.kind == b"moof") {
        let malformed = || Problem::Malformed { at: moof.at };
        let mut implicit = moof.at;
        for traf in of_kind(children(moof.body(data)).ok_or_else(malformed)?, b"traf") {
            let mut fragment = None;
            for (kind, body) in children(traf).ok_or_else(malformed)? {
                match &kind {
                    b"tfhd" => fragment = tfhd(body, moof.at, implicit, trex)?,
                    b"trun" => {
                        if let Some(fragment) = &fragment {
                            implicit = trun(body, fragment, moof.at)?;
                            end = end.max(implicit);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(end)
}

/// tfhd（内容为 `body`）给出的样本数据基准与默认样本长度；`trex` 里没有这条轨时为 None。`moof_at` 为所在 moof 的
/// 起点，`implicit` 为没写基准时用的（见 [`check`]）。
fn tfhd(
    body: &[u8],
    moof_at: u64,
    implicit: u64,
    trex: &HashMap<u32, u32>,
) -> Result<Option<Fragment>, Problem> {
    let mut fields = Fields::new(body, moof_at);
    let flags = fields.u32()? & 0x00FF_FFFF;
    let track = fields.u32()?;
    let Some(&trex_size) = trex.get(&track) else {
        return Ok(None);
    };
    // base-data-offset-present
    if flags & 0x01 != 0 {
        return Err(Problem::BaseDataOffset { at: moof_at });
    }
    // default-base-is-moof
    let base = if flags & 0x02_0000 != 0 {
        moof_at
    } else {
        implicit
    };
    // 样本描述下标、默认时长
    fields.take(4 * (usize::from(flags & 0x02 != 0) + usize::from(flags & 0x08 != 0)))?;
    let default_size = if flags & 0x10 != 0 {
        fields.u32()?
    } else {
        trex_size
    };
    // 默认样本标志
    fields.take(4 * usize::from(flags & 0x20 != 0))?;
    Ok(Some(Fragment { base, default_size }))
}

/// trun（内容为 `body`，在从 `moof_at` 起的 moof 里）的样本数据终点：从 `fragment` 的基准加 data_offset 起，各样本
/// 长度之和；样本长度没写时用 `fragment` 的默认值。
fn trun(body: &[u8], fragment: &Fragment, moof_at: u64) -> Result<u64, Problem> {
    let malformed = Problem::Malformed { at: moof_at };
    let mut fields = Fields::new(body, moof_at);
    let flags = fields.u32()? & 0x00FF_FFFF;
    let count = fields.u32()?;
    let mut start = fragment.base;
    if flags & 0x01 != 0 {
        // data_offset 是有符号数
        let offset = fields.u32()? as i32;
        start = start
            .checked_add_signed(i64::from(offset))
            .ok_or(malformed.clone())?;
    }
    // first_sample_flags
    fields.take(4 * usize::from(flags & 0x04 != 0))?;
    // 每个样本依次为时长、长度、标志、合成时间偏移，各 4 字节，按标志有无
    let present = [0x100, 0x200, 0x400, 0x800].map(|bit| flags & bit != 0);
    let entry = 4 * present.iter().filter(|&&p| p).count();
    let table = usize::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(entry))
        .ok_or(malformed.clone())?;
    let table = fields.take(table)?;
    let total = if present[1] {
        let size_at = 4 * usize::from(present[0]);
        table
            .chunks_exact(entry)
            .map(|sample| {
                let size = sample[size_at..].first_chunk().expect("样本项含长度");
                u64::from(u32::from_be_bytes(*size))
            })
            .sum()
    } else {
        u64::from(count) * u64::from(fragment.default_size)
    };
    start.checked_add(total).ok_or(malformed)
}

/// 依次读一个 box 的内容里的字段；不够读时为所在顶层 box（从 `at` 起）的 [`Problem::Malformed`]。
struct Fields<'a> {
    rest: &'a [u8],
    at: u64,
}

impl<'a> Fields<'a> {
    fn new(body: &'a [u8], at: u64) -> Self {
        Fields { rest: body, at }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Problem> {
        let (taken, rest) = self
            .rest
            .split_at_checked(len)
            .ok_or(Problem::Malformed { at: self.at })?;
        self.rest = rest;
        Ok(taken)
    }

    fn u32(&mut self) -> Result<u32, Problem> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes(bytes.try_into().expect("切片长 4")))
    }
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

    fn sealed(mut data: Vec<u8>) -> Result<Vec<u8>, Problem> {
        seal(&mut data, &HashMap::new()).map(|()| data)
    }

    #[test]
    fn trex_gives_the_default_sample_size_of_each_track() {
        let trex = |track| boxed(b"trex", &full(0, &[track, 1, 0, 7 * track, 0]));
        let init = |trex: Vec<u8>| {
            [
                boxed(b"ftyp", b"isom"),
                boxed(b"moov", &boxed(b"mvex", &trex)),
            ]
            .concat()
        };
        assert_eq!(
            trex_sample_sizes(&init([trex(1), trex(2)].concat())),
            Ok(HashMap::from([(1, 7), (2, 14)]))
        );
        // trex 的字段不全：只在它自己的范围里读
        let short = [boxed(b"trex", &full(0, &[1, 1, 0])), trex(2)].concat();
        assert_eq!(
            trex_sample_sizes(&init(short)),
            Err(Problem::Malformed { at: 12 })
        );
    }

    #[test]
    fn top_level_boxes_must_tile_the_data() {
        let moof = boxed(b"moof", &[0; 8]);
        let segment = [moof.clone(), boxed(b"mdat", &[0; 4])].concat();
        assert_eq!(sealed(segment.clone()), Ok(segment.clone()));
        // 长度为 1 时后跟 64 位长度
        let large = [
            &1u32.to_be_bytes()[..],
            b"styp",
            &20u64.to_be_bytes(),
            &[0; 4],
        ]
        .concat();
        assert_eq!(sealed(large.clone()), Ok(large));
        // 长度为 0 时到末尾，改写成实际长度
        let open = [&0u32.to_be_bytes()[..], b"mdat", &[0; 30]].concat();
        assert_eq!(
            sealed([moof.clone(), open].concat()),
            Ok([moof.clone(), boxed(b"mdat", &[0; 30])].concat())
        );

        assert_eq!(sealed(boxed(b"mdat", &[0; 8])), Err(Problem::NotFmp4));
        assert_eq!(sealed(b"moof".to_vec()), Err(Problem::NotFmp4));
        assert_eq!(sealed(Vec::new()), Err(Problem::NotFmp4));
        // 没写完：认得出的 box 截断、64 位长度不全
        assert_eq!(
            sealed(segment[..segment.len() - 2].to_vec()),
            Err(Problem::Incomplete)
        );
        let large_mdat = [&1u32.to_be_bytes()[..], b"mdat", &[0; 4]].concat();
        assert_eq!(
            sealed([moof.clone(), large_mdat].concat()),
            Err(Problem::Incomplete)
        );
        // 多出的字节：凑不成 box 头的、类型认不出而超出数据的、全 0 的填充（长度写 0）
        let trailing = |extra: &[u8]| {
            assert_eq!(
                sealed([segment.clone(), extra.to_vec()].concat()),
                Err(Problem::TrailingBytes {
                    at: 28,
                    len: extra.len() as u64
                })
            );
        };
        trailing(b"\r\n");
        trailing(b"<html><body>not found</body></html>");
        trailing(&[0; 16]);
        assert_eq!(
            sealed([&7u32.to_be_bytes()[..], b"moof"].concat()),
            Err(Problem::BoxTooSmall { at: 0, size: 7 })
        );
    }

    #[test]
    fn sample_data_must_be_where_the_truns_say() {
        let trex = HashMap::from([(1, 100)]);
        let check = |segment: &[u8]| check(segment, &trex);
        let traf = |tfhd_flags, tfhd: &[u32], trun_flags, trun: &[u32]| {
            boxed(
                b"traf",
                &[
                    boxed(b"tfhd", &full(tfhd_flags, tfhd)),
                    boxed(b"trun", &full(trun_flags, trun)),
                ]
                .concat(),
            )
        };
        // 基准为 moof 的起点（styp 之后，第 12 字节），trun 的 data_offset 为 64，两个样本的长度取 tfhd 的默认值 10：
        // 样本数据为第 76 到 96 字节，恰为 mdat 的内容
        let styp = boxed(b"styp", b"msdh");
        let moof = boxed(b"moof", &traf(0x02_0010, &[1, 10], 0x01, &[2, 64]));
        assert_eq!(styp.len() + moof.len(), 68);
        let segment = [styp, moof, boxed(b"mdat", &[0; 20])].concat();
        assert_eq!(check(&segment), Ok(()));
        // 截在 moof 之后、截在 mdat 的头里
        assert_eq!(check(&segment[..68]), Err(Problem::Incomplete));
        assert_eq!(check(&segment[..72]), Err(Problem::Incomplete));

        // 隐含的基准：第一个 traf 为 moof 的起点，3 个样本的长度取 trex 的 100；第二个接着上一个 trun 的终点，逐个写了
        // 长度 50、60；trex 里没有的轨不算。样本数据到第 410 字节
        let moof = [
            traf(0, &[1], 0, &[3]),
            traf(0, &[1], 0x200, &[2, 50, 60]),
            traf(0, &[2], 0x200, &[1, 1000]),
        ]
        .concat();
        let moof = boxed(b"moof", &moof);
        let mdat = |len: usize| boxed(b"mdat", &vec![0; len - moof.len() - 8]);
        assert_eq!(check(&[moof.clone(), mdat(410)].concat()), Ok(()));
        assert_eq!(
            check(&[moof.clone(), mdat(409)].concat()),
            Err(Problem::Incomplete)
        );

        // trun 声明 1000 个样本而内容不够；样本长度都取默认值时样本数大也不逐个累加
        let short = boxed(b"moof", &traf(0, &[1], 0x200, &[1000, 5]));
        assert_eq!(check(&short), Err(Problem::Malformed { at: 0 }));
        let many = boxed(b"moof", &traf(0, &[1], 0, &[u32::MAX]));
        assert_eq!(check(&many), Err(Problem::Incomplete));
        // 写了 base_data_offset
        let absolute = boxed(b"moof", &traf(0x01, &[1, 0, 100], 0, &[1]));
        assert_eq!(check(&absolute), Err(Problem::BaseDataOffset { at: 0 }));
    }
}
