//! 文件任务的请求：从 0 起请求（认不出范围时不带 Range 再请求），与请求一段并核对响应。

use std::ops::Range;
use std::path::Path;

use reqwest::StatusCode;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::headers;
use crate::hooks::Purpose;
use crate::http::{Http, Permit, RangeRequest, Span, Streaming, http_error};
use crate::{Error, HttpErrorKind, WorkDirProblem};

/// 一段剩下的 `remaining` 这次请求到哪里（不含）：给了块大小 `chunk` 时至多一块。
pub(super) fn request_end(remaining: &Range<u64>, chunk: Option<u64>) -> u64 {
    chunk.map_or(remaining.end, |c| {
        remaining.end.min(remaining.start.saturating_add(c))
    })
}

/// 收到了响应头的范围请求：从 `start` 起，到 `end`（不含）为止。服务器可能给得比请求的少，`end` 为它给到的。
pub(super) struct Opened {
    pub(super) stream: Streaming,
    pub(super) start: u64,
    pub(super) end: u64,
}

/// 请求 `span`（带 If-Range `validator`），核对响应：须是 206、从请求的起点起、总长度为 `length`。终点比请求的早（服务
/// 器一次只给这么多）时照收，比请求的晚时只取到请求的终点。来源的文件变了报 [`WorkDirProblem::FileChanged`]（`root` 为
/// 任务目录）：回 416（文件变短了）、总长度不同、响应带的验证器与 `validator` 不同、带了 If-Range 而回了整个文件。没带
/// If-Range 而回了整个文件（服务器这次忽略了范围请求）报 [`HttpErrorKind::RangeIgnored`]，起点不对报
/// [`HttpErrorKind::RangeMismatch`]。
pub(super) async fn open_range(
    http: &Http,
    url: &Url,
    span: Range<u64>,
    validator: Option<&str>,
    length: u64,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<Opened, Error> {
    let changed = || Error::WorkDir {
        path: root.to_path_buf(),
        problem: WorkDirProblem::FileChanged,
    };
    let request = RangeRequest {
        span: Some(Span {
            start: span.start,
            end: Some(span.end),
        }),
        if_range: validator.map(str::to_owned),
    };
    let stream = match http
        .open(Purpose::File, url, &request, Permit::Vod, cancel)
        .await
    {
        Err(Error::Http {
            kind: HttpErrorKind::Status(416),
            ..
        }) => return Err(changed()),
        other => other?,
    };
    let status = stream.status();
    if status != StatusCode::PARTIAL_CONTENT {
        return match validator {
            Some(_) => Err(changed()),
            None => Err(http_error(
                stream.url(),
                HttpErrorKind::RangeIgnored(status.as_u16()),
            )),
        };
    }
    if validator.is_some_and(|sent| headers::contradicts(stream.headers(), sent)) {
        return Err(changed());
    }
    match stream.content_range() {
        Some((_, _, Some(total))) if total != length => Err(changed()),
        Some((first, last, Some(_))) if first == span.start => {
            let end = (last + 1).min(span.end);
            Ok(Opened {
                stream,
                start: span.start,
                end,
            })
        }
        _ => {
            let value = stream
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
            Err(http_error(
                stream.url(),
                HttpErrorKind::RangeMismatch(value),
            ))
        }
    }
}

/// 从 0 起请求的结果（见 [`open_from_zero`]）。
pub(super) enum FromZero {
    /// 206，Content-Range 从 0 起、给出了总长度 `length`
    Ranged { length: u64, first: Opened },
    /// 对范围请求回了整个文件
    Ignored(Streaming),
    /// 回 416（多为空文件），或 206 而 Content-Range 不对（没有总长度、不从 0 起）：不带 Range 重新请求得到的
    Plain(Streaming),
}

/// 请求 `url` 从 0 起到 `end`（不含；None 时到文件末尾），带 If-Range `if_range`；回 416 或 Content-Range 不对时放开这
/// 个响应，不带 Range 再请求一次。
pub(super) async fn open_from_zero(
    http: &Http,
    url: &Url,
    end: Option<u64>,
    if_range: Option<&str>,
    permit: Permit,
    cancel: &CancellationToken,
) -> Result<FromZero, Error> {
    let request = RangeRequest {
        span: Some(Span { start: 0, end }),
        if_range: if_range.map(str::to_owned),
    };
    match http
        .open(Purpose::File, url, &request, permit, cancel)
        .await
    {
        Err(Error::Http {
            kind: HttpErrorKind::Status(416),
            ..
        }) => {}
        Err(error) => return Err(error),
        Ok(stream) if stream.status() != StatusCode::PARTIAL_CONTENT => {
            return Ok(FromZero::Ignored(stream));
        }
        Ok(stream) => {
            if let Some((0, last, Some(length))) = stream.content_range() {
                let end = end.map_or(last + 1, |e| e.min(last + 1));
                let first = Opened {
                    stream,
                    start: 0,
                    end,
                };
                return Ok(FromZero::Ranged { length, first });
            }
            // 放开这个响应（与它占的名额、连接）再重新请求
            drop(stream);
        }
    }
    let plain = http
        .open(Purpose::File, url, &RangeRequest::whole(), permit, cancel)
        .await?;
    Ok(FromZero::Plain(plain))
}
