//! 参数随帧携带的输入（TS 与打包音频，没有 MP4 那样的样本描述）里每个包的编码参数。MP4 的一条轨只有一个样本描述，
//! 由开头的包得出；同一路流后面的包参数变了，样本描述就与它们不符，如采样率变了时播放的音调与速度都不对。音频的
//! 参数从帧头读出（见 [`crate::frame`]）；视频（H.264、HEVC）的画面宽高与像素格式取 FFmpeg 解复用器的解析器从参数集
//! 得出的值。

use ffmpeg::codec;
use ffmpeg::format::stream::Stream;
use ffmpeg_next as ffmpeg;

use crate::{ffi, frame};

/// 一路流在一组里的参数核对：编码不变，每个包的参数须与第一个包的相同。第 0 组之后的组从第 0 组的核对开始（见
/// [`Inband::for_next_group`]），第一个包就与第 0 组的比较。
#[derive(Clone)]
pub(crate) struct Inband {
    /// 这路流开始时的编码。TS 的 PMT 中途改了这个 PID 的流类型时，FFmpeg 换掉流的编码并重建解析器
    /// （mpegts.c 的 mpegts_set_stream_info）
    codec: codec::Id,
    params: Params,
    /// 第一个包的参数取自第 0 组，这一组还没有核对过读得出的包
    leading: bool,
}

#[derive(Clone)]
enum Params {
    Audio {
        framing: frame::Framing,
        first: Option<AudioParams>,
    },
    Video {
        /// 画面的宽、高与像素格式值（见 [`ffi::parsed_picture`]）
        first: Option<(u32, u32, i32)>,
    },
}

impl Inband {
    /// 编码 `id` 的核对；不能逐帧读出参数的编码为 None。
    pub(crate) fn of(id: codec::Id) -> Option<Inband> {
        let framing = match id {
            codec::Id::H264 | codec::Id::HEVC => {
                return Some(Inband {
                    codec: id,
                    params: Params::Video { first: None },
                    leading: false,
                });
            }
            codec::Id::AAC => frame::Framing::Adts,
            codec::Id::AC3 | codec::Id::EAC3 => frame::Framing::Ac3,
            codec::Id::MP3 => frame::Framing::Mpeg,
            _ => return None,
        };
        Some(Inband {
            codec: id,
            params: Params::Audio {
                framing,
                first: None,
            },
            leading: false,
        })
    }

    /// 以这路流（第 0 组的，已读到第一个包）为准核对后面一组的同一路流。
    pub(crate) fn for_next_group(&self) -> Inband {
        Inband {
            leading: true,
            ..self.clone()
        }
    }

    /// 核对 `stream` 的一个包 `data`。解析器还没读到参数集的视频包不核对。
    pub(crate) fn check(&mut self, data: &[u8], stream: &Stream<'_>) -> Result<(), Mismatch> {
        let differs = if self.leading {
            Mismatch::Config
        } else {
            Mismatch::Changed
        };
        if stream.parameters().id() != self.codec {
            return Err(differs);
        }
        let same = match &mut self.params {
            Params::Audio { framing, first } => {
                let params = audio_params(*framing, data).ok_or(Mismatch::Unreadable)?;
                self.leading = false;
                if let AudioParams::Ac3(frames) = &params {
                    check_eac3(frames)?;
                }
                *first.get_or_insert_with(|| params.clone()) == params
            }
            Params::Video { first } => match ffi::parsed_picture(stream) {
                Some(picture) => {
                    self.leading = false;
                    *first.get_or_insert(picture) == picture
                }
                None => true,
            },
        };
        if same { Ok(()) } else { Err(differs) }
    }
}

/// 一个包通不过核对的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mismatch {
    /// 编码变了，或参数与这一组第一个包的不同
    Changed,
    /// 这一组第一个读得出的包与第 0 组的编码或参数不同
    Config,
    /// 音频包读不出帧头或帧长与包长对不上：数据损坏（如缺片处前一个 PES 剩下的字节被解析器拼进下一个包）
    Unreadable,
    /// E-AC-3 每帧不足 6 块：MP4 封装器把几帧并成一个 6 块（1536 个采样点）的样本，末尾凑不满的帧不写出
    Eac3ShortFrames,
    /// E-AC-3 有多个独立子流：MP4 封装器不支持
    Eac3Substreams,
}

