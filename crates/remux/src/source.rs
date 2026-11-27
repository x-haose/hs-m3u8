//! 一条轨在一组里（或一个完整文件）的输入读出的包：选流、补开头缺的 DTS、核对随帧携带的参数与 E-AC-3 的帧结构、
//! 记下没写出的部分。

use std::collections::VecDeque;

use crate::ffi::{Packet, Rational, Rescale};
use crate::group::Selected;
use crate::inband::{Inband, Mismatch, PacketCheck};
use crate::input::Input;
use crate::nal::{self, Framing};
use crate::timing::MICROS;
use crate::{Error, Origin, StreamKind, Unsupported};

/// 一路输入流对应的输出流。
#[derive(Clone, Copy)]
pub(crate) struct Mapping {
    pub(crate) output: usize,
    pub(crate) kind: StreamKind,
    /// 输入流的时间基
    pub(crate) time_base: Rational,
    /// 音频：采样率（Hz）与每帧的采样点数，见 [`crate::timing::SampleClock`]；视频或帧长不知道时为 None
    pub(crate) frames: Option<(u32, u32)>,
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
    pub(crate) at: Origin,
    pub(crate) input: Input,
    /// 按输入流下标；None 表示该流不进输出
    pub(crate) map: Vec<Option<Mapping>>,
    /// 按输入流下标：进输出、要逐包核对的流的核对（见 [`Selected::check`]）
    pub(crate) checks: Vec<Option<PacketCheck>>,
    /// 按输入流下标
    pub(crate) dropped: Vec<Dropped>,
    /// 按输入流下标：HEVC 视频流包里 NAL 单元的排法（见 [`Selected::hevc`]）
    hevc: Vec<Option<Framing>>,
    /// 按输入流下标：视频流在这一组里读到的第一个能开始解码的帧的 PTS（没有 PTS 时为 DTS），还没读到时为 None
    keyframe_pts: Vec<Option<i64>>,
    /// 上一个写出的包所在的分片（见 [`Source::check_jump`]）
    last_segment: Option<usize>,
    /// 补开头缺的 DTS（见 [`Source::read_input`]）
    fill: DtsFill,
    /// 已读出、待写入的包及其映射；包一定带 DTS
    pub(crate) queue: VecDeque<(Packet, Mapping)>,
    /// 读到了末尾（见 [`Source::read_next`]）
    pub(crate) finished: bool,
    /// 从输入时间线平移到输出时间线要加的量，微秒；写出前设定
    pub(crate) offset_us: i64,
}

impl Source {
    /// `at` 处的输入；`outputs[i]` 为 `selected[i]` 对应的输出流下标；`references` 见
    /// [`crate::group::references_of`]，第 0 组与完整文件为空。
    pub(crate) fn new(
        at: Origin,
        input: Input,
        selected: &[Selected],
        outputs: &[usize],
        references: &[Option<Inband>],
    ) -> Self {
        let streams = input.context().nb_streams();
        let mut map = vec![None; streams];
        let mut checks = vec![None; streams];
        let mut hevc = vec![None; streams];
        let mut mapped = vec![false; streams];
        for (s, &output) in selected.iter().zip(outputs) {
            let kind = s.desc.shape.kind();
            let frames = match s.desc.shape {
                crate::Shape::Audio { sample_rate, .. } => {
                    s.frame_samples.map(|samples| (sample_rate, samples))
                }
                crate::Shape::Video { .. } | crate::Shape::Subtitle { .. } => None,
            };
            map[s.index] = Some(Mapping {
                output,
                kind,
                time_base: s.time_base,
                frames,
            });
            hevc[s.index] = s.hevc;
            mapped[s.index] = true;
            checks[s.index] = match (&s.check, references.get(output).cloned().flatten()) {
                (Some(PacketCheck::Inband(_)), Some(reference)) => {
                    Some(PacketCheck::Inband(reference.for_next_group()))
                }
                (own, _) => own.clone(),
            };
        }
        Source {
            at,
            input,
            map,
            checks,
            dropped: vec![Dropped::default(); streams],
            hevc,
            keyframe_pts: vec![None; streams],
            fill: DtsFill::new(&mapped),
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

    /// 读下一个要写出的包，读到末尾返回 `None` 并记下 `finished`（之后不再调用）。
    ///
    /// - 跳过未映射流的包；进输出的流里的空包报 [`Error::EmptyPacket`]。
    /// - 没有 DTS 的包、组开头解不出的视频包（见 [`Source::decodable`]）与损坏（读不出帧头或帧长与包长对不上）的音频包
    ///   记入 `dropped`；打包音频丢弃的损坏字节读到末尾时才是全部（见 [`Input::discarded`]），那时一并记入。
    /// - 参数随帧携带的包核对参数，要放进 MP4 的 E-AC-3 核对帧结构。
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
            if !packet.has_payload() {
                return Err(Error::EmptyPacket {
                    at: self.at,
                    kind: mapping.kind,
                });
            }
            if packet.dts().is_none() {
                self.dropped[packet.stream()].without_dts += 1;
                continue;
            }
            if mapping.kind == StreamKind::Video && !self.decodable(&packet) {
                self.dropped[packet.stream()].before_keyframe += 1;
                continue;
            }
            if let Some(check) = self.checks[packet.stream()].as_mut() {
                let stream = self
                    .input
                    .context()
                    .stream(packet.stream())
                    .expect("包所属的流存在");
                let checked = check.check(packet.data().unwrap_or_default(), &stream);
                if !self.passes(checked, &packet, mapping)? {
                    continue;
                }
            }
            return Ok(Some((packet, mapping)));
        }
    }

