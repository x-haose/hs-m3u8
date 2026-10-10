//! 放弃任务：按记录收拾写到一半的输出，只删本库写的文件；正被使用的、不是本库建立的任务目录不动。

use axum::http::StatusCode;
use hs_m3u8_core::{Error, LeftoverKind, WorkDirProblem};

use crate::server::Server;
use crate::{engine, fixture, request, run, test_dir};

const TWO_SEGMENTS: &str =
    "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n";

/// 下载失败留下的任务目录，里面还记着上次换上输出时没能放回的旧输出与没删掉的临时输出，另有别人的文件：放弃时
/// 旧输出放回原处、临时输出删掉，本库写的文件删掉，别人的文件（与 job.json）留下并报出来。
#[tokio::test(flavor = "multi_thread")]
async fn discarding_restores_outputs_and_removes_only_library_files() {
    let dir = test_dir("discard");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.status("seg1.ts", StatusCode::NOT_FOUND);
    server.put("index.m3u8", TWO_SEGMENTS);
    let req = request(server.url("index.m3u8"), &dir);
    assert!(matches!(run(req.clone()).await, Err(Error::Segment { .. })));
    let work = req.output.resolved_work_dir().unwrap();
    // 保留名由输出路径与记录里的指纹定下
    let (target, temp, aside) = (
        dir.join("out.mp4"),
        dir.join("hsdl-0123456789abcdef.mp4.part"),
        dir.join("hsdl-0123456789abcdef.mp4.old"),
    );
    std::fs::write(&temp, "写了一半").unwrap();
    std::fs::write(&aside, "旧").unwrap();
    let record = serde_json::json!({
        "format_version": 7,
        "swapped": false,
        "outputs": [{"kind": "mp4", "target": target, "fingerprint": "0123456789abcdef"}],
    });
    std::fs::write(work.join("outputs.json"), record.to_string()).unwrap();
    std::fs::write(work.join("notes.txt"), "别人的文件").unwrap();

    let leftovers = engine().discard(&work).await.unwrap();

    let [leftover] = &leftovers[..] else {
        panic!("应留下别人的文件：{leftovers:?}");
    };
    assert_eq!(
        (&leftover.path, &leftover.kind),
        (&work, &LeftoverKind::WorkDir)
    );
    assert!(leftover.cause.contains("notes.txt"), "{leftover}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "旧");
    assert!(!temp.exists() && !aside.exists());
    let mut left: Vec<String> = std::fs::read_dir(&work)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, ["job.json", "notes.txt"]);
}

/// 目录不存在时什么也不做；不是本库建立的任务目录不动；正被任务使用时报被占用。
#[tokio::test(flavor = "multi_thread")]
async fn discarding_leaves_directories_in_use_or_not_from_the_library() {
    let dir = test_dir("discard_refused");
    assert_eq!(engine().discard(&dir.join("missing")).await.unwrap(), []);

    let other = dir.join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("notes.txt"), "别人的文件").unwrap();
    match engine().discard(&other).await {
        Err(Error::WorkDir {
            problem: WorkDirProblem::NotEmpty,
            ..
        }) => {}
        other => panic!("应报不是任务目录：{other:?}"),
    }
    assert!(other.join("notes.txt").is_file());

    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put("seg1.ts", fixture("ts_a/seg1.ts"));
    server.put("index.m3u8", TWO_SEGMENTS);
    let req = request(server.url("index.m3u8"), &dir);
    let work = req.output.resolved_work_dir().unwrap();
    let gate = server.gate("seg1.ts");
    let job = engine().start(req).unwrap();
    gate.arrived.notified().await;
    match engine().discard(&work).await {
        Err(Error::WorkDir {
            problem: WorkDirProblem::Locked,
            ..
        }) => {}
        other => panic!("应报被占用：{other:?}"),
    }
    server.ungate("seg1.ts");
    job.wait().await.unwrap();
}
