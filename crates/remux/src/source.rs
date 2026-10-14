//! 一条轨在一组里的输入读出的包：选流、核对随帧携带的参数、记下没写出的部分。

use std::collections::VecDeque;

use ffmpeg::{Packet, Rational, Rescale};
use ffmpeg_next as ffmpeg;

use crate::group::Selected;
use crate::inband::{Inband, Mismatch};
use crate::input::Input;
use crate::timing::MICROS;
use crate::{Error, StreamKind, Unsupported, ffi};

/// 一路输入流对应的输出流。
#[derive(Clone, Copy)]
pub(crate) struct Mapping {
    pub(crate) output: usize,
    pub(crate) kind: StreamKind,
    /// 输入流的时间基
    pub(crate) time_base: Rational,
}

/// 一路输入流读到而没有写出的部分。
#[derive(Clone, Copy, Default)]
pub(crate) struct Dropped {
    /// 没有 DTS 的包数
    pub(crate) without_dts: u64,
    /// 丢弃的损坏音频字节数（见 [`crate::StreamReport::discarded_corrupt_bytes`]）
    pub(crate) corrupt_bytes: u64,
    /// 组开头解不出的视频包数（见 [`crate::StreamReport::skipped_before_keyframe`]）
    pub(crate) before_keyframe: u64,
}

pub(crate) struct Source {
    pub(crate) group: usize,
    pub(crate) track: usize,
    pub(crate) input: Input,
    /// 按输入流下标；None 表示该流不进输出
    pub(crate) map: Vec<Option<Mapping>>,
    /// 按输入流下标；进输出、参数随帧携带的流为 Some
    pub(crate) inband: Vec<Option<Inband>>,
    /// 按输入流下标
    pub(crate) dropped: Vec<Dropped>,
    /// 按输入流下标：视频流在这一组里读到的第一个关键帧的 PTS（没有 PTS 时为 DTS），还没读到时为 None
    keyframe_pts: Vec<Option<i64>>,
    /// 已读出、待写入的包及其映射；包一定带 DTS
    pub(crate) queue: VecDeque<(Packet, Mapping)>,
    /// 读到了末尾（见 [`Source::read_next`]）
    pub(crate) finished: bool,
    /// 按回绕对齐时加到这个输入所有时间戳上的量，微秒（见 [`crate::timing::align_wraps`]）
    pub(crate) wrap_us: i64,
}

impl Source {
    /// `outputs[i]` 为 `selected[i]` 对应的输出流下标；`references` 见 [`crate::group::references_of`]，第 0 组为空。
    pub(crate) fn new(
        group: usize,
        track: usize,
        input: Input,
        selected: &[Selected],
        outputs: &[usize],
        references: &[Option<Inband>],
    ) -> Self {
        let streams = input.context().nb_streams() as usize;
        let mut map = vec![None; streams];
        let mut inband = vec![None; streams];
        for (s, &output) in selected.iter().zip(outputs) {
            map[s.index] = Some(Mapping {
                output,
                kind: s.desc.shape.kind(),
                time_base: s.time_base,
            });
            if input.standalone() {
                inband[s.index] = Some(match references.get(output).cloned().flatten() {
                    Some(reference) => reference.for_next_group(),
                    None => Inband::of(s.id).expect("check_codec 只收能逐帧读出参数的编码"),
                });
            }
        }
        Source {
            group,
            track,
            input,
            map,
            inband,
            dropped: vec![Dropped::default(); streams],
            keyframe_pts: vec![None; streams],
            queue: VecDeque::new(),
            finished: false,
            wrap_us: 0,
        }
    }

