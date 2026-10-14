//! 打包音频：按 ID3 时间戳定时，跳过标签与损坏处；有分片没有时间戳时只能单独成轨。

use std::path::PathBuf;

use hs_m3u8_remux::{Control, Error, Segment, Streams, TrackSegments, Unsupported, remux};

use crate::{
    concat_track, earliest, first_frames, group, packed_fixture, packed_segment, packet_sizes,
    segs, span, timelines, track, work_dir,
};

/// 打包音频按各分片 ID3 里的时间戳定时：与 TS 视频的相对时刻和输入一致，缺失的分片处留出空档。
#[test]
fn packed_audio_follows_its_id3_timestamps() {
    let dir = work_dir("packed_timestamps");
    let output = dir.join("out.mp4");
    let video = track("ts_a");
    let v_in = timelines(&concat_track(&video, &dir, "v.ts")).0;
    // 音频比视频晚 0.1 秒开始，第二个分片缺失
    let start = ((earliest(&v_in) + 0.1) * 90_000.0).round() as u64;
    let segments = packed_fixture("aac");
    let audio = TrackSegments {
        init: None,
        segments: segs([
            packed_segment(&dir, &segments[0].0, Some(start)),
            packed_segment(
                &dir,
                &segments[2].0,
                Some(start + segments[0].1 + segments[1].1),
            ),
        ]),
        gaps: vec![1],
    };

    remux(
        &[Streams::Video, Streams::Audio],
        &[group([video, audio])],
        &output,
        &Control::default(),
    )
    .unwrap();

    let (v_out, a_out) = timelines(&output);
    let lead = a_out[0].0 - earliest(&v_out);
    assert!(
        (lead - 0.1).abs() < 0.001,
        "音频应比视频晚 0.1 秒开始，实际 {lead}"
    );
    let first_part = timelines(&segments[0].0).1.len();
    let gap = a_out[first_part].0 - a_out[0].0;
    let expected = (segments[0].1 + segments[1].1) as f64 / 90_000.0;
    assert!(
        (gap - expected).abs() < 0.001,
        "第三个分片应从 {expected} 秒处开始，实际 {gap}"
    );
}

/// 打包音频的时间戳越过 33 位的回绕点时接着往后排，不当作往回跳。
#[test]
fn packed_audio_timestamps_wrap_around() {
    let dir = work_dir("packed_wrap");
    let output = dir.join("out.mp4");
    let segments = packed_fixture("aac");
    let wrap = 1u64 << 33;
    let audio = TrackSegments {
        init: None,
        segments: segs([
            packed_segment(&dir, &segments[0].0, Some(wrap - segments[0].1)),
            packed_segment(&dir, &segments[1].0, Some(0)),
        ]),
        gaps: Vec::new(),
    };

    let report = remux(
        &[Streams::Audio],
        &[group([audio])],
        &output,
        &Control::default(),
    )
    .unwrap();

    let expected = (segments[0].1 + segments[1].1) as f64 / 90_000.0;
    let duration = report.streams[0].duration.as_secs_f64();
    assert!(
        (duration - expected).abs() < 0.001,
        "应连续 {expected} 秒，实际 {duration}"
    );
}

/// 打包音频的末片只有一两帧：整条轨作为一个输入读，格式只在开头识别，短片照常读出，各帧都带时间戳。
#[test]
fn packed_audio_with_very_short_segments_is_read() {
    let dir = work_dir("packed_short");
    let output = dir.join("out.mp4");
    for name in ["aac", "ac3", "eac3", "mp3"] {
        let segments = packed_fixture(name);
        let first = &segments[0];
        let full: usize = timelines(&first.0).1.len();
        for frames in [1, 2] {
            let tail = dir.join(format!("{name}-tail{frames}"));
            std::fs::write(&tail, first_frames(&segments[1].0, frames)).unwrap();
            let audio = TrackSegments {
                init: None,
                segments: segs([
                    packed_segment(&dir, &first.0, Some(900_000)),
                    packed_segment(&dir, &tail, Some(900_000 + first.1)),
                ]),
                gaps: Vec::new(),
            };

            let report = remux(
                &[Streams::Audio],
                &[group([audio])],
                &output,
                &Control::default(),
            )
            .unwrap();

            let stream = &report.streams[0];
            assert_eq!(
                (stream.packets, stream.skipped_without_dts),
                ((full + frames) as u64, 0),
                "{name} 末片 {frames} 帧"
            );
            let a_out = timelines(&output).1;
            let tail_start = a_out[full].0 - a_out[0].0;
            let expected = first.1 as f64 / 90_000.0;
            assert!(
                (tail_start - expected).abs() < 0.001,
                "{name} 末片 {frames} 帧应从 {expected} 秒处开始，实际 {tail_start}"
            );
        }
    }
}

