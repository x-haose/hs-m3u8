//! core 的端到端测试：测试内起本地 HTTP 服务提供 HLS（分片取自 tests/fixtures/media），跑完整任务。
//! 输出与直接用 remux 合并同一批样本文件的结果逐字节比较，解密、顺序或分组的任何错误都会暴露。

mod discard;
mod live;
mod output;
mod server;
mod vod;

use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use hs_m3u8_core::{
    Engine, Error, JobRequest, Output, OutputOptions, RetryPolicy, Source, Target, Url,
};
use hs_m3u8_remux::{DiscontinuityGroup, Streams, TrackSegments, remux};

// ---------- 样本 ----------

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media")
}

fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(path)).unwrap()
}

/// 样本目录中的一条轨：`init` 为 init 段文件名，`segments` 为分片文件名。
fn track(dir: &str, init: Option<&str>, segments: &[&str]) -> TrackSegments {
    let dir = fixtures().join(dir);
    TrackSegments {
        init: init.map(|i| dir.join(i)),
        segments: segments.iter().map(|s| dir.join(s)).collect(),
    }
}

/// 直接合并样本文件得到的期望输出。
fn expected(dir: &Path, streams: &[Streams], groups: &[DiscontinuityGroup]) -> Vec<u8> {
    let path = dir.join("expected.mp4");
    remux(streams, groups, &path).unwrap();
    std::fs::read(path).unwrap()
}

/// ts_long 中的分片 `indices` 按 `groups` 切成不连续段组直接合并的期望输出；`groups` 为各组的分片数。
fn expected_long(dir: &Path, indices: &[u64], groups: &[usize]) -> Vec<u8> {
    let names: Vec<String> = indices.iter().map(|i| format!("seg{i}.ts")).collect();
    let mut rest: &[String] = &names;
    let groups: Vec<DiscontinuityGroup> = groups
        .iter()
        .map(|&n| {
            let (group, tail) = rest.split_at(n);
            rest = tail;
            let group: Vec<&str> = group.iter().map(String::as_str).collect();
            DiscontinuityGroup {
                tracks: vec![track("ts_long", None, &group)],
            }
        })
        .collect();
    assert!(rest.is_empty(), "groups 之和应等于分片数");
    expected(dir, &[Streams::All], &groups)
}

/// macOS 在 exFAT 等卷上生成的 AppleDouble 文件 `._<名字>` 的内容：开头为魔数与版本号。
fn apple_double() -> Vec<u8> {
    [[0, 5, 0x16, 7, 0, 2, 0, 0].as_slice(), b"Mac OS X        "].concat()
}

/// 每个测试独立的空目录。
fn test_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("e2e")
        .join(test);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------- 任务 ----------

fn request(url: Url, dir: &Path) -> JobRequest {
    let output = OutputOptions::new(Target::Mp4(dir.join("out.mp4")));
    let mut request = JobRequest::new(Source::new(url), output);
    request.source.http.retry = RetryPolicy {
        attempts: NonZeroU32::new(3).unwrap(),
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    request.concurrency = NonZeroUsize::new(1).unwrap();
    request
}

fn engine() -> Engine {
    Engine::new(NonZeroUsize::new(8).unwrap())
}

async fn run(request: JobRequest) -> Result<Output, Error> {
    engine().start(request)?.wait().await
}

/// 成功的任务：MP4 与期望逐字节相同，任务目录已删除。
fn assert_output(output: &Output, expected: &[u8]) {
    let mp4 = &output.mp4.as_ref().expect("应输出 MP4").path;
    assert_eq!(std::fs::read(mp4).unwrap(), expected);
    assert_eq!(output.leftovers, []);
    assert!(!mp4.with_extension("hsdl").exists());
}
