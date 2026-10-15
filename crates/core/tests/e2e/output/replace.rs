//! 替换已有的输出：已有的 HLS 目录、下载期间被占用、并发的任务、删不掉的旧输出、中途被杀之后的恢复。

use axum::http::StatusCode;
use hs_m3u8_core::{Error, Target, WorkDirProblem};

use super::{assert_hls_matches, both, names_in, request_to};
use crate::server::Server;
#[cfg(unix)]
use crate::{Writable, is_root, make_writable};
use crate::{apple_double, engine, fixture, format_version, request, run, test_dir};

/// 已有的 HLS 目录：为空（系统自动生成的元数据文件不算）可以直接写；全是本库写出的文件时不覆盖就报已存在、
/// 覆盖即替换；里面有别的文件时覆盖也不替换，报另一种错误。元数据文件含 macOS 在 exFAT 等卷上为每个文件与目录
/// 生成的 `._<名字>`。
#[tokio::test(flavor = "multi_thread")]
async fn existing_hls_directories_are_replaced_only_when_written_by_the_library() {
    let dir = test_dir("output_overwrite");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let hls = dir.join("out");
    std::fs::create_dir(&hls).unwrap();
    std::fs::write(hls.join(".DS_Store"), "访达生成的").unwrap();
    let req = request_to(server.url("index.m3u8"), &dir, Target::Hls(hls.clone()));
    run(req.clone()).await.unwrap();

    match run(req.clone()).await {
        Err(Error::OutputExists(p)) => assert_eq!(p, hls),
        other => panic!("应报输出已存在：{other:?}"),
    }

    let mut replace = req;
    replace.output.overwrite = true;
    std::fs::write(hls.join("0/Thumbs.db"), "资源管理器生成的").unwrap();
    for name in ["._index.m3u8", "._0", "0/._0.ts"] {
        std::fs::write(hls.join(name), apple_double()).unwrap();
    }
    run(replace.clone()).await.unwrap();
    assert!(hls.join("0/0.ts").is_file());

    // 别人的文件，含名字像 AppleDouble 而内容不是的
    for name in ["0/._notes", "0/notes.txt"] {
        std::fs::write(hls.join(name), "别人的文件").unwrap();
        match run(replace.clone()).await {
            Err(Error::OutputOccupied {
                path,
                ancestor: None,
            }) => assert_eq!(path, hls),
            other => panic!("{name} 应算别人的文件：{other:?}"),
        }
        assert!(hls.join(name).is_file());
        std::fs::remove_file(hls.join(name)).unwrap();
    }
}