/// 不带时间戳 PRIV 帧的 ID3v2.4 标签（如定时元数据），约 640 字节。
fn metadata_tag() -> Vec<u8> {
    let data = [b"com.example.metadata\0".as_slice(), &[0x5A; 600]].concat();
    let syncsafe =
        |size: usize| [size >> 21, size >> 14, size >> 7, size].map(|b| (b & 0x7F) as u8);
    let frame = [b"PRIV".as_slice(), &syncsafe(data.len()), &[0, 0], &data].concat();
    [
        b"ID3\x04\x00\x00".as_slice(),
        &syncsafe(frame.len()),
        &frame,
    ]
    .concat()
}

/// 帧之间夹着 ID3 标签的打包音频：标签不进输出，下一个分片（相接或隔着缺失的一片）从它自己的时间戳处开始。
#[test]
fn packed_audio_with_id3_tags_between_frames_keeps_its_timing() {
    let dir = work_dir("packed_inline_id3");
    let output = dir.join("out.mp4");
    for name in ["aac", "ac3", "eac3", "mp3"] {
        let segments = packed_fixture(name);
        let frames: Vec<usize> = segments
            .iter()
            .map(|(path, _)| timelines(path).1.len())
            .collect();
        // 第一个分片是样本的前两片，中间夹一个标签
        let first = dir.join(format!("{name}-first"));
        let data = [
            std::fs::read(packed_segment(&dir, &segments[0].0, Some(900_000))).unwrap(),
            metadata_tag(),
            std::fs::read(&segments[1].0).unwrap(),
        ]
        .concat();
        std::fs::write(&first, data).unwrap();
        for gap in [0, 90_000] {
            let second_start = 900_000 + segments[0].1 + segments[1].1 + gap;
            let audio = TrackSegments {
                init: None,
                segments: segs([
                    first.clone(),
                    packed_segment(&dir, &segments[2].0, Some(second_start)),
                ]),
                gaps: Vec::new(),
            };

            let report = remux(
                &[Streams::Audio],
                &[group([audio])],
                &output,
                &Control::default(),
            )
            .unwrap();

            assert_eq!(
                report.streams[0].packets,
                frames.iter().sum::<usize>() as u64,
                "{name} 空档 {gap}"
            );
            let a_out = timelines(&output).1;
            let second = frames[0] + frames[1];
            let expected = (second_start - 900_000) as f64 / 90_000.0;
            let found = a_out[second].0 - a_out[0].0;
            assert!(
                (found - expected).abs() < 0.001,
                "{name} 空档 {gap}：第二个分片应从 {expected} 秒处开始，实际 {found}"
            );
            assert!(
                a_out[second - 1].0 - a_out[0].0 < expected - 0.01,
                "{name} 空档 {gap}：第一个分片的帧不应排到第二个分片的位置"
            );
        }
    }
}

/// 每片开头带 Info 帧、末尾带 ID3v1 标签的打包 MP3：两者都不进输出，各片照常接续。
#[test]
fn packed_mp3_info_frames_and_id3v1_tags_are_skipped() {
    let dir = work_dir("packed_mp3_info");
    let output = dir.join("out.mp4");
    let segments = packed_fixture("mp3_info");
    let id3v1 = [b"TAG".as_slice(), &[0x20; 125]].concat();
    let mut start = 900_000;
    let mut tagged = Vec::new();
    for (path, ticks) in &segments {
        let segment = packed_segment(&dir, path, Some(start));
        let data = [std::fs::read(&segment).unwrap(), id3v1.clone()].concat();
        std::fs::write(&segment, data).unwrap();
        tagged.push(segment);
        start += ticks;
    }
    let frames: usize = segments
        .iter()
        .map(|(path, _)| timelines(path).1.len())
        .sum();
    let audio = TrackSegments {
        init: None,
        segments: segs(tagged),
        gaps: Vec::new(),
    };

    let report = remux(
        &[Streams::Audio],
        &[group([audio])],
        &output,
        &Control::default(),
    )
    .unwrap();

    assert_eq!(report.streams[0].packets, frames as u64);
    let expected = (start - 900_000) as f64 / 90_000.0;
    let span = span(&timelines(&output).1);
    assert!(
        (span - expected).abs() < 0.03,
        "输出应长 {expected} 秒，实际 {span}"
    );
}

