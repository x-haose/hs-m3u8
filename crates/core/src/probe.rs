//! 探测：开始下载前拉取来源、按偏好选轨，看清来源有什么、会选中什么。

use hs_m3u8_hls::{MasterPlaylist, MediaPlaylist, Selection};
use tokio_util::sync::CancellationToken;

use crate::http::Http;
use crate::request::Source;
use crate::{Error, resolve};

/// 探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// 来源是主播放列表时为它，列出可选的变体与音频 rendition（按下标选用见 [`crate::hls::VariantChoice::Index`]）；
    /// 来源本身是媒体播放列表时为 None
    pub master: Option<MasterPlaylist>,
    /// 按 [`Source::preference`] 选中的变体与音频，与新开的下载选的相同（续传时按任务目录的记录找回原来的轨）；
    /// 来源本身是媒体播放列表时为 None
    pub selection: Option<Selection>,
    /// 所选各轨的媒体播放列表，第 0 条为所选变体（或来源本身），第 1 条（若有）为独立的音频：`ended` 为 false
    /// 即直播，分片的 `key` 不为 None 即加密，时长为分片 `duration_us` 之和
    pub tracks: Vec<MediaPlaylist>,
}

/// 拉取来源并按偏好选轨；不下载分片，不碰任务目录。随返回的 future 被丢弃而取消。
pub(crate) async fn probe(http: &Http, source: &Source) -> Result<Probe, Error> {
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let resolved = resolve::resolve(http, source, None, &cancel)
        .await?
        .expect("没有记录的选轨时按偏好选，选不出即报错，不会是找不到记录");
    let (master, selection) = match resolved.master {
        Some(chosen) => (Some(chosen.playlist), Some(chosen.selection)),
        None => (None, None),
    };
    Ok(Probe {
        master,
        selection,
        tracks: resolved.tracks.into_iter().map(|t| t.playlist).collect(),
    })
}