/// 两者都要时，HLS 目录在下载期间被别人放进了文件：写出前的检查发现后报错，MP4 与 HLS 都没有写出、
/// 也不留临时文件，任务目录保留；腾出来后用同样的请求再运行，分片都已下完、只差合并，不核对也不重下，分片地址已经
/// 过期也照常合并。换一个地址则照常核对。
#[tokio::test(flavor = "multi_thread")]
async fn outputs_taken_during_the_download_leave_nothing_behind() {
    let dir = test_dir("output_taken");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let req = request_to(server.url("index.m3u8"), &dir, both(&dir));
    let gate = server.gate("seg0.ts");
    let job = engine().start(req.clone()).unwrap();
    gate.arrived.notified().await;
    std::fs::create_dir(dir.join("out")).unwrap();
    std::fs::write(dir.join("out/notes.txt"), "别人的文件").unwrap();
    server.ungate("seg0.ts");

    match job.wait().await {
        Err(Error::OutputOccupied {
            path,
            ancestor: None,
        }) => assert_eq!(path, dir.join("out")),
        other => panic!("应报有别人的文件：{other:?}"),
    }
    assert_eq!(names_in(&dir), ["out", "out.hsdl"]);

    std::fs::remove_dir_all(dir.join("out")).unwrap();
    // 另一个地址列出的分片名、时长都相同，内容不同：分片虽已下完也要核对，不把上一个视频当成它输出
    server.put("b/seg0.ts", fixture("ts_b/seg0.ts"));
    server.put(
        "b/index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let other = request_to(server.url("b/index.m3u8"), &dir, both(&dir));
    match run(other).await {
        Err(Error::WorkDir {
            problem: WorkDirProblem::SourceUnverified,
            ..
        }) => {}
        other => panic!("应报核对不上：{other:?}"),
    }

    server.status("seg0.ts", axum::http::StatusCode::FORBIDDEN);
    let output = run(req).await.unwrap();
    assert_hls_matches(&dir.join("out"), &output.mp4.unwrap().path);
    assert_eq!(server.hits("seg0.ts"), 1);
}

/// 两个任务（任务目录不同）同时输出到同一个 HLS 目录：各在自己的准备目录里备齐，先改名的成功，另一个报已存在；
/// 输出只含成功一方的分片，两个任务目录里的分片都还是各自的内容。
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_jobs_to_one_hls_directory_do_not_mix() {
    let dir = test_dir("output_concurrent");
    let server = Server::start().await;
    let sources = [("a", [0, 1]), ("b", [2, 3])];
    for (name, indices) in sources {
        let mut playlist = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:1\n");
        for (n, i) in indices.iter().enumerate() {
            server.put(
                &format!("{name}/seg{n}.ts"),
                fixture(&format!("ts_long/seg{i}.ts")),
            );
            playlist += &format!("#EXTINF:1,\nseg{n}.ts\n");
        }
        server.put(&format!("{name}/index.m3u8"), playlist + "#EXT-X-ENDLIST\n");
    }
    let hls = dir.join("out");
    let mut jobs = Vec::new();
    for (name, _) in sources {
        let url = server.url(&format!("{name}/index.m3u8"));
        let mut req = request_to(url, &dir, Target::Hls(hls.clone()));
        req.output.work_dir = Some(dir.join(format!("{name}.hsdl")));
        req.output.keep_work_dir = true;
        let gate = server.gate(&format!("{name}/seg1.ts"));
        jobs.push(engine().start(req).unwrap());
        gate.arrived.notified().await;
    }
    for (name, _) in sources {
        server.ungate(&format!("{name}/seg1.ts"));
    }
    let mut won = Vec::new();
    for ((name, indices), job) in sources.into_iter().zip(jobs) {
        match job.wait().await {
            Ok(_) => won.push(indices),
            Err(Error::OutputExists(path)) => assert_eq!(path, hls),
            Err(other) => panic!("{name}：{other}"),
        }
        // 任务目录里的分片仍是这个任务自己的内容
        let mut stored: Vec<Vec<u8>> = std::fs::read_dir(dir.join(format!("{name}.hsdl/tracks/0")))
            .unwrap()
            .map(|e| std::fs::read(e.unwrap().path()).unwrap())
            .collect();
        stored.sort();
        let mut own = indices
            .map(|i| fixture(&format!("ts_long/seg{i}.ts")))
            .to_vec();
        own.sort();
        assert_eq!(stored, own, "{name} 的任务目录");
    }
    let [indices] = won[..] else {
        panic!("应恰好一个成功：{won:?}");
    };
    for (n, i) in indices.iter().enumerate() {
        let written = std::fs::read(hls.join(format!("0/{n}.ts"))).unwrap();
        assert_eq!(written, fixture(&format!("ts_long/seg{i}.ts")));
    }
}

/// 要求覆盖、两者都要，旧的 HLS 目录里有删不掉的内容：新的 MP4 与 HLS 照常换上，旧的挪开后删不掉，记在
/// leftovers 里，任务目录里的记录留着（已换好）。之后再运行时，任务开头、联网之前先删这个残留，删不掉就报它的路径；
/// 能删了即照常运行，新输出被挪走了也不会把它放回去。以 root 运行时权限位不起作用，造不出删不掉的情形，不测。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn old_outputs_that_cannot_be_removed_are_reported_and_removed_later() {
    use std::os::unix::fs::PermissionsExt;

    use hs_m3u8_core::LeftoverKind;

    let dir = test_dir("output_old_kept");
    if is_root(&dir) {
        return;
    }
    let _writable = Writable(dir.clone());
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let mut req = request_to(server.url("index.m3u8"), &dir, both(&dir));
    run(req.clone()).await.unwrap();
    let index = dir.join("out/index.m3u8");
    let marked = std::fs::read_to_string(&index).unwrap() + "#旧的\n";
    std::fs::write(&index, marked).unwrap();
    std::fs::set_permissions(dir.join("out/0"), std::fs::Permissions::from_mode(0o555)).unwrap();

    req.output.overwrite = true;
    req.output.keep_work_dir = true;
    let output = run(req.clone()).await.unwrap();

    let [leftover] = &output.leftovers[..] else {
        panic!("旧的 HLS 应删不掉：{:?}", output.leftovers);
    };
    assert_eq!(leftover.kind, LeftoverKind::Removable);
    let old = leftover.path.clone();
    assert!(old.to_string_lossy().ends_with(".hls.old"), "{leftover}");
    assert_hls_matches(&dir.join("out"), &output.mp4.unwrap().path);
    assert!(dir.join("out.hsdl/outputs.json").exists());

    let hits = server.hits("index.m3u8");
    match run(req.clone()).await {
        Err(Error::Io { path, .. }) => assert_eq!(path, old),
        other => panic!("应报删不掉的残留：{other:?}"),
    }
    assert_eq!(server.hits("index.m3u8"), hits, "应在联网之前");

    // 新输出被挪走后不要求覆盖再运行：旧的只删不放回
    make_writable(&old);
    std::fs::rename(dir.join("out"), dir.join("moved")).unwrap();
    std::fs::rename(dir.join("out.mp4"), dir.join("moved.mp4")).unwrap();
    req.output.overwrite = false;
    run(req).await.unwrap();
    assert!(!old.exists());
    assert!(!std::fs::read_to_string(&index).unwrap().contains("#旧的"));
}

/// 上次在换上输出的中途被杀（旧输出已挪开、新的还没装上）：再次运行时在任务开头、联网之前按任务目录里的记录
/// 删掉临时输出、把旧输出放回，不要求覆盖时随即报已存在。
#[tokio::test(flavor = "multi_thread")]
async fn interrupted_swaps_are_recovered_before_anything_else() {
    let dir = test_dir("output_recover");
    let server = Server::start().await;
    server.status("seg0.ts", StatusCode::NOT_FOUND);
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let req = request(server.url("index.m3u8"), &dir);
    // 分片取不到而失败，留下任务目录
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
        "format_version": format_version(&work),
        "swapped": false,
        "outputs": [{"kind": "mp4", "target": target, "fingerprint": "0123456789abcdef"}],
    });
    std::fs::write(work.join("outputs.json"), record.to_string()).unwrap();
    let hits = server.hits("index.m3u8");

    match run(req).await {
        Err(Error::OutputExists(path)) => assert_eq!(path, target),
        other => panic!("应报已存在：{other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "旧");
    assert_eq!(names_in(&dir), ["out.hsdl", "out.mp4"]);
    assert!(!work.join("outputs.json").exists());
    assert_eq!(server.hits("index.m3u8"), hits, "应在联网之前");
}
