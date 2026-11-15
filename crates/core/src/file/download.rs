//! 文件任务的下载：分段（见 [`super::schedule`]）的多个连接写同一个数据文件，定时落盘并记下已完成的范围；或一个连接
//! 从头读到尾。

use std::fs::File;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::range_set::RangeSet;
use super::schedule::Schedule;
use crate::error::io_error;
use crate::hooks::Purpose;
use crate::http::{Conditions, Http, Permit, Streaming};
use crate::workdir::FileDir;
use crate::{Error, FileProgress, HttpErrorKind, HttpFailure, Throttled, WorkDirProblem, blocking};

/// 每个连接攒够这么多再写一次数据文件。
const WRITE_BUFFER: usize = 1 << 20;

/// 写了这么多还没记下时，不等定时就落盘、记一次。
const RECORD_BYTES: u64 = 8 << 20;

/// 至少每隔这么久落盘、记一次已完成的范围。
const RECORD_INTERVAL: Duration = Duration::from_secs(1);

/// 下载方式，由第一个响应定下。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Mode {
    /// 支持范围请求、知道总长度：分段下载，记已完成的范围，可续传；`validator` 为各请求带的 If-Range
    Ranged {
        length: u64,
        validator: Option<String>,
    },
    /// 一个连接从头读到尾，不记范围、不续传；`length` 为 Content-Length
    Whole { length: Option<u64> },
}

impl Mode {
    pub(super) fn length(&self) -> Option<u64> {
        match self {
            Mode::Ranged { length, .. } => Some(*length),
            Mode::Whole { length } => *length,
        }
    }
}

/// 下载用到的共享对象。
pub(super) struct Download {
    pub http: Arc<Http>,
    /// 来源地址；各请求都发往它（重定向由客户端跟随）
    pub url: Url,
    /// 每个请求至多取的字节数
    pub chunk: Option<u64>,
    /// 连接数上限
    pub connections: usize,
    pub progress: watch::Sender<FileProgress>,
    pub cancel: CancellationToken,
}

impl Download {
    /// 按 `mode` 把文件下进 `dir` 的数据文件，返回文件的字节数。`first` 为已收到响应头的第一个请求，取到 `first_end`
    /// （不含；不知道时为 None）；`done` 为续传前已落盘的范围（分段下载才有）。都已落盘时 `first` 为 None。
    pub(super) async fn run(
        self,
        dir: &FileDir,
        mode: Mode,
        first: Option<Streaming>,
        first_end: Option<u64>,
        done: RangeSet,
    ) -> Result<u64, Error> {
        match mode {
            Mode::Ranged { length, validator } => {
                let first = first.map(|s| (s, first_end.expect("范围请求都写明终点")));
                self.ranged(dir, length, validator, first, done).await
            }
            Mode::Whole { length } => self.whole(dir, length, first).await,
        }
    }

    async fn ranged(
        self,
        dir: &FileDir,
        length: u64,
        validator: Option<String>,
        first: Option<(Streaming, u64)>,
        done: RangeSet,
    ) -> Result<u64, Error> {
        let data = dir.data();
        let resumed = done.len() > 0;
        let file = {
            let data = data.clone();
            blocking(move || open_data(&data, length, resumed)).await??
        };
        let missing = done.missing(length);
        let ctx = Arc::new(Ctx {
            http: self.http,
            url: self.url,
            chunk: self.chunk,
            file: Arc::new(file),
            data,
            root: dir.root().to_path_buf(),
            length,
            validator,
            shared: Mutex::new(Shared {
                schedule: Schedule::new(missing, self.connections),
                written: done,
                unrecorded: 0,
            }),
            progress: self.progress,
            record_due: Notify::new(),
            cancel: self.cancel.child_token(),
        });
        let mut workers = JoinSet::new();
        {
            let mut shared = ctx.lock();
            let mut first = first;
            while let Some((id, _)) = shared.schedule.assign() {
                workers.spawn(worker(ctx.clone(), id, first.take()));
            }
            ctx.report_connections(&shared);
        }
        let mut failure = None;
        let mut tick = tokio::time::interval(RECORD_INTERVAL);
        loop {
            let recording = tokio::select! {
                joined = workers.join_next() => {
                    match joined {
                        None => break,
                        Some(Ok(Ok(()))) => {}
                        Some(Ok(Err(error))) => {
                            // 第一个失败是原因；之后的多为因它取消的
                            if failure.is_none() {
                                ctx.cancel.cancel();
                                failure = Some(error);
                            }
                        }
                        Some(Err(e)) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                        // 运行时关闭
                        Some(Err(_)) => return Err(Error::Cancelled),
                    }
                    continue;
                }
                _ = tick.tick() => ctx.record(dir).await,
                () = ctx.record_due.notified() => ctx.record(dir).await,
            };
            if let Err(error) = recording {
                ctx.cancel.cancel();
                failure.get_or_insert(error);
            }
        }
        // 失败或取消时同样记下已写的，续传时沿用；记不下来只是少沿用一些，以下载本身的失败为准
        let recorded = ctx.record(dir).await;
        if let Some(failure) = failure {
            return Err(failure);
        }
        recorded?;
        let shared = ctx.lock();
        assert!(
            shared.schedule.done() && shared.written.missing(length).is_empty(),
            "各连接都结束且没有失败时已写完整个文件"
        );
        Ok(length)
    }

