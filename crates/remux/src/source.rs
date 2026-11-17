//! 一条轨在一组里的输入读出的包：选流、核对随帧携带的参数、记下没写出的部分。

use std::collections::VecDeque;

use ffmpeg::{Packet, Rational, Rescale};
use ffmpeg_next as ffmpeg;

use crate::group::Selected;
use crate::inband::{Inband, Mismatch};
use crate::input::Input;
use crate::nal::{self, Framing};
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
    /// 按输入流下标：HEVC 视频流包里 NAL 单元的排法（见 [`Selected::hevc`]）
    hevc: Vec<Option<Framing>>,
    /// 按输入流下标：视频流在这一组里读到的第一个能开始解码的帧的 PTS（没有 PTS 时为 DTS），还没读到时为 None
    keyframe_pts: Vec<Option<i64>>,
    /// 上一个写出的包所在的分片（见 [`Source::check_jump`]）
    last_segment: Option<usize>,
    /// 按输入流下标：进输出的流还没读到带 DTS 的包时，暂存的开头那些缺 DTS 的包（见 [`Source::read_input`]）
    leading: Vec<Vec<Packet>>,
    /// 按输入流下标：已读到带 DTS 的包
    dts_seen: Vec<bool>,
    /// 补好 DTS、待交给 [`Source::read_next`] 处理的包
    filled: VecDeque<Packet>,
    /// 已读出、待写入的包及其映射；包一定带 DTS
    pub(crate) queue: VecDeque<(Packet, Mapping)>,
    /// 读到了末尾（见 [`Source::read_next`]）
    pub(crate) finished: bool,
    /// 加到这个输入所有时间戳上的量，微秒：读到第一个包之后先按回绕对齐（见 [`crate::timing::align_wraps`]），写出前再
    /// 加上平移到输出时间线的量
    pub(crate) offset_us: i64,
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
        let mut hevc = vec![None; streams];
        for (s, &output) in selected.iter().zip(outputs) {
            let kind = s.desc.shape.kind();
            map[s.index] = Some(Mapping {
                output,
                kind,
                time_base: s.time_base,
            });
            hevc[s.index] = s.hevc;
            inband[s.index] =
                s.inband
                    .as_ref()
                    .map(|own| match references.get(output).cloned().flatten() {
                        Some(reference) => reference.for_next_group(),
                        None => own.clone(),
                    });
        }
        Source {
            group,
            track,
            input,
            map,
            inband,
            dropped: vec![Dropped::default(); streams],
            hevc,
            keyframe_pts: vec![None; streams],
            leading: vec![Vec::new(); streams],
            dts_seen: vec![false; streams],
            filled: VecDeque::new(),
            last_segment: None,
            queue: VecDeque::new(),
            finished: false,
            offset_us: 0,
        }
    }

    /// 上一个写出的包所在的分片；还没写出过、或写出的都在第一个分片之前（fMP4 的 init 段里）时为 None。
    pub(crate) fn segment(&self) -> Option<usize> {
        self.last_segment
    }

    /// 读下一个要写出的包。跳过未映射流的包；没有 DTS 的包、组开头解不出的视频包（见 [`Source::decodable`]）与损坏（读
    /// 不出帧头或帧长与包长对不上）的音频包记入 `dropped`；参数随帧携带的包核对参数。读到末尾返回 `None` 并记下
    /// `finished`，打包音频丢弃的损坏字节这时才是全部（见 [`Input::discarded`]），一并记入 `dropped`；之后不再调用。
    pub(crate) fn read_next(&mut self) -> Result<Option<(Packet, Mapping)>, Error> {
        loop {
            let mut packet = Packet::empty();
            if !self.read_input(&mut packet)? {
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
                    Ok(()) => return Ok(Some((packet, mapping))),
                    Err(Mismatch::Unreadable) => {
                        self.dropped[packet.stream()].corrupt_bytes += packet.size() as u64;
                        continue;
                    }
                    Err(Mismatch::Changed) => Unsupported::ParamsChangedInGroup {
                        group,
                        track,
                        kind: mapping.kind,
                    },
                    Err(Mismatch::Config) => Unsupported::ConfigChanged {
                        group,
                        track,
                        kind: mapping.kind,
                    },
                    Err(Mismatch::Eac3ShortFrames) => Unsupported::Eac3ShortFrames { group, track },
                    Err(Mismatch::Eac3Substreams) => Unsupported::Eac3Substreams { group, track },
                };
                return Err(Error::Unsupported(unsupported));
            }
            return Ok(Some((packet, mapping)));
        }
    }

    /// 从输入读下一个包，补上进输出的流开头缺的 DTS：Matroska 只存 PTS，有 B 帧时 FFmpeg 按重排的延迟由 PTS 推出 DTS，
    /// 开头延迟那几个包推不出。这些包（带 PTS 与时长）先暂存，读到这路流第一个带 DTS 的包时，从它的 DTS 起按各包的时长
    /// 往前倒推（同 ffmpeg 命令行补的值）。读到末尾仍没有 DTS 的照原样交出，按缺 DTS 计数。读到末尾返回 false。
    fn read_input(&mut self, packet: &mut Packet) -> Result<bool, Error> {
        loop {
            if let Some(filled) = self.filled.pop_front() {
                *packet = filled;
                return Ok(true);
            }
            if !self.input.read(packet, self.group, self.track)? {
                for leading in &mut self.leading {
                    self.filled.extend(leading.drain(..));
                }
                match self.filled.pop_front() {
                    Some(filled) => {
                        *packet = filled;
                        return Ok(true);
                    }
                    None => return Ok(false),
                }
            }
            let stream = packet.stream();
            // TS 可能在文件中途出现新流，其下标超出建立映射时的流数；不进输出的流不补
            if self.map.get(stream).is_none_or(Option::is_none) || self.dts_seen[stream] {
                return Ok(true);
            }
            let Some(dts) = packet.dts() else {
                if packet.pts().is_some() && packet.duration() > 0 {
                    self.leading[stream].push(std::mem::replace(packet, Packet::empty()));
                    continue;
                }
                return Ok(true);
            };
            self.dts_seen[stream] = true;
            let mut leading = std::mem::take(&mut self.leading[stream]);
            if leading.is_empty() {
                return Ok(true);
            }
            let mut next = dts;
            for earlier in leading.iter_mut().rev() {
                next -= earlier.duration();
                earlier.set_dts(Some(next));
            }
            self.filled.extend(leading);
            self.filled
                .push_back(std::mem::replace(packet, Packet::empty()));
        }
    }

    /// 写出前核对：包 `packet`（输出时间线上的 DTS 为 `dts_us`，微秒）比全组已写出的最大 DTS `clock_us` 超出的部分，减去
    /// 这个输入上一个包所在的片（它在那片里之后还有多久不知道，按整片算）与其间各片声明的时长，即输出里整组都没有内容的
    /// 空档，超过容差（见 [`crate::input::SegmentMap::tolerance_us`]）时报 [`Unsupported::TimestampJump`]。一路流单独中
    /// 断时别的流推着时钟走，帧稀疏时跨片的间隔有所在的片抵着，都不算；片内的跳变照样量得到。没有位置的包（解析器从一个
    /// PES 拆出的后几帧）归在这个输入上一个写出的包所在的分片；其间有缺片或声明的时长判断不了时不核对。往回跳的由写出时
    /// 的 DTS 严格递增拦下。
    pub(crate) fn check_jump(
        &mut self,
        packet: &Packet,
        mapping: Mapping,
        dts_us: i64,
        clock_us: Option<i64>,
    ) -> Result<(), Error> {
        let segments = self.input.segments();
        let Some(segment) = segments.segment_at(packet.position()).or(self.last_segment) else {
            return Ok(());
        };
        let previous = self.last_segment.replace(segment);
        let (Some(clock_us), Some(skipped), Some(tolerance)) = (
            clock_us,
            segments.declared_from(previous, segment),
            segments.tolerance_us(),
        ) else {
            return Ok(());
        };
        let hole = (dts_us - clock_us).saturating_sub_unsigned(skipped);
        match u64::try_from(hole) {
            Ok(jump_us) if jump_us > tolerance => {
                Err(Error::Unsupported(Unsupported::TimestampJump {
                    group: self.group,
                    track: self.track,
                    kind: mapping.kind,
                    segment,
                    jump_us,
                }))
            }
            _ => Ok(()),
        }
    }

    /// 带 DTS 的视频包在这一组里解得出：从这一组第一个 FFmpeg 标的关键帧（随机访问点：IDR、带 recovery point SEI 的
    /// 帧、参考帧数不超过 1 的 I 帧等）起，除去参考上一个 GOP 的前导帧（HEVC 按 NAL 类型只去掉 RASL；其余编码没有这样
    /// 的标记，解码在起点之后、呈现在它之前的帧一律去掉）。之前的帧参考的帧不在这一组里：第 0 组之前什么也没有，其余组
    /// 之前是不连续处另一段内容的帧。FFmpeg 没标的 I 帧不当起点：它之后的帧可能参考它之前的帧，光看码流确认不了。
    fn decodable(&mut self, packet: &Packet) -> bool {
        let stream = packet.stream();
        let pts = packet.pts().or(packet.dts()).expect("调用方已确认带 DTS");
        match self.keyframe_pts[stream] {
            Some(key) if pts >= key => true,
            Some(_) => self.hevc[stream]
                .is_some_and(|framing| !nal::hevc_rasl(packet.data().unwrap_or_default(), framing)),
            None if packet.is_key() => {
                self.keyframe_pts[stream] = Some(pts);
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

    /// 读到每路映射流都有包排队、视频流另有一个呈现不早于起点的包（或读到末尾），用于确定本组各流的首个 DTS 与最早的
    /// PTS：紧跟起点、呈现在它之前的前导帧（如 HEVC 的 RADL）都要算进最早的 PTS，否则平移后落在 0 之前，被编辑列表裁掉。
    /// 读到末尾而有视频流一个能开始解码的帧都没有时报 [`Unsupported::NoKeyframe`]。
    pub(crate) fn prime(&mut self) -> Result<(), Error> {
        let mapped: Vec<Mapping> = self.map.iter().flatten().copied().collect();
        while !self.finished && !mapped.iter().all(|m| self.primed(m)) {
            if let Some(item) = self.read_next()? {
                self.queue.push_back(item);
            }
        }
        // 读完了这一组，视频一个能开始解码的帧都没有（各包都按解不出丢掉了）
        let undecodable = self.map.iter().enumerate().any(|(index, mapping)| {
            mapping.is_some_and(|m| {
                m.kind == StreamKind::Video
                    && self.dropped[index].before_keyframe > 0
                    && !self
                        .queue
                        .iter()
                        .any(|(_, queued)| queued.output == m.output)
            })
        });
        if undecodable {
            return Err(Error::Unsupported(Unsupported::NoKeyframe {
                group: self.group,
                track: self.track,
            }));
        }
        Ok(())
    }

    /// 一路流已读到足以确定本组起点的包（见 [`Source::prime`]）。
    fn primed(&self, mapping: &Mapping) -> bool {
        let mut pts = self
            .queue
            .iter()
            .filter(|(_, m)| m.output == mapping.output)
            .map(|(packet, _)| packet.pts().or(packet.dts()));
        match pts.next() {
            None => false,
            Some(_) if mapping.kind != StreamKind::Video => true,
            Some(first) => pts.any(|pts| pts >= first),
        }
    }

    /// 队首的包加上 [`Source::offset_us`] 之后的 DTS，微秒。
    pub(crate) fn front_dts_us(&self) -> Option<i64> {
        let (packet, mapping) = self.queue.front()?;
        let dts = packet.dts().expect("队列中的包都带 DTS");
        Some(dts.rescale(mapping.time_base, MICROS) + self.offset_us)
    }

    /// 这个输入里进输出的各路流最早的起始时刻，微秒：FFmpeg 给的流的起始时刻（第一个包的 PTS，已按容器的编辑列表、MP3
    /// 的 Info 帧等去掉编码器的起始延迟，延迟里的包时间戳为负），没有的取队列里这路流最早的 PTS（没有 PTS 的取 DTS）。
    /// 进输出的流都没有包时为 None。
    pub(crate) fn start_us(&self) -> Option<i64> {
        let ictx = self.input.context();
        self.map
            .iter()
            .enumerate()
            .filter_map(|(index, mapping)| {
                let mapping = mapping.as_ref()?;
                let given = ictx.stream(index).map(|s| s.start_time());
                let start = given
                    .filter(|&ts| ts != ffmpeg::ffi::AV_NOPTS_VALUE)
                    .or_else(|| {
                        self.queue
                            .iter()
                            .filter(|(_, m)| m.output == mapping.output)
                            .map(|(packet, _)| {
                                packet.pts().or(packet.dts()).expect("队列中的包都带 DTS")
                            })
                            .min()
                    })?;
                Some(start.rescale(mapping.time_base, MICROS))
            })
            .min()
    }
}
