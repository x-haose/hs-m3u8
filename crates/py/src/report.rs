//! 任务的进度与结果，以及失败原因里共用的类型：请求失败、分片取不到、直播刷新失败与停滞的原因、残留、合并出的流。

use std::time::Duration;

use hs_m3u8_core as core;
use pyo3::prelude::*;

use crate::convert::{PyPath, Seq};
use crate::error::Mp4Unsupported;
use crate::info::Selected;
use crate::record::{record, variants};

variants! {
    /// 请求失败的原因。
    pub enum HttpErrorKind {
        /// 服务器返回了非 2xx 的状态
        Status {
            /// HTTP 状态码
            code: u16,
        },
        /// 字节范围请求应返回 206，服务器却返回了别的状态
        RangeIgnored {
            /// HTTP 状态码
            code: u16,
        },
        /// 字节范围请求返回了 206，Content-Range 却不是请求的那一段（多为 CDN 缓存 206 响应时没按 Range 区分）
        RangeMismatch {
            /// 收到的 Content-Range；没有时为 None
            content_range: Option<String>,
        },
        /// 连接、读空闲或单次请求超时
        Timeout {},
        /// TLS 证书校验失败（自签名、过期、与域名不符等）；确认来源可信时可设 `HttpOptions.insecure`
        Certificate {
            /// 说明
            detail: String,
        },
        /// 跟随重定向失败（循环、次数过多）
        Redirect {
            /// 说明
            detail: String,
        },
        /// 连不上服务器或代理
        Connect {
            /// 说明
            detail: String,
        },
        /// 代理拒绝了这次连接或要求认证：HTTP 代理对 CONNECT 回了非成功的状态（如 407），或 SOCKS 代理的认证与连接
        /// 请求被拒。要检查代理设置与口令
        Proxy {
            /// 说明
            detail: String,
        },
        /// 请求无法构造：地址能解析，HTTP 库却不接受（如超过 65534 字节）
        InvalidRequest {
            /// 说明
            detail: String,
        },
        /// 读取响应体等其他传输错误
        Transport {
            /// 说明
            detail: String,
        },
    }
}

record! {
    /// 取不到的请求：失败的原因，与服务器要求的最短等待。
    pub struct HttpFailure {
        /// 失败的原因
        pub kind: HttpErrorKind,
        /// 临时故障，重试可能成功：超时、连接与传输错误，408、429 与 5xx
        pub retryable: bool,
        /// 服务器在 429/503 中要求（Retry-After）的最短等待，秒；没有要求时为 None
        pub retry_after: Option<u64>,
    }
}

variants! {
    /// 分片或 init 段取不到的原因的种类。
    pub enum FetchErrorKind {
        /// 请求取不到：404/410，或重试后仍失败的临时故障
        Http {
            /// 请求失败的原因
            kind: HttpErrorKind,
        },
        /// 重试后取到的内容仍没写完（见 `IncompleteError`）
        Incomplete {},
    }
}

variants! {
    /// 取不到的分片或 init 段。
    pub enum FetchFailure {
        /// 请求取不到：404/410，或重试后仍失败的临时故障
        Http {
            /// 失败的请求
            failure: HttpFailure,
        },
        /// 重试后取到的内容仍没写完（见 `IncompleteError`），稍后重试可能成功
        Incomplete {},
    }
}

variants! {
    /// 直播一次刷新失败的原因。
    pub enum RefreshReason {
        /// 取不到：404/410 或临时故障（刷新只发一次请求，失败了等下次刷新）
        Http {
            /// 失败的请求
            failure: HttpFailure,
        },
        /// 内容为空，多为服务器还没写完
        Empty {},
        /// 结尾截在多字节字符中间，多为服务器还没写完
        Truncated {},
        /// 语法错误，多为服务器还没写完
        #[pyo3(constructor = (*, line, detail))]
        Syntax {
            /// 出错的行，从 1 起
            line: usize,
            /// 错在哪里
            detail: String,
        },
    }
}

