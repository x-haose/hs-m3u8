#!/usr/bin/env bash
# 下载并编译本项目使用的精简 FFmpeg 静态库，安装到 third_party/ffmpeg/dist（可用第一个参数改安装目录）。
# 组件只含转封装所需（docs/adr/0002）；configure 得出的许可证不是 LGPL 时失败。
# Windows：在已加载 MSVC 环境的 MSYS2 bash 中运行，产物为 MSVC 静态库（*.lib）。
set -euo pipefail

VERSION=9.0.2
# 官方发布 tarball 的 SHA-256；该文件已用 FFmpeg 发布签名密钥 FCF986EA15E6E293A5644F10B4322F04D67658D8 验签
SHA256=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e

ROOT=$(cd "$(dirname "$0")" && pwd)
PREFIX=${1:-$ROOT/dist}
WORK=$ROOT/build
TARBALL=$WORK/ffmpeg-$VERSION.tar.xz
SRC=$WORK/ffmpeg-$VERSION

case "$(uname -s)" in
  MINGW* | MSYS*) PLATFORM=windows ;;
  Darwin) PLATFORM=macos ;;
  Linux) PLATFORM=linux ;;
  *) echo "不支持的平台: $(uname -s)" >&2; exit 1 ;;
esac

sha256_of() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

mkdir -p "$WORK"
if [[ ! -f "$TARBALL" ]] || [[ "$(sha256_of "$TARBALL")" != "$SHA256" ]]; then
  curl --fail --location --silent --show-error -o "$TARBALL.part" "https://ffmpeg.org/releases/ffmpeg-$VERSION.tar.xz"
  mv "$TARBALL.part" "$TARBALL"
fi
actual=$(sha256_of "$TARBALL")
if [[ "$actual" != "$SHA256" ]]; then
  echo "ffmpeg-$VERSION.tar.xz 的 SHA-256 不符：期望 $SHA256，实际 $actual" >&2
  exit 1
fi

rm -rf -- "${SRC:?}" "${PREFIX:?}"
tar -xJf "$TARBALL" -C "$WORK"

# --disable-asm：转封装不经过编解码热点路径，汇编优化无收益，关闭后不依赖 nasm
# --enable-pic：静态库要链接进 Python 扩展模块（共享库）
flags=(
  --prefix="$PREFIX"
  --enable-static --disable-shared --disable-programs --disable-doc --disable-autodetect --disable-network
  --disable-asm --enable-pic
  --disable-everything --disable-avdevice --disable-avfilter --disable-swscale --disable-swresample
  --enable-protocol=file
  --enable-demuxer=mpegts,mov,aac
  --enable-muxer=mp4
  --enable-parser=h264,hevc,aac
  --enable-decoder=h264,hevc,aac
  --enable-bsf=aac_adtstoasc,extract_extradata
)
if [[ $PLATFORM == windows ]]; then
  flags+=(--toolchain=msvc --target-os=win64 --arch=x86_64)
fi

cd "$SRC"
./configure "${flags[@]}" | tee "$WORK/configure.log"
if ! grep -q '^License: LGPL version 2.1 or later' "$WORK/configure.log"; then
  echo "FFmpeg 配置出的许可证不是 LGPL 2.1+，见 $WORK/configure.log" >&2
  exit 1
fi

jobs=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN)
make -j"$jobs"
make install

# ffmpeg-sys-next 按这些文件名链接：MSVC 目标找 <名>.lib，其余找 lib<名>.a
if [[ $PLATFORM == windows ]]; then
  libs=(avformat.lib avcodec.lib avutil.lib)
else
  libs=(libavformat.a libavcodec.a libavutil.a)
fi
for lib in "${libs[@]}"; do
  if [[ ! -f "$PREFIX/lib/$lib" ]]; then
    echo "安装目录缺少 $PREFIX/lib/$lib" >&2
    exit 1
  fi
done

echo "FFmpeg $VERSION 已安装到 $PREFIX"
