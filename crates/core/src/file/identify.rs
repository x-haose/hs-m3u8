//! 认出地址是什么：播放列表、网页还是文件（见 [`crate::Engine::identify`]）。

use tokio_util::sync::CancellationToken;
use url::Url;

use super::{FileInfo, FromZero, Resource, headers, open_from_zero};
use crate::hooks::Purpose;
use crate::http::{Http, Permit, RangeRequest, body_limit};
use crate::request::Source;
use crate::{Error, Probe, resolve};

/// 看内容开头的这么多字节，够认出 `#EXTM3U`（前面可以有 UTF-8 的 BOM 与空白）。
const SNIFF: usize = 64;

/// 发出请求、看响应头与内容开头得到的。
enum Sniffed {
    /// 播放列表：重定向之后的地址与完整内容
    Playlist {
        url: Url,
        body: Vec<u8>,
    },
    Page {
        content_type: String,
    },
    File(FileInfo),
}

/// 见 [`crate::Engine::identify`]。随返回的 future 被丢弃而取消。
pub(crate) async fn identify(http: &Http, source: &Source) -> Result<Resource, Error> {
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let url = &source.url;
    let sniffed = http
        .retrying(Purpose::File, url, &cancel, || sniff(http, url, &cancel))
        .await?;
    match sniffed {
        Sniffed::Playlist { url, body } => {
            let first = resolve::parse_playlist(&source.hooks, url, body).await?;
            let resolved = resolve::resolve_playlist(http, source, first, None, &cancel)
                .await?
                .expect("没有记录的选轨时按偏好选，选不出即报错，不会是找不到记录");
            Ok(Resource::Playlist(Box::new(Probe::of(&resolved))))
        }
        Sniffed::Page { content_type } => Ok(Resource::Page { content_type }),
        Sniffed::File(info) => Ok(Resource::File(info)),
    }
}

/// 从 0 起请求 `url`（见 [`open_from_zero`]），看响应头与内容开头；是播放列表时读完内容（超过播放列表的上限报
/// [`Error::ResponseTooLarge`]；206 只给了开头一部分时不带 Range 重新取）。
async fn sniff(http: &Http, url: &Url, cancel: &CancellationToken) -> Result<Sniffed, Error> {
    let (mut stream, accepts_ranges, length, partial) =
        match open_from_zero(http, url, None, None, Permit::Exempt, cancel).await? {
            FromZero::Ranged { length, first } => {
                let partial = first.end < length;
                (first.stream, true, Some(length), partial)
            }
            FromZero::Ignored(stream) | FromZero::Plain(stream) => {
                let length = headers::content_length(stream.headers());
                (stream, false, length, false)
            }
        };
    let mut head = Vec::new();
    while head.len() < SNIFF && stream.read_into(&mut head, cancel).await? > 0 {}
    if is_playlist(&head) {
        let (stream, head) = if partial {
            // 放开这个响应（与它占的连接）再取整个
            drop(stream);
            let whole = http
                .open(
                    Purpose::File,
                    url,
                    &RangeRequest::whole(),
                    Permit::Exempt,
                    cancel,
                )
                .await?;
            (whole, Vec::new())
        } else {
            (stream, head)
        };
        let response = stream
            .read_to_end(
                head,
                body_limit(Purpose::Playlist),
                Purpose::Playlist,
                None,
                cancel,
            )
            .await?;
        return Ok(Sniffed::Playlist {
            url: response.url,
            body: response.body,
        });
    }
    let content_type = headers::content_type(stream.headers());
    if let Some(content_type) = content_type.as_deref().filter(|t| headers::is_html(t)) {
        return Ok(Sniffed::Page {
            content_type: content_type.to_owned(),
        });
    }
    Ok(Sniffed::File(FileInfo {
        url: stream.final_url().clone(),
        length,
        accepts_ranges,
        etag: headers::etag(stream.headers()),
        last_modified: headers::last_modified(stream.headers()),
        content_type,
        file_name: headers::file_name(stream.headers()),
    }))
}

/// 内容开头（去掉 UTF-8 的 BOM 与空白）是 `#EXTM3U`。
fn is_playlist(head: &[u8]) -> bool {
    let text = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    text.trim_ascii_start().starts_with(b"#EXTM3U")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlists_are_recognized_by_their_first_tag() {
        for head in [
            b"#EXTM3U\n".as_slice(),
            b"\xEF\xBB\xBF#EXTM3U",
            b"\r\n  #EXTM3U",
        ] {
            assert!(is_playlist(head), "{head:?}");
        }
        for head in [b"<html>".as_slice(), b"#EXTM3", b"", b"\x00\x00\x00 ftyp"] {
            assert!(!is_playlist(head), "{head:?}");
        }
    }
}