/// 打包音频损坏：中间一片丢了一帧（同步字坏了）、帧之间夹着杂字节，最后一片丢了一帧。中间一片损坏之后的帧照常写入、
/// 保持原来的时刻（按下一片的时间戳定，不从片头按样本数往后推）；最后一片损坏之后的帧定不了时刻，不写入。丢弃的
/// 损坏字节都计入。
#[test]
fn damage_in_packed_audio_keeps_the_timing_of_later_frames() {
    let dir = work_dir("packed_damage");
    let output = dir.join("out.mp4");
    let segments = packed_fixture("aac");
    let sizes: Vec<Vec<usize>> = segments
        .iter()
        .map(|(path, _)| packet_sizes(path))
        .collect();
    // 48 kHz 的 AAC 每帧 1024 个采样点，即 1920 个 90 kHz 的 tick
    let frame = 1920;
    let (t0, t1) = (900_000, 900_000 + segments[0].1);
    let t2 = t1 + segments[1].1;
    let damaged = |i: usize, lost: usize, junk_after: Option<usize>| {
        let mut data = std::fs::read(&segments[i].0).unwrap();
        let at: usize = sizes[i][..lost].iter().sum();
        data[at] = 0;
        if let Some(after) = junk_after {
            let at: usize = sizes[i][..after].iter().sum();
            data.splice(at..at, *b"junk");
        }
        let path = dir.join(format!("damaged{i}"));
        std::fs::write(&path, data).unwrap();
        path
    };
    let audio = TrackSegments {
        init: None,
        segments: segs([
            packed_segment(&dir, &segments[0].0, Some(t0)),
            packed_segment(&dir, &damaged(1, 10, Some(20)), Some(t1)),
            packed_segment(&dir, &damaged(2, 5, None), Some(t2)),
        ]),
        gaps: Vec::new(),
    };

    let report = remux(
        &[Streams::Audio],
        &[group([audio])],
        &output,
        &Control::default(),
    )
    .unwrap();

    let stream = &report.streams[0];
    let (n0, n1) = (sizes[0].len(), sizes[1].len());
    assert_eq!(stream.packets, (n0 + n1 - 1 + 5) as u64);
    let lost_in_last: usize = sizes[2][5..].iter().sum();
    assert_eq!(
        stream.discarded_corrupt_bytes,
        (sizes[1][10] + 4 + lost_in_last) as u64
    );
    let a_out = timelines(&output).1;
    let ticks = |seconds: f64| ((seconds - a_out[0].0) * 90_000.0).round() as u64;
    // 中间一片：第 10 帧丢了，第 11 帧起各在原来的时刻
    assert_eq!(ticks(a_out[n0 + 10].0), t1 - t0 + 11 * frame);
    assert_eq!(
        ticks(a_out[n0 + n1 - 2].0),
        t1 - t0 + (n1 as u64 - 1) * frame
    );
    // 最后一片从它的时间戳处开始
    assert_eq!(ticks(a_out[n0 + n1 - 1].0), t2 - t0);
}