    /// 读下一个要写出的包。跳过未映射流的包；没有 DTS 的包、组开头解不出的视频包（见 [`Source::decodable`]）与损坏
    /// （读不出帧头或帧长与包长对不上）的音频包记入 `dropped`；参数随帧携带的包核对参数。读到末尾返回 `None` 并记下 `finished`，打包音频丢弃的损坏字节这时才是
    /// 全部（见 [`Input::discarded`]），一并记入 `dropped`；之后不再调用。
    pub(crate) fn read_next(&mut self) -> Result<Option<(Packet, Mapping)>, Error> {
        loop {
            let mut packet = Packet::empty();
            if !self.input.read(&mut packet, self.group, self.track)? {
                if let Some((stream, bytes)) = self.input.discarded() {
                    self.dropped[stream].corrupt_bytes += bytes;
                }
                self.finished = true;
                return Ok(None);
            }
            let Some(mapping) = self.map.get(packet.stream()).copied().flatten() else {
                self.check_replaced(packet.stream())?;
                continue;
            };
            if packet.dts().is_none() {
                self.dropped[packet.stream()].without_dts += 1;
                continue;
            }
            if mapping.kind == StreamKind::Video && !self.decodable(&packet) {
                self.dropped[packet.stream()].before_keyframe += 1;
                continue;
            }
            if let Some(check) = self.inband[packet.stream()].as_mut() {
                let stream = self
                    .input
                    .context()
                    .stream(packet.stream())
                    .expect("包所属的流存在");
                let (group, track) = (self.group, self.track);
                let unsupported = match check.check(packet.data().unwrap_or_default(), &stream) {
                    Ok(()) => None,
                    Err(Mismatch::Unreadable) => {
                        self.dropped[packet.stream()].corrupt_bytes += packet.size() as u64;
                        continue;
                    }
                    Err(Mismatch::Changed) => Some(Unsupported::ParamsChangedInGroup {
                        group,
                        track,
                        kind: mapping.kind,
                    }),
                    Err(Mismatch::Config) => Some(Unsupported::ConfigChanged {
                        group,
                        track,
                        kind: mapping.kind,
                    }),
                    Err(Mismatch::Eac3ShortFrames) => {
                        Some(Unsupported::Eac3ShortFrames { group, track })
                    }
                    Err(Mismatch::Eac3Substreams) => {
                        Some(Unsupported::Eac3Substreams { group, track })
                    }
                };
                if let Some(unsupported) = unsupported {
                    return Err(Error::Unsupported(unsupported));
                }
            }
            return Ok(Some((packet, mapping)));
        }
    }

    /// 带 DTS 的视频包在这一组里解得出：从这一组第一个关键帧起，除去呈现时刻早于这个关键帧的前导帧（开放 GOP 里参考
    /// 上一个 GOP 的帧，如 HEVC 的 RASL）。第一个关键帧之前的包与前导帧参考的帧不在这一组里：第 0 组之前什么也没有，
    /// 其余组之前是不连续处另一段内容的帧。
    fn decodable(&mut self, packet: &Packet) -> bool {
        let pts = packet.pts().or(packet.dts()).expect("调用方已确认带 DTS");
        let key = &mut self.keyframe_pts[packet.stream()];
        match *key {
            Some(key) => pts >= key,
            None if packet.is_key() => {
                *key = Some(pts);
                true
            }
            None => false,
        }
    }

    /// 读到不进输出的流 `index` 的包：TS 的 PMT 中途把这条轨要取的一路换成了它（原来那路已不在节目里、它在）时报错；
    /// 其余（同类的另一路、定时元数据等）跳过。
    fn check_replaced(&self, index: usize) -> Result<(), Error> {
        let ictx = self.input.context();
        // TS 可能在文件中途出现新流，其下标超出建立映射时的流数
        let medium = ictx
            .stream(index)
            .expect("包所属的流存在")
            .parameters()
            .medium();
        let replaced = self.map.iter().enumerate().find_map(|(selected, mapping)| {
            mapping.filter(|m| {
                m.kind.medium() == medium
                    && ffi::in_program(ictx, selected) == Some(false)
                    && ffi::in_program(ictx, index) == Some(true)
            })
        });
        match replaced {
            Some(mapping) => Err(Error::Unsupported(Unsupported::StreamChangedInGroup {
                group: self.group,
                track: self.track,
                kind: mapping.kind,
            })),
            None => Ok(()),
        }
    }

    /// 读到每路映射流都至少有一个包排队（或读到末尾），用于确定本组各流的首个 DTS 与最早的 PTS。
    pub(crate) fn prime(&mut self) -> Result<(), Error> {
        let mapped: Vec<usize> = self.map.iter().flatten().map(|m| m.output).collect();
        while !self.finished
            && !mapped
                .iter()
                .all(|&out| self.queue.iter().any(|(_, m)| m.output == out))
        {
            if let Some(item) = self.read_next()? {
                self.queue.push_back(item);
            }
        }
        Ok(())
    }

    /// 队首的包对齐回绕后的 DTS，微秒。
    pub(crate) fn front_dts_us(&self) -> Option<i64> {
        let (packet, mapping) = self.queue.front()?;
        let dts = packet.dts().expect("队列中的包都带 DTS");
        Some(dts.rescale(mapping.time_base, MICROS) + self.wrap_us)
    }
}
