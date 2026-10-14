//! 合并的控制：写出进度与取消。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use hs_m3u8_remux::{Control, Error, Streams, WriteProgress, remux};

use crate::{counting_cancel, group, track, work_dir};

/// 写出的进度：每条轨的 init 段与分片随写出逐个报为写完（写到下一个分片的包时上一个才算完，一组写完时补齐），不在
/// 打开输入时成批报出；回读核对输出不计。
#[test]
fn write_progress_reports_each_input_file_as_it_is_written() {
    let dir = work_dir("write_progress");
    let output = dir.join("out.mp4");
    let cases = [
        (
            Streams::Video,
            vec![
                group([track("fmp4_a/video")]),
                group([track("fmp4_b/video")]),
            ],
        ),
        (Streams::All, vec![group([track("ts_long")])]),
        (Streams::Audio, vec![group([track("packed/aac")])]),
    ];
    for (streams, groups) in cases {
        let files: usize = groups
            .iter()
            .flat_map(|g| &g.tracks)
            .map(|t| t.segments.len() + usize::from(t.init.is_some()))
            .sum();
        let reported = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = reported.clone();
        let control = Control {
            progress: WriteProgress::new(move |n| recorded.lock().unwrap().push(n)),
            ..Control::default()
        };

        remux(&[streams], &groups, &output, &control).unwrap();

        assert_eq!(*reported.lock().unwrap(), vec![1; files]);
    }
}

/// 取消后合并在下一处检查以 Cancelled 停下：打开输入（含探测时读后面的数据）、写出每个包、FFmpeg 读写输出文件（含
/// moov 前置的第二遍）与回读核对，在哪一处检查时取消都是 Cancelled，不会报成别的错误。逐个检查点都试：音频晚 1.2 秒
/// 开始的 TS（探测要读到后面），与音视频分离的 fMP4。
#[test]
fn a_merge_cancelled_at_any_check_stops_as_cancelled() {
    let dir = work_dir("cancelled");
    let output = dir.join("out.mp4");
    let cases = [
        (vec![Streams::All], vec![group([track("ts_late_audio")])]),
        (
            vec![Streams::Video, Streams::Audio],
            vec![group([track("fmp4_a/video"), track("fmp4_a/audio")])],
        ),
    ];
    for (streams, groups) in &cases {
        let (cancel, checks) = counting_cancel(|_| false);
        remux(streams, groups, &output, &cancel).unwrap();
        let total = checks.load(Ordering::Relaxed);
        for from in 0..total {
            let (cancel, _) = counting_cancel(move |n| n >= from);
            let result = remux(streams, groups, &output, &cancel);
            assert!(
                matches!(result, Err(Error::Cancelled)),
                "第 {from} 次检查（共 {total} 次）起取消：{result:?}"
            );
        }
    }
}