/// 打包音频片内损坏之后的帧：下一片之前缺了分片时定不了时刻，不写入、计入丢弃，不挪到缺口之后；下一片在、但一帧也
/// 切不出（开头的帧头声明 8191 字节，超出分片，之后没有同步点）时，照样结束在它的时间戳处。
#[test]
fn damage_before_a_gap_or_a_frameless_segment_in_packed_audio() {
    let dir = work_dir("packed_damage_gap");
    let output = dir.join("out.mp4");
    let segments = packed_fixture("aac");
    let sizes: Vec<Vec<usize>> = segments
        .iter()
        .map(|(path, _)| packet_sizes(path))
        .collect();
    // 48 kHz 的 AAC 每帧 1024 个采样点，即 1920 个 90 kHz 的 tick
    let frame = 1920;
    let (t0, t1) = (900_000, 900_000 + segments[0].1);
    let t2 = t1 + segments[1].1;
    let mut data = std::fs::read(&segments[0].0).unwrap();
    data[sizes[0][..10].iter().sum::<usize>()] = 0;
    let damaged = dir.join("damaged0");
    std::fs::write(&damaged, data).unwrap();
    let mut frameless = std::fs::read(&segments[1].0).unwrap()[..7].to_vec();
    frameless[3] |= 0x03;
    frameless[4] = 0xFF;
    frameless[5] |= 0xE0;
    frameless.resize(5000, 0);
    let frameless_path = dir.join("frameless1");
    std::fs::write(&frameless_path, frameless).unwrap();
    let (n0, n2) = (sizes[0].len(), sizes[2].len());
    // 损坏的分片读不出完整的时长，声明的时长取样本里对应那一片的
    let declared_as = |i: usize, path: PathBuf| Segment {
        path,
        duration_us: segments[i].1 * 100 / 9,
    };

    let gap = TrackSegments {
        init: None,
        segments: vec![
            declared_as(0, packed_segment(&dir, &damaged, Some(t0))),
            declared_as(2, packed_segment(&dir, &segments[2].0, Some(t2))),
        ],
        gaps: vec![1],
    };
    let report = remux(
        &[Streams::Audio],
        &[group([gap])],
        &output,
        &Control::default(),
    )
    .unwrap();
    assert_eq!(report.streams[0].packets, (10 + n2) as u64);
    let lost: usize = sizes[0][10..].iter().sum();
    assert_eq!(report.streams[0].discarded_corrupt_bytes, lost as u64);
    let a_out = timelines(&output).1;
    let ticks = |seconds: f64| ((seconds - a_out[0].0) * 90_000.0).round() as u64;
    assert_eq!(ticks(a_out[10].0), t2 - t0);

    let empty_next = TrackSegments {
        init: None,
        segments: vec![
            declared_as(0, packed_segment(&dir, &damaged, Some(t0))),
            declared_as(1, packed_segment(&dir, &frameless_path, Some(t1))),
            declared_as(2, packed_segment(&dir, &segments[2].0, Some(t2))),
        ],
        gaps: Vec::new(),
    };
    let report = remux(
        &[Streams::Audio],
        &[group([empty_next])],
        &output,
        &Control::default(),
    )
    .unwrap();
    assert_eq!(report.streams[0].packets, (n0 - 1 + n2) as u64);
    assert_eq!(
        report.streams[0].discarded_corrupt_bytes,
        (sizes[0][10] + 5000) as u64
    );
    let a_out = timelines(&output).1;
    let ticks = |seconds: f64| ((seconds - a_out[0].0) * 90_000.0).round() as u64;
    // 第 10 帧丢了，第 11 帧起各在原来的时刻；第 2 片从它的时间戳处开始
    assert_eq!(ticks(a_out[10].0), 11 * frame);
    assert_eq!(ticks(a_out[n0 - 2].0), (n0 as u64 - 1) * frame);
    assert_eq!(ticks(a_out[n0 - 1].0), t2 - t0);
}

/// 打包音频连着两片损坏，后一片从第一帧起就在损坏之后（开头的帧头声明 8191 字节，超出分片）：前一片损坏之后的帧结束在
/// 后一片的时间戳处，后一片的帧结束在再下一片的时间戳处；后一片是最后一片时它的帧定不了时刻，不写入、计入丢弃。
#[test]
fn consecutive_damaged_segments_in_packed_audio_are_timed_separately() {
    let dir = work_dir("packed_damage_consecutive");
    let output = dir.join("out.mp4");
    let segments = packed_fixture("aac");
    let sizes: Vec<Vec<usize>> = segments
        .iter()
        .map(|(path, _)| packet_sizes(path))
        .collect();
    // 48 kHz 的 AAC 每帧 1024 个采样点，即 1920 个 90 kHz 的 tick
    let frame = 1920;
    let (t0, t1) = (900_000, 900_000 + segments[0].1);
    let t2 = t1 + segments[1].1;
    let mut a = std::fs::read(&segments[0].0).unwrap();
    a[sizes[0][..10].iter().sum::<usize>()] = 0;
    let a_path = dir.join("a");
    std::fs::write(&a_path, a).unwrap();
    let seg1 = std::fs::read(&segments[1].0).unwrap();
    let mut oversized = seg1[..7].to_vec();
    oversized[3] |= 0x03;
    oversized[4] = 0xFF;
    oversized[5] |= 0xE0;
    let twenty: usize = sizes[1][..20].iter().sum();
    let b_path = dir.join("b");
    std::fs::write(&b_path, [oversized, seg1[..twenty].to_vec()].concat()).unwrap();
    let n0 = sizes[0].len();
    let lost_in_a = sizes[0][10] as u64 + 7;

    let three = TrackSegments {
        init: None,
        segments: segs([
            packed_segment(&dir, &a_path, Some(t0)),
            packed_segment(&dir, &b_path, Some(t1)),
            packed_segment(&dir, &segments[2].0, Some(t2)),
        ]),
        gaps: Vec::new(),
    };
    let report = remux(
        &[Streams::Audio],
        &[group([three])],
        &output,
        &Control::default(),
    )
    .unwrap();
    let n2 = sizes[2].len();
    assert_eq!(report.streams[0].packets, (n0 - 1 + 20 + n2) as u64);
    assert_eq!(report.streams[0].discarded_corrupt_bytes, lost_in_a);
    let a_out = timelines(&output).1;
    let ticks = |seconds: f64| ((seconds - a_out[0].0) * 90_000.0).round() as u64;
    assert_eq!(ticks(a_out[10].0), 11 * frame);
    assert_eq!(ticks(a_out[n0 - 2].0), (n0 as u64 - 1) * frame);
    assert_eq!(ticks(a_out[n0 - 1].0), t2 - t0 - 20 * frame);
    assert_eq!(ticks(a_out[n0 - 1 + 20].0), t2 - t0);

    let two = TrackSegments {
        init: None,
        segments: segs([
            packed_segment(&dir, &a_path, Some(t0)),
            packed_segment(&dir, &b_path, Some(t1)),
        ]),
        gaps: Vec::new(),
    };
    let report = remux(
        &[Streams::Audio],
        &[group([two])],
        &output,
        &Control::default(),
    )
    .unwrap();
    assert_eq!(report.streams[0].packets, (n0 - 1) as u64);
    assert_eq!(
        report.streams[0].discarded_corrupt_bytes,
        lost_in_a + twenty as u64
    );
}

