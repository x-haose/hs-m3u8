//! 点播：解密、轨道与分片（音视频分离、字节范围、EXT-X-GAP）、重试、没写完的分片、失败与续传、回调，以及错误说明与下载
//! 前的拒绝。

mod decrypt;
mod errors;
mod hooks;
mod incomplete;
mod resume;
mod retry;
mod tracks;

use std::path::Path;

use hs_m3u8_remux::{DiscontinuityGroup, Streams};

use crate::server::Server;
use crate::{expected, fixture, track};

/// ts_a 的分片（H.264 + AAC 的 TS）。
const TS_A: [&str; 2] = ["seg0.ts", "seg1.ts"];

/// ts_a 两个分片单轨合并的期望输出。
fn expected_ts_a(dir: &Path) -> Vec<u8> {
    let tracks = vec![track("ts_a", None, &TS_A)];
    expected(dir, &[Streams::All], &[DiscontinuityGroup { tracks }])
}

/// ts_a 的两个分片，明文；`duration` 为每个分片声明的时长。
fn put_ts_a(server: &Server, playlist: &str, duration: f64) {
    for name in TS_A {
        server.put(name, fixture(&format!("ts_a/{name}")));
    }
    server.put(
        playlist,
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:{duration},\nseg0.ts\n\
             #EXTINF:{duration},\nseg1.ts\n#EXT-X-ENDLIST\n"
        ),
    );
}
