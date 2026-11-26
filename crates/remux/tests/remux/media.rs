//! 测试里直接用链接的 FFmpeg 读文件：各路流的信息、逐个读包、解码视频。打不开、读错（读到末尾除外）时为 FFmpeg 的错误
//! 说明。

use std::ffi::{CStr, CString, c_int};
use std::path::Path;
use std::ptr;

use hs_m3u8_ffmpeg_sys as sys;
use hs_m3u8_remux::StreamKind;

/// FFmpeg 的错误码 `code` 的说明。
fn message(code: c_int) -> String {
    let mut buf = [0; 128];
    // SAFETY: buf 可写 buf.len() 字节，av_strerror 写入 NUL 结尾、不超出给定长度的说明。
    unsafe { sys::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    // SAFETY: 同上。
    let message = unsafe { CStr::from_ptr(buf.as_ptr()) };
    format!("{}（AVERROR {code}）", message.to_string_lossy())
}

/// 一路流。
pub(crate) struct StreamInfo {
    pub(crate) index: usize,
    /// 视频、音频或字幕；其余种类为 None
    pub(crate) kind: Option<StreamKind>,
    /// FFmpeg 的编码名
    pub(crate) codec: &'static str,
    /// 分子与分母
    pub(crate) time_base: (i32, i32),
    /// 元数据里的语言；没有时为空串
    pub(crate) language: String,
    /// 元数据里的标题；没有时为空串
    pub(crate) title: String,
    pub(crate) default: bool,
    pub(crate) forced: bool,
}

impl StreamInfo {
    /// 时间基，秒。
    pub(crate) fn seconds(&self) -> f64 {
        f64::from(self.time_base.0) / f64::from(self.time_base.1)
    }
}

/// 读出的一个包。
pub(crate) struct Packet {
    pub(crate) stream: usize,
    pub(crate) pts: Option<i64>,
    pub(crate) dts: Option<i64>,
    pub(crate) duration: i64,
    pub(crate) data: Vec<u8>,
}

/// 打开的媒体文件。
pub(crate) struct MediaFile {
    ctx: *mut sys::AVFormatContext,
    /// 最近读出的包
    packet: *mut sys::AVPacket,
}

impl MediaFile {
    /// 打开 `path` 并探测流信息。
    pub(crate) fn open(path: &Path) -> Result<MediaFile, String> {
        let c_path = CString::new(path.to_str().unwrap()).unwrap();
        let mut ctx = ptr::null_mut();
        // SAFETY: 路径是 NUL 结尾的字符串；成功时 ctx 指向打开的上下文，失败时为空。
        let opened = unsafe {
            sys::avformat_open_input(&raw mut ctx, c_path.as_ptr(), ptr::null(), ptr::null_mut())
        };
        if opened < 0 {
            return Err(message(opened));
        }
        // SAFETY: 分配一个空包。
        let packet = unsafe { sys::av_packet_alloc() };
        assert!(!packet.is_null());
        let file = MediaFile { ctx, packet };
        // SAFETY: 上下文已打开；失败时 file 析构，关闭上下文。
        let found = unsafe { sys::avformat_find_stream_info(ctx, ptr::null_mut()) };
        if found < 0 {
            return Err(message(found));
        }
        Ok(file)
    }

    fn raw_streams(&self) -> &[*mut sys::AVStream] {
        // SAFETY: 上下文有效；streams 指向 nb_streams 个有效的流指针。
        unsafe {
            let ctx = &*self.ctx;
            if ctx.nb_streams == 0 {
                return &[];
            }
            std::slice::from_raw_parts(ctx.streams, ctx.nb_streams as usize)
        }
    }

    pub(crate) fn streams(&self) -> Vec<StreamInfo> {
        self.raw_streams()
            .iter()
            .map(|&stream| {
                // SAFETY: 流与它的 codecpar、元数据归上下文所有，此处只读并复制出来。
                unsafe {
                    let stream = &*stream;
                    let par = &*stream.codecpar;
                    let meta = |key: &CStr| {
                        sys::av_dict_get(stream.metadata, key.as_ptr(), ptr::null(), 0)
                            .as_ref()
                            .map(|entry| CStr::from_ptr(entry.value).to_string_lossy().into_owned())
                            .unwrap_or_default()
                    };
                    StreamInfo {
                        index: stream.index as usize,
                        kind: match par.codec_type {
                            sys::AVMEDIA_TYPE_VIDEO => Some(StreamKind::Video),
                            sys::AVMEDIA_TYPE_AUDIO => Some(StreamKind::Audio),
                            sys::AVMEDIA_TYPE_SUBTITLE => Some(StreamKind::Subtitle),
                            _ => None,
                        },
                        codec: CStr::from_ptr(sys::avcodec_get_name(par.codec_id))
                            .to_str()
                            .unwrap(),
                        time_base: (stream.time_base.num, stream.time_base.den),
                        language: meta(c"language"),
                        title: meta(c"title"),
                        default: stream.disposition & sys::AV_DISPOSITION_DEFAULT != 0,
                        forced: stream.disposition & sys::AV_DISPOSITION_FORCED != 0,
                    }
                }
            })
            .collect()
    }

    /// 第一路 `kind` 类的流。
    pub(crate) fn first(&self, kind: StreamKind) -> Option<StreamInfo> {
        self.streams().into_iter().find(|s| s.kind == Some(kind))
    }

    /// FFmpeg 认为最该用的一路 `kind` 类的流的下标（av_find_best_stream）。
    pub(crate) fn best(&self, kind: StreamKind) -> Option<usize> {
        let medium = match kind {
            StreamKind::Video => sys::AVMEDIA_TYPE_VIDEO,
            StreamKind::Audio => sys::AVMEDIA_TYPE_AUDIO,
            StreamKind::Subtitle => sys::AVMEDIA_TYPE_SUBTITLE,
        };
        // SAFETY: 上下文有效；不要求返回解码器。
        let index =
            unsafe { sys::av_find_best_stream(self.ctx, medium, -1, -1, ptr::null_mut(), 0) };
        usize::try_from(index).ok()
    }

    /// 读下一个包到 `self.packet`；读到末尾为 false。
    fn read_raw(&mut self) -> Result<bool, String> {
        // SAFETY: 上下文与包有效；先释放上一个包的内容。
        let read = unsafe {
            sys::av_packet_unref(self.packet);
            sys::av_read_frame(self.ctx, self.packet)
        };
        match read {
            0.. => Ok(true),
            sys::AVERROR_EOF => Ok(false),
            code => Err(message(code)),
        }
    }

    /// 读下一个包；读到末尾为 None。
    pub(crate) fn read(&mut self) -> Result<Option<Packet>, String> {
        if !self.read_raw()? {
            return Ok(None);
        }
        // SAFETY: 包刚读出，data 为空或指向 size 字节；此处复制出来。
        let packet = unsafe {
            let raw = &*self.packet;
            let present = |ts: i64| (ts != sys::AV_NOPTS_VALUE).then_some(ts);
            Packet {
                stream: raw.stream_index as usize,
                pts: present(raw.pts),
                dts: present(raw.dts),
                duration: raw.duration,
                data: if raw.data.is_null() {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(raw.data, raw.size as usize).to_vec()
                },
            }
        };
        Ok(Some(packet))
    }

    /// 读完剩下的包；读错时 panic。
    pub(crate) fn packets(mut self) -> Vec<Packet> {
        let mut packets = Vec::new();
        while let Some(packet) = self.read().unwrap() {
            packets.push(packet);
        }
        packets
    }
}

impl Drop for MediaFile {
    fn drop(&mut self) {
        // SAFETY: 包与上下文归本结构所有。
        unsafe {
            sys::av_packet_free(&raw mut self.packet);
            sys::avformat_close_input(&raw mut self.ctx);
        }
    }
}

/// 文件里各路流的编码名，按流的顺序。
pub(crate) fn codecs(path: &Path) -> Vec<&'static str> {
    let file = MediaFile::open(path).unwrap();
    file.streams().iter().map(|s| s.codec).collect()
}

