//! remux 的编解码测试：用 tests/fixtures/media 下按 1 秒切好的合成 HLS 分片，核对输出的包数、时间线与 MP4 结构。

mod check;
mod codecs;
mod control;
mod fmp4;
mod io;
mod keyframes;
mod merge;
mod packed;
mod params;
mod timing;
mod ts_damage;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ffmpeg_next as ffmpeg;
use hs_m3u8_remux::{Cancel, Control, DiscontinuityGroup, Error, Segment, Streams, TrackSegments};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media")
}

/// 每个测试独立的空目录。
fn work_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 目录下一条轨的分片：有 init.mp4 时作为 init 段，seg<N>.* 按 N 排序。
fn track_in(dir: &Path) -> TrackSegments {
    let init = dir.join("init.mp4");
    let mut segments: Vec<(u32, PathBuf)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.strip_prefix("seg")?.parse().ok()?;
            Some((stem, p))
        })
        .collect();
    segments.sort();
    let init = init.exists().then_some(init);
    TrackSegments {
        segments: segments
            .into_iter()
            .map(|(_, p)| declared(init.as_deref(), p))
            .collect(),
        init,
        gaps: Vec::new(),
    }
}

/// 分片 `path` 带上声明的时长（EXTINF）：取它的实际时长，即各路流从最早的呈现时刻到最晚的结束时刻，相当于来源写的
/// EXTINF 准确；fMP4 连同 `init` 读。读不出时长（不是完整的媒体）时为 0。
fn declared(init: Option<&Path>, path: PathBuf) -> Segment {
    let media = match init {
        Some(init) => {
            static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("declared");
            std::fs::create_dir_all(&dir).unwrap();
            let joined = dir.join(format!("{}-{n}.mp4", std::process::id()));
            let data = [std::fs::read(init).unwrap(), std::fs::read(&path).unwrap()].concat();
            std::fs::write(&joined, data).unwrap();
            joined
        }
        None => path.clone(),
    };
    let duration_us = media_span(&media).map_or(0, |span| (span * 1e6).round() as u64);
    Segment { path, duration_us }
}

/// 没有 init 段的各分片（TS 与打包音频），带上声明的时长（见 [`declared`]）。
fn segs(paths: impl IntoIterator<Item = PathBuf>) -> Vec<Segment> {
    paths.into_iter().map(|path| declared(None, path)).collect()
}

/// 文件里各路流从最早的呈现时刻到最晚的结束时刻，秒；打不开或读不出包时为 None。
fn media_span(path: &Path) -> Option<f64> {
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(path).ok()?;
    let bases: Vec<f64> = ictx.streams().map(|s| f64::from(s.time_base())).collect();
    let (mut start, mut end) = (f64::INFINITY, f64::NEG_INFINITY);
    loop {
        let mut packet = ffmpeg::Packet::empty();
        if packet.read(&mut ictx).is_err() {
            break;
        }
        let tb = bases[packet.stream()];
        let Some(pts) = packet.pts().or(packet.dts()) else {
            continue;
        };
        start = start.min(pts as f64 * tb);
        end = end.max((pts + packet.duration()) as f64 * tb);
    }
    (end > start).then_some(end - start)
}

fn track(name: &str) -> TrackSegments {
    track_in(&fixtures().join(name))
}

/// 按顺序核对各组（组号即下标），见 [`hs_m3u8_remux::check`]。
fn check(streams: &[Streams], groups: &[DiscontinuityGroup]) -> Result<(), Error> {
    let numbered: Vec<(usize, &DiscontinuityGroup)> = groups.iter().enumerate().collect();
    hs_m3u8_remux::check(streams, &numbered)
}

fn group<const N: usize>(tracks: [TrackSegments; N]) -> DiscontinuityGroup {
    DiscontinuityGroup {
        tracks: tracks.into(),
    }
}

/// 一路流的包：(pts 秒, 结束时刻秒)，按文件中的顺序。
type Timeline = Vec<(f64, f64)>;

/// 文件中第一路视频与第一路音频的包时间线。
fn timelines(path: &Path) -> (Timeline, Timeline) {
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(path).unwrap();
    let first = |medium| {
        ictx.streams()
            .find(|s| s.parameters().medium() == medium)
            .map(|s| (s.index(), f64::from(s.time_base())))
    };
    let (video, audio) = (
        first(ffmpeg::media::Type::Video),
        first(ffmpeg::media::Type::Audio),
    );
    let (mut v, mut a) = (Vec::new(), Vec::new());
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut ictx) {
            Ok(()) => {
                let target = match (video, audio) {
                    (Some((i, tb)), _) if i == packet.stream() => Some((&mut v, tb)),
                    (_, Some((i, tb))) if i == packet.stream() => Some((&mut a, tb)),
                    _ => None,
                };
                if let Some((list, tb)) = target {
                    let pts = packet.pts().or(packet.dts()).unwrap() as f64 * tb;
                    list.push((pts, pts + packet.duration() as f64 * tb));
                }
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => panic!("读取 {} 失败: {e}", path.display()),
        }
    }
    (v, a)
}

