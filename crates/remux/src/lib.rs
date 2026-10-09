//! 转封装：把若干输入文件中的视频流与音频流原样复制（不重编码）进一个 MP4。
//!
//! 每个输入贡献它的第一路视频和（或）第一路音频，同一类流只能由一个输入提供。
//! 输出先写到 `<输出>.part`，写完后重新读取、核对每路流的包数与写入数一致，再改名为最终文件；
//! 任一步失败都会删除临时文件并返回错误，不留下半截产物。已存在的输出文件会被替换。

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ffmpeg::format::stream::StreamMut;
use ffmpeg::{Dictionary, Packet, Rational, Rescale, codec, encoder, format, media};
use ffmpeg_next as ffmpeg;

/// 在多个输入之间排序包时使用的公共时间基（微秒）。
const MICROS: Rational = Rational(1, 1_000_000);

/// 输出流的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    Audio,
}

impl StreamKind {
    fn medium(self) -> media::Type {
        match self {
            StreamKind::Video => media::Type::Video,
            StreamKind::Audio => media::Type::Audio,
        }
    }
}

impl fmt::Display for StreamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StreamKind::Video => "视频",
            StreamKind::Audio => "音频",
        })
    }
}

/// 一路输出流的合并结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamReport {
    pub kind: StreamKind,
    /// FFmpeg 的编码名，如 `h264`、`hevc`、`aac`
    pub codec: &'static str,
    /// 写入、且回读核对一致的包数
    pub packets: u64,
    /// 输入中没有 DTS、因而未写入的包数
    pub skipped_without_dts: u64,
}

/// 合并结果，`streams` 按输出流下标排列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub streams: Vec<StreamReport>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("FFmpeg 初始化失败: {0}")]
    Init(ffmpeg::Error),
    #[error("至少需要一个输入文件")]
    NoInputs,
    #[error("打开输入 {path} 失败: {source}")]
    OpenInput {
        path: PathBuf,
        source: ffmpeg::Error,
    },
    #[error("输入 {path} 中没有视频或音频流")]
    NoStreams { path: PathBuf },
    #[error("输入 {path} 的{kind}流与前面的输入重复")]
    DuplicateKind { path: PathBuf, kind: StreamKind },
    #[error("创建输出 {path} 失败: {source}")]
    OpenOutput {
        path: PathBuf,
        source: ffmpeg::Error,
    },
    #[error("MP4 封装器不接受选项 {0:?}")]
    RejectedOptions(Vec<(String, String)>),
    #[error("读取 {path} 失败: {source}")]
    Read {
        path: PathBuf,
        source: ffmpeg::Error,
    },
    #[error("写入 MP4 失败: {0}")]
    Mux(ffmpeg::Error),
    #[error("输出回读到 {found} 路流，应为 {expected} 路")]
    VerifyStreams { expected: usize, found: usize },
    #[error("输出第 {stream} 路流回读到 {found} 个包，写入了 {written} 个")]
    VerifyPackets {
        stream: usize,
        written: u64,
        found: u64,
    },
    #[error("{action} {path} 失败: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("{cause}；清理临时文件 {path} 也失败: {source}")]
    Cleanup {
        cause: Box<Error>,
        path: PathBuf,
        source: io::Error,
    },
}

/// 把 `inputs` 中的视频与音频复制进 `output`（MP4，moov 前置）。
pub fn remux<P: AsRef<Path>>(inputs: &[P], output: &Path) -> Result<Report, Error> {
    init()?;
    if inputs.is_empty() {
        return Err(Error::NoInputs);
    }
    let part = part_path(output);
    write_verified(inputs, &part)
        .and_then(|report| {
            std::fs::rename(&part, output).map_err(|source| Error::Io {
                action: "重命名",
                path: part.clone(),
                source,
            })?;
            Ok(report)
        })
        .map_err(|cause| discard(&part, cause))
}

