//! 下载器：拉取分片，解密、校验后写入任务目录；另提供拉取 init 段的函数。
//!
//! 分片可以随时加入（点播一次加入全部，直播随刷新加入）；同时在途的不超过任务的并发数，其余排队。
//! 每项完成后由驱动方取走结果（[`Fetcher::next`] / [`Fetcher::drain`]），据此更新进度、决定失败如何处理。

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hs_m3u8_hls::{InitSection, Segment};
use hs_m3u8_remux::fmp4;
use tokio::sync::{OnceCell, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::crypto::decrypt;
use crate::error::io_error;
use crate::hooks::{HookKind, Hooks, Purpose, run_hook};
use crate::http::{Http, Permit};
use crate::request::KeyOverride;
use crate::verify::{ContentError, Fmp4, check_standalone_segment, decrypt_failure};
use crate::workdir::{self, SegmentName};
use crate::{Error, Progress};

/// 一项下载：第 `track` 条轨的分片，`name` 为它在任务目录中的名字，`path` 为由名字得出的写入位置，`init` 为它所用的
/// init 段在任务目录里的文件（排入下载之前已落盘；分片没有 init 段时为 None）。
pub(crate) struct Item {
    pub track: usize,
    pub segment: Box<Segment>,
    pub name: SegmentName,
    pub path: PathBuf,
    pub init: Option<PathBuf>,
}

/// 随结果返回的项的信息：哪条轨的哪个分片（序号、声明时长、所在的组等见 `name`）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ItemInfo {
    pub track: usize,
    pub name: SegmentName,
}

impl ItemInfo {
    pub(crate) fn of(item: &Item) -> Self {
        ItemInfo {
            track: item.track,
            name: item.name,
        }
    }
}

/// 一项的结果：成功时为写入的字节数。失败包装为 [`Error::Segment`]（取消除外）。
pub(crate) type Finished = (ItemInfo, Result<u64, Error>);

