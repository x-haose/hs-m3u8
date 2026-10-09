//! 下载器：拉取分片，解密、校验后写入任务目录；另提供拉取 init 段的函数。
//!
//! 分片可以随时加入（点播一次加入全部，直播随刷新加入）；同时在途的不超过任务的并发数，其余排队。
//! 每项完成后由驱动方取走结果（[`Fetcher::next`] / [`Fetcher::drain`]），据此更新进度、决定失败如何处理。

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hs_m3u8_hls::{InitSection, Segment};
use tokio::sync::{OnceCell, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::crypto::decrypt;
use crate::hooks::{HookKind, Hooks, Purpose, run_hook};
use crate::http::{Http, Permit};
use crate::verify::{check_fmp4, check_standalone_segment};
use crate::{Error, Integrity, Progress, workdir};

/// 一项下载：第 `track` 条轨的分片，`path` 为写入任务目录的位置。
pub(crate) struct Item {
    pub track: usize,
    pub segment: Box<Segment>,
    pub path: PathBuf,
}

/// 随结果返回的项标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ItemId {
    pub track: usize,
    pub sequence: u64,
}

/// 一项的结果：成功时为写入的字节数。失败包装为 [`Error::Segment`]（取消除外）。
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

    /// 立即拉取一个分片（经回调、解密、校验），不排队、不写盘；用于续录时核对内容。失败时返回原始错误，
    /// 不包装为 [`Error::Segment`]。
    pub(crate) async fn fetch(&self, segment: &Segment) -> Result<Vec<u8>, Error> {
        fetch_segment(&self.ctx, segment, &self.abort).await
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.running.is_empty()
    }

    /// 丢弃排队的项并取消在途的项；在途的项随后以 [`Error::Cancelled`] 结束。
    pub(crate) fn abort(&mut self) {
        self.pending.clear();
        self.abort.cancel();
    }

    /// 等排队与在途的项全部结束。`on_finished` 处理每项结果，返回 `Err` 时取消其余项；
    /// 返回第一个真正的失败，其余项因此以 Cancelled 结束不覆盖它。
    pub(crate) async fn drain(
        &mut self,
        mut on_finished: impl FnMut(ItemId, Result<u64, Error>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut outcome = Ok(());
        while let Some((id, result)) = self.next().await {
            if let Err(e) = on_finished(id, result) {
                self.abort();
                if matches!(outcome, Ok(()) | Err(Error::Cancelled)) {
                    outcome = Err(e);
                }
            }
        }
        outcome
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

/// 一个分片成功后更新进度。
pub(crate) fn record_done(progress: &watch::Sender<Progress>, len: u64) {
    progress.send_modify(|p| {
        p.bytes += len;
        p.segments_done += 1;
    });
}

async fn run(ctx: Arc<Ctx>, item: Item, cancel: CancellationToken) -> Finished {
    let Item {
        track,
        segment,
        path,
    } = item;
    let id = ItemId {
        track,
        sequence: segment.sequence,
    };
    let data = fetch_segment(&ctx, &segment, &cancel)
        .await
        .map_err(|e| match e {
            Error::Cancelled => Error::Cancelled,
            e => Error::Segment {
                track,
                sequence: segment.sequence,
                url: Box::new(segment.uri.clone()),
                cause: Box::new(e),
            },
        });
    let result = match data {
        Ok(data) => {
            let len = data.len() as u64;
            workdir::write(path, data).await.map(|()| len)
        }
        Err(e) => Err(e),
    };
    (id, result)
}

/// 拉取 init 段并校验是 fMP4。
pub(crate) async fn fetch_init(
    http: &Http,
    init: &InitSection,
    permit: Permit,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    let data = http
        .get(Purpose::Init, &init.uri, init.byte_range, permit, cancel)
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
        .get(
            Purpose::Segment,
            &segment.uri,
            segment.byte_range,
            Permit::Required,
            cancel,
        )
        .await?
        .body;
    let url = segment.uri.clone();
    let data = run_hook(&ctx.hooks, HookKind::Segment, move |hooks| {
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
        None => check_standalone_segment(&data),
    }
    .map_err(integrity)?;
    Ok(data)
}

/// 取 key：同一地址只拉取一次，经 `on_key` 回调后必须是 16 字节。拉取失败不缓存，下次重新拉取。
/// 请求与回调的错误包装为 [`Error::Key`]（取消除外）。
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
            let wrap = |e: Error| match e {
                Error::Cancelled => Error::Cancelled,
                e => Error::Key {
                    url: Box::new(url.clone()),
                    cause: Box::new(e),
                },
            };
            let body = ctx
                .http
                .get(Purpose::Key, url, None, Permit::Required, cancel)
                .await
                .map_err(wrap)?
                .body;
            let hook_url = url.clone();
            let data = run_hook(&ctx.hooks, HookKind::Key, move |hooks| {
                hooks.on_key(&hook_url, body)
            })
            .await
            .map_err(wrap)?;
            <[u8; 16]>::try_from(data.as_slice()).map_err(|_| Error::KeyLength {
                url: Box::new(url.clone()),
                length: data.len(),
            })
        })
        .await?;
    Ok(*key)
}
