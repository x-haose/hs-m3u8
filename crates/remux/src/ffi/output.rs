//! 输出：以某种格式写一个文件，装了取消用的中断回调。

use std::ffi::{CString, c_int, c_void};
use std::path::PathBuf;
use std::ptr::NonNull;

use hs_m3u8_ffmpeg_sys as sys;

use super::stream::{Stream, streams_of};
use super::{CodecId, Dictionary, Packet, Rational, error, out_of_memory};
use crate::{Cancel, Container, FfmpegError};

/// 装了中断回调的输出。关闭文件时 FFmpeg 刷出缓冲还会调用回调：析构时先关闭文件，再释放上下文，最后释放回调用的
/// 取消信号。
pub(crate) struct OutputFile {
    ctx: NonNull<sys::AVFormatContext>,
    pub(crate) path: PathBuf,
    /// 装在中断回调上的取消信号
    pub(crate) cancel: Cancel,
    /// 中断回调的 opaque：`cancel` 的副本，由 Box::into_raw 交出，析构时收回
    interrupt: NonNull<Cancel>,
}

/// 打开输出时失败的一步。
pub(crate) enum OpenFailure {
    /// 建立封装器的上下文
    Muxer(FfmpegError),
    /// 创建文件
    File(FfmpegError),
}

/// 中断回调：取消了为 1，FFmpeg 随后的读写以 AVERROR_EXIT 失败。
///
/// # Safety
/// `opaque` 为 [`OutputFile`] 的 `interrupt`。
unsafe extern "C" fn interrupted(opaque: *mut c_void) -> c_int {
    // SAFETY: 见函数的前提；OutputFile 在上下文与文件释放之后才释放它。
    let cancel = unsafe { &*opaque.cast::<Cancel>() };
    c_int::from(cancel.is_cancelled())
}

impl OutputFile {
    /// 以 `container` 的格式打开输出 `path`（不含 NUL 的 UTF-8），装上中断回调：FFmpeg 每次读写文件前轮询它（含写
    /// moov 前置时重新打开输出搬动数据的第二遍），`cancel` 取消后读写以 AVERROR_EXIT 失败。
    pub(crate) fn open(
        path: &str,
        container: Container,
        cancel: &Cancel,
    ) -> Result<OutputFile, OpenFailure> {
        let c_path = CString::new(path).expect("调用方保证路径不含 NUL");
        let mut ctx = std::ptr::null_mut();
        // SAFETY: 格式名与路径是 NUL 结尾的字符串；成功时 ctx 指向新分配的上下文，失败时为空。
        let created = unsafe {
            sys::avformat_alloc_output_context2(
                &raw mut ctx,
                std::ptr::null(),
                container.muxer().as_ptr(),
                c_path.as_ptr(),
            )
        };
        if created < 0 {
            return Err(OpenFailure::Muxer(error(created)));
        }
        let mut file = OutputFile {
            ctx: NonNull::new(ctx).expect("建立成功时上下文不为空"),
            path: PathBuf::from(path),
            cancel: cancel.clone(),
            interrupt: NonNull::from(Box::leak(Box::new(cancel.clone()))),
        };
        let opaque = file.interrupt.as_ptr().cast::<c_void>();
        let raw = file.raw_mut();
        raw.interrupt_callback = sys::AVIOInterruptCB {
            callback: Some(interrupted),
            opaque,
        };
        // SAFETY: 上下文归 file 所有；路径是 NUL 结尾的字符串；中断回调在上下文里，随上下文存活。失败时 pb 仍为空，file
        // 析构时释放上下文。
        let opened = unsafe {
            sys::avio_open2(
                &raw mut raw.pb,
                c_path.as_ptr(),
                sys::AVIO_FLAG_WRITE,
                &raw const raw.interrupt_callback,
                std::ptr::null_mut(),
            )
        };
        if opened < 0 {
            return Err(OpenFailure::File(error(opened)));
        }
        Ok(file)
    }

    fn raw(&self) -> &sys::AVFormatContext {
        // SAFETY: 上下文在 self 存活期间有效；只在 &mut self 的方法里改动。
        unsafe { self.ctx.as_ref() }
    }

