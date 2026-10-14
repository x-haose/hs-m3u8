//! 录制派出的后台任务：刷新播放列表、准备暂存的播放列表（拉它要用的 init 段）、续录时核对内容、续录时核对之前录齐的组
//! 能否放进 MP4。事件循环不等它们，由 [`Tasks`] 持有，完成后作为事件交回；录制结束时取消并等它们退出（协作取消：在途
//! 的回调返回、在核对的那一组核完才结束）。

use std::path::PathBuf;
use std::sync::Arc;

use hs_m3u8_hls::MediaPlaylist;
use hs_m3u8_remux::Streams;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::mp4_check::Batch;
use super::track::{Fetched, RefreshRequest};
use super::window::{InitsToFetch, NewInits};
use crate::fetch::Direct;
use crate::hooks::Hooks;
use crate::http::{Http, Permit};
use crate::output;
use crate::recheck::{Candidate, OnTransient, Recheck, recheck};
use crate::resolve::Attempts;
use crate::{Error, blocking, resolve};

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
    /// 续录时重叠的分片核对的结果
    Checked {
        track: usize,
        result: Result<Recheck, Error>,
    },
    /// 之前录齐的组能否放进 MP4
    Mp4Checked { result: Result<(), Error> },
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

    /// 刷新第 `track` 条轨；新 init 段经 `direct` 拉取。
    pub(super) fn refresh(
        &mut self,
        http: Arc<Http>,
        hooks: Arc<dyn Hooks>,
        direct: Direct,
        track: usize,
        request: RefreshRequest,
    ) {
        let cancel = self.cancel.clone();
        self.set.spawn(async move {
            let started = Instant::now();
            let result = async {
                let playlist =
                    resolve::fetch_media(&http, &hooks, &request.url, Attempts::Once, &cancel)
                        .await?;
                direct.require_key_uris(&request.url, &playlist)?;
                let inits = match &request.inits {
                    Some(inits) => Some(new_inits(&direct, &playlist, inits, &cancel).await?),
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

    /// 准备第 `track` 条轨暂存的一份播放列表：经 `direct` 拉它要用的 init 段。
    pub(super) fn prepare(
        &mut self,
        direct: Direct,
        track: usize,
        fetched: Fetched,
        inits: InitsToFetch,
    ) {
        let cancel = self.cancel.clone();
        self.set.spawn(async move {
            let result = new_inits(&direct, &fetched.playlist, &inits, &cancel).await;
            Done::Prepared {
                track,
                fetched,
                result,
            }
        });
    }

    /// 核对第 `track` 条轨能否接着最近的会话录：`overlaps` 为窗口与已录分片重叠的部分，序号从大到小（见
    /// [`recheck`]）。
    pub(super) fn check(&mut self, direct: Direct, track: usize, overlaps: Vec<Candidate>) {
        let cancel = self.cancel.clone();
        self.set.spawn(async move {
            let result = recheck(&direct, track, overlaps, OnTransient::TryNext, &cancel).await;
            Done::Checked { track, result }
        });
    }

    /// 核对之前录齐的组能否放进 MP4：按顺序核对 `batches`（见 [`super::mp4_check::Mp4Check::new`]）；`root` 为任务目录。
    pub(super) fn check_mp4(&mut self, streams: Vec<Streams>, batches: Vec<Batch>, root: PathBuf) {
        let cancel = self.cancel.clone();
        self.set.spawn(async move {
            let result = check_mp4(streams, batches, root, &cancel).await;
            Done::Mp4Checked { result }
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

/// 见 [`Tasks::check_mp4`]：按顺序核对各批，批与批之间看取消信号，取消时返回 [`Error::Cancelled`]。
async fn check_mp4(
    streams: Vec<Streams>,
    batches: Vec<Batch>,
    root: PathBuf,
    cancel: &CancellationToken,
) -> Result<(), Error> {
    let (streams, root) = (Arc::new(streams), Arc::new(root));
    for batch in batches {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let (streams, root) = (streams.clone(), root.clone());
        blocking(move || output::check_mp4(&streams, &batch, &root)).await??;
    }
    Ok(())
}

/// 拉取 `inits` 所说的新 init 段；各自的失败随结果返回，只有取消中止。它们是刷新的一部分，不占名额（见
/// [`crate::Engine::new`]）。
async fn new_inits(
    direct: &Direct,
    playlist: &MediaPlaylist,
    inits: &InitsToFetch,
    cancel: &CancellationToken,
) -> Result<NewInits, Error> {
    let mut fetched = NewInits::new();
    for s in &playlist.segments {
        if !inits.processed.should_fetch(s) {
            continue;
        }
        if let Some(init) = &s.init
            && !inits.known.contains(init)
            && !fetched.iter().any(|(i, _)| i == init)
        {
            match direct.fetch_init(init, Permit::Exempt, cancel).await {
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                result => fetched.push((init.clone(), result)),
            }
        }
    }
    Ok(fetched)
}