record! {
    /// 直播一次刷新失败；这类失败等下次刷新，不使任务立即失败。
    pub struct RefreshCause {
        /// 失败的原因
        pub reason: RefreshReason,
        /// 一直这样失败到停滞时，稍后续录可能成功：取不到的看原因；内容不完整的当作暂时故障，同样可以
        pub retryable: bool,
        /// 服务器要求的最短等待，秒，下次刷新不早于它；没有要求时为 None
        pub retry_after: Option<u64>,
    }
}

variants! {
    /// 一条轨停滞时看起来已结束的原因。
    pub enum StallCause {
        /// 刷新成功，但不再出现新分片
        NoNewSegments {},
        /// 播放列表已被删除
        PlaylistGone {
            /// 最近一次刷新返回的 HTTP 状态（404 或 410）
            status: u16,
        },
    }
}

variants! {
    /// 残留是什么，决定怎么处理。
    pub enum LeftoverKind {
        /// 本库写的、可以直接删除的东西：写了一半的临时输出、已被新输出替换的旧输出、任务目录里的记录
        Removable {},
        /// 没能放回原处的旧输出。失败的异常里报出的多数仍记在任务目录里：下次用同一任务目录运行或放弃任务时，原处
        /// 空着或是空目录就放回，原处是输出（更新的输出已装上）就删掉，原处是别的东西就留着；放弃任务返回的已没有
        /// 记录，之后用同一任务目录运行时不动它，在任务开头报出来。留着的要用户处理
        Displaced {
            /// 原来的路径
            target: PyPath,
        },
        /// 写输出失败后没能撤下的新输出：完整可用，留在原处
        Installed {},
        /// 没删干净的任务目录：里面有不是本库写的文件（例如经符号链接写进去的输出本身），要用户处理；或删除时
        /// 出错，可用 `Engine.discard` 重试。不要整个删除
        WorkDir {},
    }
}

record! {
    /// 收尾时没能删掉或放回的一样东西。
    pub struct Leftover {
        /// 它现在的路径
        pub path: PyPath,
        /// 是什么，决定怎么处理
        pub kind: LeftoverKind,
        /// 没能删掉或放回的原因
        pub cause: String,
    }
}

variants! {
    /// 流的种类。
    pub enum StreamKind {
        /// 视频
        Video {},
        /// 音频
        Audio {},
    }
}

variants! {
    /// 一路流的编码参数。
    pub enum Shape {
        /// 视频
        #[pyo3(constructor = (*, codec, width, height, pixel_format))]
        Video {
            /// FFmpeg 的编码名，如 `h264`、`hevc`
            codec: String,
            /// 宽，像素
            width: u32,
            /// 高，像素
            height: u32,
            /// FFmpeg 的像素格式名（色度采样与位深），如 `yuv420p`、`yuv420p10le`；探测不出时为 None
            pixel_format: Option<String>,
        },
        /// 音频
        #[pyo3(constructor = (*, codec, sample_rate, channels))]
        Audio {
            /// FFmpeg 的编码名，如 `aac`
            codec: String,
            /// 采样率，Hz
            sample_rate: u32,
            /// 声道数
            channels: u32,
        },
    }
}

variants! {
    /// 任务所处的阶段。失败时停在原来的阶段，结果只能从 `Job.wait` 取得。
    pub enum Stage {
        /// 准备：检查输出、读取任务目录，下载时还要拉取播放列表、选轨
        Preparing {},
        /// 点播：下载分片
        Downloading {},
        /// 直播：刷新播放列表并下载新分片
        Recording {},
        /// 写出输出（合并 MP4、写本地 HLS）
        Writing {},
        /// 完成
        Done {},
    }
}

