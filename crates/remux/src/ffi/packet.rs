//! 包（AVPacket）。

use std::ptr::NonNull;

use hs_m3u8_ffmpeg_sys as sys;

use super::out_of_memory;

/// 一个包，由 av_packet_alloc 分配、独占，析构时连同数据一起释放。时间戳的时间基为所属流的。
pub(crate) struct Packet(NonNull<sys::AVPacket>);

impl Packet {
    /// 空包：没有数据，时间戳为没有。
    pub(crate) fn empty() -> Packet {
        // SAFETY: 分配一个空包；失败时为空。
        match NonNull::new(unsafe { sys::av_packet_alloc() }) {
            Some(packet) => Packet(packet),
            None => out_of_memory(size_of::<sys::AVPacket>()),
        }
    }

    /// 数据为 `data` 的副本的包。
    pub(crate) fn copy(data: &[u8]) -> Packet {
        let mut packet = Packet::empty();
        let size = i32::try_from(data.len()).expect("包长在 int 范围内");
        // SAFETY: 包是空的；av_new_packet 分配 size 字节（另加补零）的引用计数缓冲区作为包的数据，失败时包不变。
        if unsafe { sys::av_new_packet(packet.as_mut_ptr(), size) } < 0 {
            out_of_memory(data.len());
        }
        // SAFETY: 包的数据为刚分配的 data.len() 字节，与 data 不重叠。
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), packet.raw_mut().data, data.len()) };
        packet
    }

    pub(super) fn as_mut_ptr(&mut self) -> *mut sys::AVPacket {
        self.0.as_ptr()
    }

    fn raw(&self) -> &sys::AVPacket {
        // SAFETY: 包归 self 所有，在 self 存活期间有效。
        unsafe { self.0.as_ref() }
    }

    fn raw_mut(&mut self) -> &mut sys::AVPacket {
        // SAFETY: 同上，由 &mut self 独占。
        unsafe { self.0.as_mut() }
    }

    pub(crate) fn pts(&self) -> Option<i64> {
        Some(self.raw().pts).filter(|&pts| pts != sys::AV_NOPTS_VALUE)
    }

    pub(crate) fn set_pts(&mut self, pts: Option<i64>) {
        self.raw_mut().pts = pts.unwrap_or(sys::AV_NOPTS_VALUE);
    }

    pub(crate) fn dts(&self) -> Option<i64> {
        Some(self.raw().dts).filter(|&dts| dts != sys::AV_NOPTS_VALUE)
    }

    pub(crate) fn set_dts(&mut self, dts: Option<i64>) {
        self.raw_mut().dts = dts.unwrap_or(sys::AV_NOPTS_VALUE);
    }

    /// 时长；不知道时为 0。
    pub(crate) fn duration(&self) -> i64 {
        self.raw().duration
    }

    pub(crate) fn set_duration(&mut self, duration: i64) {
        self.raw_mut().duration = duration;
    }

    /// 在输入里的字节位置；不知道时为 -1。
    pub(crate) fn position(&self) -> i64 {
        self.raw().pos
    }

    pub(crate) fn set_position(&mut self, position: i64) {
        self.raw_mut().pos = position;
    }

    /// 所属流的下标。
    pub(crate) fn stream(&self) -> usize {
        usize::try_from(self.raw().stream_index).expect("流下标非负")
    }

    pub(crate) fn set_stream(&mut self, index: usize) {
        self.raw_mut().stream_index = i32::try_from(index).expect("流下标在 int 范围内");
    }

    /// 解复用器标为关键帧（随机访问点）。
    pub(crate) fn is_key(&self) -> bool {
        self.raw().flags & sys::AV_PKT_FLAG_KEY != 0
    }

    /// 标为关键帧，其余标志清掉。
    pub(crate) fn set_key(&mut self) {
        self.raw_mut().flags = sys::AV_PKT_FLAG_KEY;
    }

    /// 有数据或附加数据（side data）。都没有的包 FFmpeg 的封装器不当作样本：MP4 封装器丢掉它而不报错。
    pub(crate) fn has_payload(&self) -> bool {
        self.raw().size > 0 || self.raw().side_data_elems > 0
    }

    /// 包的数据；没有数据时为 None。
    pub(crate) fn data(&self) -> Option<&[u8]> {
        let raw = self.raw();
        if raw.data.is_null() {
            return None;
        }
        // SAFETY: 非空的 data 指向 size 字节，归包所有，在借用期间不变。
        Some(unsafe { std::slice::from_raw_parts(raw.data, self.size()) })
    }

    /// 数据的字节数。
    pub(crate) fn size(&self) -> usize {
        usize::try_from(self.raw().size).expect("包长非负")
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        let mut raw = self.0.as_ptr();
        // SAFETY: 包归本结构所有；av_packet_free 释放数据与包本身。
        unsafe { sys::av_packet_free(&raw mut raw) };
    }
}
