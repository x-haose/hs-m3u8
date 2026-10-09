//! 下载器：拉取 init 段与分片，解密、校验后写入任务目录。
//!
//! 项可以随时加入（点播一次加入全部，直播随刷新加入）；同时在途的项不超过任务的并发数，
//! 其余排队。每项完成后由驱动方取走结果（[`Fetcher::next`]），据此更新进度、决定失败如何处理。

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hs_m3u8_hls::{InitSection, Segment};
use tokio::sync::{OnceCell, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::crypto::{check_fmp4, check_ts, decrypt};
use crate::http::Http;
use crate::plan::Plan;
use crate::workdir::{self, WorkDir};
use crate::{Error, Hooks, Integrity, Progress, Purpose, Stage, blocking, run_hook};

/// 一项下载，`path` 为写入任务目录的位置。
pub(crate) enum Item {
    Init {
        track: usize,
        init: InitSection,
        path: PathBuf,
    },
    Segment {
        track: usize,
        segment: Box<Segment>,
        path: PathBuf,
    },
}

/// 随结果返回的项标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemId {
    Init { track: usize },
    Segment { track: usize, sequence: u64 },
}

/// 一项的结果：成功时为写入的字节数。分片失败包装为 [`Error::Segment`]（取消除外）。
pub(crate) type Finished = (ItemId, Result<u64, Error>);

struct Ctx {
    http: Arc<Http>,
    hooks: Arc<dyn Hooks>,
    /// key 地址 → 处理后的 16 字节 key；每个地址只拉取一次
    keys: Mutex<HashMap<Url, Arc<OnceCell<[u8; 16]>>>>,
}

pub(crate) struct Fetcher {
    ctx: Arc<Ctx>,
    limit: usize,
    pending: VecDeque<Item>,
    running: JoinSet<Finished>,
    /// 任务取消令牌的子令牌；[`Fetcher::abort`] 只取消下载，不影响任务的其他部分
    abort: CancellationToken,
}

impl Fetcher {
    pub(crate) fn new(
        http: Arc<Http>,
        hooks: Arc<dyn Hooks>,
        limit: NonZeroUsize,
        cancel: &CancellationToken,
    ) -> Self {
        Fetcher {
            ctx: Arc::new(Ctx {
                http,
                hooks,
                keys: Mutex::new(HashMap::new()),
            }),
            limit: limit.get(),
            pending: VecDeque::new(),
            running: JoinSet::new(),
            abort: cancel.child_token(),
        }
    }

    pub(crate) fn push(&mut self, item: Item) {
        self.pending.push_back(item);
        self.fill();
    }

    /// 下一项的结果；没有排队也没有在途的项时为 None。
    pub(crate) async fn next(&mut self) -> Option<Finished> {
        let joined = self.running.join_next().await?;
        self.fill();
        Some(match joined {
            Ok(finished) => finished,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => unreachable!("下载项不会被单独中止；运行时关闭时驱动方也不再被轮询"),
        })
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.running.is_empty()
    }

    /// 丢弃排队的项并取消在途的项；在途的项随后以 [`Error::Cancelled`] 结束。
    pub(crate) fn abort(&mut self) {
        self.pending.clear();
        self.abort.cancel();
    }

    fn fill(&mut self) {
        while self.running.len() < self.limit
            && let Some(item) = self.pending.pop_front()
        {
            self.running
                .spawn(run(self.ctx.clone(), item, self.abort.clone()));
        }
    }
}

async fn run(ctx: Arc<Ctx>, item: Item, cancel: CancellationToken) -> Finished {
    match item {
        Item::Init { track, init, path } => {
            let result = async {
                let data = fetch_init(&ctx.http, &init, &cancel).await?;
                write(path, data).await
            };
            (ItemId::Init { track }, result.await)
        }
        Item::Segment {
            track,
            segment,
            path,
        } => {
            let id = ItemId::Segment {
                track,
                sequence: segment.sequence,
            };
            let data = fetch_segment(&ctx, &segment, &cancel)
                .await
                .map_err(|e| match e {
                    Error::Cancelled => Error::Cancelled,
                    e => Error::Segment {
                        sequence: segment.sequence,
                        url: Box::new(segment.uri.clone()),
                        source: Box::new(e),
                    },
                });
            let result = match data {
                Ok(data) => write(path, data).await,
                Err(e) => Err(e),
            };
            (id, result)
        }
    }
}

async fn write(path: PathBuf, data: Vec<u8>) -> Result<u64, Error> {
    let len = data.len() as u64;
    workdir::write(path, data).await?;
    Ok(len)
}