record! {
    /// 任务进度快照（`Job.progress`）。
    pub struct Progress {
        /// 所处的阶段
        pub stage: Stage,
        /// 所选的变体与音频；解析完成前、或来源本身是媒体播放列表时为 None。有已完成分片的任务目录按记录找回原来的
        /// 轨，不一定是偏好会选的那条
        pub selected: Option<Selected>,
        /// 已完成的分片，各轨合计；含续传前已完成的
        pub segments_done: usize,
        /// 要下载的分片，各轨合计：已完成的（含续传前的）加上排入下载的；直播另含列出了但 init 段取不到的，随录制
        /// 增长。全部处理完时等于 `segments_done` 加 `segments_failed`
        pub segments_total: usize,
        /// 直播本次运行中列出了、但分片或其 init 段取不到的分片，计入 `segments_total`；点播恒为 0
        pub segments_failed: usize,
        /// 直播本次运行中两次刷新之间已滑出窗口、没有列出过的分片，不在 `segments_total` 里；点播恒为 0
        pub segments_expired: usize,
        /// 第 0 条轨已完成分片的声明时长之和；含续传前已完成的。直播即已录时长
        pub duration: Duration,
        /// 任务目录中已完成的分片与 init 段的字节数（解密后）；含续传前已完成的
        pub bytes: u64,
        /// 本次运行从网络读到的响应体字节数，读到即计：含播放列表与 key，含没读完、校验失败与重试前的请求。按它的
        /// 变化算下载速度
        pub received: u64,
        /// 直播：各轨（下标为轨道编号）最近一次刷新失败的原因。这条轨之后刷新成功、不再刷新或录制结束即为 None；
        /// 一直失败到停滞时任务以 `LiveStalledError` 结束。点播与录制开始前为空
        pub refresh_errors: Seq<Option<RefreshCause>>,
        /// 直播且输出含 MP4：录到的内容放不进 MP4 的原因。续录时任务目录里已录齐的组在开始后核对一次，之后每个（会话,
        /// 不连续段）组在各轨都有分片时核对（录制循环结束、收尾时才下完的不核对）；第 0 组开头的分片判断不了时，各组
        /// 只核对自身、不与它比较。组号为核对时的，之后才录齐、排在前面的组会让合并时的组号变大。录制照常进行，停不停
        /// 由调用方定（直播错过的内容录不回来）；合并 MP4 时照样以 `Mp4UnsupportedError` 失败，可改为只输出本地 HLS
        /// 再合并。点播（下载各组的第一个分片后核对，放不进时任务直接失败）与不输出 MP4 时为 None
        pub mp4_unsupported: Option<Mp4Unsupported>,
    }
}

record! {
    /// 合并出的一路流。
    pub struct StreamReport {
        /// 编码参数
        pub shape: Shape,
        /// 写入、且回读核对一致的包数
        pub packets: u64,
        /// 输入中没有解码时间戳、因而未写入的包数
        pub skipped_without_dts: u64,
        /// 各组开头解不出、因而未写入的视频包数：第一个能开始解码的帧（关键帧；H.264 另认整帧都是 I slice 的帧）之前的
        /// 包，与参考上一个 GOP 的前导帧（HEVC 为 RASL；H.264 为解码在起点之后、呈现在它之前的帧）。音频为 0
        pub skipped_before_keyframe: u64,
        /// 损坏而没有写入的音频字节数：TS 里读不出帧头或帧长与包长对不上的包（来源丢了 TS 包，或缺片处 PES 不完整，
        /// 拼出的包），打包音频里切帧时跳过的不完整的帧或标签、认不出的字节，以及片内损坏之后定不了时刻（这一片是最后
        /// 一片，或下一片之前缺了分片）的帧。视频包照常写入，由解码器做错误隐藏
        pub discarded_corrupt_bytes: u64,
        /// 输出时间线上从最早的呈现时刻到最晚的结束时刻；含组内时间戳间断（如缺失的分片）留下的空档
        pub duration: Duration,
    }
}

record! {
    /// 合并 MP4 的结果。
    pub struct MediaReport {
        /// 各路输出流，按流的下标排列
        pub streams: Seq<StreamReport>,
    }
}

