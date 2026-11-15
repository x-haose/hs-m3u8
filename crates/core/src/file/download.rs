//! 文件任务的下载：分段（见 [`super::schedule`]）的多个连接写同一个数据文件，定时落盘并记下已完成的范围；或一个连接
//! 从头读到尾。

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::open::{Opened, open_range, request_end};
use super::range_set::RangeSet;
use super::schedule::Schedule;
use crate::error::io_error;
use crate::hooks::Purpose;
use crate::http::{Http, Permit, RangeRequest, Streaming};
use crate::workdir::FileDir;
use crate::{Error, FileProgress, HttpErrorKind, HttpFailure, Throttled, WorkDirProblem, blocking};

/// 每个连接攒够这么多再写一次数据文件。
const WRITE_BUFFER: usize = 1 << 20;

/// 写了这么多还没记下时，不等定时就落盘、记一次。
const RECORD_BYTES: u64 = 8 << 20;

/// 至少每隔这么久落盘、记一次已完成的范围。
const RECORD_INTERVAL: Duration = Duration::from_secs(1);

/// 下载方式与第一个请求，由第一个响应定下。
pub(super) enum Mode {
    /// 支持范围请求、知道总长度：分段下载，记已完成的范围，可续传。`first` 为已收到响应头的第一个请求（从第一段没下的
    /// 起点起），都已落盘时为 None
    Ranged { length: u64, first: Option<Opened> },
    /// 一个连接从头读到尾，不记范围、不续传；`length` 为 Content-Length
    Whole {
        length: Option<u64>,
        stream: Streaming,
    },
}

impl Mode {
    pub(super) fn length(&self) -> Option<u64> {
        match self {
            Mode::Ranged { length, .. } => Some(*length),
            Mode::Whole { length, .. } => *length,
        }
    }
}

/// 下载的配置：由 [`super::run`] 建出，按 [`Mode`] 分派。
pub(super) struct Download {
    pub http: Arc<Http>,
    /// 来源地址；各请求都发往它（重定向由客户端跟随）
    pub url: Url,
    /// 每个请求至多取的字节数
    pub chunk: Option<u64>,
    /// 连接数上限
    pub connections: usize,
    /// 分段下载各请求带的 If-Range（见 [`super::headers::validator`]）
    pub validator: Option<String>,
    pub progress: watch::Sender<FileProgress>,
    /// 任务的取消
    pub cancel: CancellationToken,
}

impl Download {
    /// 按 `mode` 把文件下进 `dir` 的数据文件，返回文件的字节数；`done` 为续传前已落盘的范围（分段下载才有，为空即新
    /// 开始）。
    pub(super) async fn run(self, dir: &FileDir, mode: Mode, done: RangeSet) -> Result<u64, Error> {
        match mode {
            Mode::Ranged { length, first } => ranged(self, dir, length, first, done).await,
            Mode::Whole { length, stream } => whole(self, dir, length, stream).await,
        }
    }
}

/// 一个连接从头读到尾：失败按重试策略从头再来（数据文件清空重写）；`length` 为 Content-Length，有时核对读到的字节
/// 数。
async fn whole(
    download: Download,
    dir: &FileDir,
    length: Option<u64>,
    first: Streaming,
) -> Result<u64, Error> {
    let data = dir.data();
    let file = {
        let data = data.clone();
        blocking(move || create_data(&data)).await??
    };
    let file = Arc::new(file);
    download.progress.send_modify(|p| p.connections = 1);
    let mut first = Some(first);
    let mut tried = 1;
    let result = loop {
        let attempt = async {
            let stream = match first.take() {
                Some(stream) => stream,
                None => {
                    let request = RangeRequest::whole();
                    download
                        .http
                        .open(
                            Purpose::File,
                            &download.url,
                            &request,
                            Permit::Vod,
                            &download.cancel,
                        )
                        .await?
                }
            };
            // 从头再来：上次写的可能比这次的长，先清掉
            let (cleared, path) = (file.clone(), data.clone());
            blocking(move || cleared.set_len(0).map_err(io_error("写入", &path))).await??;
            read_whole(&download, stream, &file, &data, length).await
        };
        match attempt.await {
            Ok(bytes) => break Ok(bytes),
            Err(error) => {
                download.progress.send_modify(|p| {
                    p.bytes = 0;
                    p.completed.clear();
                });
                if let Err(error) = download
                    .http
                    .wait_to_retry(
                        Purpose::File,
                        &download.url,
                        error,
                        &mut tried,
                        &download.cancel,
                    )
                    .await
                {
                    break Err(error);
                }
            }
        }
    };
    download.progress.send_modify(|p| p.connections = 0);
    let bytes = result?;
    blocking(move || file.sync_data().map_err(io_error("落盘", &data))).await??;
    Ok(bytes)
}