fn init() -> Result<(), Error> {
    static INIT: OnceLock<Result<(), ffmpeg::Error>> = OnceLock::new();
    let result = *INIT.get_or_init(|| {
        ffmpeg::init()?;
        // 封装过程中的 info/warning（如 faststart 第二遍）不输出；错误经返回值上抛
        ffmpeg::log::set_level(ffmpeg::log::Level::Error);
        Ok(())
    });
    result.map_err(Error::Init)
}

fn part_path(output: &Path) -> PathBuf {
    let mut name = OsString::from(output.as_os_str());
    name.push(".part");
    PathBuf::from(name)
}

/// 删除失败任务留下的临时文件；删除本身失败时把两个错误一并返回。
fn discard(part: &Path, cause: Error) -> Error {
    match std::fs::remove_file(part) {
        Ok(()) => cause,
        Err(e) if e.kind() == io::ErrorKind::NotFound => cause,
        Err(source) => Error::Cleanup {
            cause: Box::new(cause),
            path: part.to_path_buf(),
            source,
        },
    }
}

struct Source<'a> {
    path: &'a Path,
    ictx: format::context::Input,
    /// 输入流下标 → 输出流下标；None 表示该流不进输出
    map: Vec<Option<usize>>,
    time_bases: Vec<Rational>,
    /// 已读出、待写入的包及其输出流下标；包一定带 DTS
    pending: Option<(Packet, usize)>,
    finished: bool,
}

impl Source<'_> {
    /// 读下一个要写出的包。跳过未映射流的包；没有 DTS 的包计入 `skipped`。读到末尾返回 `None`。
    fn read_next(&mut self, skipped: &mut [u64]) -> Result<Option<(Packet, usize)>, Error> {
        loop {
            let mut packet = Packet::empty();
            match packet.read(&mut self.ictx) {
                Ok(()) => {}
                Err(ffmpeg::Error::Eof) => return Ok(None),
                Err(source) => {
                    return Err(Error::Read {
                        path: self.path.to_path_buf(),
                        source,
                    });
                }
            }
            // TS 可能在文件中途出现新流，其下标超出建立映射时的流数
            let Some(out) = self.map.get(packet.stream()).copied().flatten() else {
                continue;
            };
            if packet.dts().is_none() {
                skipped[out] += 1;
                continue;
            }
            return Ok(Some((packet, out)));
        }
    }
}

