//! 一组分片很多时，合并同一时刻只打开一个分片文件：把进程的文件描述符上限压到 64，合并一组一百多个文件。
//! 单独成一个测试文件（独立进程），压低上限不影响其他测试。
#![cfg(unix)]

use std::path::{Path, PathBuf};

use hs_m3u8_remux::{DiscontinuityGroup, Streams, TrackSegments, remux};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media/ts_long")
}

fn single_group(segments: Vec<PathBuf>) -> Vec<DiscontinuityGroup> {
    vec![DiscontinuityGroup {
        tracks: vec![TrackSegments {
            init: None,
            segments,
        }],
    }]
}

/// 把本进程能同时打开的文件数（软上限）压到 `soft`。
fn lower_descriptor_limit(soft: libc::rlim_t) {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: 传入的是本函数栈上有效的 rlimit；只读取并降低本进程的软上限，不涉及其他内存。
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        limit.rlim_cur = soft.min(limit.rlim_max);
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
    }
}

#[test]
fn many_segments_in_one_group_stay_within_the_descriptor_limit() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("many_files");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    let originals: Vec<PathBuf> = (0..4)
        .map(|i| fixtures().join(format!("seg{i}.ts")))
        .collect();
    // 按 TS 包（188 字节）对齐切成小文件：前后拼起来的字节流与原来相同
    let bytes: Vec<u8> = originals
        .iter()
        .flat_map(|p| std::fs::read(p).unwrap())
        .collect();
    let chunks: Vec<PathBuf> = bytes
        .chunks(188 * 4)
        .enumerate()
        .map(|(i, chunk)| {
            let path = dir.join(format!("chunk{i:04}.ts"));
            std::fs::write(&path, chunk).unwrap();
            path
        })
        .collect();
    assert!(chunks.len() > 128, "分片数应远超压低后的上限");
    let expected = dir.join("expected.mp4");
    remux(&[Streams::All], &single_group(originals), &expected).unwrap();

    lower_descriptor_limit(64);
    let output = dir.join("out.mp4");
    remux(&[Streams::All], &single_group(chunks), &output).unwrap();

    assert_eq!(
        std::fs::read(&output).unwrap(),
        std::fs::read(&expected).unwrap()
    );
}
