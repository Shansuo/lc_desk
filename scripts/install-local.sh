#!/usr/bin/env bash
# 源码目录专用：构建 → 装成 LC-Deck.app → 建桌面快捷入口 → 启动。
#
# 和 install-macos.sh 的分工：
#   install-macos.sh  给「下载发布包的人」用，二进制就在脚本旁边；
#   本脚本            跑在源码目录里，先把 target/release/lc_deck 构建出来再装。
#
# 装到 ~/Applications 而不是直接放桌面：屏幕录制 / 辅助功能授权是按 bundle id
# 记录的，位置稳定、id 固定（top.swwarn.lcdeck）才能跨版本沿用，不必反复授权。
# 桌面入口是指向它的符号链接，双击、拖进 Dock 都正常。
#
# 用法:
#   ./scripts/install-local.sh             # 构建 + 安装 + 建桌面入口 + 启动
#   ./scripts/install-local.sh --no-run    # 只安装，不启动
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="$ROOT/target/release/lc_deck"
APPS="$HOME/Applications"
APP="$APPS/LC-Deck.app"
DESKTOP="$HOME/Desktop"
ENTRY="$DESKTOP/LC-Deck.app"
ICON_DIR="$SCRIPT_DIR/icon-build"
PYTHON="${PYTHON:-python3}"

RUN=1
[[ "${1:-}" == "--no-run" ]] && RUN=0

echo "==> 1/4 构建 release"
cargo build --release --manifest-path "$ROOT/Cargo.toml"

echo "==> 2/4 准备图标"
if [[ -f "$ICON_DIR/LC-Deck.icns" ]]; then
  echo "     已存在，跳过（删掉 $ICON_DIR 可重新生成）"
else
  "$PYTHON" "$SCRIPT_DIR/gen-icon.py" "$ICON_DIR"
fi

echo "==> 3/4 打包 LC-Deck.app"
LC_NO_OPEN=1 LC_ICON="$ICON_DIR/LC-Deck.icns" \
  "$SCRIPT_DIR/install-macos.sh" "$APPS" "$BIN"

echo "==> 4/4 建桌面快捷入口"
mkdir -p "$DESKTOP"
if [[ -L "$ENTRY" ]]; then
  rm -f "$ENTRY"
elif [[ -e "$ENTRY" ]]; then
  echo "     桌面已有同名项且不是本脚本建的软链，跳过：$ENTRY"
  ENTRY=""
fi
if [[ -n "$ENTRY" ]]; then
  ln -snf "$APP" "$ENTRY"
  echo "     $ENTRY -> $APP"
fi

if [[ "$RUN" == "1" ]]; then
  echo
  echo "==> 启动 LC-Deck"
  open "$APP"
else
  echo
  echo "已安装完成，双击桌面上的 LC-Deck 即可启动。"
fi