/// 下载点播计划中尚未完成的 init 段与分片。返回 `Ok` 即全部已在任务目录中。
///
/// 任一项失败时取消其余项并返回该错误；任务被取消时返回 [`Error::Cancelled`]。
pub(crate) async fn download(
    fetcher: &mut Fetcher,
    plan: Arc<Plan>,
    dir: &WorkDir,
    progress: &watch::Sender<Progress>,
) -> Result<(), Error> {
    let (scan_plan, scan_dir) = (plan.clone(), dir.clone());
    let (items, done, bytes) = blocking(move || pending(&scan_plan, &scan_dir)).await??;
    progress.send_modify(|p| {
        p.stage = Stage::Downloading;
        p.segments_total = plan.segment_count();
        p.segments_done = done;
        p.bytes = bytes;
    });
    for item in items {
        fetcher.push(item);
    }
    let mut outcome = Ok(());
    while let Some((id, result)) = fetcher.next().await {
        match result {
            Ok(len) => progress.send_modify(|p| {
                p.bytes += len;
                if matches!(id, ItemId::Segment { .. }) {
                    p.segments_done += 1;
                }
            }),
            Err(e) => {
                fetcher.abort();
                // 保留第一个真正的失败；其余项因取消而返回的 Cancelled 不覆盖它
                if matches!(outcome, Ok(()) | Err(Error::Cancelled)) {
                    outcome = Err(e);
                }
            }
        }
    }
    outcome
}

/// 尚未完成的项（init 段在前），以及已完成的分片数与字节数（含 init 段）。
fn pending(plan: &Plan, dir: &WorkDir) -> Result<(Vec<Item>, usize, u64), Error> {
    let mut items = Vec::new();
    let (mut done, mut bytes) = (0, 0);
    for (track, t) in plan.tracks.iter().enumerate() {
        for (index, init) in t.inits.iter().enumerate() {
            let path = dir.init(track, index);
            match workdir::completed_len(&path)? {
                Some(len) => bytes += len,
                None => items.push(Item::Init {
                    track,
                    init: init.clone(),
                    path,
                }),
            }
        }
    }
    for (track, t) in plan.tracks.iter().enumerate() {
        for segment in &t.segments {
            let path = dir.segment(
                track,
                segment.sequence,
                segment.discontinuity,
                t.init_index(segment),
            );
            match workdir::completed_len(&path)? {
                Some(len) => {
                    done += 1;
                    bytes += len;
                }
                None => items.push(Item::Segment {
                    track,
                    segment: Box::new(segment.clone()),
                    path,
                }),
            }
        }
    }
    Ok((items, done, bytes))
}

/// 拉取 init 段并校验是 fMP4。
pub(crate) async fn fetch_init(
    http: &Http,
    init: &InitSection,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    let data = http
        .get(Purpose::Init, &init.uri, init.byte_range, cancel)
        .await?
        .body;
    check_fmp4(&data).map_err(|kind| Error::Integrity {
        url: Box::new(init.uri.clone()),
        kind,
    })?;
    Ok(data)
}

/// 拉取分片，经 `on_segment` 回调后解密并校验内容。
async fn fetch_segment(
    ctx: &Ctx,
    segment: &Segment,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    let key = match &segment.key {
        Some(key) => Some((key_for(ctx, &key.uri, cancel).await?, key.iv)),
        None => None,
    };
    let body = ctx
        .http
        .get(Purpose::Segment, &segment.uri, segment.byte_range, cancel)
        .await?
        .body;
    let url = segment.uri.clone();
    let data = run_hook(&ctx.hooks, Purpose::Segment, move |hooks| {
        hooks.on_segment(&url, body)
    })
    .await?;
    let integrity = |kind: Integrity| Error::Integrity {
        url: Box::new(segment.uri.clone()),
        kind,
    };
    let data = match key {
        Some((key, iv)) => decrypt(data, &key, &iv).map_err(integrity)?,
        None => data,
    };
    match segment.init {
        Some(_) => check_fmp4(&data),
        None => check_ts(&data),
    }
    .map_err(integrity)?;
    Ok(data)
}

/// 取 key：同一地址只拉取一次，经 `on_key` 回调后必须是 16 字节。拉取失败不缓存，下次重新拉取。
async fn key_for(ctx: &Ctx, url: &Url, cancel: &CancellationToken) -> Result<[u8; 16], Error> {
    let cell = ctx
        .keys
        .lock()
        .expect("持锁期间只做 HashMap 查找与插入，不会 panic")
        .entry(url.clone())
        .or_default()
        .clone();
    let key = cell
        .get_or_try_init(|| async {
            let body = ctx.http.get(Purpose::Key, url, None, cancel).await?.body;
            let hook_url = url.clone();
            let data = run_hook(&ctx.hooks, Purpose::Key, move |hooks| {
                hooks.on_key(&hook_url, body)
            })
            .await?;
            <[u8; 16]>::try_from(data.as_slice()).map_err(|_| Error::KeyLength {
                url: Box::new(url.clone()),
                length: data.len(),
            })
        })
        .await?;
    Ok(*key)
}
