//! 下载阶段：并发拉取 init 段与分片，解密、校验后写入任务目录；已完成的文件跳过。

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use hs_m3u8_hls::Segment;
use tokio::sync::{OnceCell, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::crypto::{check_fmp4, check_ts, decrypt};
use crate::http::Http;
use crate::plan::Plan;
use crate::workdir::{self, WorkDir};
use crate::{Error, Hooks, Integrity, Progress, Purpose, Stage, blocking, run_hook};

/// 一项待下载的内容；`index` 为 `Track::inits` 或 `Track::segments` 的下标。
#[derive(Debug, Clone, Copy)]
enum Item {
    Init { track: usize, index: usize },
    Segment { track: usize, index: usize },
}

/// 各 worker 共享的状态。
struct Shared {
    http: Arc<Http>,
    hooks: Arc<dyn Hooks>,
    plan: Arc<Plan>,
    dir: WorkDir,
    items: Vec<Item>,
    /// 下一个待领取的 `items` 下标
    next: AtomicUsize,
    /// key 地址 → 处理后的 16 字节 key；每个地址只拉取一次
    keys: Mutex<HashMap<Url, Arc<OnceCell<[u8; 16]>>>>,
    progress: watch::Sender<Progress>,
}

/// 下载计划中所有尚未完成的 init 段与分片。返回 `Ok` 即全部已在任务目录中。
///
/// 任一项失败时取消其余在途请求并返回该错误；`cancel` 被取消时返回 [`Error::Cancelled`]。
pub(crate) async fn download(
    http: Arc<Http>,
    hooks: Arc<dyn Hooks>,
    plan: Arc<Plan>,
    dir: WorkDir,
    concurrency: NonZeroUsize,
    progress: watch::Sender<Progress>,
    cancel: &CancellationToken,
) -> Result<(), Error> {
    let scan_plan = plan.clone();
    let scan_dir = dir.clone();
    let (items, done, bytes) = blocking(move || pending(&scan_plan, &scan_dir)).await??;
    progress.send_modify(|p| {
        p.stage = Stage::Downloading;
        p.segments_total = plan.segment_count();
        p.segments_done = done;
        p.bytes = bytes;
    });
    if items.is_empty() {
        return Ok(());
    }

    let workers = concurrency.get().min(items.len());
    let shared = Arc::new(Shared {
        http,
        hooks,
        plan,
        dir,
        items,
        next: AtomicUsize::new(0),
        keys: Mutex::new(HashMap::new()),
        progress,
    });
    // 一项失败即取消其余 worker，调用方的取消同样传到这里
    let abort = cancel.child_token();
    let mut set = JoinSet::new();
    for _ in 0..workers {
        set.spawn(worker(shared.clone(), abort.clone()));
    }
    let mut outcome = Ok(());
    while let Some(joined) = set.join_next().await {
        let result = match joined {
            Ok(result) => result,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => Err(Error::Cancelled),
        };
        if let Err(e) = result {
            abort.cancel();
            // 保留第一个真正的失败；其余 worker 因取消而返回的 Cancelled 不覆盖它
            if matches!(outcome, Ok(()) | Err(Error::Cancelled)) {
                outcome = Err(e);
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
        for index in 0..t.inits.len() {
            match workdir::completed_len(&dir.init(track, index))? {
                Some(len) => bytes += len,
                None => items.push(Item::Init { track, index }),
            }
        }
    }
    for (track, t) in plan.tracks.iter().enumerate() {
        for (index, segment) in t.segments.iter().enumerate() {
            match workdir::completed_len(&dir.segment(track, segment.sequence))? {
                Some(len) => {
                    done += 1;
                    bytes += len;
                }
                None => items.push(Item::Segment { track, index }),
            }
        }
    }
    Ok((items, done, bytes))
}

/// 依次领取并完成待下载项，直到领完或出错。
async fn worker(shared: Arc<Shared>, cancel: CancellationToken) -> Result<(), Error> {
    loop {
        let next = shared.next.fetch_add(1, Ordering::Relaxed);
        let Some(&item) = shared.items.get(next) else {
            return Ok(());
        };
        let track = match item {
            Item::Init { track, .. } | Item::Segment { track, .. } => track,
        };
        let t = &shared.plan.tracks[track];
        let (path, data, is_segment) = match item {
            Item::Init { index, .. } => {
                let data = fetch_init(&shared, &t.inits[index], &cancel).await?;
                (shared.dir.init(track, index), data, false)
            }
            Item::Segment { index, .. } => {
                let segment = &t.segments[index];
                let data = fetch_segment(&shared, segment, &cancel)
                    .await
                    .map_err(|e| match e {
                        Error::Cancelled => Error::Cancelled,
                        e => Error::Segment {
                            sequence: segment.sequence,
                            url: Box::new(segment.uri.clone()),
                            source: Box::new(e),
                        },
                    })?;
                (shared.dir.segment(track, segment.sequence), data, true)
            }
        };
        let len = data.len() as u64;
        workdir::write(path, data).await?;
        shared.progress.send_modify(|p| {
            p.bytes += len;
            if is_segment {
                p.segments_done += 1;
            }
        });
    }
}

async fn fetch_init(
    shared: &Shared,
    init: &hs_m3u8_hls::InitSection,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    let data = shared
        .http
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
    shared: &Shared,
    segment: &Segment,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    let key = match &segment.key {
        Some(key) => Some((key_for(shared, &key.uri, cancel).await?, key.iv)),
        None => None,
    };
    let body = shared
        .http
        .get(Purpose::Segment, &segment.uri, segment.byte_range, cancel)
        .await?
        .body;
    let url = segment.uri.clone();
    let data = run_hook(&shared.hooks, Purpose::Segment, move |hooks| {
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
async fn key_for(
    shared: &Shared,
    url: &Url,
    cancel: &CancellationToken,
) -> Result<[u8; 16], Error> {
    let cell = shared
        .keys
        .lock()
        .expect("持锁期间只做 HashMap 查找与插入，不会 panic")
        .entry(url.clone())
        .or_default()
        .clone();
    let key = cell
        .get_or_try_init(|| async {
            let body = shared.http.get(Purpose::Key, url, None, cancel).await?.body;
            let hook_url = url.clone();
            let data = run_hook(&shared.hooks, Purpose::Key, move |hooks| {
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
