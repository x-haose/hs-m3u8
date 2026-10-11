#!/usr/bin/env bash
# 不许用属性或注释让检查闭嘴：被拦住时改代码，或改 Cargo.toml、pyproject.toml 里的配置并写明理由。
# - Rust：#[allow(...)] / #[expect(...)]（含空格写法与 cfg_attr 包裹的），查 crates/ 下的 .rs；third_party/ 下是上游代码。
# - Python：# noqa、# type: ignore、# pyright:、# mypy:，查 python/ 与 tests/python/；生成的 __init__.py 与存根除外
#   （生成器写入的 ruff 文件头；它们本就不格式化、不检查）。
# 用 git grep 查已跟踪与未跟踪（未忽略）的文件。
set -euo pipefail

found=0

# 运行查找命令 "${@:2}"：找到时打印 $1，找不到时什么也不做，命令出错时退出。
check() {
  local status=0
  "${@:2}" || status=$?
  case $status in
    0)
      echo "以上位置$1" >&2
      found=1
      ;;
    1) ;;
    *)
      echo "查找失败（退出码 ${status}）" >&2
      exit "$status"
      ;;
  esac
}

check "用了 allow/expect 属性压制检查" git grep --untracked -nE \
  '#!?\[[[:space:]]*(cfg_attr[[:space:]]*\(.*[^[:alnum:]_])?(allow|expect)[[:space:]]*\(' -- 'crates/*.rs'
check "用了注释压制检查" git grep --untracked -nE \
  '#[[:space:]]*(noqa|type:[[:space:]]*ignore|pyright:|mypy:)' -- \
  'python/*.py' 'python/*.pyi' 'tests/python/*.py' \
  ':!python/hs_m3u8/__init__.py' ':!python/hs_m3u8/_native/__init__.pyi'
exit "$found"
