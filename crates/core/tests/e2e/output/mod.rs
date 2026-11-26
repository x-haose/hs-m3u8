//! 输出：本地 HLS 目录、MP4 与 HLS 两者都要、替换已有的输出、写出之后删除任务目录、输出路径。
//!
//! 本库的 FFmpeg 没有 HLS 读取器，HLS 输出的验证方式：用本库的解析器读回写出的播放列表，按列出的文件
//! 直接合并，结果须与同一任务输出的 MP4 逐字节相同。

mod hls;
mod paths;
mod replace;
mod workdir;

use std::path::Path;

use hs_m3u8_core::{JobRequest, Target, Url};
use hs_m3u8_hls::{MediaPlaylist, Playlist, RenditionKind, parse};
use hs_m3u8_remux::{
    Container, Control, DiscontinuityGroup, Label, Segment, Streams, SubtitleMode, Track,
    TrackSegments,
};

use crate::{both_targets, request};

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

/// 读回 HLS 目录：各组各轨列出的文件，以及各轨的取流方式与标注（取自主播放列表里的 rendition）。
fn read_hls(dir: &Path) -> (Vec<Track>, Vec<DiscontinuityGroup>) {
    let media = |uri: &Url| match read_playlist(&uri.to_file_path().unwrap()) {
        Playlist::Media(media) => media,
        Playlist::Master(_) => panic!("应为媒体播放列表"),
    };
    let (tracks, playlists): (Vec<Track>, Vec<MediaPlaylist>) =
        match read_playlist(&dir.join("index.m3u8")) {
            Playlist::Media(media) => (vec![crate::all()], vec![media]),
            Playlist::Master(master) => {
                let mut tracks = vec![(crate::video(), media(&master.variants[0].uri))];
                for r in &master.renditions {
                    let streams = match r.kind {
                        RenditionKind::Audio => Streams::Audio,
                        _ => Streams::Subtitles,
                    };
                    let label = Label {
                        language: r.language.clone(),
                        name: r.name.clone(),
                        default: r.default,
                        forced: r.forced,
                    };
                    tracks.push((Track { streams, label }, media(r.uri.as_ref().unwrap())));
                }
                tracks.into_iter().unzip()
            }
        };
    for media in &playlists {
        assert!(media.ended, "本地 HLS 应是点播");
    }
    let per_track: Vec<Vec<TrackSegments>> = playlists.iter().map(media_groups).collect();
    let groups = (0..per_track[0].len())
        .map(|g| DiscontinuityGroup {
            tracks: per_track.iter().map(|t| t[g].clone()).collect(),
        })
        .collect();
    (tracks, groups)
}

/// 按 HLS 目录列出的文件合并，须与 `mp4` 逐字节相同。
fn assert_hls_matches(hls: &Path, mp4: &Path) {
    let (tracks, groups) = read_hls(hls);
    let merged = hls.with_extension("check.mp4");
    hs_m3u8_remux::remux(
        &tracks,
        &groups,
        &merged,
        Container::Mp4,
        SubtitleMode::Embed,
        &Control::default(),
    )
    .unwrap();
    assert_eq!(std::fs::read(merged).unwrap(), std::fs::read(mp4).unwrap());
}

fn both(dir: &Path) -> Target {
    both_targets(dir.join("out.mp4"), dir.join("out"))
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
