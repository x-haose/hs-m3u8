//! core 的端到端测试：测试内起本地 HTTP 服务提供 HLS（分片取自 tests/fixtures/media），跑完整任务。
//! 输出与直接用 remux 合并同一批样本文件的结果逐字节比较，解密、顺序或分组的任何错误都会暴露。

mod live;
mod server;
mod vod;

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hs_m3u8_core::{Engine, Error, JobRequest, Output, RetryPolicy, Url};
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
    let mut request = JobRequest::new(url, dir.join("out.mp4"));
    request.retry = RetryPolicy {
        attempts: 3,
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

/// 成功的任务：输出与期望逐字节相同，任务目录已删除。
fn assert_output(output: &Output, expected: &[u8]) {
    assert_eq!(std::fs::read(&output.path).unwrap(), expected);
    assert_eq!(output.cleanup_error, None);
    let mut work_dir = output.path.clone().into_os_string();
    work_dir.push(".hsdl");
    assert!(!Path::new(&work_dir).exists());
}