    /// 包 `packet` 的核对结果 `checked`：通过为真；读不出的计入损坏字节、为假；其余报错。
    fn passes(
        &mut self,
        checked: Result<(), Mismatch>,
        packet: &Packet,
        mapping: Mapping,
    ) -> Result<bool, Error> {
        let (at, kind) = (self.at, mapping.kind);
        let unsupported = match checked {
            Ok(()) => return Ok(true),
            Err(Mismatch::Unreadable) => {
                self.dropped[packet.stream()].corrupt_bytes += packet.size() as u64;
                return Ok(false);
            }
            Err(Mismatch::Changed) => Unsupported::ParamsChangedInGroup { at, kind },
            Err(Mismatch::Config) => Unsupported::ConfigChanged { at, kind },
            Err(Mismatch::Eac3ShortFrames) => Unsupported::Eac3ShortFrames { at },
            Err(Mismatch::Eac3Substreams) => Unsupported::Eac3Substreams { at },
        };
        Err(Error::Unsupported(unsupported))
    }

    /// 从输入读下一个包，补上进输出的流开头缺的 DTS（见 [`DtsFill`]）；读到末尾返回 false。
    fn read_input(&mut self, packet: &mut Packet) -> Result<bool, Error> {
        loop {
            if let Some(ready) = self.fill.pop() {
                *packet = ready;
                return Ok(true);
            }
            let mut read = Packet::empty();
            if !self.input.read(&mut read, self.at)? {
                if self.fill.finish() {
                    continue;
                }
                return Ok(false);
            }
            if let Some(read) = self.fill.push(read) {
                *packet = read;
                return Ok(true);
            }
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
                    at: self.at,
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
        let kind = ictx.stream(index).expect("包所属的流存在").kind();
        let replaced = self.map.iter().enumerate().find_map(|(selected, mapping)| {
            mapping.filter(|m| {
                Some(m.kind) == kind
                    && ictx.in_program(selected) == Some(false)
                    && ictx.in_program(index) == Some(true)
            })
        });
        match replaced {
            Some(mapping) => Err(Error::Unsupported(Unsupported::StreamChangedInGroup {
                at: self.at,
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
            return Err(Error::Unsupported(Unsupported::NoKeyframe { at: self.at }));
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

    /// 队首的包在输入时间线上的 DTS，微秒。
    pub(crate) fn front_input_dts_us(&self) -> Option<i64> {
        let (packet, mapping) = self.queue.front()?;
        let dts = packet.dts().expect("队列中的包都带 DTS");
        Some(dts.rescale(mapping.time_base, MICROS))
    }

    /// 队首的包在输出时间线上（加上 [`Source::offset_us`]）的 DTS，微秒。
    pub(crate) fn front_dts_us(&self) -> Option<i64> {
        Some(self.front_input_dts_us()? + self.offset_us)
    }

    /// 这个输入（完整文件）的起始时刻，输入时间线上的微秒：进输出的各路流里 FFmpeg 给了起始时刻的（第一个包的 PTS，
    /// 已按容器的编辑列表、MP3 的 Info 帧等去掉编码器的起始延迟，延迟里的包时间戳为负）取最早的。打开时探测到的范围
    /// 里有包的流才有起始时刻，没有的流开始得更晚，不影响最早的。都没有时取队列里最早的 PTS（没有 PTS 的取 DTS），队列
    /// 也为空时为 None。
    pub(crate) fn start_us(&self) -> Option<i64> {
        let ictx = self.input.context();
        let given = self.map.iter().enumerate().filter_map(|(index, mapping)| {
            let mapping = mapping.as_ref()?;
            let start = ictx.stream(index)?.start_time()?;
            Some(start.rescale(mapping.time_base, MICROS))
        });
        given.min().or_else(|| {
            self.queue
                .iter()
                .map(|(packet, mapping)| {
                    let ts = packet.pts().or(packet.dts()).expect("队列中的包都带 DTS");
                    ts.rescale(mapping.time_base, MICROS)
                })
                .min()
        })
    }
}

/// 补上进输出的流开头缺的 DTS。Matroska 只存 PTS，TS 的 PES 也可以只带 PTS；FFmpeg 按重排的延迟由 PTS 推出 DTS，
/// 开头几个包推不出（延迟还没定下时也是这样，如探测读满上限而解码器还没解出够多的帧）。这些包（带 PTS 与时长）先
/// 暂存，读到这路流第一个带 DTS 的包时往前补：每个包取「后一个包的 DTS 减去它的时长」与它自己的 PTS 中较小的，DTS
/// 因而严格递增、不大于 PTS（有 B 帧时同 ffmpeg 命令行补的值）。暂存的超过 [`DtsFill::LIMIT`] 个、或读到末尾仍没有
/// DTS 时照原样交出，按缺 DTS 计数。对各种输入都生效；带 DTS 的输入（fMP4、打包音频）用不到。
struct DtsFill {
    /// 按输入流下标：还在等第一个 DTS 的进输出的流为真；已读到 DTS 或已放弃补的为假
    waiting: Vec<bool>,
    /// 按输入流下标：暂存的包
    held: Vec<Vec<Packet>>,
    /// 补好或放弃补、待交出的包，按读出的顺序
    ready: VecDeque<Packet>,
}

impl DtsFill {
    /// 一路流最多暂存几个包：FFmpeg 推 DTS 时最多的重排延迟（链接的 FFmpeg 中 libavformat 的 MAX_REORDER_DELAY）。
    const LIMIT: usize = 16;

    /// `mapped` 按输入流下标，进输出的流为真。
    fn new(mapped: &[bool]) -> DtsFill {
        DtsFill {
            waiting: mapped.to_vec(),
            held: mapped.iter().map(|_| Vec::new()).collect(),
            ready: VecDeque::new(),
        }
    }

    fn pop(&mut self) -> Option<Packet> {
        self.ready.pop_front()
    }

    /// 读出的一个包：不用补的原样返回；暂存或连同暂存的一起放进待交出的，返回 None。
    fn push(&mut self, packet: Packet) -> Option<Packet> {
        let stream = packet.stream();
        // TS 可能在文件中途出现新流，其下标超出建立映射时的流数
        if !self.waiting.get(stream).copied().unwrap_or(false) {
            return Some(packet);
        }
        let held = &mut self.held[stream];
        let Some(dts) = packet.dts() else {
            if packet.pts().is_none() || packet.duration() <= 0 {
                return Some(packet);
            }
            held.push(packet);
            if held.len() > DtsFill::LIMIT {
                self.waiting[stream] = false;
                self.ready.extend(held.drain(..));
            }
            return None;
        };
        self.waiting[stream] = false;
        if held.is_empty() {
            return Some(packet);
        }
        let mut next = dts;
        for earlier in held.iter_mut().rev() {
            let pts = earlier.pts().expect("暂存的包都带 PTS");
            next = (next - earlier.duration()).min(pts);
            earlier.set_dts(Some(next));
        }
        self.ready.extend(held.drain(..));
        self.ready.push_back(packet);
        None
    }

    /// 读到了末尾：还暂存着的照原样放进待交出的；有这样的包时为真。
    fn finish(&mut self) -> bool {
        for held in &mut self.held {
            self.ready.extend(held.drain(..));
        }
        !self.ready.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 流 0 的包：PTS、DTS（None 为缺）、时长。
    fn packet(pts: i64, dts: Option<i64>, duration: i64) -> Packet {
        let mut packet = Packet::empty();
        packet.set_stream(0);
        packet.set_pts(Some(pts));
        packet.set_dts(dts);
        packet.set_duration(duration);
        packet
    }

    /// 依次放进各包，读完后交出的各包的 DTS。
    fn filled(packets: Vec<Packet>) -> Vec<Option<i64>> {
        let mut fill = DtsFill::new(&[true]);
        let mut out = Vec::new();
        for p in packets {
            out.extend(fill.push(p));
            while let Some(p) = fill.pop() {
                out.push(p);
            }
        }
        fill.finish();
        while let Some(p) = fill.pop() {
            out.push(p);
        }
        out.iter().map(Packet::dts).collect()
    }

    #[test]
    fn leading_packets_get_increasing_dts_not_after_their_pts() {
        // 两层 B 帧：开头两个包按时长倒推
        let b_frames = vec![
            packet(0, None, 40),
            packet(120, None, 40),
            packet(40, Some(0), 40),
        ];
        assert_eq!(filled(b_frames), [Some(-80), Some(-40), Some(0)]);
        // 没有重排、时长取整：倒推的值会大于 PTS 时取 PTS
        let rounded = vec![
            packet(0, None, 20),
            packet(30, None, 20),
            packet(70, Some(70), 20),
        ];
        assert_eq!(filled(rounded), [Some(0), Some(30), Some(70)]);
        // 一直没有 DTS：超过上限后照原样交出
        let never: Vec<Packet> = (0..20).map(|i| packet(i * 40, None, 40)).collect();
        assert_eq!(filled(never), vec![None; 20]);
    }
}