struct Ctx {
    http: Arc<Http>,
    hooks: Arc<dyn Hooks>,
    /// 有值时加密的分片都用它，不取 key
    key_override: Option<KeyOverride>,
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
        key_override: Option<KeyOverride>,
        limit: NonZeroUsize,
        cancel: &CancellationToken,
    ) -> Self {
        Fetcher {
            ctx: Arc::new(Ctx {
                http,
                hooks,
                key_override,
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

    /// 不排队、不写盘地拉取分片的句柄，与本下载器共用 key 缓存。
    pub(crate) fn direct(&self) -> Direct {
        Direct {
            ctx: self.ctx.clone(),
        }
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
        mut on_finished: impl FnMut(ItemInfo, Result<u64, Error>) -> Result<(), Error>,
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

/// 立即拉取分片（经回调、解密、校验）的句柄，可交给后台任务；用于续录时核对内容。
#[derive(Clone)]
pub(crate) struct Direct {
    ctx: Arc<Ctx>,
}

impl Direct {
    /// 拉取第 `track` 条轨的一个分片，不写盘；`init` 见 [`Item`]。失败同排队下载的分片一样包装为
    /// [`Error::Segment`]（取消除外）。
    pub(crate) async fn fetch(
        &self,
        track: usize,
        segment: &Segment,
        init: Option<&Path>,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, Error> {
        fetch_segment(&self.ctx, segment, init, cancel)
            .await
            .map_err(|e| segment_error(track, segment, e))
    }
}

/// 分片失败包装为 [`Error::Segment`]，取消原样返回。
fn segment_error(track: usize, segment: &Segment, error: Error) -> Error {
    match error {
        Error::Cancelled => Error::Cancelled,
        cause => Error::Segment {
            track,
            sequence: segment.sequence,
            url: Box::new(segment.uri.clone()),
            cause: Box::new(cause),
        },
    }
}

/// 一个分片成功后计入进度；`len` 为写入的字节数。
pub(crate) fn count_done(progress: &watch::Sender<Progress>, id: ItemInfo, len: u64) {
    progress.send_modify(|p| p.count_segment(id.track, id.name.duration_us, len));
}

async fn run(ctx: Arc<Ctx>, item: Item, cancel: CancellationToken) -> Finished {
    let id = ItemInfo::of(&item);
    let Item {
        track,
        segment,
        path,
        init,
        ..
    } = item;
    let data = fetch_segment(&ctx, &segment, init.as_deref(), &cancel)
        .await
        .map_err(|e| segment_error(track, &segment, e));
    let result = match data {
        Ok(data) => {
            let len = data.len() as u64;
            workdir::write(path, data).await.map(|()| len)
        }
        Err(e) => Err(e),
    };
    (id, result)
}

/// 拉取 init 段并核对是 fMP4（见 [`check_fmp4`]）；没写完的与请求的临时故障一起重试。
pub(crate) async fn fetch_init(
    http: &Http,
    init: &InitSection,
    permit: Permit,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    http.retrying(&init.uri, cancel, || async move {
        let mut data = http
            .get_once(Purpose::Init, &init.uri, init.byte_range, permit, cancel)
            .await?
            .body;
        check_fmp4(&mut data, None, &init.uri).await?;
        Ok(data)
    })
    .await
}

/// 拉取分片，经 `on_segment` 回调后解密并校验内容（fMP4 见 [`check_fmp4`]，`init` 见 [`Item`]；解密失败见
/// [`decrypt_failure`]）；有自定义 key 时用它解密。请求、回调、解密与校验作为一次尝试，没写完的与请求的临时故障一起
/// 重试。
async fn fetch_segment(
    ctx: &Ctx,
    segment: &Segment,
    init: Option<&Path>,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    let key = match (&segment.key, ctx.key_override) {
        (None, _) => None,
        (Some(key), Some(given)) => Some((given.key, given.iv.unwrap_or(key.iv))),
        (Some(key), None) => Some((key_for(ctx, &key.uri, cancel).await?, key.iv)),
    };
    ctx.http
        .retrying(&segment.uri, cancel, || async move {
            let body = ctx
                .http
                .get_once(
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
            let fmp4 = segment.init.is_some();
            let mut data = match key {
                Some((key, iv)) => decrypt(data, &key, &iv)
                    .map_err(|e| decrypt_failure(e, fmp4).into_error(&segment.uri))?,
                None => data,
            };
            if fmp4 {
                check_fmp4(&mut data, init, &segment.uri).await?;
            } else {
                check_standalone_segment(&data)
                    .map_err(|kind| ContentError::Invalid(kind).into_error(&segment.uri))?;
            }
            Ok(data)
        })
        .await
}

/// 核对取自 `url` 的 fMP4 分片或 init 段的结构，并把长度写 0 的最后一个 box 改写成实际长度（见 [`Fmp4`]）。最后一个
/// box 是长度写 0 的 mdat 时，样本长度可能要用 init 段里 trex 的默认值：`init` 为分片所用的 init 段的文件，这时才读；
/// 核对的是 init 段本身时为 None。
async fn check_fmp4(data: &mut [u8], init: Option<&Path>, url: &Url) -> Result<(), Error> {
    let content = |e: ContentError| e.into_error(url);
    let fmp4 = Fmp4::of(data).map_err(content)?;
    if fmp4.open_mdat() {
        let trex = match init {
            Some(path) => {
                let init = tokio::fs::read(path)
                    .await
                    .map_err(io_error("读取", path))?;
                fmp4::trex_sample_sizes(&init)
            }
            None => HashMap::new(),
        };
        fmp4.check_samples(data, &trex).map_err(content)?;
    }
    fmp4.seal(data).map_err(content)
}

/// 取 key：同一地址只拉取一次。`on_key` 回调不改时拉到的必须是 16 字节。拉取失败不缓存，下次重新拉取。
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
            let (body, transformed) = run_hook(&ctx.hooks, HookKind::Key, move |hooks| {
                let transformed = hooks.on_key(&hook_url, &body)?;
                Ok((body, transformed))
            })
            .await
            .map_err(wrap)?;
            match transformed {
                Some(key) => Ok(key),
                None => <[u8; 16]>::try_from(body.as_slice()).map_err(|_| Error::KeyLength {
                    url: Box::new(url.clone()),
                    length: body.len(),
                }),
            }
        })
        .await?;
    Ok(*key)
}
