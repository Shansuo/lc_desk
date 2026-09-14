#!/usr/bin/env bash
# 本地打包脚本：构建当前平台 release 二进制并归档。
# 用法: ./scripts/package.sh
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')
OS=$(uname -s)
ARCH=$(uname -m)
NAME="lc_deck-v${VERSION}-${OS}-${ARCH}"
OUT="dist/${NAME}"

echo "==> 构建 release..."
cargo build --release

mkdir -p "$OUT"
BIN="target/release/lc_deck"
[[ "$OS" == "Windows_NT" ]] && BIN="target/release/lc_deck.exe"
cp "$BIN" "$OUT/"
cp README.md "$OUT/" 2>/dev/null || true

cd dist
if [[ "$OS" == "Windows_NT" ]]; then
  zip -r "${NAME}.zip" "${NAME}"
else
  tar czf "${NAME}.tar.gz" "${NAME}"
fi
cd ..

echo "==> 完成: dist/${NAME}"
