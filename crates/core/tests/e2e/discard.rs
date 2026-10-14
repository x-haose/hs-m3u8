//! 放弃任务：按记录收拾写到一半的输出，只删本库写的文件；收拾不了的按调用方的选择报错或记为残留；正被使用的、
//! 不是本库建立的任务目录不动。

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use hs_m3u8_core::{Error, LeftoverKind, Unsettled, WorkDirProblem};

use crate::server::Server;
#[cfg(unix)]
use crate::{Writable, is_root};
use crate::{engine, fixture, request, run, test_dir};

const TWO_SEGMENTS: &str =
    "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n";

/// 记录里的任务目录指纹；保留名由输出路径与它定下。
const FINGERPRINT: &str = "0123456789abcdef";

/// 下载失败留下的任务目录（输出为 `dir` 下的 out.mp4）。
async fn failed_job(dir: &Path) -> PathBuf {
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.status("seg1.ts", StatusCode::NOT_FOUND);
    server.put("index.m3u8", TWO_SEGMENTS);
    let req = request(server.url("index.m3u8"), dir);
    assert!(matches!(run(req.clone()).await, Err(Error::Segment { .. })));
    req.output.resolved_work_dir().unwrap()
}

/// 在任务目录 `work` 里记下 MP4 输出 `target` 换上没做完，返回它的临时名与挪开的名字。
fn record_unfinished(work: &Path, target: &Path) -> (PathBuf, PathBuf) {
    let record = serde_json::json!({
        "format_version": 10,
        "swapped": false,
        "outputs": [{"kind": "mp4", "target": target, "fingerprint": FINGERPRINT}],
    });
    std::fs::write(work.join("outputs.json"), record.to_string()).unwrap();
    let name = |suffix| target.with_file_name(format!("hsdl-{FINGERPRINT}.mp4.{suffix}"));
    (name("part"), name("old"))
}

/// 下载失败留下的任务目录，里面还记着上次换上输出时没能放回的旧输出与没删掉的临时输出，另有别人的文件：放弃时
/// 旧输出放回原处、临时输出删掉，本库写的文件删掉，别人的文件（与 job.json）留下并报出来。
#[tokio::test(flavor = "multi_thread")]
async fn discarding_restores_outputs_and_removes_only_library_files() {
    let dir = test_dir("discard");
    let work = failed_job(&dir).await;
    let target = dir.join("out.mp4");
    let (temp, aside) = record_unfinished(&work, &target);
    std::fs::write(&temp, "写了一半").unwrap();
    std::fs::write(&aside, "旧").unwrap();
    std::fs::write(work.join("notes.txt"), "别人的文件").unwrap();

    let leftovers = engine().discard(&work, Unsettled::Fail).await.unwrap();

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

/// 收拾不了记录里的输出（这里是输出所在的目录没有写权限，临时输出删不掉）：从严时报错，任务目录与记录留着；记为
/// 残留时两个保留名各报一项（换上没做完，旧输出是没能放回原处的），照常删掉任务目录，保留名上的东西不动。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn outputs_that_cannot_be_settled_fail_or_are_reported_as_asked() {
    use std::os::unix::fs::PermissionsExt;

    let dir = test_dir("discard_unsettled");
    if is_root(&dir) {
        return;
    }
    let _writable = Writable(dir.clone());
    let work = failed_job(&dir).await;
    let locked = dir.join("locked");
    std::fs::create_dir(&locked).unwrap();
    let target = locked.join("a.mp4");
    let (temp, aside) = record_unfinished(&work, &target);
    std::fs::write(&temp, "写了一半").unwrap();
    std::fs::write(&aside, "旧").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

    match engine().discard(&work, Unsettled::Fail).await {
        Err(Error::Io { path, .. }) => assert_eq!(path, temp),
        other => panic!("应报删不掉临时输出：{other:?}"),
    }
    assert!(work.join("outputs.json").is_file());

    let leftovers = engine().discard(&work, Unsettled::Report).await.unwrap();
    let kinds: Vec<_> = leftovers.iter().map(|l| (&l.path, &l.kind)).collect();
    let displaced = LeftoverKind::Displaced { target };
    assert_eq!(
        kinds,
        [(&temp, &LeftoverKind::Removable), (&aside, &displaced)]
    );
    assert!(!work.exists());
    assert!(temp.is_file() && aside.is_file());
}

