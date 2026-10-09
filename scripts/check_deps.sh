#!/usr/bin/env bash
# 依赖方向检查（docs/design/architecture.md 第 3 节）：各 crate 的正常依赖与构建依赖（含传递依赖、
# 全部目标平台）中不得出现所列的包。测试依赖不在此列。命中时打印依赖路径并以 1 退出。
set -euo pipefail

# 每行：<包> <不得依赖的包>...
rules=(
  "hs-m3u8-hls tokio reqwest ffmpeg-next ffmpeg-sys-next hs-m3u8-remux hs-m3u8-core"
  "hs-m3u8-remux tokio reqwest hs-m3u8-hls hs-m3u8-core"
  "hs-m3u8-core tauri pyo3"
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
exit "$status"
