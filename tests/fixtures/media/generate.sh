#!/usr/bin/env bash
# 生成 remux 测试样本：2 秒、320x180、ffmpeg 合成画面与正弦音，不含第三方内容。
# 需要带 libx264、libx265 的 ffmpeg 命令行。产物已提交到仓库，仅在需要重建时运行。
set -euo pipefail

DIR=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf -- "${TMP:?}"' EXIT

src=(-f lavfi -i "testsrc2=size=320x180:rate=25" -f lavfi -i "sine=frequency=440:sample_rate=48000" -t 2)
ff=(ffmpeg -hide_banner -loglevel error -y)

# TS：H.264 + AAC（ADTS）
"${ff[@]}" "${src[@]}" -c:v libx264 -preset veryfast -g 25 -b:v 200k -c:a aac -b:a 64k -f mpegts "$DIR/h264_aac.ts"

# TS：HEVC + AAC
"${ff[@]}" "${src[@]}" -c:v libx265 -preset ultrafast -x265-params log-level=error -g 25 -b:v 200k \
  -c:a aac -b:a 64k -f mpegts "$DIR/hevc_aac.ts"

# 音视频分流：fMP4 HLS，视频与音频为两条 rendition；每条按 init + 分片顺序拼成一个文件，即下载器拿到的形态
"${ff[@]}" "${src[@]}" -c:v libx264 -preset veryfast -g 25 -b:v 200k -c:a aac -b:a 64k \
  -map 0:v -map 1:a -f hls -hls_segment_type fmp4 -hls_time 1 -hls_playlist_type vod \
  -var_stream_map "v:0,agroup:aud a:0,agroup:aud" -master_pl_name master.m3u8 \
  -hls_fmp4_init_filename init.mp4 -hls_segment_filename "$TMP/track_%v/seg%d.m4s" "$TMP/track_%v/index.m3u8"
for v in 0 1; do
  name=$([[ $v == 0 ]] && echo split_video.mp4 || echo split_audio.mp4)
  {
    cat "$TMP/track_$v/init_$v.mp4"
    grep -v '^#' "$TMP/track_$v/index.m3u8" | while read -r segment; do cat "$TMP/track_$v/$segment"; done
  } > "$DIR/$name"
done

ls -l "$DIR"
