//! 分片链：把一个或几个文件（或其中的几段）当作一个连续的字节流交给 FFmpeg 读，同一时刻只打开一个文件：一条轨在一组
//! 里的分片，或回读的输出。
//!
//! FFmpeg 的 concat 协议在打开时就打开全部文件并持有到关闭，一组几百个分片就会超过进程的文件描述符
//! 上限（macOS 图形程序默认 256）；这里按偏移定位，读到哪个文件才打开哪个。

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ffmpeg::format::context::StreamIo;
use ffmpeg::{Dictionary, Packet, format};
use ffmpeg_next as ffmpeg;

use crate::{Cancel, Error};

/// 读取中止的原因：FFmpeg 只拿到错误码，这里留下供调用方报告。
pub(crate) enum Stopped {
    /// 读文件出错：路径与原始错误
    File(PathBuf, io::Error),
    /// 调用方取消了
    Cancelled,
}

impl Stopped {
    /// 换成带路径的原始错误（`action` 为报错时说的动作）或取消。
    pub(crate) fn into_error(self, action: &'static str) -> Error {
        match self {
            Stopped::File(path, cause) => Error::Io {
                action,
                path,
                cause,
            },
            Stopped::Cancelled => Error::Cancelled,
        }
    }
}

/// 第一次读取中止的原因（见 [`Stopped`]）。
pub(crate) type Failure = Arc<Mutex<Option<Stopped>>>;

/// 经分片链读的 FFmpeg 输入出错：分片链记下的中止原因，或 FFmpeg 自己的错误。
pub(crate) enum Halt {
    Stopped(Stopped),
    Ffmpeg(ffmpeg::Error),
}

/// 取走分片链记下的中止原因。
fn taken(failure: &Failure) -> Option<Stopped> {
    failure.lock().expect("持锁时只赋值，不会 panic").take()
}

/// 把分片链 `chain`（`failure` 为它记中止原因的位置）交给 FFmpeg 打开，格式由开头的内容识别，`options` 为打开选项。
/// 分片链记下的中止原因优先：FFmpeg 探测时把读错误当作读到末尾，有流的参数齐了就照样打开成功。
pub(crate) fn open(
    chain: SegmentChain,
    failure: &Failure,
    options: Option<Dictionary<'static>>,
) -> Result<format::context::Input, Halt> {
    let opened =
        StreamIo::from_read_seek(chain).and_then(|io| format::input_from_stream(io, None, options));
    if let Some(stopped) = taken(failure) {
        return Err(Halt::Stopped(stopped));
    }
    opened.map_err(Halt::Ffmpeg)
}

/// 读下一个包；读到末尾为 false。分片链记下的中止原因优先：FFmpeg 把一部分读错误报成读到末尾（如 TS 读到不足一个包的
/// 数据时），只看它的结果会把读了一半的输入当作读完。用 `Packet::read` 而不是 ffmpeg-next 的 `packets()` 迭代器：后者
/// 把读错误当作读到末尾，并静默跳过 `InvalidData` 的包。
pub(crate) fn read(
    ictx: &mut format::context::Input,
    failure: &Failure,
    packet: &mut Packet,
) -> Result<bool, Halt> {
    let read = packet.read(ictx);
    if let Some(stopped) = taken(failure) {
        return Err(Halt::Stopped(stopped));
    }
    match read {
        Ok(()) => Ok(true),
        Err(ffmpeg::Error::Eof) => Ok(false),
        Err(cause) => Err(Halt::Ffmpeg(cause)),
    }
}

pub(crate) struct SegmentChain {
    /// 各段所在的文件
    paths: Vec<PathBuf>,
    /// 各段在文件里的字节区间
    ranges: Vec<Range<u64>>,
    /// 各段在整个流中的起始偏移；`starts[i + 1] - starts[i]` 为第 i 段的长度，末项为总长
    starts: Vec<u64>,
    position: u64,
    /// 当前打开的文件：(段的下标, 文件, 在段内的读写位置)
    open: Option<(usize, File, u64)>,
    failure: Failure,
    cancel: Cancel,
}

