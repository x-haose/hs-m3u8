//! 认出地址是什么：播放列表、网页还是文件（见 [`crate::Engine::identify`]）。

use reqwest::StatusCode;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{FileInfo, Resource, content_length, headers, ranged_length};
use crate::hooks::Purpose;
use crate::http::{Conditions, Http, Permit, Streaming, body_limit};
use crate::request::Source;
use crate::{Error, HttpErrorKind, Probe, resolve};

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

/// 请求 `url`（先带 `Range: bytes=0-`，回 416 或 Content-Range 不对时不带），看响应头与内容开头；是播放列表时读完内容
/// （超过播放列表的上限报 [`Error::ResponseTooLarge`]）。
async fn sniff(http: &Http, url: &Url, cancel: &CancellationToken) -> Result<Sniffed, Error> {
    let open = |conditions: Conditions| async move {
        http.open(Purpose::File, url, &conditions, Permit::Exempt, cancel)
            .await
    };
    let ranged = open(Conditions {
        range: Some("bytes=0-".to_owned()),
        if_range: None,
    })
    .await;
    let (mut stream, accepts_ranges, length) = match ranged {
        Err(Error::Http {
            kind: HttpErrorKind::Status(416),
            ..
        }) => whole(open(Conditions::default()).await?),
        Err(error) => return Err(error),
        Ok(stream) if stream.status() != StatusCode::PARTIAL_CONTENT => whole(stream),
        Ok(stream) => match ranged_length(&stream) {
            Some(length) => (stream, true, Some(length)),
            None => {
                drop(stream);
                whole(open(Conditions::default()).await?)
            }
        },
    };
    let mut head = Vec::new();
    while head.len() < SNIFF && stream.read_into(&mut head, cancel).await? > 0 {}
    if is_playlist(&head) {
        let mut body = head;
        let limit = body_limit(Purpose::Playlist).expect("播放列表有上限");
        while stream.read_into(&mut body, cancel).await? > 0 {
            if body.len() as u64 > limit {
                return Err(Error::ResponseTooLarge {
                    url: Box::new(stream.final_url().clone()),
                    purpose: Purpose::Playlist,
                    limit,
                });
            }
        }
        return Ok(Sniffed::Playlist {
            url: stream.final_url().clone(),
            body,
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

/// 不分段的响应：不支持范围请求，长度取 Content-Length。
fn whole(stream: Streaming) -> (Streaming, bool, Option<u64>) {
    let length = content_length(&stream);
    (stream, false, length)
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
