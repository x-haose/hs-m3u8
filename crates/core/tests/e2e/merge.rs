//! 合并完整文件的异步入口：结果与直接用 remux 合并逐字节相同（MP4 输出；Matroska 每次写随机的 UID），令牌取消后以
//! 取消结束。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hs_m3u8_core::remux::{Container, FileInput, Streams, WriteProgress};
use hs_m3u8_core::{CancellationToken, Error};
use hs_m3u8_remux::Control;

use crate::{fixtures, test_dir};

fn inputs() -> Vec<FileInput> {
    let files = fixtures().join("files");
    vec![
        FileInput {
            path: files.join("video_vp9.webm"),
            streams: Streams::Video,
        },
        FileInput {
            path: files.join("audio_opus.webm"),
            streams: Streams::Audio,
        },
    ]
}

#[tokio::test]
async fn merges_like_remux_and_stops_when_cancelled() {
    let dir = test_dir("merge_files");
    let expected = dir.join("expected.mp4");
    hs_m3u8_remux::merge_files(&inputs(), &expected, Container::Mp4, &Control::default()).unwrap();

    let output = dir.join("out.mp4");
    let written = Arc::new(AtomicU64::new(0));
    let counted = written.clone();
    let progress = WriteProgress::new(move |bytes| {
        counted.fetch_add(bytes, Ordering::Relaxed);
    });
    let report = hs_m3u8_core::merge_files(
        inputs(),
        output.clone(),
        Container::Mp4,
        CancellationToken::new(),
        progress,
    )
    .await
    .unwrap();
    assert_eq!(report.streams.len(), 2);
    assert_eq!(
        std::fs::read(&output).unwrap(),
        std::fs::read(&expected).unwrap()
    );
    let total: u64 = inputs()
        .iter()
        .map(|i| std::fs::metadata(&i.path).unwrap().len())
        .sum();
    assert_eq!(written.load(Ordering::Relaxed), total);

    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = hs_m3u8_core::merge_files(
        inputs(),
        dir.join("cancelled.mp4"),
        Container::Mp4,
        cancel,
        WriteProgress::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
}
