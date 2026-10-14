//! 输出：本地 HLS 目录、MP4 与 HLS 两者都要、替换已有的输出、写 HLS 失败时的清理、路径校验。
//!
//! 本库的 FFmpeg 没有 HLS 读取器，HLS 输出的验证方式：用本库的解析器读回写出的播放列表，按列出的文件
//! 直接合并，结果须与同一任务输出的 MP4 逐字节相同。

mod hls;
mod paths;
mod replace;
mod workdir;

use std::path::Path;

use hs_m3u8_core::{JobRequest, Target, Url};
use hs_m3u8_hls::{MediaPlaylist, Playlist, parse};
use hs_m3u8_remux::{Control, DiscontinuityGroup, Segment, Streams, TrackSegments, remux};

use crate::request;

fn read_playlist(path: &Path) -> Playlist {
    let text = std::fs::read_to_string(path).unwrap();
    parse(&text, &Url::from_file_path(path).unwrap()).unwrap()
}

/// 媒体播放列表按不连续段分组，换成本地文件。用于把读回的 HLS 重新合并、与 MP4 比对字节：HLS 不标出组内缺失的分片，
/// 这里不知道缺在哪，声明的时长取 0（不核对时间戳推进）。
fn media_groups(media: &MediaPlaylist) -> Vec<TrackSegments> {
    let mut groups: Vec<TrackSegments> = Vec::new();
    let mut current = None;
    for segment in &media.segments {
        if current != Some(segment.discontinuity) {
            current = Some(segment.discontinuity);
            groups.push(TrackSegments {
                init: segment.init.as_ref().map(|i| i.uri.to_file_path().unwrap()),
                segments: Vec::new(),
                gaps: Vec::new(),
            });
        }
        groups.last_mut().unwrap().segments.push(Segment {
            path: segment.uri.to_file_path().unwrap(),
            duration_us: 0,
        });
    }
    groups
}

/// 读回 HLS 目录：各组各轨列出的文件，以及各轨的取流方式。
fn read_hls(dir: &Path) -> (Vec<Streams>, Vec<DiscontinuityGroup>) {
    let tracks: Vec<MediaPlaylist> = match read_playlist(&dir.join("index.m3u8")) {
        Playlist::Media(media) => vec![media],
        Playlist::Master(master) => {
            let audio = master.renditions[0].uri.as_ref().unwrap();
            [&master.variants[0].uri, audio]
                .map(|uri| match read_playlist(&uri.to_file_path().unwrap()) {
                    Playlist::Media(media) => media,
                    Playlist::Master(_) => panic!("应为媒体播放列表"),
                })
                .into()
        }
    };
    for media in &tracks {
        assert!(media.ended, "本地 HLS 应是点播");
    }
    let streams = match tracks.len() {
        1 => vec![Streams::All],
        _ => vec![Streams::Video, Streams::Audio],
    };
    let per_track: Vec<Vec<TrackSegments>> = tracks.iter().map(media_groups).collect();
    let groups = (0..per_track[0].len())
        .map(|g| DiscontinuityGroup {
            tracks: per_track.iter().map(|t| t[g].clone()).collect(),
        })
        .collect();
    (streams, groups)
}

/// 按 HLS 目录列出的文件合并，须与 `mp4` 逐字节相同。
fn assert_hls_matches(hls: &Path, mp4: &Path) {
    let (streams, groups) = read_hls(hls);
    let merged = hls.with_extension("check.mp4");
    remux(&streams, &groups, &merged, &Control::default()).unwrap();
    assert_eq!(std::fs::read(merged).unwrap(), std::fs::read(mp4).unwrap());
}

fn both(dir: &Path) -> Target {
    Target::Both {
        mp4: dir.join("out.mp4"),
        hls: dir.join("out"),
    }
}

/// 输出目标为 `target`，任务目录取默认值。
fn request_to(url: Url, dir: &Path, target: Target) -> JobRequest {
    let mut req = request(url, dir);
    req.output.target = target;
    req
}

/// 目录里各项的名字，排好序。
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}
