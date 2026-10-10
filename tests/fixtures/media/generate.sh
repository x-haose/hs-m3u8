#!/usr/bin/env bash
# 生成 remux 与 core 的测试样本：ffmpeg 合成画面与正弦音，按 1 秒切成 HLS 分片，不含第三方内容。
# 每个目录是一条轨在一个不连续段组里的分片（fMP4 另带 init.mp4）；各节目的时间戳都从头开始，
# 前后拼接即构成 EXT-X-DISCONTINUITY。需要带 libx264、libx265、libmp3lame 的 ffmpeg 命令行。
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

# 节目 A：2 秒；节目 B：1 秒，画面与音高不同
read -r -a a_src <<< "$(source_args testsrc2 320x180 440 2)"
read -r -a b_src <<< "$(source_args testsrc 320x180 880 1)"

ts_segments ts_a "${a_src[@]}" "${h264[@]}" "${aac[@]}"
ts_segments ts_b "${b_src[@]}" "${h264[@]}" "${aac[@]}"
fmp4_split fmp4_a "${a_src[@]}"
fmp4_split fmp4_b "${b_src[@]}"

# 编码与参数：分辨率与前面的组不同（合并应报参数变化）；HEVC（支持）；MP3 音频（不支持）
read -r -a small_src <<< "$(source_args testsrc2 160x90 440 1)"
ts_segments ts_small "${small_src[@]}" "${h264[@]}" "${aac[@]}"
read -r -a one_src <<< "$(source_args testsrc2 320x180 440 1)"
ts_segments ts_hevc "${one_src[@]}" -c:v libx265 -preset ultrafast -x265-params log-level=error -g 25 -b:v 150k "${aac[@]}"
ts_segments ts_mp3 "${one_src[@]}" "${h264[@]}" -c:a libmp3lame -b:a 64k

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

find "$DIR" -type f ! -name generate.sh | sort | xargs ls -l