impl SegmentChain {
    /// 按顺序串起各段，每段为文件与其中要读的字节区间；不打开文件。各段长度之和超出 64 位整数范围时报那一段的文件。
    /// `cancel` 取消后读取失败（见 [`Stopped::Cancelled`]）。
    pub(crate) fn new(
        pieces: Vec<(PathBuf, Range<u64>)>,
        cancel: Cancel,
    ) -> Result<(Self, Failure), (PathBuf, io::Error)> {
        let (paths, ranges): (Vec<PathBuf>, Vec<Range<u64>>) = pieces.into_iter().unzip();
        let mut starts = Vec::with_capacity(paths.len() + 1);
        let mut total = 0u64;
        for (path, range) in paths.iter().zip(&ranges) {
            starts.push(total);
            total = total
                .checked_add(range.end.saturating_sub(range.start))
                .ok_or_else(|| {
                    (
                        path.clone(),
                        io::Error::other("各分片长度之和超出 64 位整数范围"),
                    )
                })?;
        }
        starts.push(total);
        let failure = Failure::default();
        let chain = SegmentChain {
            paths,
            ranges,
            starts,
            position: 0,
            open: None,
            failure: failure.clone(),
            cancel,
        };
        Ok((chain, failure))
    }

    fn total(&self) -> u64 {
        *self.starts.last().expect("starts 至少有总长一项")
    }

    /// 含 `position` 的段的下标；长度为 0 的段不会被选中；读到末尾时为 None。
    fn index_at(&self, position: u64) -> Option<usize> {
        if position >= self.total() {
            return None;
        }
        Some(self.starts.partition_point(|&start| start <= position) - 1)
    }

    /// 记下失败的文件与原因后返回错误；`Interrupted` 会被调用方重试，不记。
    fn fail(&self, index: usize, error: io::Error) -> io::Error {
        if error.kind() != io::ErrorKind::Interrupted {
            let copy = io::Error::new(error.kind(), error.to_string());
            self.stop(Stopped::File(self.paths[index].clone(), copy));
        }
        error
    }

    /// 记下第一次中止的原因。
    fn stop(&self, stopped: Stopped) {
        let mut slot = self.failure.lock().expect("持锁时只赋值，不会 panic");
        slot.get_or_insert(stopped);
    }

    /// 打开（或复用）第 `index` 段的文件并定位到流位置 `self.position`。
    fn file_at(&mut self, index: usize) -> io::Result<&mut File> {
        let offset = self.position - self.starts[index];
        let reusable = matches!(&self.open, Some((i, _, at)) if *i == index && *at == offset);
        if !reusable {
            // 先关掉上一个文件，保证同一时刻只占一个文件描述符
            self.open = None;
            let mut file = File::open(&self.paths[index]).map_err(|e| self.fail(index, e))?;
            file.seek(SeekFrom::Start(self.ranges[index].start + offset))
                .map_err(|e| self.fail(index, e))?;
            self.open = Some((index, file, offset));
        }
        Ok(&mut self.open.as_mut().expect("上面刚打开").1)
    }
}

impl Read for SegmentChain {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            self.stop(Stopped::Cancelled);
            return Err(io::Error::other("合并已取消"));
        }
        let Some(index) = self.index_at(self.position) else {
            return Ok(0);
        };
        let remaining = self.starts[index + 1] - self.position;
        let limit = buf
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let result = self.file_at(index)?.read(&mut buf[..limit]);
        let read = result.map_err(|e| self.fail(index, e))?;
        if read == 0 && limit > 0 {
            let truncated = io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "文件比开始读时短（读的期间被改动）",
            );
            return Err(self.fail(index, truncated));
        }
        self.position += read as u64;
        if let Some((_, _, at)) = &mut self.open {
            *at += read as u64;
        }
        Ok(read)
    }
}

impl Seek for SegmentChain {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "定位到流的开头之前");
        self.position = match to {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => self.position.checked_add_signed(d).ok_or_else(invalid)?,
            SeekFrom::End(d) => self.total().checked_add_signed(d).ok_or_else(invalid)?,
        };
        Ok(self.position)
    }
}

