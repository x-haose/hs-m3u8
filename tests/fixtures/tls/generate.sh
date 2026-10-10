#!/usr/bin/env bash
# 生成 core 测试用的自签名证书：CN 为 localhost、另含 IP 127.0.0.1，有效期 100 年，EC P-256；只给测试里的本地 TLS
# 服务用，不受任何系统信任，私钥不是秘密。cert.der 为证书，key.der 为 PKCS#8 私钥，都是 DER。需要 openssl 命令行。
# 产物已提交到仓库，仅在需要重建时运行。
set -euo pipefail

DIR=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf -- "${TMP:?}"' EXIT

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 36500 \
  -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" 2>/dev/null
openssl x509 -in "$TMP/cert.pem" -outform DER -out "$DIR/cert.der"
openssl pkcs8 -topk8 -nocrypt -in "$TMP/key.pem" -outform DER -out "$DIR/key.der"
