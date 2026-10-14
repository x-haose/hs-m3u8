//! fMP4 分片的结构核对：没写完与最后一个 box 的长度写 0。

use hs_m3u8_remux::{Control, Error, Streams, TrackSegments, remux};

use crate::{declared, group, track, work_dir};

/// `data`（fMP4 分片）的顶层 box 依次排开，最后一个须为 `kind`，返回它的起点。只认 32 位长度的 box，样本都是这样。
fn last_box(data: &[u8], kind: &[u8; 4]) -> usize {
    let mut at = 0;
    loop {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        if at + size == data.len() {
            assert_eq!(&data[at + 4..at + 8], kind);
            return at;
        }
        at += size;
    }
}

/// 样本里 fMP4 分片的 trun 声明的样本数据恰好到分片末尾：mdat 的长度写 0 时少一个字节、或截在 mdat 之前，都看得出
/// 没写完。合并时同样核对：没写完（截在 mdat 里或 mdat 之前）、最后一个 box 的长度写 0 都报错，不越过文件边界把下一片
/// 读成这一片的样本。
#[test]
fn fmp4_structure_is_checked() {
    use hs_m3u8_remux::fmp4::{self, Problem};

    let dir = work_dir("fmp4_structure");
    for name in ["fmp4_a/video", "fmp4_a/audio", "fmp4_av1_opus", "fmp4_alac"] {
        let t = track(name);
        let init = std::fs::read(t.init.as_ref().unwrap()).unwrap();
        let trex = fmp4::trex_sample_sizes(&init).unwrap();
        assert_eq!(fmp4::check(&init, &trex), Ok(()), "{name}");
        let data = std::fs::read(&t.segments[0].path).unwrap();
        let mdat = last_box(&data, b"mdat");
        let mut open = data.clone();
        open[mdat..][..4].fill(0);
        let mut sealed = open.clone();
        assert_eq!(fmp4::seal(&mut sealed, &trex), Ok(()), "{name}");
        assert_eq!(sealed, data, "{name}");
        let incomplete = Err(Problem::Incomplete);
        assert_eq!(
            fmp4::check(&open[..open.len() - 1], &trex),
            incomplete,
            "{name}"
        );
        assert_eq!(fmp4::check(&data[..mdat], &trex), incomplete, "{name}");
    }

    let video = track("fmp4_a/video");
    let original = std::fs::read(&video.segments[0].path).unwrap();
    let mdat = last_box(&original, b"mdat");
    let mut open = original.clone();
    open[mdat..mdat + 4].fill(0);
    let truncated = original[..original.len() - 10].to_vec();
    let before_mdat = original[..mdat].to_vec();
    for (name, data) in [
        ("open", open),
        ("truncated", truncated),
        ("before_mdat", before_mdat),
    ] {
        let path = dir.join(format!("{name}.m4s"));
        std::fs::write(&path, data).unwrap();
        let segments = TrackSegments {
            init: video.init.clone(),
            segments: vec![
                declared(video.init.as_deref(), path.clone()),
                video.segments[1].clone(),
            ],
            gaps: Vec::new(),
        };
        let err = remux(
            &[Streams::Video],
            &[group([segments])],
            &dir.join("out.mp4"),
            &Control::default(),
        )
        .unwrap_err();
        let expected = match name {
            "open" => {
                matches!(&err, Error::Fmp4OpenEnded { path: p, at } if *p == path && *at == mdat as u64)
            }
            _ => {
                matches!(&err, Error::Fmp4 { path: p, problem: Problem::Incomplete } if *p == path)
            }
        };
        assert!(expected, "{name}：实际 {err:?}");
    }
}
