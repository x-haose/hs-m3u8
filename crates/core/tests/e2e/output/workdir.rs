//! 写出之后删除任务目录：只删本库写的文件，系统生成的元数据文件不妨碍删除。

use crate::server::Server;
use crate::{apple_double, engine, fixture, request, run, test_dir};

/// 输出经符号链接落在任务目录里（按字面比较看不出）：删除任务目录时只删本库写的文件，输出留下，
/// 没删干净记在 leftovers 里；job.json 也留下，目录仍是任务目录，再运行（覆盖）照常进行。
/// AppleDouble 文件随主文件：输出与 job.json 的留下，锁文件的删掉。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn outputs_inside_the_work_dir_survive_its_removal() {
    use hs_m3u8_core::{LeftoverKind, Target};

    use super::request_to;

    let dir = test_dir("output_alias");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let work = dir.join("work");
    std::fs::create_dir(&work).unwrap();
    for name in ["._out.mp4", "._job.json", "._lock"] {
        std::fs::write(work.join(name), apple_double()).unwrap();
    }
    std::os::unix::fs::symlink(&work, dir.join("link")).unwrap();
    let target = Target::Mp4(dir.join("link/out.mp4"));
    let mut req = request_to(server.url("index.m3u8"), &dir, target);
    req.output.work_dir = Some(work.clone());

    let output = run(req.clone()).await.unwrap();

    assert!(work.join("out.mp4").is_file());
    let [leftover] = &output.leftovers[..] else {
        panic!("留有输出，任务目录应没删干净：{:?}", output.leftovers);
    };
    assert_eq!(
        (&leftover.path, &leftover.kind),
        (&work, &LeftoverKind::WorkDir)
    );
    assert!(leftover.cause.contains("out.mp4"), "{leftover}");
    for name in ["job.json", "._out.mp4", "._job.json"] {
        assert!(work.join(name).is_file(), "{name} 应留下");
    }
    for name in ["lock", "._lock", "tracks"] {
        assert!(!work.join(name).exists(), "{name} 应已删除");
    }

    req.output.overwrite = true;
    run(req).await.unwrap();
    assert!(work.join("out.mp4").is_file());
}

/// 删除任务目录时因为有别人的文件而留下了它与 job.json；之后同一输出换一个来源（没有已完成的分片，改为当前任务）：
/// 只删本库写的文件，别人的文件仍然留着。
#[tokio::test(flavor = "multi_thread")]
async fn files_kept_in_the_work_dir_survive_the_next_job() {
    let dir = test_dir("output_kept_in_tracks");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put("seg1.ts", fixture("ts_a/seg1.ts"));
    let media = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n";
    server.put("index.m3u8", media);
    server.put("other.m3u8", media);
    let req = request(server.url("index.m3u8"), &dir);
    let work = req.output.resolved_work_dir().unwrap();
    let notes = work.join("tracks/0/notes.txt");
    let gate = server.gate("seg1.ts");
    let job = engine().start(req).unwrap();
    gate.arrived.notified().await;
    std::fs::write(&notes, "别人的文件").unwrap();
    server.ungate("seg1.ts");
    job.wait().await.unwrap();
    assert!(work.join("job.json").is_file() && notes.is_file());

    let mut other = request(server.url("other.m3u8"), &dir);
    other.output.overwrite = true;
    run(other).await.unwrap();

    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "别人的文件");
}

/// 下载期间系统在任务目录里生成了元数据文件（在访达里打开过；在 exFAT 等卷上，macOS 为每个文件与目录生成
/// `._<名字>`，主文件不在的也算）：成功后照样删掉整个任务目录。
#[tokio::test(flavor = "multi_thread")]
async fn system_files_in_the_work_dir_do_not_block_its_removal() {
    let dir = test_dir("output_work_dir_ds_store");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put("seg1.ts", fixture("ts_a/seg1.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n",
    );
    let req = request(server.url("index.m3u8"), &dir);
    let work = req.output.resolved_work_dir().unwrap();
    let gate = server.gate("seg1.ts");
    let job = engine().start(req).unwrap();
    gate.arrived.notified().await;
    std::fs::write(work.join(".DS_Store"), "访达生成的").unwrap();
    std::fs::write(work.join("tracks/0/.DS_Store"), "访达生成的").unwrap();
    for name in [
        "._job.json",
        "._lock",
        "._tracks",
        "tracks/._0",
        "tracks/0/._gone.seg",
    ] {
        std::fs::write(work.join(name), apple_double()).unwrap();
    }
    server.ungate("seg1.ts");

    let output = job.wait().await.unwrap();

    assert_eq!(output.leftovers, []);
    assert!(!work.exists());
}
