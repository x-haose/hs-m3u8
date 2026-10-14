//! 地址变了之后核对取到的是不是同一份内容：已完成的分片按新地址重新下载，与任务目录里已存的逐字节比较。点播续传与
//! 直播续录共用。

use std::path::PathBuf;

use hs_m3u8_hls::Segment;
use tokio_util::sync::CancellationToken;

use crate::error::io_error;
use crate::fetch::Direct;
use crate::{Error, FetchFailure, HttpError, HttpFailure, blocking};

/// 重新下载后与已存的比较的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recheck {
    Same,
    Different,
    /// 都已取不到（404/410），无从核对
    Gone,
}

/// 核对用的一个已完成的分片：按新地址取的 `segment`，它在任务目录里已存的文件 `stored`，所用的 init 段的文件 `init`
/// （见 [`crate::fetch::Item`]）。
pub(crate) struct Candidate {
    pub segment: Segment,
    pub stored: PathBuf,
    pub init: Option<PathBuf>,
}

/// 第 `track` 条轨的 `candidates`（按从新到旧排）逐个试，用第一个取得到的重新下载后与已存的比较（有无 init 段与已存
/// 的不同的不必取，即是不同）；取不到的（404/410，或重试后仍失败的临时故障）换更旧的试。都取不到时，有临时故障则如实
/// 上抛（可重试）；全是 404/410 则为 [`Recheck::Gone`]。其余失败（403 等、key、校验、回调）如实上抛，与下载时一样。
pub(crate) async fn recheck(
    direct: &Direct,
    track: usize,
    candidates: Vec<Candidate>,
    cancel: &CancellationToken,
) -> Result<Recheck, Error> {
    // 最近一次重试后仍失败的临时故障
    let mut transient = None;
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
                })) => continue,
                Some(_) => {
                    transient = Some(e);
                    continue;
                }
                None => return Err(e),
            },
        };
        return Ok(if same_file(stored, data).await? {
            Recheck::Same
        } else {
            Recheck::Different
        });
    }
    match transient {
        Some(error) => Err(error),
        None => Ok(Recheck::Gone),
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
