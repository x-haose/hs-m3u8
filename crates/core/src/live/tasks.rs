//! 录制派出的后台任务：刷新播放列表、准备暂存的播放列表（拉它要用的 init 段）、续录时核对内容。事件循环不等它们，
//! 由 [`Tasks`] 持有，完成后作为事件交回；录制结束时取消并等它们退出（协作取消，在途的回调返回后才结束）。

use std::path::PathBuf;
use std::sync::Arc;

use hs_m3u8_hls::{MediaPlaylist, Segment};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::session::Verdict;
use super::track::{Fetched, RefreshRequest};
use super::window::{InitsToFetch, NewInits};
use crate::error::io_error;
use crate::fetch::{self, Direct};
use crate::hooks::Hooks;
use crate::http::{Http, Permit};
use crate::{Error, HttpError, blocking, resolve};

/// 一个后台任务的结果。
pub(super) enum Done {
    /// 刷新：拉到的播放列表，以及要录的新分片引用的新 init 段（会话还没定下时不拉，为 None）
    Refreshed {
        track: usize,
        started: Instant,
        result: Result<(MediaPlaylist, Option<NewInits>), Error>,
    },
    /// 准备好了暂存的一份播放列表
    Prepared {
        track: usize,
        fetched: Fetched,
        result: Result<NewInits, Error>,
    },
    /// 续录时能否接着最近的会话录
    Checked {
        track: usize,
        result: Result<Verdict, Error>,
    },
}

/// 核对用的一个重叠分片与它已存的文件。
pub(super) struct StoredOverlap {
    pub segment: Segment,
    pub stored: PathBuf,
}

/// 在途的后台任务。
pub(super) struct Tasks {
    set: JoinSet<Done>,
    /// 任务取消令牌的子令牌：录制结束时只取消这些任务，不影响下载
    cancel: CancellationToken,
}

impl Tasks {
    pub(super) fn new(cancel: &CancellationToken) -> Self {
        Tasks {
            set: JoinSet::new(),
            cancel: cancel.child_token(),
        }
    }

    /// 刷新第 `track` 条轨。
    pub(super) fn refresh(
        &mut self,
        http: Arc<Http>,
        hooks: Arc<dyn Hooks>,
        track: usize,
        request: RefreshRequest,
    ) {
        let cancel = self.cancel.clone();
        self.set.spawn(async move {
            let started = Instant::now();
            let result = async {
                let playlist = resolve::fetch_media(&http, &hooks, &request.url, &cancel).await?;
                let inits = match &request.inits {
                    Some(inits) => Some(new_inits(&http, &playlist, inits, &cancel).await?),
                    None => None,
                };
                Ok((playlist, inits))
            }
            .await;
            Done::Refreshed {
                track,
                started,
                result,
            }
        });
    }

    /// 准备第 `track` 条轨暂存的一份播放列表：拉它要用的 init 段。
    pub(super) fn prepare(
        &mut self,
        http: Arc<Http>,
        track: usize,
        fetched: Fetched,
        inits: InitsToFetch,
    ) {
        let cancel = self.cancel.clone();
        self.set.spawn(async move {
            let result = new_inits(&http, &fetched.playlist, &inits, &cancel).await;
            Done::Prepared {
                track,
                fetched,
                result,
            }
        });
    }

    /// 核对第 `track` 条轨能否接着最近的会话录：`overlaps` 为窗口与已录分片重叠的部分，序号从大到小。
    pub(super) fn check(&mut self, direct: Direct, track: usize, overlaps: Vec<StoredOverlap>) {
        let cancel = self.cancel.clone();
        self.set.spawn(async move {
            let result = check(&direct, track, overlaps, &cancel).await;
            Done::Checked { track, result }
        });
    }

    /// 下一个完成的任务；没有在途的任务时一直等。
    pub(super) async fn next(&mut self) -> Done {
        match self.set.join_next().await {
            Some(Ok(done)) => done,
            Some(Err(e)) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Some(Err(_)) => unreachable!("后台任务只经协作取消结束，不被中止"),
            None => std::future::pending().await,
        }
    }

    /// 取消在途的任务并等它们退出。
    pub(super) async fn shutdown(&mut self) {
        self.cancel.cancel();
        while let Some(joined) = self.set.join_next().await {
            if let Err(e) = joined
                && e.is_panic()
            {
                std::panic::resume_unwind(e.into_panic());
            }
        }
    }
}

/// 重叠的分片从新到旧逐个试，用第一个取得到的重新下载后与已存的比较；取不到的（404/410，或重试后仍失败的临时
/// 故障）换更旧的试。都取不到时，有临时故障则如实上抛（可重试）；全是 404/410 则无从核对，按接不上算（它们也录
/// 不到，另起会话不会重复）。其余失败（403 等、key、校验、回调）如实上抛，与录制时一样。
async fn check(
    direct: &Direct,
    track: usize,
    overlaps: Vec<StoredOverlap>,
    cancel: &CancellationToken,
) -> Result<Verdict, Error> {
    // 最近一次重试后仍失败的临时故障
    let mut transient = None;
    for StoredOverlap { segment, stored } in overlaps {
        let data = match direct.fetch(track, &segment, cancel).await {
            Ok(data) => data,
            Err(e) => match e.missable() {
                Some(HttpError::Status(404 | 410)) => continue,
                Some(_) => {
                    transient = Some(e);
                    continue;
                }
                None => return Err(e),
            },
        };
        return Ok(if same_file(stored, data).await? {
            Verdict::Matches
        } else {
            Verdict::Differs
        });
    }
    match transient {
        Some(error) => Err(error),
        None => Ok(Verdict::Differs),
    }
}

/// `data` 与目录中已存的 `stored` 逐字节相同。
async fn same_file(stored: PathBuf, data: Vec<u8>) -> Result<bool, Error> {
    blocking(move || {
        std::fs::read(&stored)
            .map(|existing| existing == data)
            .map_err(io_error("读取", &stored))
    })
    .await?
}

/// 拉取 `inits` 所说的新 init 段；各自的失败随结果返回，只有取消中止。不占引擎名额：它们是刷新的一部分，
/// 排在大批下载之后会让直播停滞。
async fn new_inits(
    http: &Http,
    playlist: &MediaPlaylist,
    inits: &InitsToFetch,
    cancel: &CancellationToken,
) -> Result<NewInits, Error> {
    let mut fetched = NewInits::new();
    for s in &playlist.segments {
        if !inits.processed.is_new(s.sequence) {
            continue;
        }
        if let Some(init) = &s.init
            && !inits.known.contains(init)
            && !fetched.iter().any(|(i, _)| i == init)
        {
            match fetch::fetch_init(http, init, Permit::Exempt, cancel).await {
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                result => fetched.push((init.clone(), result)),
            }
        }
    }
    Ok(fetched)
}