record! {
    /// 生成的 MP4。
    pub struct Mp4Output {
        /// 绝对路径，相对路径按开始任务时的当前目录补全
        pub path: PyPath,
        /// 各路输出流的编码参数、包数与时长
        pub report: MediaReport,
    }
}

variants! {
    /// 本次运行的录制如何结束。
    pub enum LiveEnd {
        /// 所有轨出现 EXT-X-ENDLIST
        EndList {},
        /// 调用了 `Job.stop`
        Stopped {},
        /// 各轨都录满了 `LiveOptions.max_duration`
        DurationReached {},
        /// 一条轨停滞，且看起来是直播已结束：它与其他轨都不再出新分片；故障导致的停滞是 `LiveStalledError`
        #[pyo3(constructor = (*, track, cause))]
        Stalled {
            /// 第几条轨
            track: usize,
            /// 看起来已结束的原因
            cause: StallCause,
        },
        /// 一条轨的播放列表与之前刷新得到的接不上，且录的不止一条轨：其他轨从哪里起与它对得上不知道，不另起会话，
        /// 之后的分片未录。只录一条轨时另起会话接着录（见 `NewSession`）
        #[pyo3(constructor = (*, track, cause))]
        Broken {
            /// 第几条轨
            track: usize,
            /// 接不上的方式
            cause: SourceBreak,
        },
    }
}

variants! {
    /// 媒体播放列表与之前刷新得到的接不上的方式。
    pub enum SourceBreak {
        /// 媒体序号回退、且与上次的窗口没有重叠，连续两次刷新都如此（多为编码器重启；只一次多为 CDN 的旧缓存，不算）
        Restarted {},
        /// 在某个序号处矛盾：同一序号换了分片，或不连续段编号对不上（服务器错误）
        Inconsistent {
            /// 矛盾处的媒体序号
            sequence: u64,
        },
    }
}

record! {
    /// 录制中途另起的会话：只录一条轨时，它的播放列表与之前刷新得到的接不上，不结束录制，另起会话接着录。序号回退时
    /// 从新窗口的起点录；矛盾时跳过已处理到的序号，只录更新的分片。
    pub struct NewSession {
        /// 另起的会话编号
        pub session: u32,
        /// 接不上的方式
        pub cause: SourceBreak,
    }
}

variants! {
    /// 分片不在输出中的原因。
    pub enum MissReason {
        /// 两次刷新之间已滑出播放列表窗口，没有被列出过
        Expired {},
        /// 列出了，但取不到：404/410，可重试的错误重试后仍失败，或重试后内容仍没写完
        Failed {
            /// 取不到的原因
            cause: FetchErrorKind,
        },
        /// 列出了，但它的 init 段取不到（同上）
        InitFailed {
            /// 取不到的原因
            cause: FetchErrorKind,
        },
        /// 所在不连续段不是每条轨都录到，无法合并
        Unmergeable {},
        /// 之前的运行中缺失，原因没有记录
        Unknown {},
    }
}

record! {
    /// 一段连续的缺失分片：第 `session` 个会话中第 `track` 条轨序号 `first` 到 `last`（含）的分片不在输出中。
    pub struct Missed {
        /// 会话编号，从 0 开始
        pub session: u32,
        /// 第几条轨
        pub track: usize,
        /// 第一个缺失的媒体序号
        pub first: u64,
        /// 最后一个缺失的媒体序号
        pub last: u64,
        /// 区间里的分片数，即 `last - first + 1`；超出 64 位整数时为 2**64 - 1
        pub count: usize,
        /// 缺失的原因
        pub reason: MissReason,
    }
}