/// 两条轨的起点跨在回绕点前 60 秒两侧：打包音频按 ID3 时间戳展开，与 TS 视频读出的起点差 2^33，合并时对齐回来。
#[test]
fn packed_audio_aligns_with_ts_across_the_wrap_edge() {
    let dir = work_dir("packed_wrap_edge");
    let output = dir.join("out.mp4");
    let video = track("ts_wrap");
    let v_in = timelines(&concat_track(&video, &dir, "v.ts")).0;
    let video_start = (earliest(&v_in) * 90_000.0).round() as u64;
    let start = video_start + 9_000;
    let edge = (1u64 << 33) - 60 * 90_000;
    assert!(video_start < edge && start >= edge, "样本应跨在界线两侧");
    let segments = packed_fixture("aac");
    let audio = TrackSegments {
        init: None,
        segments: segs([packed_segment(&dir, &segments[0].0, Some(start))]),
        gaps: Vec::new(),
    };

    remux(
        &[Streams::Video, Streams::Audio],
        &[group([video, audio])],
        &output,
        &Control::default(),
    )
    .unwrap();

    let (v_out, a_out) = timelines(&output);
    let lead = a_out[0].0 - earliest(&v_out);
    assert!(
        (lead - 0.1).abs() < 0.001,
        "音频应比视频晚 0.1 秒开始，实际 {lead}"
    );
}

/// 打包音频有分片没有 ID3 时间戳：与别的轨同组时不知道怎样对齐，放不进 MP4；只有它一条轨时按样本数从头接续。
#[test]
fn untimed_packed_audio_is_used_only_alone() {
    let dir = work_dir("packed_untimed");
    let output = dir.join("out.mp4");
    let segments = packed_fixture("aac");
    let untimed = TrackSegments {
        init: None,
        segments: segs(segments.iter().map(|(path, _)| path.clone())),
        gaps: Vec::new(),
    };
    // 只有第一片有时间戳，第二片前缺了一片
    let partly = TrackSegments {
        init: None,
        segments: segs([
            packed_segment(&dir, &segments[0].0, Some(900_000)),
            segments[2].0.clone(),
        ]),
        gaps: Vec::new(),
    };

    for audio in [&untimed, &partly] {
        let err = remux(
            &[Streams::Video, Streams::Audio],
            &[group([track("ts_a"), audio.clone()])],
            &output,
            &Control::default(),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                Error::Unsupported(Unsupported::UntimedPackedAudio { group: 0, track: 1 })
            ),
            "实际 {err:?}"
        );
    }

    let report = remux(
        &[Streams::Audio],
        &[group([untimed])],
        &output,
        &Control::default(),
    )
    .unwrap();
    let packets: usize = segments
        .iter()
        .map(|(path, _)| timelines(path).1.len())
        .sum();
    assert_eq!(report.streams[0].packets, packets as u64);
    // 有时间戳的那片也按样本数接续：缺的那片不留空档
    let report = remux(
        &[Streams::Audio],
        &[group([partly])],
        &output,
        &Control::default(),
    )
    .unwrap();
    let expected = (segments[0].1 + segments[2].1) as f64 / 90_000.0;
    let duration = report.streams[0].duration.as_secs_f64();
    assert!(
        (duration - expected).abs() < 0.001,
        "应连续 {expected} 秒，实际 {duration}"
    );
}