/// 把一条轨的 init 段与分片按字节拼成一个文件，便于用 FFmpeg 直接读取输入的时间线。
fn concat_track(t: &TrackSegments, dir: &Path, name: &str) -> PathBuf {
    let mut bytes = Vec::new();
    for p in t.init.iter().chain(t.segments.iter().map(|s| &s.path)) {
        bytes.extend(std::fs::read(p).unwrap());
    }
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// 打包音频样本 `name`（packed 下的目录）的各分片与各自的时长（90 kHz 时钟的 tick）。分片是裸的音频帧，不带
/// ID3 标签。
fn packed_fixture(name: &str) -> Vec<(PathBuf, u64)> {
    track(&format!("packed/{name}"))
        .segments
        .into_iter()
        .map(|segment| {
            let ticks = (span(&timelines(&segment.path).1) * 90_000.0).round() as u64;
            (segment.path, ticks)
        })
        .collect()
}

/// 在 `dir` 下写一个打包音频分片：`src` 的音频帧前面加上 ID3v2.4 标签，其中的 PRIV 帧给出时间戳 `timestamp`
/// （90 kHz）；`timestamp` 为 None 时原样复制。
fn packed_segment(dir: &Path, src: &Path, timestamp: Option<u64>) -> PathBuf {
    let audio = std::fs::read(src).unwrap();
    let data = match timestamp {
        Some(timestamp) => {
            let owner = b"com.apple.streaming.transportStreamTimestamp\0";
            let frame_data = [owner.as_slice(), &timestamp.to_be_bytes()].concat();
            let syncsafe =
                |size: usize| [size >> 21, size >> 14, size >> 7, size].map(|b| (b & 0x7F) as u8);
            let frame = [
                b"PRIV".as_slice(),
                &syncsafe(frame_data.len()),
                &[0, 0],
                &frame_data,
            ]
            .concat();
            [
                b"ID3\x04\x00\x00".as_slice(),
                &syncsafe(frame.len()),
                &frame,
                &audio,
            ]
            .concat()
        }
        None => audio,
    };
    let name = format!(
        "{}-{}",
        timestamp.map_or("untimed".to_owned(), |t| t.to_string()),
        src.file_name().unwrap().to_str().unwrap()
    );
    let path = dir.join(name);
    std::fs::write(&path, data).unwrap();
    path
}

/// 文件开头 `frames` 个包（打包音频样本里即前几帧）的字节。
fn first_frames(path: &Path, frames: usize) -> Vec<u8> {
    let len: usize = packet_sizes(path)[..frames].iter().sum();
    std::fs::read(path).unwrap()[..len].to_vec()
}

/// 文件里各包的字节数，按读出的顺序（裸音频样本里即各帧）。
fn packet_sizes(path: &Path) -> Vec<usize> {
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(path).unwrap();
    let mut sizes = Vec::new();
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut ictx) {
            Ok(()) => sizes.push(packet.size()),
            Err(ffmpeg::Error::Eof) => return sizes,
            Err(e) => panic!("{e}"),
        }
    }
}

/// 时间线上最早的呈现时刻，秒。
fn earliest(timeline: &Timeline) -> f64 {
    timeline.iter().map(|p| p.0).fold(f64::INFINITY, f64::min)
}

fn span(timeline: &Timeline) -> f64 {
    let start = timeline.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    let end = timeline
        .iter()
        .map(|p| p.1)
        .fold(f64::NEG_INFINITY, f64::max);
    end - start
}

/// 只带取消信号的控制：每次检查取消时以检查的序号（从 0 起）调用 `act`、以它的返回值为是否取消，另返回已检查的次数。
/// `act` 在 FFmpeg 的回调里调用，不得 panic。
fn counting_cancel(
    act: impl Fn(usize) -> bool + Send + Sync + 'static,
) -> (Control, Arc<AtomicUsize>) {
    let checks = Arc::new(AtomicUsize::new(0));
    let counted = checks.clone();
    let cancel = Cancel::new(move || act(counted.fetch_add(1, Ordering::Relaxed)));
    let control = Control {
        cancel,
        ..Control::default()
    };
    (control, checks)
}