record! {
    /// 直播的录制结果。
    pub struct LiveReport {
        /// 本次运行的录制如何结束；只合并（`Engine.merge_recorded`）时为 None
        pub end: Option<LiveEnd>,
        /// 合并进输出的录制会话数。会话是一段时间线连续的录制：中断后续录时，若各轨都与之前录到的内容接得上，仍是
        /// 同一个会话；否则另起一个，与之前的首尾相接。录制中途另起的（见 `new_sessions`）也算
        pub session_count: usize,
        /// 本次运行录制中途另起的会话，按先后；只合并时为空
        pub new_sessions: Seq<NewSession>,
        /// 不在输出中的分片，按会话、轨道、序号排列，相接且原因相同的合成一个区间。不含之前的运行里、某个会话第一个
        /// 已完成分片之前或最后一个之后缺的（原因与范围随那次运行一起丢了）
        pub missed: Seq<Missed>,
    }
}

record! {
    /// 任务结果：下载、录制或只合并。
    pub struct Output {
        /// 生成的 MP4；没要 MP4（`Target.Hls`）时为 None
        pub mp4: Option<Mp4Output>,
        /// 生成的本地 HLS 目录（绝对路径）；没要 HLS 时为 None。入口为其中的 `index.m3u8`：单轨时是媒体播放列表，视频与
        /// 独立音频分离时是主播放列表；各轨的分片、init 段（与分离时这条轨的 `index.m3u8`）在 `<轨道编号>/` 下。内容与
        /// MP4 相同：只放各轨都有的不连续段组，组内缺失的分片不标出、保留原时间戳
        pub hls: Option<PyPath>,
        /// 输出的分片数，各轨合计
        pub segments: usize,
        /// 同 `Progress.bytes`
        pub bytes: u64,
        /// 收尾没删掉的东西：被替换的旧输出、没删干净的任务目录等；输出不受影响。都删干净时为空
        pub leftovers: Seq<Leftover>,
        /// 直播的录制结果；点播为 None
        pub live: Option<LiveReport>,
    }
}

impl From<core::HttpError> for HttpErrorKind {
    fn from(kind: core::HttpError) -> Self {
        match kind {
            core::HttpError::Status(code) => HttpErrorKind::Status { code },
            core::HttpError::RangeIgnored(code) => HttpErrorKind::RangeIgnored { code },
            core::HttpError::RangeMismatch(content_range) => {
                HttpErrorKind::RangeMismatch { content_range }
            }
            core::HttpError::Timeout => HttpErrorKind::Timeout {},
            core::HttpError::Certificate(detail) => HttpErrorKind::Certificate { detail },
            core::HttpError::Redirect(detail) => HttpErrorKind::Redirect { detail },
            core::HttpError::Connect(detail) => HttpErrorKind::Connect { detail },
            core::HttpError::Proxy(detail) => HttpErrorKind::Proxy { detail },
            core::HttpError::InvalidRequest(detail) => HttpErrorKind::InvalidRequest { detail },
            core::HttpError::Transport(detail) => HttpErrorKind::Transport { detail },
        }
    }
}

impl From<core::FetchError> for FetchErrorKind {
    fn from(kind: core::FetchError) -> Self {
        match kind {
            core::FetchError::Http(kind) => FetchErrorKind::Http { kind: kind.into() },
            core::FetchError::Incomplete => FetchErrorKind::Incomplete {},
        }
    }
}

impl From<core::FetchFailure> for FetchFailure {
    fn from(failure: core::FetchFailure) -> Self {
        match failure {
            core::FetchFailure::Http(failure) => FetchFailure::Http {
                failure: failure.into(),
            },
            core::FetchFailure::Incomplete => FetchFailure::Incomplete {},
        }
    }
}

impl From<core::HttpFailure> for HttpFailure {
    fn from(failure: core::HttpFailure) -> Self {
        HttpFailure {
            retryable: failure.kind.retryable(),
            kind: failure.kind.into(),
            retry_after: failure.retry_after.map(|wait| wait.as_secs()),
        }
    }
}

