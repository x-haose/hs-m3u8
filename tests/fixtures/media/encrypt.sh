#!/usr/bin/env bash
# 由 ts_a 生成 AES-128 加密的分片 aes/seg0.ts、aes/seg1.ts：key 为 00 01 … 0f，两个分片的 IV 都是 10 11 … 1f，PKCS#7
# 填充。参数固定，结果可复现；ts_a 重新生成后要重跑。需要 openssl 命令行。产物已提交到仓库，仅在需要重建时运行。
set -euo pipefail

DIR=$(cd "$(dirname "$0")" && pwd)
KEY=000102030405060708090a0b0c0d0e0f
IV=101112131415161718191a1b1c1d1e1f

rm -rf -- "${DIR:?}/aes"
mkdir -p "$DIR/aes"
for n in 0 1; do
  openssl enc -aes-128-cbc -K "$KEY" -iv "$IV" -in "$DIR/ts_a/seg$n.ts" -out "$DIR/aes/seg$n.ts"
done
ls -l "$DIR/aes"
