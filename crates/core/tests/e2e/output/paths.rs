//! 输出路径：下载前拒绝不合法的与被占用的、输出所在的目录的重建与建不出、很长的输出名。

use hs_m3u8_core::{Error, Target, Url};

use super::{both, request_to};
use crate::server::Server;
#[cfg(unix)]
use crate::{Writable, is_root};
use crate::{engine, fixture, run, test_dir};

/// 输出与任务目录的路径相同或互相包含、MP4 路径以分隔符结尾、路径以 `.` 结尾时，下载前即拒绝。
#[tokio::test(flavor = "multi_thread")]
async fn invalid_output_paths_are_rejected() {
    let dir = test_dir("output_paths");
    let url = Url::parse("http://127.0.0.1:9/index.m3u8").unwrap();
    let cases = [
        (both(&dir), Some(dir.join("out"))),
        (Target::Hls(dir.join("work/hls")), Some(dir.join("work"))),
        // 按字面规整 `..` 之后才看得出在任务目录里
        (
            Target::Mp4(dir.join("x/../work/out.mp4")),
            Some(dir.join("work")),
        ),
        (
            Target::Both {
                mp4: dir.join("out/a.mp4"),
                hls: dir.join("out"),
            },
            None,
        ),
        (Target::Mp4(dir.join("out.mp4/")), None),
        (Target::Mp4(dir.join("out.mp4/.")), None),
        (Target::Hls(dir.join("out/.")), None),
        (Target::Hls(dir.join("out")), Some(dir.join("work/."))),
    ];
    for (target, work_dir) in cases {
        let mut req = request_to(url.clone(), &dir, target.clone());
        req.output.work_dir = work_dir;
        match run(req).await {
            Err(Error::InvalidInput(_)) => {}
            other => panic!("{target:?} 应被拒绝：{other:?}"),
        }
    }
}

/// 输出路径的上一级是文件（报错带上它），或 HLS 路径以分隔符结尾而那里是文件：任务开头检查输出时报路径被占用，
/// 指向输出路径本身，不是内部的临时名。
#[tokio::test(flavor = "multi_thread")]
async fn an_output_under_a_file_is_reported_as_occupied() {
    let dir = test_dir("output_under_file");
    std::fs::write(dir.join("file"), "别人的文件").unwrap();
    let url = Url::parse("http://127.0.0.1:9/index.m3u8").unwrap();
    let file = dir.join("file");
    let mp4 = file.join("out.mp4");
    // 结尾带本平台的分隔符：补全后的路径用的也是它，按字节比较才看得出结尾的分隔符去掉了
    let mut hls = file.clone().into_os_string();
    hls.push(std::path::MAIN_SEPARATOR_STR);
    let work = Some(dir.join("work"));
    // 默认的任务目录与输出同级，同样建不出来：报的仍是输出路径被占用
    let cases = [
        (
            Target::Mp4(mp4.clone()),
            work.clone(),
            mp4.clone(),
            Some(file.clone()),
        ),
        (Target::Mp4(mp4.clone()), None, mp4, Some(file.clone())),
        (Target::Hls(hls.into()), work, file, None),
    ];
    for (target, work_dir, occupied, blocking) in cases {
        let mut req = request_to(url.clone(), &dir, target);
        req.output.work_dir = work_dir;

        match run(req).await {
            Err(Error::OutputOccupied { path, ancestor }) => {
                assert_eq!(path.as_os_str(), occupied.as_os_str());
                assert_eq!(ancestor, blocking);
            }
            other => panic!("应报路径被占用：{other:?}"),
        }
    }
}

/// 下载期间输出所在的目录被移走或删掉：写出前重建，MP4 与 HLS 照常写出。
#[tokio::test(flavor = "multi_thread")]
async fn output_directories_removed_during_the_download_are_recreated() {
    let dir = test_dir("output_dir_removed");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let outdir = dir.join("outdir");
    let target = Target::Both {
        mp4: outdir.join("a.mp4"),
        hls: outdir.join("a"),
    };
    let mut req = request_to(server.url("index.m3u8"), &dir, target);
    req.output.work_dir = Some(dir.join("work"));
    let gate = server.gate("seg0.ts");
    let job = engine().start(req).unwrap();
    gate.arrived.notified().await;
    std::fs::remove_dir_all(&outdir).unwrap();
    server.ungate("seg0.ts");

    let output = job.wait().await.unwrap();

    assert!(output.mp4.unwrap().path.is_file());
    assert!(output.hls.unwrap().join("index.m3u8").is_file());
}

/// 输出所在的目录建不出来（这里是没有写权限）：任务开头、联网之前就报出，不等下载完。以 root 运行时权限位不起
/// 作用，不测。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn output_directories_that_cannot_be_created_fail_before_downloading() {
    use std::os::unix::fs::PermissionsExt;

    let dir = test_dir("output_dir_denied");
    if is_root(&dir) {
        return;
    }
    let _writable = Writable(dir.clone());
    let locked = dir.join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let server = Server::start().await;
    let target = Target::Mp4(locked.join("sub/out.mp4"));
    let mut req = request_to(server.url("index.m3u8"), &dir, target);
    req.output.work_dir = Some(dir.join("work"));

    match run(req).await {
        Err(Error::Io { action, path, .. }) => {
            assert_eq!((action, path), ("创建", locked.join("sub")))
        }
        other => panic!("应报建不出输出所在的目录：{other:?}"),
    }
    assert_eq!(server.hits("index.m3u8"), 0);
}

/// 输出名很长（238 字节，在文件名上限 255 字节之内）：临时名定长，不因输出名变长而超出上限。
#[tokio::test(flavor = "multi_thread")]
async fn long_output_names_are_written() {
    let dir = test_dir("output_long_name");
    let server = Server::start().await;
    server.put("seg0.ts", fixture("ts_a/seg0.ts"));
    server.put(
        "index.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
    );
    let name = format!("{}.mp4", "长".repeat(78));
    assert_eq!(name.len(), 238);
    let target = Target::Mp4(dir.join(&name));

    let output = run(request_to(server.url("index.m3u8"), &dir, target))
        .await
        .unwrap();

    assert!(output.mp4.unwrap().path.is_file());
}