/// 路径能否交给 FFmpeg：ffmpeg-next 把路径转成 C 字符串，遇到非 UTF-8 或含 NUL 的路径会 panic。
pub(crate) fn ffmpeg_path(path: &Path) -> Option<&str> {
    path.to_str().filter(|s| !s.contains('\0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试写的文件，放在独立的目录里；测试通过后删除目录，失败时留着查看。读这些文件的分片链要在它之后声明，
    /// 从而先被释放：Windows 上开着的文件删不掉。
    struct Files {
        dir: PathBuf,
        paths: Vec<PathBuf>,
    }

    impl Drop for Files {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                std::fs::remove_dir_all(&self.dir).unwrap();
            }
        }
    }

    /// 把 `parts` 各写成一个文件。
    fn files(name: &str, parts: &[&[u8]]) -> Files {
        let dir = std::env::temp_dir().join(format!("hs-m3u8-chain-{}-{name}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        let paths = parts
            .iter()
            .enumerate()
            .map(|(i, data)| {
                let path = dir.join(i.to_string());
                std::fs::write(&path, data).unwrap();
                path
            })
            .collect();
        Files { dir, paths }
    }

    /// `files`（内容为 `parts`）整个相连的分片链。
    fn chain_of(files: &Files, parts: &[&[u8]]) -> SegmentChain {
        let pieces = files
            .paths
            .iter()
            .zip(parts)
            .map(|(path, data)| (path.clone(), 0..data.len() as u64))
            .collect();
        SegmentChain::new(pieces, Cancel::never()).unwrap().0
    }

    #[test]
    fn reads_files_back_to_back_skipping_empty_ones() {
        let parts: [&[u8]; 4] = [b"abc", b"", b"de", b"f"];
        let files = files("concat", &parts);
        let mut chain = chain_of(&files, &parts);
        let mut all = Vec::new();
        chain.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"abcdef");
    }

    #[test]
    fn seeks_across_file_boundaries() {
        let parts: [&[u8]; 4] = [b"abc", b"", b"de", b"f"];
        let files = files("seek", &parts);
        let mut chain = chain_of(&files, &parts);
        let mut read_from = |to: SeekFrom, n: usize| {
            let at = chain.seek(to).unwrap();
            let mut buf = vec![0; n];
            let got = chain.read(&mut buf).unwrap();
            buf.truncate(got);
            (at, buf)
        };
        assert_eq!(read_from(SeekFrom::Start(2), 8), (2, b"c".to_vec()));
        assert_eq!(read_from(SeekFrom::Start(3), 8), (3, b"de".to_vec()));
        assert_eq!(read_from(SeekFrom::End(-1), 8), (5, b"f".to_vec()));
        assert_eq!(read_from(SeekFrom::Current(-4), 1), (2, b"c".to_vec()));
        assert_eq!(read_from(SeekFrom::Start(6), 8), (6, Vec::new()));
        assert_eq!(read_from(SeekFrom::Start(99), 8), (99, Vec::new()));
        assert!(chain.seek(SeekFrom::Current(-200)).is_err());
    }

    #[test]
    fn only_the_given_ranges_are_read() {
        let files = files("ranges", &[b"abcdef", b"gh"]);
        let paths = &files.paths;
        let mut chain = SegmentChain::new(
            vec![
                (paths[0].clone(), 1..3),
                (paths[0].clone(), 4..6),
                (paths[1].clone(), 1..2),
            ],
            Cancel::never(),
        )
        .unwrap()
        .0;
        let mut all = Vec::new();
        chain.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"bcefh");
        assert_eq!(chain.seek(SeekFrom::Start(3)).unwrap(), 3);
        let mut rest = Vec::new();
        chain.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"fh");
    }

    #[test]
    fn a_file_shrunk_after_opening_is_reported_with_its_path() {
        let parts: [&[u8]; 2] = [b"abc", b"def"];
        let files = files("shrunk", &parts);
        let mut chain = chain_of(&files, &parts);
        std::fs::write(&chain.paths[1], b"d").unwrap();
        let failure = chain.failure.clone();
        let mut all = Vec::new();
        let err = chain.read_to_end(&mut all).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        let Some(Stopped::File(path, _)) = failure.lock().unwrap().take() else {
            panic!("应记下出错的文件");
        };
        assert_eq!(path, chain.paths[1]);
    }
}
