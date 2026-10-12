#!/usr/bin/env bash
# 生成 remux 与 core 的测试样本：ffmpeg 合成画面与正弦音，按 1 秒切成 HLS 分片，不含第三方内容。
# 每个目录是一条轨在一个不连续段组里的分片（fMP4 另带 init.mp4）；各节目的时间戳都从头开始，
# 前后拼接即构成 EXT-X-DISCONTINUITY。需要带 libx264、libx265、libmp3lame、libaom、libvpx、libopus 的
# ffmpeg 命令行。
# 产物已提交到仓库，仅在需要重建时运行。
set -euo pipefail

DIR=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf -- "${TMP:?}"' EXIT

ff=(ffmpeg -hide_banner -loglevel error -y)
h264=(-c:v libx264 -preset veryfast -g 25 -b:v 150k)
aac=(-c:a aac -b:a 64k)

# 输入：$1 画面源，$2 尺寸，$3 正弦频率，$4 时长（秒）
source_args() {
  echo "-f lavfi -i $1=size=$2:rate=25 -f lavfi -i sine=frequency=$3:sample_rate=48000 -t $4"
}

# TS 分片：$1 目录名，其余为编码参数；只保留分片，丢弃播放列表
ts_segments() {
  local name=$1
  shift
  rm -rf -- "${DIR:?}/$name"
  mkdir -p "$DIR/$name"
  "${ff[@]}" "$@" -f hls -hls_time 1 -hls_playlist_type vod \
    -hls_segment_filename "$DIR/$name/seg%d.ts" "$TMP/$name.m3u8"
}

# fMP4 音视频分流：$1 目录名，其余为输入参数；产出 <目录>/video 与 <目录>/audio，各含 init.mp4 与 seg*.m4s
fmp4_split() {
  local name=$1
  shift
  local out=$TMP/$name
  "${ff[@]}" "$@" "${h264[@]}" "${aac[@]}" -map 0:v -map 1:a \
    -f hls -hls_segment_type fmp4 -hls_time 1 -hls_playlist_type vod \
    -var_stream_map "v:0,agroup:aud a:0,agroup:aud" -master_pl_name master.m3u8 \
    -hls_fmp4_init_filename init.mp4 -hls_segment_filename "$out/%v/seg%d.m4s" "$out/%v/index.m3u8"
  rm -rf -- "${DIR:?}/$name"
  for v in 0 1; do
    local track=$([[ $v == 0 ]] && echo video || echo audio)
    mkdir -p "$DIR/$name/$track"
    mv "$out/$v/init_$v.mp4" "$DIR/$name/$track/init.mp4"
    mv "$out/$v"/seg*.m4s "$DIR/$name/$track/"
  done
}

# fMP4 单轨（画面与声音在同一个分片里）：$1 目录名，其余为输入与编码参数；产出 <目录>/init.mp4 与 seg*.m4s
fmp4_muxed() {
  local name=$1
  shift
  local out=$TMP/$name
  mkdir -p "$out"
  "${ff[@]}" "$@" -f hls -hls_segment_type fmp4 -hls_time 1 -hls_playlist_type vod \
    -hls_fmp4_init_filename init.mp4 -hls_segment_filename "$out/seg%d.m4s" "$out/index.m3u8"
  rm -rf -- "${DIR:?}/$name"
  mkdir -p "$DIR/$name"
  mv "$out/init.mp4" "$out"/seg*.m4s "$DIR/$name/"
}

# 节目 A：2 秒；节目 B：1 秒，画面与音高不同
read -r -a a_src <<< "$(source_args testsrc2 320x180 440 2)"
read -r -a b_src <<< "$(source_args testsrc 320x180 880 1)"

ts_segments ts_a "${a_src[@]}" "${h264[@]}" "${aac[@]}"
ts_segments ts_b "${b_src[@]}" "${h264[@]}" "${aac[@]}"
fmp4_split fmp4_a "${a_src[@]}"
fmp4_split fmp4_b "${b_src[@]}"

# 编码与参数：分辨率与前面的组不同（合并应报参数变化）；HEVC、AC-3、E-AC-3、MP3（支持）；MP2 与 8 kHz 的 MP3
# 音频（不支持）
read -r -a small_src <<< "$(source_args testsrc2 160x90 440 1)"
ts_segments ts_small "${small_src[@]}" "${h264[@]}" "${aac[@]}"
read -r -a one_src <<< "$(source_args testsrc2 320x180 440 1)"
ts_segments ts_hevc "${one_src[@]}" -c:v libx265 -preset ultrafast -x265-params log-level=error -g 25 -b:v 150k "${aac[@]}"
ts_segments ts_mp3 "${one_src[@]}" "${h264[@]}" -c:a libmp3lame -b:a 64k
ts_segments ts_ac3 "${one_src[@]}" "${h264[@]}" -c:a ac3 -b:a 96k
ts_segments ts_eac3 "${one_src[@]}" "${h264[@]}" -c:a eac3 -b:a 96k
ts_segments ts_mp2 "${one_src[@]}" "${h264[@]}" -c:a mp2 -b:a 64k
ts_segments ts_mp3_8k "${one_src[@]}" "${h264[@]}" -c:a libmp3lame -ar 8000 -b:a 16k
# 只出现在 fMP4 里的编码：AV1 与 Opus、VP9 与 FLAC、H.264 与 ALAC
read -r -a tiny_src <<< "$(source_args testsrc2 160x90 440 2)"
fmp4_muxed fmp4_av1_opus "${tiny_src[@]}" -c:v libaom-av1 -cpu-used 8 -g 25 -b:v 100k -c:a libopus -b:a 48k
fmp4_muxed fmp4_vp9_flac "${tiny_src[@]}" -c:v libvpx-vp9 -deadline realtime -g 25 -b:v 100k -c:a flac
fmp4_muxed fmp4_alac "${tiny_src[@]}" -c:v libx264 -preset veryfast -g 25 -b:v 100k -c:a alac