/// 盘不在。任务目录所在的盘不在：从严时报读不到，记为残留时报任务目录没删干净。记录里的输出所在的盘不在：从严时
/// 报读不到、记录留着，记为残留时报它的两个保留名，照常删掉任务目录。
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn missing_drives_fail_or_are_reported_as_asked() {
    let drive = ('D'..='Z')
        .rev()
        .map(|letter| PathBuf::from(format!("{letter}:\\")))
        .find(|drive| !drive.exists())
        .expect("D 到 Z 里至少有一个没用的盘符");
    let assert_unavailable = |result: Result<Vec<_>, Error>| match result {
        Err(Error::VolumeUnavailable { root, .. }) => assert_eq!(root, drive),
        other => panic!("应报读不到 {}：{other:?}", drive.display()),
    };

    let missing_work = drive.join("jobs").join("a");
    assert_unavailable(engine().discard(&missing_work, Unsettled::Fail).await);
    let leftovers = engine()
        .discard(&missing_work, Unsettled::Report)
        .await
        .unwrap();
    let [leftover] = &leftovers[..] else {
        panic!("应报任务目录没删干净：{leftovers:?}");
    };
    assert_eq!(
        (&leftover.path, &leftover.kind),
        (&missing_work, &LeftoverKind::WorkDir)
    );

    let work = failed_job(&test_dir("discard_missing_drive")).await;
    let target = drive.join("movies").join("a.mp4");
    let (temp, aside) = record_unfinished(&work, &target);
    assert_unavailable(engine().discard(&work, Unsettled::Fail).await);
    assert!(work.join("outputs.json").is_file());

    let leftovers = engine().discard(&work, Unsettled::Report).await.unwrap();
    let kinds: Vec<_> = leftovers.iter().map(|l| (&l.path, &l.kind)).collect();
    let displaced = LeftoverKind::Displaced { target };
    assert_eq!(
        kinds,
        [(&temp, &LeftoverKind::Removable), (&aside, &displaced)]
    );
    assert!(!work.exists());
}

/// 目录不存在时什么也不做；不是本库建立的任务目录不动；正被任务使用时报被占用。两种取值都一样。
#[tokio::test(flavor = "multi_thread")]
async fn discarding_leaves_directories_in_use_or_not_from_the_library() {
    let dir = test_dir("discard_refused");
    let other = dir.join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("notes.txt"), "别人的文件").unwrap();
    for unsettled in [Unsettled::Fail, Unsettled::Report] {
        let missing = engine().discard(&dir.join("missing"), unsettled).await;
        assert_eq!(missing.unwrap(), []);
        match engine().discard(&other, unsettled).await {
            Err(Error::WorkDir {
                problem: WorkDirProblem::NotEmpty,
                ..
            }) => {}
            other => panic!("应报不是任务目录：{other:?}"),
        }
        assert!(other.join("notes.txt").is_file());
    }

    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put("seg1.ts", fixture("ts_a/seg1.ts"));
    server.put("index.m3u8", TWO_SEGMENTS);
    let req = request(server.url("index.m3u8"), &dir);
    let work = req.output.resolved_work_dir().unwrap();
    let gate = server.gate("seg1.ts");
    let job = engine().start(req).unwrap();
    gate.arrived.notified().await;
    for unsettled in [Unsettled::Fail, Unsettled::Report] {
        match engine().discard(&work, unsettled).await {
            Err(Error::WorkDir {
                problem: WorkDirProblem::Locked,
                ..
            }) => {}
            other => panic!("应报被占用：{other:?}"),
        }
    }
    server.ungate("seg1.ts");
    job.wait().await.unwrap();
}
