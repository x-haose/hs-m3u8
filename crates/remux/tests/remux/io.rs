//! 合并中途读写文件出错：分片变短、盘满。

use std::sync::atomic::Ordering;

use hs_m3u8_remux::{Control, Error, Segment, Streams, TrackSegments, remux};

use crate::{counting_cancel, group, track, work_dir};

/// 合并中途分片文件变短（在第 n 次检查时把第 2 片截掉一半）：报读文件出错，或在截短之前已读完这一片而照常合并出完整
/// 的输出；不会读了一半就当作读完，合并出缺内容的 MP4 而报成功。逐个检查点都试。
#[test]
fn a_segment_shrinking_during_the_merge_is_reported() {
    let dir = work_dir("shrinking");
    let output = dir.join("out.mp4");
    let source = track("ts_long");
    let complete = {
        let report = remux(
            &[Streams::All],
            &[group([source.clone()])],
            &output,
            &Control::default(),
        )
        .unwrap();
        report.streams.iter().map(|s| s.packets).collect::<Vec<_>>()
    };
    let (cancel, checks) = counting_cancel(|_| false);
    remux(
        &[Streams::All],
        &[group([source.clone()])],
        &output,
        &cancel,
    )
    .unwrap();
    let total = checks.load(Ordering::Relaxed);
    for at in 0..total {
        let copy = dir.join(format!("copy{at}"));
        std::fs::create_dir_all(&copy).unwrap();
        let segments: Vec<Segment> = source
            .segments
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let path = copy.join(format!("seg{i}.ts"));
                std::fs::copy(&s.path, &path).unwrap();
                Segment {
                    path,
                    duration_us: s.duration_us,
                }
            })
            .collect();
        let victim = segments[2].path.clone();
        // 截短在取消回调里做，不能 panic：结果记下来，合并结束后再断言
        let truncated = std::sync::Arc::new(std::sync::Mutex::new(None));
        let result = truncated.clone();
        let (cancel, _) = counting_cancel(move |n| {
            if n == at {
                let cut = std::fs::File::options()
                    .write(true)
                    .open(&victim)
                    .and_then(|file| file.set_len(file.metadata()?.len() / 2));
                *result.lock().unwrap_or_else(|e| e.into_inner()) = Some(cut);
            }
            false
        });
        let shrinking = TrackSegments {
            segments,
            ..source.clone()
        };
        let merged = remux(&[Streams::All], &[group([shrinking])], &output, &cancel);
        let cut = truncated.lock().unwrap().take();
        assert!(matches!(cut, Some(Ok(()))), "第 {at} 次检查时截短：{cut:?}");
        match merged {
            Ok(report) => assert_eq!(
                report.streams.iter().map(|s| s.packets).collect::<Vec<_>>(),
                complete,
                "第 {at} 次检查时截短：照常合并的应完整"
            ),
            Err(Error::Io { .. }) => {}
            Err(other) => panic!("第 {at} 次检查时截短：应报读文件出错，实际 {other:?}"),
        }
    }
}

/// 写输出时盘满（Linux 的 /dev/full 每次写都报 ENOSPC）：报写文件出错、带上 errno 对应的种类，不报成封装器的错误。
#[cfg(target_os = "linux")]
#[test]
fn a_full_disk_while_writing_is_reported_as_a_file_error() {
    let err = remux(
        &[Streams::All],
        &[group([track("ts_long")])],
        Path::new("/dev/full"),
        &Control::default(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::Io { action: "写入", cause, .. }
            if cause.kind() == std::io::ErrorKind::StorageFull),
        "实际 {err:?}"
    );
}