    /// 一个连接从头读到尾：失败按重试策略从头再来；`length` 为 Content-Length，有时核对读到的字节数。
    async fn whole(
        self,
        dir: &FileDir,
        length: Option<u64>,
        mut first: Option<Streaming>,
    ) -> Result<u64, Error> {
        let data = dir.data();
        let file = {
            let data = data.clone();
            blocking(move || {
                File::options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&data)
                    .map_err(io_error("创建", &data))
            })
            .await??
        };
        let file = Arc::new(file);
        self.progress.send_modify(|p| p.connections = 1);
        let mut tried = 1;
        let result = loop {
            let attempt = async {
                let stream = match first.take() {
                    Some(stream) => stream,
                    None => {
                        self.http
                            .open(
                                Purpose::File,
                                &self.url,
                                &Conditions::default(),
                                Permit::Vod,
                                &self.cancel,
                            )
                            .await?
                    }
                };
                self.read_whole(stream, &file, &data, length).await
            };
            match attempt.await {
                Ok(bytes) => break Ok(bytes),
                Err(error) => {
                    self.progress.send_modify(|p| {
                        p.bytes = 0;
                        p.completed.clear();
                    });
                    if let Err(error) = self
                        .http
                        .wait_to_retry(Purpose::File, &self.url, error, &mut tried, &self.cancel)
                        .await
                    {
                        break Err(error);
                    }
                }
            }
        };
        self.progress.send_modify(|p| p.connections = 0);
        let bytes = result?;
        blocking(move || file.sync_data().map_err(io_error("落盘", &data))).await??;
        Ok(bytes)
    }

    /// 把 `stream` 的响应体从数据文件开头写起，返回字节数；`length` 有值而读到的不同时报 [`Error::Incomplete`]。
    async fn read_whole(
        &self,
        mut stream: Streaming,
        file: &Arc<File>,
        data: &Path,
        length: Option<u64>,
    ) -> Result<u64, Error> {
        let mut buf = Vec::new();
        let mut written = 0u64;
        loop {
            let n = stream.read_into(&mut buf, &self.cancel).await?;
            let end = n == 0;
            if end || buf.len() >= WRITE_BUFFER {
                let len = buf.len() as u64;
                buf = write_at(file, data, buf, written).await?;
                written += len;
                self.progress.send_modify(|p| {
                    p.bytes = written;
                    p.completed = std::iter::once(0..written)
                        .filter(|r| !r.is_empty())
                        .collect();
                });
            }
            if end {
                break;
            }
        }
        if length.is_some_and(|l| l != written) {
            return Err(Error::Incomplete {
                url: Box::new(stream.url().clone()),
            });
        }
        Ok(written)
    }
}

/// 打开分段下载的数据文件：续传（`resumed`）时须已存在、长度为 `length`，否则为任务目录损坏；新开始时建出长度为
/// `length` 的文件（多为稀疏文件，不先占空间）。
fn open_data(data: &Path, length: u64, resumed: bool) -> Result<File, Error> {
    let mut options = File::options();
    options.read(true).write(true);
    if !resumed {
        options.create(true).truncate(true);
    }
    let file = options.open(data).map_err(|cause| {
        if resumed && cause.kind() == io::ErrorKind::NotFound {
            corrupt(data, "范围记录在，数据文件却不在".to_owned())
        } else {
            io_error("打开", data)(cause)
        }
    })?;
    let len = file.metadata().map_err(io_error("读取", data))?.len();
    if resumed && len != length {
        return Err(corrupt(
            data,
            format!("数据文件长 {len} 字节，应为 {length}"),
        ));
    }
    if !resumed {
        file.set_len(length).map_err(io_error("写入", data))?;
    }
    Ok(file)
}

/// 任务目录里的数据文件 `data` 与记录对不上。
fn corrupt(data: &Path, detail: String) -> Error {
    let root = data.parent().expect("数据文件在任务目录里");
    Error::WorkDir {
        path: root.to_path_buf(),
        problem: WorkDirProblem::Corrupt(format!("{}：{detail}", data.display())),
    }
}

