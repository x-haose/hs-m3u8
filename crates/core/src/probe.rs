//! 探测：开始下载前拉取来源、按偏好选轨，看清来源有什么、会选中什么。

use tokio_util::sync::CancellationToken;

use crate::http::Http;
use crate::info::{MasterInfo, TrackInfo};
use crate::request::Source;
use crate::{Error, resolve};

/// 探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// 来源是主播放列表时：可选的变体与音频，以及按 [`Source::preference`] 选中的。新开的下载选的与它相同；探测
    /// 不读任务目录，续传时下载按记录找回原来的轨，可能与它不同。来源本身是媒体播放列表时为 None
    pub master: Option<MasterInfo>,
    /// 所选各轨的概况：第 0 条为所选变体（或来源本身），第 1 条（若有）为独立的音频
    pub tracks: Vec<TrackInfo>,
}

impl Probe {
    /// 是否直播：有任一条轨没有 EXT-X-ENDLIST，与下载的判定相同。
    pub fn is_live(&self) -> bool {
        self.tracks.iter().any(|t| !t.ended)
    }
}

/// 拉取来源并按偏好选轨；不下载分片，不碰任务目录。随返回的 future 被丢弃而取消。
pub(crate) async fn probe(http: &Http, source: &Source) -> Result<Probe, Error> {
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let resolved = resolve::resolve(http, source, None, &cancel)
        .await?
        .expect("没有记录的选轨时按偏好选，选不出即报错，不会是找不到记录");
    Ok(Probe {
        master: resolved
            .master
            .as_ref()
            .map(|m| MasterInfo::of(&m.playlist, &m.selection)),
        tracks: resolved
            .tracks
            .iter()
            .map(|t| TrackInfo::of(&t.playlist))
            .collect(),
    })
}
