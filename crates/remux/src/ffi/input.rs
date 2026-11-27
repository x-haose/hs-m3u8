//! 输入：FFmpeg 经自定义读写上下文从 [`Read`] 与 [`Seek`] 读数据，格式由开头的内容识别。

use std::ffi::{CStr, c_int, c_void};
use std::io::{self, Read, Seek, SeekFrom};
use std::ptr::NonNull;

use hs_m3u8_ffmpeg_sys as sys;

use super::stream::{Stream, streams_of};
use super::{Dictionary, Packet, error, out_of_memory};
use crate::FfmpegError;

/// 读写上下文的缓冲区大小，字节：libavformat 自己打开文件时的默认值。
const BUFFER_SIZE: usize = 32 * 1024;

/// 读写回调读的数据。
trait ReadSeek: Read + Seek {}

impl<T: Read + Seek> ReadSeek for T {}

/// 回调的 opaque：数据与读取用的中转缓冲区。FFmpeg 给读回调的缓冲区可能未初始化、或是它正在用的缓冲窗口，读失败时
/// 不能动，所以先读进 `scratch`，成功后再复制过去。
struct Reader {
    source: Box<dyn ReadSeek>,
    scratch: Vec<u8>,
}

/// 自定义的读写上下文与回调读的数据，析构时一起释放。
struct CustomIo {
    io: NonNull<sys::AVIOContext>,
    reader: NonNull<Reader>,
}

impl CustomIo {
    fn new(source: impl Read + Seek + 'static) -> CustomIo {
        // SAFETY: 分配 BUFFER_SIZE 字节、清零；失败时为空。
        let buffer = unsafe { sys::av_mallocz(BUFFER_SIZE) }.cast::<u8>();
        if buffer.is_null() {
            out_of_memory(BUFFER_SIZE);
        }
        let reader = NonNull::from(Box::leak(Box::new(Reader {
            source: Box::new(source),
            scratch: Vec::new(),
        })));
        // SAFETY: buffer 为 av_mallocz 分配的 BUFFER_SIZE 字节，成功时归读写上下文所有；opaque 为 reader，在上下文释放
        // 之前一直有效（见 Drop）；只读，不给写回调。
        let io = unsafe {
            sys::avio_alloc_context(
                buffer,
                c_int::try_from(BUFFER_SIZE).expect("缓冲区大小在 int 范围内"),
                0,
                reader.as_ptr().cast(),
                Some(read),
                None,
                Some(seek),
            )
        };
        let Some(io) = NonNull::new(io) else {
            // SAFETY: 分配失败时 buffer 与 reader 仍归这里所有，各自释放。
            unsafe {
                sys::av_free(buffer.cast());
                drop(Box::from_raw(reader.as_ptr()));
            }
            out_of_memory(size_of::<sys::AVIOContext>());
        };
        CustomIo { io, reader }
    }
}

impl Drop for CustomIo {
    fn drop(&mut self) {
        // SAFETY: io 与 reader 归本结构所有，使用它们的输入上下文已先关闭（见 InputContext）。缓冲区可能被 FFmpeg
        // 换过，释放上下文里现在的那块。
        unsafe {
            sys::av_freep((&raw mut (*self.io.as_ptr()).buffer).cast());
            let mut io = self.io.as_ptr();
            sys::avio_context_free(&raw mut io);
            drop(Box::from_raw(self.reader.as_ptr()));
        }
    }
}

