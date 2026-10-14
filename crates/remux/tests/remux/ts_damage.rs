//! TS 里损坏的音频：缺片处拼坏的包丢弃并计数，标了损坏而数据完好的照常写入。

use hs_m3u8_remux::{Control, Segment, Streams, TrackSegments, remux};

use crate::{concat_track, group, timelines, track, work_dir};

/// 缺了一片、音频 PES 跨在缺片处的 TS：只丢缺片处拼坏的音频包并计数，其余照常合并；视频不丢。
#[test]
fn audio_damaged_at_a_missing_segment_is_dropped_and_counted() {
    let dir = work_dir("damaged_audio");
    let output = dir.join("out.mp4");
    let mut holed = track("ts_cut");
    holed.segments.remove(2);
    holed.gaps = vec![2];
    let (v_in, a_in) = timelines(&concat_track(&holed, &dir, "in.ts"));

    let report = remux(
        &[Streams::All],
        &[group([holed])],
        &output,
        &Control::default(),
    )
    .unwrap();

    let [video, audio] = &report.streams[..] else {
        panic!("应有视频与音频两路流：{report:?}");
    };
    assert_eq!(
        (video.packets, video.discarded_corrupt_bytes),
        (v_in.len() as u64, 0)
    );
    // 缺片处前后两段拼成的只有一两个包；解复用器给整个 PES 打的损坏标志不作数
    let lost = a_in.len() as u64 - audio.packets;
    assert!(
        (1..=2).contains(&lost) && audio.discarded_corrupt_bytes > 0,
        "{audio:?}"
    );
}

/// 改写 TS 的音频（PID 0x101）：PES 长度写 0（不定长），连续计数器从 0 重新编号；`overstate` 为真时 PES 长度改为多报
/// 4 字节。数据都在：逐片独立生成的切片器常这样写，解复用器却给这些 PES 打上损坏标志。
fn rewrite_audio_pes(data: &[u8], overstate: bool) -> Vec<u8> {
    let mut out = data.to_vec();
    let mut cc = 0;
    for packet in out.chunks_mut(188) {
        let pid = u16::from(packet[1] & 0x1F) << 8 | u16::from(packet[2]);
        if pid != 0x101 || packet[3] & 0x10 == 0 {
            continue;
        }
        if !overstate {
            packet[3] = packet[3] & 0xF0 | cc;
            cc = (cc + 1) & 0x0F;
        }
        if packet[1] & 0x40 != 0 {
            // 有效载荷起点（跳过调整字段）：00 00 01、stream_id、PES_packet_length
            let start = if packet[3] & 0x20 != 0 {
                5 + usize::from(packet[4])
            } else {
                4
            };
            let len = u16::from_be_bytes([packet[start + 4], packet[start + 5]]);
            let len = if overstate { len + 4 } else { 0 };
            packet[start + 4..start + 6].copy_from_slice(&len.to_be_bytes());
        }
    }
    out
}

/// 音频 PES 不定长且各片的连续计数器从 0 重来、PES 长度多报几个字节：解复用器给整个 PES 打损坏标志，帧都完好，照常
/// 全部写入。
#[test]
fn audio_in_pes_flagged_corrupt_but_intact_is_kept() {
    let dir = work_dir("flagged_audio");
    let output = dir.join("out.mp4");
    let source = track("ts_long");
    let a_in = timelines(&concat_track(&source, &dir, "in.ts")).1;
    for overstate in [false, true] {
        let segments = source
            .segments
            .iter()
            .enumerate()
            .map(|(i, segment)| {
                let rewritten = dir.join(format!("{overstate}-{i}.ts"));
                let data = rewrite_audio_pes(&std::fs::read(&segment.path).unwrap(), overstate);
                std::fs::write(&rewritten, data).unwrap();
                Segment {
                    path: rewritten,
                    duration_us: segment.duration_us,
                }
            })
            .collect();
        let rewritten = TrackSegments {
            init: None,
            segments,
            gaps: Vec::new(),
        };

        let report = remux(
            &[Streams::All],
            &[group([rewritten])],
            &output,
            &Control::default(),
        )
        .unwrap();

        let audio = &report.streams[1];
        assert_eq!(
            (audio.packets, audio.discarded_corrupt_bytes),
            (a_in.len() as u64, 0),
            "多报长度 {overstate}"
        );
    }
}