fn write_verified<P: AsRef<Path>>(inputs: &[P], part: &Path) -> Result<Report, Error> {
    let mut octx = format::output_as(part, "mp4").map_err(|source| Error::OpenOutput {
        path: part.to_path_buf(),
        source,
    })?;

    // 下标即输出流下标
    let mut kinds: Vec<StreamKind> = Vec::new();
    let mut codecs: Vec<&'static str> = Vec::new();
    let mut sources = Vec::with_capacity(inputs.len());

    for path in inputs {
        let path = path.as_ref();
        let ictx = format::input(path).map_err(|source| Error::OpenInput {
            path: path.to_path_buf(),
            source,
        })?;
        let n = ictx.nb_streams() as usize;
        let mut map = vec![None; n];
        let mut time_bases = vec![Rational(0, 1); n];

        for kind in [StreamKind::Video, StreamKind::Audio] {
            let Some(ist) = ictx
                .streams()
                .find(|s| s.parameters().medium() == kind.medium())
            else {
                continue;
            };
            if kinds.contains(&kind) {
                return Err(Error::DuplicateKind {
                    path: path.to_path_buf(),
                    kind,
                });
            }
            let id = ist.parameters().id();
            let mut ost = octx
                .add_stream(encoder::find(codec::Id::None))
                .map_err(Error::Mux)?;
            ost.set_parameters(ist.parameters());
            set_codec_tag(&mut ost, id);
            map[ist.index()] = Some(ost.index());
            time_bases[ist.index()] = ist.time_base();
            kinds.push(kind);
            codecs.push(id.name());
        }
        if map.iter().all(Option::is_none) {
            return Err(Error::NoStreams {
                path: path.to_path_buf(),
            });
        }
        sources.push(Source {
            path,
            ictx,
            map,
            time_bases,
            pending: None,
            finished: false,
        });
    }

    let mut options = Dictionary::new();
    options.set("movflags", "+faststart");
    let rejected: Vec<(String, String)> = octx
        .write_header_with(options)
        .map_err(Error::Mux)?
        .iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    if !rejected.is_empty() {
        return Err(Error::RejectedOptions(rejected));
    }

    let mut written = vec![0u64; kinds.len()];
    let mut skipped = vec![0u64; kinds.len()];
    loop {
        for source in &mut sources {
            if source.pending.is_none() && !source.finished {
                source.pending = source.read_next(&mut skipped)?;
                source.finished = source.pending.is_none();
            }
        }
        // 各输入按 DTS 交错写出，避免封装器为凑齐交错缓存整条流
        let next = sources
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let (packet, _) = s.pending.as_ref()?;
                let dts = packet.dts().expect("read_next 只放行带 DTS 的包");
                Some((i, dts.rescale(s.time_bases[packet.stream()], MICROS)))
            })
            .min_by_key(|&(_, key)| key);
        let Some((i, _)) = next else { break };

        let source = &mut sources[i];
        let (mut packet, out) = source.pending.take().expect("next 只指向有待写包的输入");
        let in_tb = source.time_bases[packet.stream()];
        let out_tb = octx
            .stream(out)
            .expect("输出流在 add_stream 时创建")
            .time_base();
        packet.rescale_ts(in_tb, out_tb);
        packet.set_position(-1);
        packet.set_stream(out);
        packet.write_interleaved(&mut octx).map_err(Error::Mux)?;
        written[out] += 1;
    }
    octx.write_trailer().map_err(Error::Mux)?;
    drop(octx);

    let found = count_packets(part)?;
    if found.len() != written.len() {
        return Err(Error::VerifyStreams {
            expected: written.len(),
            found: found.len(),
        });
    }
    for (stream, (&written, &found)) in written.iter().zip(&found).enumerate() {
        if written != found {
            return Err(Error::VerifyPackets {
                stream,
                written,
                found,
            });
        }
    }

    let streams = kinds
        .into_iter()
        .zip(codecs)
        .zip(written.into_iter().zip(skipped))
        .map(
            |((kind, codec), (packets, skipped_without_dts))| StreamReport {
                kind,
                codec,
                packets,
                skipped_without_dts,
            },
        )
        .collect();
    Ok(Report { streams })
}

/// HEVC 标 `hvc1`（QuickTime/Safari 不播 `hev1`）；其余置 0，由 MP4 封装器按编码选择。
fn set_codec_tag(ost: &mut StreamMut<'_>, id: codec::Id) {
    let tag = if id == codec::Id::HEVC {
        u32::from_le_bytes(*b"hvc1")
    } else {
        0
    };
    // SAFETY: codecpar 由 avformat_new_stream 分配、归输出上下文所有且非空；此时尚未写头，改 codec_tag 不影响其他状态。
    unsafe {
        (*ost.parameters().as_mut_ptr()).codec_tag = tag;
    }
}

/// 回读文件，按流下标统计包数。
fn count_packets(path: &Path) -> Result<Vec<u64>, Error> {
    let read_err = |source| Error::Read {
        path: path.to_path_buf(),
        source,
    };
    let mut ictx = format::input(path).map_err(read_err)?;
    let mut counts = vec![0u64; ictx.nb_streams() as usize];
    loop {
        let mut packet = Packet::empty();
        match packet.read(&mut ictx) {
            Ok(()) => counts[packet.stream()] += 1,
            Err(ffmpeg::Error::Eof) => return Ok(counts),
            Err(source) => return Err(read_err(source)),
        }
    }
}
