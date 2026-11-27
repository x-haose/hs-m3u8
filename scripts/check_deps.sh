#!/usr/bin/env bash
# 边界检查，命中时打印位置并以 1 退出：
# - 依赖方向：各 crate 的正常依赖与构建依赖（全部目标平台）中不得出现所列的包。测试依赖不在此列。
# - hs-m3u8-remux 的 unsafe 只在 src/ffi 下（FFmpeg 的薄封装）；测试不在此列。
set -euo pipefail

# 每行：<包> <不得依赖的包>...，含传递依赖
rules=(
  "hs-m3u8-hls tokio reqwest hs-m3u8-ffmpeg-sys hs-m3u8-remux hs-m3u8-sysproxy hs-m3u8-core"
  "hs-m3u8-remux tokio reqwest hs-m3u8-hls hs-m3u8-sysproxy hs-m3u8-core"
  "hs-m3u8-sysproxy tokio reqwest hs-m3u8-hls hs-m3u8-remux hs-m3u8-core hs-m3u8-ffmpeg-sys"
  "hs-m3u8-ffmpeg-sys hs-m3u8-hls hs-m3u8-remux hs-m3u8-subtitle hs-m3u8-sysproxy hs-m3u8-core hs-m3u8-py"
  "hs-m3u8-core pyo3"
)

# 每行：<包> <不得直接依赖的包>...：只经由 core 的再导出（core::hls、core::remux）使用
direct_rules=(
  "hs-m3u8-py hs-m3u8-hls hs-m3u8-remux hs-m3u8-sysproxy"
)

tree() {
  cargo tree --locked -e normal,build --target all "$@"
}

status=0
for rule in "${rules[@]}"; do
  read -r -a words <<< "$rule"
  package=${words[0]}
  deps=$(tree -p "$package" --prefix none --format '{p}' | awk '{print $1}' | sort -u)
  for name in "${words[@]:1}"; do
    if grep -qx -- "$name" <<< "$deps"; then
      echo "${package} 不得依赖 ${name}，依赖路径：" >&2
      tree -p "$package" -i "$name" >&2
      status=1
    fi
  done
done
for rule in "${direct_rules[@]}"; do
  read -r -a words <<< "$rule"
  package=${words[0]}
  deps=$(tree -p "$package" --depth 1 --prefix none --format '{p}' | awk '{print $1}' | sort -u)
  for name in "${words[@]:1}"; do
    if grep -qx -- "$name" <<< "$deps"; then
      echo "${package} 不得直接依赖 ${name}，经由 hs-m3u8-core 的再导出使用" >&2
      status=1
    fi
  done
done
# unsafe 块、函数、impl、trait、extern 块与属性（`#[unsafe(no_mangle)]`）：unsafe 后跟 `{`、`(` 或这几个关键字；注释里
# 写成这样的也算
unsafe_outside=$(git grep --untracked -nE '(^|[^[:alnum:]_])unsafe[[:space:]]*(\{|\(|fn|impl|trait|extern)' -- \
  'crates/remux/src/*.rs' ':!crates/remux/src/ffi/*' || [[ $? == 1 ]])
if [[ -n $unsafe_outside ]]; then
  echo "$unsafe_outside" >&2
  echo "以上位置在 crates/remux/src/ffi 之外用了 unsafe" >&2
  status=1
fi
exit "$status"