/// 读回调：读到末尾为 AVERROR_EOF；读错误换成 AVERROR（见 [`io_error_code`]）；`Interrupted` 重试。读源都是本 crate
/// 的（分片链与文件），报读到的字节数比缓冲区大即违反 [`Read`] 的约定，是缺陷：panic，在 `extern "C"` 回调里即中止进程。
///
/// # Safety
/// `opaque` 为 [`CustomIo`] 的 reader，`buf` 可写 `size` 字节；FFmpeg 不并发调用同一上下文的回调。
unsafe extern "C" fn read(opaque: *mut c_void, buf: *mut u8, size: c_int) -> c_int {
    // SAFETY: 见函数的前提。
    let reader = unsafe { &mut *opaque.cast::<Reader>() };
    let size = match usize::try_from(size) {
        Ok(size) if size > 0 => size,
        _ => return sys::AVERROR(libc::EINVAL),
    };
    reader.scratch.resize(size, 0);
    loop {
        return match reader.source.read(&mut reader.scratch) {
            Ok(0) => sys::AVERROR_EOF,
            Ok(n) => {
                assert!(n <= size, "Read 报读到 {n} 字节，超出缓冲区的 {size} 字节");
                // SAFETY: buf 可写 size 字节，scratch 有 size 字节，n 不超过 size，两者不重叠。
                unsafe { std::ptr::copy_nonoverlapping(reader.scratch.as_ptr(), buf, n) };
                c_int::try_from(n).expect("不超过 size")
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => io_error_code(&e),
        };
    }
}

/// 定位回调：`whence` 为 SEEK_SET、SEEK_CUR、SEEK_END 或 AVSEEK_SIZE（问总长），可带 AVSEEK_FORCE（不影响）。
///
/// # Safety
/// `opaque` 为 [`CustomIo`] 的 reader；FFmpeg 不并发调用同一上下文的回调。
unsafe extern "C" fn seek(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    // SAFETY: 见函数的前提。
    let source = unsafe { &mut (*opaque.cast::<Reader>()).source };
    let whence = whence & !sys::AVSEEK_FORCE;
    let to = match whence {
        sys::AVSEEK_SIZE => SeekFrom::End(0),
        libc::SEEK_SET => match u64::try_from(offset) {
            Ok(offset) => SeekFrom::Start(offset),
            Err(_) => return i64::from(sys::AVERROR(libc::EINVAL)),
        },
        libc::SEEK_CUR => SeekFrom::Current(offset),
        libc::SEEK_END => SeekFrom::End(offset),
        _ => return i64::from(sys::AVERROR(libc::EINVAL)),
    };
    loop {
        // 问总长时要回到原处：FFmpeg 不认为问总长会移动读写位置
        let result = if whence == sys::AVSEEK_SIZE {
            source.stream_position().and_then(|at| {
                let end = source.seek(to)?;
                source.seek(SeekFrom::Start(at))?;
                Ok(end)
            })
        } else {
            source.seek(to)
        };
        return match result {
            Ok(position) => {
                i64::try_from(position).unwrap_or(i64::from(sys::AVERROR(libc::EOVERFLOW)))
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => i64::from(io_error_code(&e)),
        };
    }
}

/// 读写错误交给 FFmpeg 的错误码：Unix 上带 errno 的照原样（Windows 上的是 Win32 错误码，不是 errno，不用）；其余按
/// 种类：提前读到末尾为 AVERROR_EOF，TimedOut、Unsupported 为 ETIMEDOUT、ENOSYS，别的为 EIO。
///
/// 不交给 FFmpeg EAGAIN（含 WouldBlock）：读源是阻塞的，而 avio 把回调的错误记下后不再调用回调，每次读都返回同一个
/// EAGAIN，`avformat_find_stream_info` 遇到 EAGAIN 重试，探测就停不下来；按 EIO 报。
fn io_error_code(error: &io::Error) -> c_int {
    #[cfg(unix)]
    if let Some(errno) = error
        .raw_os_error()
        .filter(|&errno| errno > 0 && errno != libc::EAGAIN && errno != libc::EWOULDBLOCK)
    {
        return sys::AVERROR(errno);
    }
    match error.kind() {
        io::ErrorKind::UnexpectedEof => sys::AVERROR_EOF,
        io::ErrorKind::TimedOut => sys::AVERROR(libc::ETIMEDOUT),
        io::ErrorKind::Unsupported => sys::AVERROR(libc::ENOSYS),
        _ => sys::AVERROR(libc::EIO),
    }
}

/// 打开的输入（AVFormatContext），已探测出流信息。
pub(crate) struct InputContext {
    ctx: NonNull<sys::AVFormatContext>,
    /// 晚于上下文释放（字段在 Drop 之后析构）：关闭上下文时解复用器可能还要读
    _io: CustomIo,
}

impl InputContext {
    /// 打开 `source`：格式由开头的内容识别，`options` 为打开选项（解复用器与 AVFormatContext 的），随后探测各路流的
    /// 参数。失败时为 FFmpeg 的错误。
    pub(crate) fn open(
        source: impl Read + Seek + 'static,
        options: Option<Dictionary>,
    ) -> Result<InputContext, FfmpegError> {
        let io = CustomIo::new(source);
        // SAFETY: 分配一个空的上下文；失败时为空。
        let Some(mut ctx) = NonNull::new(unsafe { sys::avformat_alloc_context() }) else {
            out_of_memory(size_of::<sys::AVFormatContext>());
        };
        // SAFETY: ctx 为新分配的上下文；AVFMT_FLAG_CUSTOM_IO 让 FFmpeg 不关闭、不释放 pb，pb 由 io 释放。
        unsafe {
            ctx.as_mut().pb = io.io.as_ptr();
            ctx.as_mut().flags |= sys::AVFMT_FLAG_CUSTOM_IO;
        }
        let mut options = options;
        let options_ptr = options
            .as_mut()
            .map_or(std::ptr::null_mut(), Dictionary::as_mut_ptr);
        let mut raw = ctx.as_ptr();
        // SAFETY: raw 为预先分配、装好 pb 的上下文；失败时 FFmpeg 释放它并置空。选项字典为空指针或归 options 所有，
        // FFmpeg 把没用掉的选项留在里面，随 options 释放。
        let opened = unsafe {
            sys::avformat_open_input(
                &raw mut raw,
                std::ptr::null(),
                std::ptr::null(),
                options_ptr,
            )
        };
        if opened < 0 {
            return Err(error(opened));
        }
        let input = InputContext {
            ctx: NonNull::new(raw).expect("打开成功时上下文不为空"),
            _io: io,
        };
        // SAFETY: 上下文已打开；不给各流的解码选项。失败时 input 析构，关闭上下文。
        let found =
            unsafe { sys::avformat_find_stream_info(input.ctx.as_ptr(), std::ptr::null_mut()) };
        if found < 0 {
            return Err(error(found));
        }
        Ok(input)
    }

    fn raw(&self) -> &sys::AVFormatContext {
        // SAFETY: 上下文在 self 存活期间有效；只在 &mut self 的方法里交给 FFmpeg 改动。
        unsafe { self.ctx.as_ref() }
    }

    pub(crate) fn nb_streams(&self) -> usize {
        usize::try_from(self.raw().nb_streams).expect("流数在 usize 范围内")
    }

    pub(crate) fn streams(&self) -> impl Iterator<Item = Stream<'_>> {
        // SAFETY: 上下文有效，借用期间不被改动（见 raw）。
        unsafe { streams_of(self.raw()) }
    }

    /// 下标为 `index` 的流；超出流数时为 None。
    pub(crate) fn stream(&self, index: usize) -> Option<Stream<'_>> {
        self.streams().nth(index)
    }

    /// 识别出的格式（解复用器）的名称，如 `mpegts`、`mov,mp4,m4a,3gp,3g2,mj2`。
    pub(crate) fn format_name(&self) -> &str {
        // SAFETY: 打开成功的输入的 iformat 指向 FFmpeg 的静态格式描述，名称为静态的 NUL 结尾字符串。
        let name = unsafe { CStr::from_ptr((*self.raw().iformat).name) };
        name.to_str().expect("FFmpeg 的格式名是 ASCII")
    }

    /// 读下一个包到 `packet`（原有的内容先释放）；读到末尾为 false，其余失败为 FFmpeg 的错误。
    pub(crate) fn read(&mut self, packet: &mut Packet) -> Result<bool, FfmpegError> {
        // SAFETY: 上下文有效、由 &mut self 独占；包有效，av_read_frame 要求传入的包是空的，先释放原有的内容。失败时包
        // 为空。
        let read = unsafe {
            sys::av_packet_unref(packet.as_mut_ptr());
            sys::av_read_frame(self.ctx.as_ptr(), packet.as_mut_ptr())
        };
        match read {
            0.. => Ok(true),
            sys::AVERROR_EOF => Ok(false),
            code => Err(error(code)),
        }
    }

    /// 输入的节目（AVProgram）里列不列出流 `index`：TS 每读到一份 PMT，FFmpeg 先清空节目的流列表再按它重新加入
    /// （mpegts.c 的 clear_avprogram），列表即最近一份 PMT 里的流。没有节目的输入（不是 TS）为 None。
    pub(crate) fn in_program(&self, index: usize) -> Option<bool> {
        let ctx = self.raw();
        let count = usize::try_from(ctx.nb_programs).expect("节目数在 usize 范围内");
        if count == 0 || ctx.programs.is_null() {
            return None;
        }
        // SAFETY: programs 指向 nb_programs 个有效的节目指针，各节目的 stream_index 为空或指向 nb_stream_indexes 个
        // 下标，都归上下文所有，借用期间不变；只读。
        let programs = unsafe { std::slice::from_raw_parts(ctx.programs, count) };
        Some(programs.iter().any(|&program| {
            // SAFETY: 同上，节目指针非空且有效。
            let program = unsafe { &*program };
            if program.stream_index.is_null() {
                return false;
            }
            let len = usize::try_from(program.nb_stream_indexes).expect("流数在 usize 范围内");
            // SAFETY: 同上。
            let indexes = unsafe { std::slice::from_raw_parts(program.stream_index, len) };
            indexes
                .iter()
                .any(|&i| usize::try_from(i).is_ok_and(|i| i == index))
        }))
    }
}

impl Drop for InputContext {
    fn drop(&mut self) {
        let mut raw = self.ctx.as_ptr();
        // SAFETY: 上下文归本结构所有；装了 AVFMT_FLAG_CUSTOM_IO，关闭时不动 pb，pb 随后由 _io 释放。
        unsafe { sys::avformat_close_input(&raw mut raw) };
    }
}