/// 解码文件中的第一路视频（FFmpeg 认为最该用的一路）：(送进解码器的包数, 解码器拒收的包数, 解出的帧数)。
pub(crate) fn decode_video(path: &Path) -> (usize, usize, usize) {
    let mut file = MediaFile::open(path).unwrap();
    let index = file.best(StreamKind::Video).unwrap();
    // SAFETY: 流与 codecpar 归上下文所有；解码器上下文与帧由这里分配、最后释放；送进解码器的包在送入期间有效。
    unsafe {
        let par = (*file.raw_streams()[index]).codecpar;
        let codec = sys::avcodec_find_decoder((*par).codec_id);
        assert!(!codec.is_null(), "链接的 FFmpeg 有这路视频的解码器");
        let mut decoder = sys::avcodec_alloc_context3(codec);
        assert!(!decoder.is_null());
        assert!(sys::avcodec_parameters_to_context(decoder, par) >= 0);
        assert!(sys::avcodec_open2(decoder, codec, ptr::null_mut()) >= 0);
        let mut frame = sys::av_frame_alloc();
        assert!(!frame.is_null());
        let (mut packets, mut rejected, mut frames) = (0, 0, 0);
        loop {
            match file.read_raw() {
                Ok(true) if (*file.packet).stream_index as usize == index => {
                    packets += 1;
                    if sys::avcodec_send_packet(decoder, file.packet) < 0 {
                        rejected += 1;
                    }
                    while sys::avcodec_receive_frame(decoder, frame) >= 0 {
                        frames += 1;
                    }
                }
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => panic!("读取 {} 失败: {e}", path.display()),
            }
        }
        // 送入空包即读完，解码器交出剩下的帧
        assert!(sys::avcodec_send_packet(decoder, ptr::null()) >= 0);
        while sys::avcodec_receive_frame(decoder, frame) >= 0 {
            frames += 1;
        }
        sys::av_frame_free(&raw mut frame);
        sys::avcodec_free_context(&raw mut decoder);
        (packets, rejected, frames)
    }
}