impl From<core::RefreshCause> for RefreshCause {
    fn from(cause: core::RefreshCause) -> Self {
        let (retryable, retry_after) = (cause.retryable(), cause.retry_after());
        let reason = match cause {
            core::RefreshCause::Http(failure) => RefreshReason::Http {
                failure: failure.into(),
            },
            core::RefreshCause::Empty => RefreshReason::Empty {},
            core::RefreshCause::Truncated => RefreshReason::Truncated {},
            core::RefreshCause::Syntax { line, kind } => RefreshReason::Syntax {
                line,
                detail: kind.to_string(),
            },
        };
        RefreshCause {
            reason,
            retryable,
            retry_after: retry_after.map(|wait| wait.as_secs()),
        }
    }
}

impl From<core::StallCause> for StallCause {
    fn from(cause: core::StallCause) -> Self {
        match cause {
            core::StallCause::NoNewSegments => StallCause::NoNewSegments {},
            core::StallCause::PlaylistGone(status) => StallCause::PlaylistGone { status },
        }
    }
}

impl From<core::LeftoverKind> for LeftoverKind {
    fn from(kind: core::LeftoverKind) -> Self {
        match kind {
            core::LeftoverKind::Removable => LeftoverKind::Removable {},
            core::LeftoverKind::Displaced { target } => LeftoverKind::Displaced {
                target: target.into(),
            },
            core::LeftoverKind::Installed => LeftoverKind::Installed {},
            core::LeftoverKind::WorkDir => LeftoverKind::WorkDir {},
        }
    }
}

impl From<core::Leftover> for Leftover {
    fn from(leftover: core::Leftover) -> Self {
        Leftover {
            path: leftover.path.into(),
            kind: leftover.kind.into(),
            cause: leftover.cause,
        }
    }
}

impl From<core::remux::StreamKind> for StreamKind {
    fn from(kind: core::remux::StreamKind) -> Self {
        match kind {
            core::remux::StreamKind::Video => StreamKind::Video {},
            core::remux::StreamKind::Audio => StreamKind::Audio {},
        }
    }
}

impl From<core::remux::Shape> for Shape {
    fn from(shape: core::remux::Shape) -> Self {
        match shape {
            core::remux::Shape::Video {
                codec,
                width,
                height,
                pixel_format,
            } => Shape::Video {
                codec: codec.to_owned(),
                width,
                height,
                pixel_format: pixel_format.map(str::to_owned),
            },
            core::remux::Shape::Audio {
                codec,
                sample_rate,
                channels,
            } => Shape::Audio {
                codec: codec.to_owned(),
                sample_rate,
                channels,
            },
        }
    }
}

impl From<core::Stage> for Stage {
    fn from(stage: core::Stage) -> Self {
        match stage {
            core::Stage::Preparing => Stage::Preparing {},
            core::Stage::Downloading => Stage::Downloading {},
            core::Stage::Recording => Stage::Recording {},
            core::Stage::Writing => Stage::Writing {},
            core::Stage::Done => Stage::Done {},
        }
    }
}

impl From<core::Progress> for Progress {
    fn from(p: core::Progress) -> Self {
        Progress {
            stage: p.stage.into(),
            selected: p.selected.map(Selected::from),
            segments_done: p.segments_done,
            segments_total: p.segments_total,
            segments_failed: p.segments_failed,
            segments_expired: p.segments_expired,
            duration: Duration::from_micros(p.duration_us),
            bytes: p.bytes,
            received: p.received,
            refresh_errors: p
                .refresh_errors
                .into_iter()
                .map(|error| error.map(RefreshCause::from))
                .collect(),
            mp4_unsupported: p.mp4_unsupported.map(Mp4Unsupported::from),
        }
    }
}

impl From<core::remux::StreamReport> for StreamReport {
    fn from(s: core::remux::StreamReport) -> Self {
        StreamReport {
            shape: s.shape.into(),
            packets: s.packets,
            skipped_without_dts: s.skipped_without_dts,
            skipped_before_keyframe: s.skipped_before_keyframe,
            discarded_corrupt_bytes: s.discarded_corrupt_bytes,
            duration: Duration::from_micros(s.duration_us),
        }
    }
}