/// 在阻塞线程里把 `bytes` 写到数据文件 `data` 的 `offset` 处，返回清空的 `bytes` 以便复用。
async fn write_at(
    file: &Arc<File>,
    data: &Path,
    bytes: Vec<u8>,
    offset: u64,
) -> Result<Vec<u8>, Error> {
    if bytes.is_empty() {
        return Ok(bytes);
    }
    let (file, path) = (file.clone(), data.to_path_buf());
    blocking(move || {
        write_all_at(&file, &bytes, offset).map_err(io_error("写入", &path))?;
        let mut bytes = bytes;
        bytes.clear();
        Ok(bytes)
    })
    .await?
}

#[cfg(unix)]
fn write_all_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, bytes, offset)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        match file.seek_write(bytes, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                bytes = &bytes[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 分段下载各连接共用的。
struct Ctx {
    http: Arc<Http>,
    url: Url,
    chunk: Option<u64>,
    file: Arc<File>,
    /// 数据文件
    data: PathBuf,
    /// 任务目录，用于报错
    root: PathBuf,
    length: u64,
    validator: Option<String>,
    shared: Mutex<Shared>,
    progress: watch::Sender<FileProgress>,
    /// 写了 [`RECORD_BYTES`] 还没记时通知主循环
    record_due: Notify,
    /// 本次下载的取消：任务取消，或有连接失败
    cancel: CancellationToken,
}

struct Shared {
    schedule: Schedule,
    /// 已写入数据文件的范围（不一定已落盘），含续传前的
    written: RangeSet,
    /// 上次记录之后写入的字节数
    unrecorded: u64,
}

/// 一个连接上一段的结局。
enum Piece {
    /// 这一段下完了（或被劈得只剩已下的）
    Done,
    /// 服务器限制了连接数，这一段没下完的放回去
    Throttled(HttpFailure),
}

/// 攒着待写的字节：从数据文件的 `start` 处起。
struct Buffer {
    start: u64,
    bytes: Vec<u8>,
}

/// 一个连接：下完分到的段 `id` 就要下一段，直到没有可分的；`first` 为已收到响应头的请求与它取到哪里。
async fn worker(
    ctx: Arc<Ctx>,
    mut id: u64,
    mut first: Option<(Streaming, u64)>,
) -> Result<(), Error> {
    loop {
        let end = ctx.piece(id, &mut first).await;
        let next = {
            let mut shared = ctx.lock();
            let next = match &end {
                Ok(Piece::Done) => {
                    shared.schedule.release(id);
                    shared.schedule.assign()
                }
                Ok(Piece::Throttled(failure)) => {
                    let limit = shared.schedule.lower_limit();
                    shared.schedule.release(id);
                    let throttled = Throttled {
                        limit,
                        failure: failure.clone(),
                    };
                    ctx.progress.send_modify(|p| {
                        p.limit = limit;
                        p.throttled = Some(throttled);
                    });
                    None
                }
                Err(_) => {
                    shared.schedule.release(id);
                    None
                }
            };
            ctx.report_connections(&shared);
            next
        };
        end?;
        match next {
            Some((next, _)) => id = next,
            None => return Ok(()),
        }
    }
}

/// 同时有多个连接时，新请求的这种失败视为服务器限制了连接数：收到响应之前连接失败（含被重置），或回了 429、503、403。
fn throttling(error: &Error) -> Option<HttpFailure> {
    match error {
        Error::Http {
            kind:
                kind @ (HttpErrorKind::Status(403 | 429 | 503)
                | HttpErrorKind::Connect(_)
                | HttpErrorKind::Transport(_)),
            retry_after,
            ..
        } => Some(HttpFailure {
            kind: kind.clone(),
            retry_after: *retry_after,
        }),
        _ => None,
    }
}

impl Ctx {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .expect("持锁期间只改调度与范围，不会 panic")
    }

    fn report_connections(&self, shared: &Shared) {
        let active = shared.schedule.active();
        self.progress.send_modify(|p| p.connections = active);
    }

    /// 下连接 `id` 分到的段，失败按重试策略再试（从已下到的地方接着下；有进展就重新计次）。
    async fn piece(&self, id: u64, first: &mut Option<(Streaming, u64)>) -> Result<Piece, Error> {
        let mut tried = 1;
        loop {
            let start = self.lock().schedule.remaining(id).start;
            match self.attempt(id, first).await {
                Ok(piece) => return Ok(piece),
                Err(error) => {
                    if self.lock().schedule.remaining(id).start > start {
                        tried = 1;
                    }
                    self.http
                        .wait_to_retry(Purpose::File, &self.url, error, &mut tried, &self.cancel)
                        .await?;
                }
            }
        }
    }

    /// 一次尝试：逐个请求下这一段剩下的，每个请求取到这一段的终点（给了块大小时至多一块）；读到的都写进数据文件后才
    /// 返回，失败也是。
    async fn attempt(&self, id: u64, first: &mut Option<(Streaming, u64)>) -> Result<Piece, Error> {
        loop {
            let remaining = self.lock().schedule.remaining(id);
            if remaining.is_empty() {
                return Ok(Piece::Done);
            }
            let (mut stream, request_end) = match first.take() {
                Some(opened) => opened,
                None => {
                    let end = self.chunk.map_or(remaining.end, |c| {
                        remaining.end.min(remaining.start.saturating_add(c))
                    });
                    match self.open(remaining.start..end).await {
                        Ok(stream) => (stream, end),
                        Err(error) => {
                            if let Some(failure) = throttling(&error)
                                && self.lock().schedule.active() > 1
                            {
                                return Ok(Piece::Throttled(failure));
                            }
                            return Err(error);
                        }
                    }
                }
            };
            let mut buf = Buffer {
                start: remaining.start,
                bytes: Vec::new(),
            };
            let read = self.read(id, &mut stream, request_end, &mut buf).await;
            drop(stream);
            self.flush(&mut buf).await?;
            read?;
        }
    }

    /// 请求 `range`（带 If-Range），核对响应（见 [`super::check_partial`]）。
    async fn open(&self, range: Range<u64>) -> Result<Streaming, Error> {
        let conditions = Conditions {
            range: Some(format!("bytes={}-{}", range.start, range.end - 1)),
            if_range: self.validator.clone(),
        };
        let stream = self
            .http
            .open(
                Purpose::File,
                &self.url,
                &conditions,
                Permit::Vod,
                &self.cancel,
            )
            .await?;
        super::check_partial(&stream, range.start, range.end, self.length, &self.root)?;
        Ok(stream)
    }

    /// 读 `stream` 的响应体进 `buf`，到这个请求的终点 `request_end` 或这一段（可能已被劈短）的终点为止，攒够了就写；
    /// 不到终点就读完了报 [`Error::Incomplete`]。
    async fn read(
        &self,
        id: u64,
        stream: &mut Streaming,
        request_end: u64,
        buf: &mut Buffer,
    ) -> Result<(), Error> {
        let mut cursor = buf.start + buf.bytes.len() as u64;
        loop {
            let before = buf.bytes.len();
            let n = stream.read_into(&mut buf.bytes, &self.cancel).await? as u64;
            if n == 0 {
                return Err(Error::Incomplete {
                    url: Box::new(stream.url().clone()),
                });
            }
            let (taken, piece_left) = {
                let mut shared = self.lock();
                let taken = shared.schedule.advance(id, n.min(request_end - cursor));
                (taken, !shared.schedule.remaining(id).is_empty())
            };
            buf.bytes.truncate(before + taken as usize);
            cursor += taken;
            if buf.bytes.len() >= WRITE_BUFFER {
                self.flush(buf).await?;
            }
            if cursor >= request_end || !piece_left {
                return Ok(());
            }
        }
    }

    /// 把 `buf` 写进数据文件，计入已写的范围与进度；写满 [`RECORD_BYTES`] 还没记时通知主循环记一次。
    async fn flush(&self, buf: &mut Buffer) -> Result<(), Error> {
        let len = buf.bytes.len() as u64;
        if len == 0 {
            return Ok(());
        }
        let bytes = std::mem::take(&mut buf.bytes);
        buf.bytes = write_at(&self.file, &self.data, bytes, buf.start).await?;
        let range = buf.start..buf.start + len;
        buf.start = range.end;
        let due = {
            let mut shared = self.lock();
            shared.written.insert(range);
            shared.unrecorded += len;
            let (bytes, completed) = (shared.written.len(), shared.written.ranges().to_vec());
            self.progress.send_modify(|p| {
                p.bytes = bytes;
                p.completed = completed;
            });
            shared.unrecorded >= RECORD_BYTES
        };
        if due {
            self.record_due.notify_one();
        }
        Ok(())
    }

    /// 让已写的落盘，再记下已完成的范围；上次之后没写过时什么也不做。
    async fn record(&self, dir: &FileDir) -> Result<(), Error> {
        let ranges = {
            let mut shared = self.lock();
            if shared.unrecorded == 0 {
                return Ok(());
            }
            shared.unrecorded = 0;
            shared.written.ranges().to_vec()
        };
        let (file, data) = (self.file.clone(), self.data.clone());
        blocking(move || file.sync_data().map_err(io_error("落盘", &data))).await??;
        dir.record_ranges(ranges).await
    }
}