/// 把 `stream` 的响应体从数据文件开头写起，返回字节数；`length` 有值而读到的不同时报 [`Error::Incomplete`]。
async fn read_whole(
    download: &Download,
    mut stream: Streaming,
    file: &Arc<File>,
    data: &Path,
    length: Option<u64>,
) -> Result<u64, Error> {
    let mut buf = Vec::new();
    let mut written = 0u64;
    loop {
        let n = stream.read_into(&mut buf, &download.cancel).await?;
        let end = n == 0;
        if end || buf.len() >= WRITE_BUFFER {
            let len = buf.len() as u64;
            buf = write_at(file, data, buf, written).await?;
            written += len;
            download.progress.send_modify(|p| {
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

/// 分段下载：各连接写同一个数据文件，主循环等它们结束，定时落盘、记已完成的范围；失败或取消时同样记下已写的再返回。
async fn ranged(
    download: Download,
    dir: &FileDir,
    length: u64,
    first: Option<Opened>,
    done: RangeSet,
) -> Result<u64, Error> {
    let data = dir.data();
    let file = {
        let (data, root) = (data.clone(), dir.root().to_path_buf());
        let resumed = !done.ranges().is_empty();
        blocking(move || open_data(&root, &data, length, resumed)).await??
    };
    let missing = done.missing(length);
    let Download {
        http,
        url,
        chunk,
        connections,
        validator,
        progress,
        cancel,
    } = download;
    let ctx = Arc::new(Ctx {
        cancel: cancel.child_token(),
        http,
        url,
        chunk,
        validator,
        progress,
        shared: Mutex::new(Shared {
            schedule: Schedule::new(missing, connections),
            written: done,
            unrecorded: 0,
        }),
        file: Arc::new(file),
        data,
        root: dir.root().to_path_buf(),
        length,
        record_due: Notify::new(),
    });
    let mut workers = JoinSet::new();
    {
        let mut shared = ctx.lock();
        let mut first = first;
        while let Some(id) = shared.schedule.assign() {
            workers.spawn(worker(ctx.clone(), id, first.take()));
        }
        ctx.report_connections(&shared);
    }
    let mut failure = None;
    let mut tick = tokio::time::interval(RECORD_INTERVAL);
    loop {
        let recorded = tokio::select! {
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
        if let Err(error) = recorded {
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
        "各连接都结束且没有失败时已写完整个文件：放回的段总有在途的连接接手（见 Ctx::throttled）"
    );
    Ok(length)
}

/// 建出数据文件；已有同名的（不是本库写的，见 [`crate::workdir::FileDir::open`]）时报错，不覆盖它。
fn create_data(data: &Path) -> Result<File, Error> {
    File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(data)
        .map_err(io_error("创建", data))
}

/// 打开任务目录 `root` 里分段下载的数据文件 `data`：续传（`resumed`）时须已存在、长度为 `length`，否则为任务目录损坏；
/// 新开始时建出长度为 `length` 的文件（多为稀疏文件，不先占空间）。
fn open_data(root: &Path, data: &Path, length: u64, resumed: bool) -> Result<File, Error> {
    if !resumed {
        let file = create_data(data)?;
        file.set_len(length).map_err(io_error("写入", data))?;
        return Ok(file);
    }
    let file = File::options()
        .read(true)
        .write(true)
        .open(data)
        .map_err(|cause| match cause.kind() {
            io::ErrorKind::NotFound => corrupt(root, data, "范围记录在，数据文件却不在".to_owned()),
            _ => io_error("打开", data)(cause),
        })?;
    let len = file.metadata().map_err(io_error("读取", data))?.len();
    if len != length {
        return Err(corrupt(
            root,
            data,
            format!("数据文件长 {len} 字节，应为 {length}"),
        ));
    }
    Ok(file)
}

/// 任务目录 `root` 里的数据文件 `data` 与记录对不上。
fn corrupt(root: &Path, data: &Path, detail: String) -> Error {
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
    validator: Option<String>,
    progress: watch::Sender<FileProgress>,
    file: Arc<File>,
    /// 数据文件
    data: PathBuf,
    /// 任务目录，用于报错
    root: PathBuf,
    length: u64,
    shared: Mutex<Shared>,
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

/// 一个连接下一段的结局。
enum Outcome {
    /// 这一段下完了（或被劈得只剩已下的）
    Done,
    /// 服务器限制了连接数：这一段没下完的已放回（见 [`Ctx::throttled`]）
    Throttled,
}

/// 攒着待写的字节：从数据文件的 `start` 处起，`start` 即这个连接已写入的位置。
struct Buffer {
    start: u64,
    bytes: Vec<u8>,
}

/// 一个连接：下完分到的段 `id` 就要下一段，直到没有可分的；`first` 为已收到响应头的请求。
async fn worker(ctx: Arc<Ctx>, mut id: u64, mut first: Option<Opened>) -> Result<(), Error> {
    loop {
        let outcome = ctx.piece(id, &mut first).await;
        let next = {
            let mut shared = ctx.lock();
            let next = match &outcome {
                // 已在 Ctx::throttled 里放回、报过
                Ok(Outcome::Throttled) => return Ok(()),
                Ok(Outcome::Done) => {
                    shared.schedule.release(id);
                    shared.schedule.assign()
                }
                Err(_) => {
                    shared.schedule.release(id);
                    None
                }
            };
            ctx.report_connections(&shared);
            next
        };
        outcome?;
        match next {
            Some(next) => id = next,
            None => return Ok(()),
        }
    }
}

/// 同时有多个连接时，新请求的这种失败视为服务器限制了连接数：收到响应之前连接失败（含被重置），或回了 429、503、403。
///
/// 简化：上限降了不再升，网络短暂中断时也会降下来，本次运行之后只用剩下的连接；限连的判定只看本任务在途的连接，几个
/// 任务同时下同一个按连接数限制的主机时，降到一个之后再被限按普通失败处理。要分清这些时，按同时有没有请求成功来判、
/// 并在一段时间没再被限后回升。
fn throttle_failure(error: &Error) -> Option<HttpFailure> {
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

    /// 进度里的连接数：分到了段的连接（含等着引擎名额、还没发出请求的）。
    fn report_connections(&self, shared: &Shared) {
        let active = shared.schedule.active();
        self.progress.send_modify(|p| p.connections = active);
    }

    /// 连接 `id` 的新请求以 `error` 失败：是限连的样子（见 [`throttle_failure`]）且还有别的连接在途时，降低上限、把这一
    /// 段放回（同一次加锁里：放回的段总有在途的连接下完自己的之后接手），报进进度，返回 true；否则为 false，按普通失败
    /// 处理。
    fn throttled(&self, id: u64, error: &Error) -> bool {
        let Some(failure) = throttle_failure(error) else {
            return false;
        };
        let mut shared = self.lock();
        if shared.schedule.active() <= 1 {
            return false;
        }
        let limit = shared.schedule.lower_limit();
        shared.schedule.release(id);
        let active = shared.schedule.active();
        self.progress.send_modify(|p| {
            p.limit = limit;
            p.connections = active;
            p.throttled = Some(Throttled { limit, failure });
        });
        true
    }

    /// 下连接 `id` 分到的段，失败按重试策略再试（从已写到的地方接着下；有进展就重新计次）。
    async fn piece(&self, id: u64, first: &mut Option<Opened>) -> Result<Outcome, Error> {
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

    /// 一次尝试：逐个请求下这一段剩下的，每个请求取到这一段的终点（给了块大小时至多一块；服务器给得少时到它给的为止）；
    /// 读到的都写进数据文件后才返回，失败也是。
    async fn attempt(&self, id: u64, first: &mut Option<Opened>) -> Result<Outcome, Error> {
        loop {
            let remaining = self.lock().schedule.remaining(id);
            if remaining.is_empty() {
                return Ok(Outcome::Done);
            }
            let opened = match first.take() {
                Some(opened) => opened,
                None => {
                    let end = request_end(&remaining, self.chunk);
                    let opened = open_range(
                        &self.http,
                        &self.url,
                        remaining.start..end,
                        self.validator.as_deref(),
                        self.length,
                        &self.root,
                        &self.cancel,
                    )
                    .await;
                    match opened {
                        Ok(opened) => opened,
                        Err(error) if self.throttled(id, &error) => return Ok(Outcome::Throttled),
                        Err(error) => return Err(error),
                    }
                }
            };
            assert_eq!(opened.start, remaining.start, "请求从这一段已写入的位置起");
            self.read(id, opened).await?;
        }
    }

    /// 读 `opened` 的响应体写进数据文件，到这个请求的终点或这一段（可能已被劈短）的终点为止，攒够了就写；不到终点就读完
    /// 了报 [`Error::Incomplete`]。读出错时先写完攒着的再报。
    async fn read(&self, id: u64, opened: Opened) -> Result<(), Error> {
        let Opened {
            mut stream,
            start,
            end,
        } = opened;
        let mut buf = Buffer {
            start,
            bytes: Vec::new(),
        };
        loop {
            // 劈开处按已写入的位置算，写盘期间被劈时它可能落在正写的数据中间：写完后位置已越过终点（越过的部分内容相
            // 同，接手的连接会再写一遍），不用再读
            let limit = end.min(self.lock().schedule.remaining(id).end);
            if buf.start >= limit {
                return Ok(());
            }
            let read = stream.read_into(&mut buf.bytes, &self.cancel).await;
            let limit = end.min(self.lock().schedule.remaining(id).end);
            // 被劈之后多收到的属于别的连接
            let kept = limit.saturating_sub(buf.start).min(buf.bytes.len() as u64);
            buf.bytes.truncate(kept as usize);
            let reached = buf.start + kept >= limit;
            match read {
                Err(error) => {
                    self.flush(id, &mut buf).await?;
                    return Err(error);
                }
                Ok(0) if !reached => {
                    self.flush(id, &mut buf).await?;
                    return Err(Error::Incomplete {
                        url: Box::new(stream.url().clone()),
                    });
                }
                Ok(_) if reached || buf.bytes.len() >= WRITE_BUFFER => {
                    self.flush(id, &mut buf).await?;
                    if reached {
                        return Ok(());
                    }
                }
                Ok(_) => {}
            }
        }
    }

    /// 把 `buf` 写进数据文件，再前移连接 `id` 的位置、计入已写的范围与进度；写满 [`RECORD_BYTES`] 还没记时通知主循环记
    /// 一次。写失败时位置不动，重试从这里接着下。
    async fn flush(&self, id: u64, buf: &mut Buffer) -> Result<(), Error> {
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
            shared.schedule.advance(id, len);
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