    fn raw_mut(&mut self) -> &mut sys::AVFormatContext {
        // SAFETY: 同上，由 &mut self 独占。
        unsafe { self.ctx.as_mut() }
    }

    /// 下标为 `index` 的输出流（AVStream），可写。
    fn stream_mut(&mut self, index: usize) -> &mut sys::AVStream {
        let count = self.nb_streams();
        assert!(index < count, "输出流下标 {index} 超出流数 {count}");
        // SAFETY: streams 指向 nb_streams 个有效的流指针，各流归上下文所有，由 &mut self 独占。
        unsafe { &mut **self.raw().streams.add(index) }
    }

    pub(crate) fn nb_streams(&self) -> usize {
        usize::try_from(self.raw().nb_streams).expect("流数在 usize 范围内")
    }

    /// 下标为 `index` 的输出流；超出流数时为 None。
    pub(crate) fn stream(&self, index: usize) -> Option<Stream<'_>> {
        // SAFETY: 上下文有效，借用期间不被改动（见 raw）。
        unsafe { streams_of(self.raw()) }.nth(index)
    }

    /// 新建一路输出流，返回它的下标。
    pub(crate) fn add_stream(&mut self) -> Result<usize, FfmpegError> {
        // SAFETY: 上下文有效、由 &mut self 独占；不指定编码器。失败时为空（分配失败，或流数超过上下文的上限）。
        let stream = unsafe { sys::avformat_new_stream(self.ctx.as_ptr(), std::ptr::null()) };
        if stream.is_null() {
            return Err(error(sys::AVERROR(libc::ENOMEM)));
        }
        Ok(self.nb_streams() - 1)
    }

    /// 输出流 `index` 的编码参数设为 `from`（输入流）的副本。
    pub(crate) fn copy_parameters(
        &mut self,
        index: usize,
        from: Stream<'_>,
    ) -> Result<(), FfmpegError> {
        let to = self.stream_mut(index).codecpar;
        // SAFETY: 两边的 codecpar 都有效、不为空，各归所属的流；目标由 &mut self 独占，来源只读。
        let copied = unsafe { sys::avcodec_parameters_copy(to, from.params_ptr()) };
        if copied < 0 {
            return Err(error(copied));
        }
        Ok(())
    }

    /// 写头之前：设输出流 `index` 的时间基（封装器写头时可能换掉）。
    pub(crate) fn set_time_base(&mut self, index: usize, time_base: Rational) {
        self.stream_mut(index).time_base = time_base.raw();
    }

    /// 写头之前：设输出流 `index` 的编码标签（如 `hvc1`）；0 为由封装器按编码选择。
    pub(crate) fn set_codec_tag(&mut self, index: usize, tag: u32) {
        // SAFETY: codecpar 不为空、归流所有，由 &mut self 独占。
        unsafe { (*self.stream_mut(index).codecpar).codec_tag = tag };
    }

    /// 写头之前：把新建的输出流 `index` 设成字幕流，编码为 `codec`，解码配置为 `extradata`（空的不设）。
    pub(crate) fn set_subtitle_parameters(
        &mut self,
        index: usize,
        codec: CodecId,
        extradata: &[u8],
    ) {
        let stream = self.stream_mut(index);
        // SAFETY: codecpar 不为空、归流所有，由 &mut self 独占；新建的流没有 extradata。新的 extradata 用 av_mallocz
        // 分配（含 FFmpeg 要求的补零），由 codecpar 持有、随它释放。
        unsafe {
            let par = &mut *stream.codecpar;
            par.codec_type = sys::AVMEDIA_TYPE_SUBTITLE;
            par.codec_id = codec.0;
            if !extradata.is_empty() {
                let padding =
                    usize::try_from(sys::AV_INPUT_BUFFER_PADDING_SIZE).expect("补零的字节数为正");
                let size = extradata.len() + padding;
                let buffer = sys::av_mallocz(size).cast::<u8>();
                if buffer.is_null() {
                    out_of_memory(size);
                }
                std::ptr::copy_nonoverlapping(extradata.as_ptr(), buffer, extradata.len());
                par.extradata = buffer;
                par.extradata_size =
                    c_int::try_from(extradata.len()).expect("解码配置在 int 范围内");
            }
        }
    }

    /// 写头之前：输出流 `index` 的元数据换成 `metadata`。
    pub(crate) fn set_metadata(&mut self, index: usize, metadata: Dictionary) {
        let stream = self.stream_mut(index);
        // SAFETY: 原有的元数据为空或归流所有，先释放；新的交给流持有、随它释放。
        unsafe {
            sys::av_dict_free(&raw mut stream.metadata);
            stream.metadata = metadata.into_raw();
        }
    }

    /// 写头之前：设输出流 `index` 的默认与强制标志（AVStream 的 disposition），其余标志清掉。
    pub(crate) fn set_disposition(&mut self, index: usize, default: bool, forced: bool) {
        let mut disposition = 0;
        if default {
            disposition |= sys::AV_DISPOSITION_DEFAULT;
        }
        if forced {
            disposition |= sys::AV_DISPOSITION_FORCED;
        }
        self.stream_mut(index).disposition = disposition;
    }

    /// 写头；`options` 为封装器的选项，没用掉的留在里面。
    pub(crate) fn write_header(&mut self, options: &mut Dictionary) -> Result<(), FfmpegError> {
        // SAFETY: 上下文有效、由 &mut self 独占，流已建好；选项字典归 options 所有，FFmpeg 把没用掉的留在里面。
        let written =
            unsafe { sys::avformat_write_header(self.ctx.as_ptr(), options.as_mut_ptr()) };
        if written < 0 {
            return Err(error(written));
        }
        Ok(())
    }

    /// 交错写出一个包（时间戳为输出流的时间基）；FFmpeg 取走包的数据，包随后为空。
    pub(crate) fn write_interleaved(&mut self, packet: &mut Packet) -> Result<(), FfmpegError> {
        // SAFETY: 上下文有效、已写头，由 &mut self 独占；包有效，FFmpeg 取走它的数据引用并把它置空。
        let written =
            unsafe { sys::av_interleaved_write_frame(self.ctx.as_ptr(), packet.as_mut_ptr()) };
        if written < 0 {
            return Err(error(written));
        }
        Ok(())
    }

    /// 写尾。封装器可能返回正数（如 movenc 分段时的 mfra 长度），不小于 0 都算成功。
    pub(crate) fn write_trailer(&mut self) -> Result<(), FfmpegError> {
        // SAFETY: 上下文有效、已写头，由 &mut self 独占。
        let written = unsafe { sys::av_write_trailer(self.ctx.as_ptr()) };
        if written < 0 {
            return Err(error(written));
        }
        Ok(())
    }

    /// 关闭文件：刷出缓冲、关闭文件描述符；已关闭时什么也不做。失败时为 FFmpeg 的错误，文件系统到关闭时才报的写入
    /// 错误（如网络盘的延迟写入）在这里出现。
    pub(crate) fn close(&mut self) -> Result<(), FfmpegError> {
        // SAFETY: pb 为 open 里 avio_open2 打开的读写上下文或空；avio_closep 关闭后置空，对空的什么也不做。关闭时刷出
        // 缓冲会调用的中断回调此时仍有效。
        let closed = unsafe { sys::avio_closep(&raw mut self.raw_mut().pb) };
        if closed < 0 {
            return Err(error(closed));
        }
        Ok(())
    }

    /// 输出的读写上下文记下的错误：FFmpeg 写文件失败时记在这里，封装器自身的错误不记；没有时为 None。
    pub(crate) fn io_error(&self) -> Option<FfmpegError> {
        // SAFETY: pb 为空或指向打开的读写上下文，此处只读。
        let code = unsafe { self.raw().pb.as_ref() }?.error;
        (code < 0).then(|| error(code))
    }
}

impl Drop for OutputFile {
    /// 失败路径上的收尾：已经在报别的错误，关闭的结果不再看；成功路径先调 [`OutputFile::close`]。
    fn drop(&mut self) {
        let _ = self.close();
        // SAFETY: 上下文与 interrupt 归本结构所有；文件已关闭（pb 为空），avformat_free_context 释放上下文与各流，之后
        // 不再调用中断回调。
        unsafe {
            sys::avformat_free_context(self.ctx.as_ptr());
            drop(Box::from_raw(self.interrupt.as_ptr()));
        }
    }
}