impl From<core::Mp4Output> for Mp4Output {
    fn from(m: core::Mp4Output) -> Self {
        Mp4Output {
            path: m.path.into(),
            report: MediaReport {
                streams: m
                    .report
                    .streams
                    .into_iter()
                    .map(StreamReport::from)
                    .collect(),
            },
        }
    }
}

impl From<core::LiveEnd> for LiveEnd {
    fn from(end: core::LiveEnd) -> Self {
        match end {
            core::LiveEnd::EndList => LiveEnd::EndList {},
            core::LiveEnd::Stopped => LiveEnd::Stopped {},
            core::LiveEnd::DurationReached => LiveEnd::DurationReached {},
            core::LiveEnd::Stalled { track, cause } => LiveEnd::Stalled {
                track,
                cause: cause.into(),
            },
            core::LiveEnd::Broken { track, cause } => LiveEnd::Broken {
                track,
                cause: cause.into(),
            },
        }
    }
}

impl From<core::SourceBreak> for SourceBreak {
    fn from(cause: core::SourceBreak) -> Self {
        match cause {
            core::SourceBreak::Restarted => SourceBreak::Restarted {},
            core::SourceBreak::Inconsistent { sequence } => SourceBreak::Inconsistent { sequence },
        }
    }
}

impl From<core::NewSession> for NewSession {
    fn from(s: core::NewSession) -> Self {
        NewSession {
            session: s.session,
            cause: s.cause.into(),
        }
    }
}

impl From<core::MissReason> for MissReason {
    fn from(reason: core::MissReason) -> Self {
        match reason {
            core::MissReason::Expired => MissReason::Expired {},
            core::MissReason::Failed(cause) => MissReason::Failed {
                cause: cause.into(),
            },
            core::MissReason::InitFailed(cause) => MissReason::InitFailed {
                cause: cause.into(),
            },
            core::MissReason::Unmergeable => MissReason::Unmergeable {},
            core::MissReason::Unknown => MissReason::Unknown {},
        }
    }
}

impl From<core::Missed> for Missed {
    fn from(m: core::Missed) -> Self {
        Missed {
            count: m.count(),
            session: m.session,
            track: m.track,
            first: m.first,
            last: m.last,
            reason: m.reason.into(),
        }
    }
}

impl From<core::LiveReport> for LiveReport {
    fn from(l: core::LiveReport) -> Self {
        LiveReport {
            end: l.end.map(LiveEnd::from),
            session_count: l.session_count,
            new_sessions: l.new_sessions.into_iter().map(NewSession::from).collect(),
            missed: l.missed.into_iter().map(Missed::from).collect(),
        }
    }
}

impl From<core::Output> for Output {
    fn from(o: core::Output) -> Self {
        Output {
            mp4: o.mp4.map(Mp4Output::from),
            hls: o.hls.map(PyPath::from),
            segments: o.segments,
            bytes: o.bytes,
            leftovers: o.leftovers.into_iter().map(Leftover::from).collect(),
            live: o.live.map(LiveReport::from),
        }
    }
}

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<HttpErrorKind>()?;
    module.add_class::<HttpFailure>()?;
    module.add_class::<FetchErrorKind>()?;
    module.add_class::<FetchFailure>()?;
    module.add_class::<RefreshReason>()?;
    module.add_class::<RefreshCause>()?;
    module.add_class::<StallCause>()?;
    module.add_class::<LeftoverKind>()?;
    module.add_class::<Leftover>()?;
    module.add_class::<StreamKind>()?;
    module.add_class::<Shape>()?;
    module.add_class::<Stage>()?;
    module.add_class::<Progress>()?;
    module.add_class::<StreamReport>()?;
    module.add_class::<MediaReport>()?;
    module.add_class::<Mp4Output>()?;
    module.add_class::<LiveEnd>()?;
    module.add_class::<SourceBreak>()?;
    module.add_class::<NewSession>()?;
    module.add_class::<MissReason>()?;
    module.add_class::<Missed>()?;
    module.add_class::<LiveReport>()?;
    module.add_class::<Output>()?;
    Ok(())
}
