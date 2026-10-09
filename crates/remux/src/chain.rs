//! 分片链：把一条轨在一组里的多个文件当作一个连续的字节流读取，同一时刻只打开一个文件。
//!
//! FFmpeg 的 concat 协议在打开时就打开全部文件并持有到关闭，一组几百个分片就会超过进程的文件描述符
//! 上限（macOS 图形程序默认 256）；这里按偏移定位，读到哪个文件才打开哪个。

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 读取中遇到的文件错误：FFmpeg 只拿到错误码，这里留下路径与原始错误供调用方报告。
pub(crate) type Failure = Arc<Mutex<Option<(PathBuf, io::Error)>>>;

pub(crate) struct SegmentChain {
    paths: Vec<PathBuf>,
    /// 各文件在整个流中的起始偏移；`starts[i + 1] - starts[i]` 为第 i 个文件的长度，末项为总长
    starts: Vec<u64>,
    position: u64,
    /// 当前打开的文件：(下标, 文件, 文件内的读写位置)
    open: Option<(usize, File, u64)>,
    failure: Failure,
}

impl SegmentChain {
    /// 按顺序串起 `paths`；只读取各文件的长度，不打开文件。
    pub(crate) fn new(paths: Vec<PathBuf>) -> Result<(Self, Failure), (PathBuf, io::Error)> {
        let mut starts = Vec::with_capacity(paths.len() + 1);
        let mut total = 0u64;
        for path in &paths {
            starts.push(total);
            let len = std::fs::metadata(path)
                .map_err(|e| (path.clone(), e))?
                .len();
            total = total.checked_add(len).ok_or_else(|| {
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
            starts,
            position: 0,
            open: None,
            failure: failure.clone(),
        };
        Ok((chain, failure))
    }

    fn total(&self) -> u64 {
        *self.starts.last().expect("starts 至少有总长一项")
    }

    /// 含 `position` 的文件下标；长度为 0 的文件不会被选中；读到末尾时为 None。
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
            let mut slot = self.failure.lock().unwrap_or_else(|p| p.into_inner());
            slot.get_or_insert((self.paths[index].clone(), copy));
        }
        error
    }

    /// 打开（或复用）第 `index` 个文件并定位到流位置 `self.position`。
    fn file_at(&mut self, index: usize) -> io::Result<&mut File> {
        let offset = self.position - self.starts[index];
        let reusable = matches!(&self.open, Some((i, _, at)) if *i == index && *at == offset);
        if !reusable {
            // 先关掉上一个文件，保证同一时刻只占一个文件描述符
            self.open = None;
            let mut file = File::open(&self.paths[index]).map_err(|e| self.fail(index, e))?;
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| self.fail(index, e))?;
            self.open = Some((index, file, offset));
        }
        Ok(&mut self.open.as_mut().expect("上面刚打开").1)
    }
}

impl Read for SegmentChain {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
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
                "文件比开始合并时短（合并期间被改动）",
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

    fn chain_of(name: &str, parts: &[&[u8]]) -> SegmentChain {
        let dir = std::env::temp_dir().join(format!("hs-m3u8-chain-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
        SegmentChain::new(paths).unwrap().0
    }

    #[test]
    fn reads_files_back_to_back_skipping_empty_ones() {
        let mut chain = chain_of("concat", &[b"abc", b"", b"de", b"f"]);
        let mut all = Vec::new();
        chain.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"abcdef");
    }

    #[test]
    fn seeks_across_file_boundaries() {
        let mut chain = chain_of("seek", &[b"abc", b"", b"de", b"f"]);
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
    fn a_file_shrunk_after_opening_is_reported_with_its_path() {
        let mut chain = chain_of("shrunk", &[b"abc", b"def"]);
        std::fs::write(&chain.paths[1], b"d").unwrap();
        let failure = chain.failure.clone();
        let mut all = Vec::new();
        let err = chain.read_to_end(&mut all).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        let (path, _) = failure.lock().unwrap().take().unwrap();
        assert_eq!(path, chain.paths[1]);
    }
}
