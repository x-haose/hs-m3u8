//! 续传、续录时核对这次取到的是不是同一份内容：已完成的分片按这次的播放列表重新下载，与任务目录里已存的逐字节
//! 比较。点播续传与直播续录共用。

use std::path::PathBuf;

use hs_m3u8_hls::Segment;
use tokio_util::sync::CancellationToken;

use crate::error::io_error;
use crate::fetch::Direct;
use crate::{Error, FetchFailure, HttpError, HttpFailure, blocking};

/// 重新下载后与已存的比较的结果。
pub(crate) enum Recheck {
    Same,
    Different,
    /// 都已取不到（404/410），无从核对；为最后一个的失败
    Gone(Error),
}

/// 核对用的一个已完成的分片：按这次的播放列表取的 `segment`，它在任务目录里已存的文件 `stored`，所用的 init 段的文件
/// `init`（见 [`crate::fetch::Item`]）。
pub(crate) struct Candidate {
    pub segment: Segment,
    pub stored: PathBuf,
    pub init: Option<PathBuf>,
}

/// 第 `track` 条轨的 `candidates`（不为空）按给出的顺序逐个试，用第一个取得到的重新下载后与已存的比较（有无 init 段
/// 与已存的不同的不必取，即是不同）；取不到（404/410）的换下一个，都取不到为 [`Recheck::Gone`]。其余失败如实上抛，
/// 与下载时一样：重试后仍失败的临时故障不再换下一个（别的多半也取不到，逐个重试只会拖长），标为可重试。
pub(crate) async fn recheck(
    direct: &Direct,
    track: usize,
    candidates: Vec<Candidate>,
    cancel: &CancellationToken,
) -> Result<Recheck, Error> {
    let mut gone = None;
    for Candidate {
        segment,
        stored,
        init,
    } in candidates
    {
        if segment.init.is_some() != init.is_some() {
            return Ok(Recheck::Different);
        }
        let data = match direct.fetch(track, &segment, init.as_deref(), cancel).await {
            Ok(data) => data,
            Err(e) => match e.missable() {
                Some(FetchFailure::Http(HttpFailure {
                    kind: HttpError::Status(404 | 410),
                    ..
                })) => {
                    gone = Some(e);
                    continue;
                }
                _ => return Err(e),
            },
        };
        return Ok(if same_file(stored, data).await? {
            Recheck::Same
        } else {
            Recheck::Different
        });
    }
    Ok(Recheck::Gone(gone.expect("调用方给的候选不为空")))
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
