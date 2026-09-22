#!/usr/bin/env bash
# macOS 一键安装 LC-Deck。
#
# 解决两件事：
#   1. Release 产物未经 Apple 签名，浏览器下载后会被打上 com.apple.quarantine，
#      双击弹「无法验证…恶意软件」。这里清除该标记。
#   2. 直接分发裸二进制时，屏幕录制 / 辅助功能权限绑定在二进制的 ad-hoc 签名上，
#      每次升级都要重新授权。这里包装成 .app 并固定 CFBundleIdentifier，
#      权限可跨版本保留。
#
# 用法:
#   ./install-macos.sh                 # 安装到 /Applications
#   ./install-macos.sh ~/Applications  # 安装到指定目录（无需 sudo）
#
# 可选:
#   第 2 个参数  指定 lc_deck 二进制路径（默认取本脚本同目录下的 lc_deck，
#                供源码目录使用：./install-macos.sh ~/Applications ../target/release/lc_deck）
#   环境变量 LC_ICON  指定 .icns 路径，用于给 .app 加图标（默认取同目录 LC-Deck.icns）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC="${2:-$SCRIPT_DIR/lc_deck}"
TARGET_ROOT="${1:-/Applications}"
APP="$TARGET_ROOT/LC-Deck.app"
ICON="${LC_ICON:-$SCRIPT_DIR/LC-Deck.icns}"

if [[ ! -f "$SRC" ]]; then
  echo "错误：找不到二进制 $SRC，请在解压后的目录里运行本脚本，或用第 2 个参数指定路径。" >&2
  exit 1
fi

if [[ "$TARGET_ROOT" == "/Applications" ]] && [[ ! -w /Applications ]]; then
  echo "没有 /Applications 的写权限，将安装到 ~/Applications。" >&2
  TARGET_ROOT="$HOME/Applications"
  APP="$TARGET_ROOT/LC-Deck.app"
fi

echo "==> 安装到 $APP"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$SRC" "$APP/Contents/MacOS/lc_deck"
chmod 755 "$APP/Contents/MacOS/lc_deck"

# 图标（可选）：CFBundleIconFile 无条件写入，缺图标文件时 Finder 会自动
# 回落到默认图标，不会出错。
HAS_ICON="no"
if [[ -f "$ICON" ]]; then
  mkdir -p "$APP/Contents/Resources"
  cp "$ICON" "$APP/Contents/Resources/LC-Deck.icns"
  HAS_ICON="yes"
fi

# CFBundleIdentifier 固定不变：macOS 的屏幕录制/辅助功能授权按 bundle id 记录，
# 保持恒定才能跨版本沿用，否则每次升级都要重新授权。
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>
  <string>LC-Deck</string>
  <key>CFBundleDisplayName</key>
  <string>LC-Deck</string>
  <key>CFBundleIdentifier</key>
  <string>top.swwarn.lcdeck</string>
  <key>CFBundleExecutable</key>
  <string>lc_deck</string>
  <key>CFBundleIconFile</key>
  <string>LC-Deck</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleVersion</key>
  <string>1</string>
  <key>CFBundleShortVersionString</key>
  <string>1</string>
  <key>LSMinimumSystemVersion</key>
  <string>11.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSScreenCaptureUsageDescription</key>
  <string>LC-Deck 需要屏幕录制权限，用于把本机画面发送给主控端。</string>
  <key>NSAccessibilityUsageDescription</key>
  <string>LC-Deck 需要辅助功能权限，用于接受远端的键鼠操作。</string>
</dict>
</plist>
PLIST

# 清除隔离标记：Gatekeeper 只对带该标记的文件做拦截检查
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true

# ad-hoc 重签：让 .app 具备一致的签名标识（不是 Developer ID，仍需清除隔离标记）
codesign --force --deep --sign - "$APP" 2>/dev/null || true

echo "==> 完成"
echo "     二进制：$SRC"
echo "     图标：$HAS_ICON"
echo
echo "现在可以在启动台 / 访达里打开 LC-Deck。"
echo "若提示需要权限，到 系统设置 > 隐私与安全性 中授权："
echo "  · 屏幕录制（被控端画面必需）"
echo "  · 辅助功能（键鼠注入必需）"

# LC_NO_OPEN=1 时不自动启动（install-local.sh 需要自行控制启动时机）
if [[ "${LC_NO_OPEN:-0}" != "1" ]]; then
  open "$APP" 2>/dev/null || true
fi
