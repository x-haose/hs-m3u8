//! 下载器：拉取分片，解密、校验后写入任务目录；另提供不排队、不写盘的拉取（init 段、核对用的分片）。
//!
//! 分片可以随时加入（点播一次加入全部，直播随刷新加入）；同时在途的不超过任务的并发数，其余排队。
//! 每项完成后由驱动方取走结果（[`Fetcher::next`] / [`Fetcher::drain`]），据此更新进度、决定失败如何处理。

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hs_m3u8_hls::{InitSection, Segment, SegmentKey};
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
use crate::verify::{Container, ContentError, check, decrypt_failure};
use crate::workdir::{self, SegmentName};
use crate::{Error, Progress, Unsupported, WorkDirProblem};

/// 一项下载：第 `track` 条轨的分片，`name` 为它在任务目录中的名字，`path` 为由名字得出的写入位置，`init` 为它所用的
/// init 段在任务目录里的文件（排入下载之前已落盘；分片没有 init 段、不是 fMP4 时为 None）。
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
    /// 任务目录
    work_dir: PathBuf,
    /// 有值时加密的分片都用它，不取 key
    key_override: Option<KeyOverride>,
    /// 分片与 key 的请求占哪一份引擎名额
    permit: Permit,
    keys: Mutex<Keys>,
}

/// key 缓存最多记的地址数：远多于常见的并发数（同时在用的 key 不超过在途的分片数）；被淘汰的下次用到时重新拉取。
const KEY_CACHE: usize = 256;

/// 一个地址处理后的 16 字节 key，拉到后填入；同时取同一地址的只拉一次。
type KeyCell = Arc<OnceCell<[u8; 16]>>;

/// key 地址 → 处理后的 key，满了淘汰最久没用到的（见 [`KEY_CACHE`]）；在缓存里的地址只拉取一次。
#[derive(Default)]
struct Keys {
    /// 地址 → (key 所在的单元, 最近一次取用时的 `clock`)
    cells: HashMap<Url, (KeyCell, u64)>,
    /// 每次取用加一
    clock: u64,
}