# 打包音频（RFC 8216 3.4）：约 3 秒正弦音的裸音频帧按 1 秒切成 3 片，不带 ID3 标签，测试按要的时间戳加上。$1 目录名，
# $2 扩展名，其余为编码与切分参数
packed() {
  local name=$1 ext=$2
  shift 2
  rm -rf -- "${DIR:?}/packed/$name"
  mkdir -p "$DIR/packed/$name"
  "${ff[@]}" -f lavfi -i sine=frequency=440:sample_rate=48000 -t 2.95 "$@" \
    -f segment -segment_time 1 "$DIR/packed/$name/seg%d.$ext"
}
packed aac aac "${aac[@]}" -segment_format adts
packed ac3 ac3 -c:a ac3 -b:a 96k -segment_format ac3
packed eac3 ec3 -c:a eac3 -b:a 96k -segment_format eac3
packed mp3 mp3 -c:a libmp3lame -b:a 64k -segment_format mp3 -segment_format_options id3v2_version=0:write_xing=0

# 直播录制：4 秒、时间戳连续的节目，用于逐段放出、窗口滑动与缺失的分片
read -r -a long_src <<< "$(source_args testsrc2 160x90 440 4)"
ts_segments ts_long "${long_src[@]}" "${h264[@]}" "${aac[@]}"

# 边界重复一帧：两个 10 帧/秒、没有 B 帧的纯视频分片，第二个从第一个的最后一帧开始，两帧的解码时间戳相同
rm -rf -- "${DIR:?}/ts_repeat"
mkdir -p "$DIR/ts_repeat"
repeat=(-f lavfi -i testsrc2=size=160x90:rate=10)
repeat_enc=(-c:v libx264 -preset veryfast -bf 0 -g 10 -an -f mpegts)
"${ff[@]}" "${repeat[@]}" -t 1 "${repeat_enc[@]}" "$DIR/ts_repeat/seg0.ts"
"${ff[@]}" "${repeat[@]}" -ss 0.9 -t 1 -output_ts_offset 0.9 "${repeat_enc[@]}" "$DIR/ts_repeat/seg1.ts"

# E-AC-3 每帧 3 块（码率高于 1.024 Mbps 时 FFmpeg 的编码器这样编），合并应报放不进 MP4
rm -rf -- "${DIR:?}/ts_eac3_short"
mkdir -p "$DIR/ts_eac3_short"
"${ff[@]}" -f lavfi -i sine=frequency=440:sample_rate=48000 -t 0.5 -ac 6 -c:a eac3 -b:a 1500k -f mpegts \
  "$DIR/ts_eac3_short/seg0.ts"

# 组内换参数：两个时间戳相接（第二个晚 0.1 秒开始）、没有 B 帧的分片，第二个的分辨率与采样率都不同
rm -rf -- "${DIR:?}/ts_param_change"
mkdir -p "$DIR/ts_param_change"
change_enc=(-c:v libx264 -preset veryfast -bf 0 -g 10 -b:v 150k "${aac[@]}" -f mpegts)
"${ff[@]}" -f lavfi -i testsrc2=size=320x180:rate=10 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 1 \
  "${change_enc[@]}" "$DIR/ts_param_change/seg0.ts"
"${ff[@]}" -f lavfi -i testsrc2=size=160x90:rate=10 -f lavfi -i sine=frequency=440:sample_rate=44100 -t 1 \
  -output_ts_offset 1.1 "${change_enc[@]}" "$DIR/ts_param_change/seg1.ts"

# 回绕点前：一个纯视频分片，第一个时间戳比 33 位回绕点前 60 秒早 0.05 秒（mpegts 封装器另加 1.4 秒的起始延迟）
rm -rf -- "${DIR:?}/ts_wrap"
mkdir -p "$DIR/ts_wrap"
"${ff[@]}" "${repeat[@]}" -t 1 -output_ts_offset 95382.26768 "${repeat_enc[@]}" "$DIR/ts_wrap/seg0.ts"
# 回绕点前 60 秒之后：一个纯音频分片，第一个时间戳比那条界线晚约 0.03 秒，与 ts_wrap 跨在界线两侧
rm -rf -- "${DIR:?}/ts_wrap_audio"
mkdir -p "$DIR/ts_wrap_audio"
"${ff[@]}" -f lavfi -i sine=frequency=440:sample_rate=48000 -t 1 "${aac[@]}" -output_ts_offset 95382.36768 \
  -f mpegts "$DIR/ts_wrap_audio/seg0.ts"

find "$DIR" -type f ! -name generate.sh | sort | xargs ls -l
