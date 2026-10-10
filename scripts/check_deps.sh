#!/usr/bin/env bash
# 依赖方向检查（docs/design/architecture.md 第 3 节）：各 crate 的正常依赖与构建依赖（全部目标平台）中不得出现
# 所列的包。测试依赖不在此列。命中时打印依赖路径并以 1 退出。
set -euo pipefail

# 每行：<包> <不得依赖的包>...，含传递依赖
rules=(
  "hs-m3u8-hls tokio reqwest ffmpeg-next ffmpeg-sys-next hs-m3u8-remux hs-m3u8-core"
  "hs-m3u8-remux tokio reqwest hs-m3u8-hls hs-m3u8-core"
  "hs-m3u8-core tauri pyo3"
)

# 每行：<包> <不得直接依赖的包>...：只经由 core 的再导出（core::hls、core::remux）使用
direct_rules=(
  "hs-m3u8-py hs-m3u8-hls hs-m3u8-remux"
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
exit "$status"