/// E-AC-3 的帧结构 MP4 封装器能否处理（FFmpeg 9.0.2 movenc 的 handle_eac3）。
fn check_eac3(frames: &[frame::Ac3Frame]) -> Result<(), Mismatch> {
    for frame in frames {
        if frame.blocks < 6 {
            return Err(Mismatch::Eac3ShortFrames);
        }
        // strmtyp 为 1 的是从属子流，其余是独立子流
        if matches!(frame.substream, Some((strmtyp, id)) if strmtyp != 1 && id != 0) {
            return Err(Mismatch::Eac3Substreams);
        }
    }
    Ok(())
}

/// 一个音频包的编码参数；只与同一路流的其他包比较。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AudioParams {
    /// ADTS 与 MPEG 音频：包里的一帧。MP4 的 AudioSpecificConfig 由第一帧的 ADTS 帧头写成
    Frame(frame::Header),
    /// 包里各同步帧（E-AC-3 为独立帧及其从属帧）
    Ac3(Vec<frame::Ac3Frame>),
}

/// 包 `data` 的参数：AC-3 与 E-AC-3 为包里的各同步帧，须恰好由整帧组成；其余为一帧，帧长须等于包长。读不出时为 None。
fn audio_params(framing: frame::Framing, data: &[u8]) -> Option<AudioParams> {
    if framing != frame::Framing::Ac3 {
        let (header, len) = frame::header(framing, data)?;
        return (len == Some(data.len())).then_some(AudioParams::Frame(header));
    }
    let mut frames = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (frame, size) = frame::ac3(rest)?;
        frames.push(frame);
        rest = rest.get(size..)?;
    }
    (!frames.is_empty()).then_some(AudioParams::Ac3(frames))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::testing::{ac3_syncframe, eac3_syncframe};

    fn ac3(data: &[u8]) -> Option<AudioParams> {
        audio_params(frame::Framing::Ac3, data)
    }

    fn frames(data: &[u8]) -> Vec<frame::Ac3Frame> {
        match ac3(data) {
            Some(AudioParams::Ac3(frames)) => frames,
            other => panic!("应读出 AC-3 帧：{other:?}"),
        }
    }

    #[test]
    fn eac3_packets_list_every_substream_frame() {
        let independent = eac3_syncframe(0, 7, None, 384);
        let dependent = eac3_syncframe(1, 2, Some(0x0A00), 128);
        let five_one = ac3(&independent).unwrap();
        let seven_one = ac3(&[independent.clone(), dependent.clone()].concat()).unwrap();
        assert_ne!(five_one, seven_one);
        let other_map = eac3_syncframe(1, 2, Some(0x0500), 128);
        assert_ne!(
            ac3(&[independent.clone(), other_map].concat()),
            Some(seven_one)
        );
        // AC-3 帧之后跟 E-AC-3 从属帧
        let core = ac3_syncframe(8, 7, true);
        assert_eq!(frames(&[core.clone(), dependent].concat()).len(), 2);
        // 帧长对不上包长
        assert_eq!(ac3(&core[..200]), None);
    }

    #[test]
    fn eac3_frames_need_six_blocks_and_one_independent_substream() {
        let independent = eac3_syncframe(0, 7, None, 384);
        let dependent = eac3_syncframe(1, 2, Some(0x0A00), 128);
        assert_eq!(
            check_eac3(&frames(&[independent.clone(), dependent].concat())),
            Ok(())
        );
        assert_eq!(check_eac3(&frames(&ac3_syncframe(8, 7, true))), Ok(()));
        // numblkscod 为 2：每帧 3 块
        let mut three_blocks = independent.clone();
        three_blocks[4] &= !0b0001_0000;
        assert_eq!(
            check_eac3(&frames(&three_blocks)),
            Err(Mismatch::Eac3ShortFrames)
        );
        // 第二个独立子流（substreamid 1）
        let mut second = independent.clone();
        second[2] |= 0b0000_1000;
        assert_eq!(
            check_eac3(&frames(&[independent, second].concat())),
            Err(Mismatch::Eac3Substreams)
        );
    }
}