impl Keys {
    /// 地址 `url` 的 key 所在的单元；没有时新建，满了先淘汰最久没用到的。
    fn cell(&mut self, url: &Url) -> KeyCell {
        self.clock += 1;
        if !self.cells.contains_key(url) && self.cells.len() >= KEY_CACHE {
            let oldest = self
                .cells
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(url, _)| url.clone())
                .expect("满了即非空");
            self.cells.remove(&oldest);
        }
        let (cell, used) = self.cells.entry(url.clone()).or_default();
        *used = self.clock;
        cell.clone()
    }
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
    /// `work_dir` 为任务目录，各项的 init 段在其中；`permit` 为分片与 key 的请求占哪一份引擎名额（点播的或直播的，见
    /// [`crate::Engine::new`]）。
    pub(crate) fn new(
        http: Arc<Http>,
        hooks: Arc<dyn Hooks>,
        key_override: Option<KeyOverride>,
        limit: NonZeroUsize,
        work_dir: &Path,
        permit: Permit,
        cancel: &CancellationToken,
    ) -> Self {
        Fetcher {
            ctx: Arc::new(Ctx {
                http,
                hooks,
                work_dir: work_dir.to_path_buf(),
                key_override,
                permit,
                keys: Mutex::new(Keys::default()),
            }),
            limit: limit.get(),
            pending: VecDeque::new(),
            running: JoinSet::new(),
            abort: cancel.child_token(),
        }
    }

    /// 同时下载的分片数上限（任务的并发数）。
    pub(crate) fn limit(&self) -> usize {
        self.limit
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

/// 立即拉取分片（经回调、解密、校验）与 init 段（解密、校验）的句柄，可交给后台任务；用于拉 init 段与续传、续录时
/// 核对内容。
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

    /// 拉取 init 段，解密（见 [`decryption`]）并校验（见 [`check`]，须为 fMP4）；没写完的与请求的临时故障一起重试。
    /// `permit` 为 init 段的请求占哪一份引擎名额（key 的请求同分片）。
    pub(crate) async fn fetch_init(
        &self,
        init: &InitSection,
        permit: Permit,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, Error> {
        let ctx = &self.ctx;
        let key = decryption(ctx, init.key.as_ref(), cancel).await?;
        ctx.http
            .retrying(&init.uri, cancel, || async move {
                let response = ctx
                    .http
                    .get_once(Purpose::Init, &init.uri, init.byte_range, permit, cancel)
                    .await?;
                let body = nonempty(response.body, &init.uri)?;
                let content = |e: ContentError| e.into_error(&init.uri);
                let mut data = match key {
                    Some((key, iv)) => decrypt(body, &key, &iv)
                        .map_err(|e| content(decrypt_failure(e, &Container::Init)))?,
                    None => body,
                };
                check(&mut data, &Container::Init).map_err(content)?;
                Ok(data)
            })
            .await
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

/// 拉取分片，经 `on_segment` 回调后解密（见 [`decryption`]）并校验内容（见 [`check`]，`init` 见 [`Item`]；解密失败见
/// [`decrypt_failure`]）。请求、回调、解密与校验作为一次尝试，没写完的与请求的临时故障一起重试。来源标了 EXT-X-GAP
/// 的分片不取（RFC 8216 4.4.4.7），交到这里是本库的错误。
async fn fetch_segment(
    ctx: &Ctx,
    segment: &Segment,
    init: Option<&Path>,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    assert!(!segment.gap, "EXT-X-GAP 的分片在排入下载与核对之前已排除");
    let key = decryption(ctx, segment.key.as_ref(), cancel).await?;
    let container = &match init {
        Some(path) => Container::Fmp4 {
            trex: stored_trex(&ctx.work_dir, path).await?,
        },
        None => Container::Standalone,
    };
    ctx.http
        .retrying(&segment.uri, cancel, || async move {
            let response = ctx
                .http
                .get_once(
                    Purpose::Segment,
                    &segment.uri,
                    segment.byte_range,
                    ctx.permit,
                    cancel,
                )
                .await?;
            let body = nonempty(response.body, &segment.uri)?;
            let url = segment.uri.clone();
            let data = run_hook(&ctx.hooks, HookKind::Segment, move |hooks| {
                hooks.on_segment(&url, body)
            })
            .await?;
            let content = |e: ContentError| e.into_error(&segment.uri);
            let mut data = match key {
                Some((key, iv)) => {
                    decrypt(data, &key, &iv).map_err(|e| content(decrypt_failure(e, container)))?
                }
                None => data,
            };
            check(&mut data, container).map_err(content)?;
            Ok(data)
        })
        .await
}

/// 按 `key` 解密用的 key 与 IV；不加密时为 None。有自定义 key 时用它（IV 没给的用 `key` 的），否则按 `key` 的地址取
/// （见 [`key_for`]），没有地址报 [`Unsupported::KeyWithoutUri`]。
async fn decryption(
    ctx: &Ctx,
    key: Option<&SegmentKey>,
    cancel: &CancellationToken,
) -> Result<Option<([u8; 16], [u8; 16])>, Error> {
    let Some(key) = key else {
        return Ok(None);
    };
    if let Some(given) = ctx.key_override {
        return Ok(Some((given.key, given.iv.unwrap_or(key.iv))));
    }
    let url = key
        .uri
        .as_ref()
        .ok_or(Error::Unsupported(Unsupported::KeyWithoutUri))?;
    Ok(Some((key_for(ctx, url, cancel).await?, key.iv)))
}

/// 源站返回空的内容（多为文件刚建出、还没写进内容）时为取自 `url` 的内容没写完。
fn nonempty(body: Vec<u8>, url: &Url) -> Result<Vec<u8>, Error> {
    if body.is_empty() {
        return Err(Error::Incomplete {
            url: Box::new(url.clone()),
        });
    }
    Ok(body)
}

/// 任务目录 `work_dir` 里的 init 段 `path` 里各轨的默认样本长度（见 [`fmp4::trex_sample_sizes`]）。init 段落盘前校验
/// 过，解析不了即目录里的文件被改动过。
async fn stored_trex(work_dir: &Path, path: &Path) -> Result<HashMap<u32, u32>, Error> {
    let init = tokio::fs::read(path)
        .await
        .map_err(io_error("读取", path))?;
    fmp4::trex_sample_sizes(&init).map_err(|problem| Error::WorkDir {
        path: work_dir.to_path_buf(),
        problem: WorkDirProblem::Corrupt(format!("{} 的 fMP4 结构不对：{problem}", path.display())),
    })
}

/// 取 key：在缓存里的地址只拉取一次（见 [`Keys`]）。`on_key` 回调不改时拉到的必须是 16 字节。拉取失败不缓存，下次
/// 重新拉取。请求与回调的错误包装为 [`Error::Key`]（取消除外）。
async fn key_for(ctx: &Ctx, url: &Url, cancel: &CancellationToken) -> Result<[u8; 16], Error> {
    let cell = ctx
        .keys
        .lock()
        .expect("持锁期间只做 HashMap 的查找、插入与删除，不会 panic")
        .cell(url);
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
                .get(Purpose::Key, url, None, ctx.permit, cancel)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_cache_evicts_the_least_recently_used_address() {
        let url = |n: usize| Url::parse(&format!("https://keys.example/{n}")).unwrap();
        let mut keys = Keys::default();
        let first = keys.cell(&url(0));
        for n in 1..KEY_CACHE {
            keys.cell(&url(n));
        }
        // 再取用第 0 个：满了之后新地址淘汰的是第 1 个
        assert!(Arc::ptr_eq(&first, &keys.cell(&url(0))));
        keys.cell(&url(KEY_CACHE));
        assert_eq!(keys.cells.len(), KEY_CACHE);
        assert!(keys.cells.contains_key(&url(0)));
        assert!(!keys.cells.contains_key(&url(1)));
    }
}
