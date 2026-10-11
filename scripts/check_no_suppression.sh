#!/usr/bin/env bash
# 不许用属性或注释让检查闭嘴：被拦住时改代码，或改 Cargo.toml、pyproject.toml 里的配置并写明理由。
# - Rust：#[allow(...)]、#![allow(...)]、#[expect(...)]，含 cfg_attr 包裹的，查 crates/ 下的 .rs；third_party/ 下是上游
#   代码。格式检查（rs_fmt）在前：rustfmt 去掉 # 与 [ 之间的空格、把短的属性写成一行，所以只有两种写法，属性写在一行
#   里，或很长的 cfg_attr 跨行、allow( 单独起一行。
# - Python：noqa（含 ruff: noqa、flake8: noqa 整文件的）、type: ignore、pyright:、mypy:、fmt: off/skip、isort: skip，不分
#   大小写，查 python/、tests/python/、scripts/、examples/ 与 README（其中的示例要过类型检查）；生成的 __init__.py
#   与存根除外（生成器写入 ruff: noqa 文件头，它们本就不格式化、不检查）。
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
  -e '#!?\[[[:space:]]*(cfg_attr[[:space:]]*\(.*[^[:alnum:]_])?(allow|expect)[[:space:]]*\(' \
  -e '^[[:space:]]*(allow|expect)[[:space:]]*\(' -- 'crates/*.rs'
check "用了注释压制检查" git grep --untracked -niE \
  -e '#[[:space:]]*((ruff|flake8)[[:space:]]*:[[:space:]]*)?noqa' \
  -e '#[[:space:]]*type[[:space:]]*:[[:space:]]*ignore' \
  -e '#[[:space:]]*(pyright|mypy)[[:space:]]*:' \
  -e '#[[:space:]]*(fmt[[:space:]]*:[[:space:]]*(off|skip)|isort[[:space:]]*:[[:space:]]*skip)' -- \
  'python/*.py' 'python/*.pyi' 'tests/python/*.py' 'scripts/*.py' 'examples/*.py' README.md \
  ':!python/hs_m3u8/__init__.py' ':!python/hs_m3u8/_native/__init__.pyi'
exit "$found"
