#!/usr/bin/env bash
# 不许用 #[allow(...)] / #[expect(...)]（含空格写法与 cfg_attr 包裹的）让检查闭嘴：被拦住时改代码，
# 或改 Cargo.toml 里的 lint 配置并写明理由。只查 crates/ 下的 .rs；third_party/ 下是上游代码。
set -euo pipefail

status=0
grep -rnE --include='*.rs' \
  '#!?\[[[:space:]]*(cfg_attr[[:space:]]*\(.*)?\b(allow|expect)[[:space:]]*\(' crates || status=$?
case $status in
  0)
    echo "以上位置用了 allow/expect 属性压制检查" >&2
    exit 1
    ;;
  1) exit 0 ;;
  *)
    echo "grep 执行失败（退出码 ${status}）" >&2
    exit "$status"
    ;;
esac
