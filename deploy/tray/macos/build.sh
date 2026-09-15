#!/bin/bash
# 构建 macOS 托盘 app。产物：${TRAY_APP_DIR:-$HOME/Applications}/walgit-tray.app
#
# 托盘本体是跨平台 tray-rs(issue #183):这里只负责 App Bundle 组装
# (Info.plist / 图标 / 资源)与 ad-hoc 签名,不再编译 Swift。
#   WALGIT_TRAY_BIN  可选:预构建的 walgit-tray;缺省时用 cargo 构建 tray-rs
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../../.. && pwd)"

VERSION="${1:-$(git -C "$ROOT" describe --tags --abbrev=0 2>/dev/null || echo v0.0.0)}"
VERSION="${VERSION#v}"
case "$VERSION" in
    ''|*[!0-9A-Za-z.+-]*) echo "invalid version: $VERSION" >&2; exit 1 ;;
esac

WALGIT_BIN="${WALGIT_BIN:-$ROOT/target/release/walgit}"
[ -x "$WALGIT_BIN" ] || { echo "missing walgit binary: $WALGIT_BIN (WALGIT_BIN 可覆盖)" >&2; exit 1; }
GOT_VERSION="$("$WALGIT_BIN" --version 2>&1 || true)"
# 精确取完整版本 token:不接受 v0.5.0-beta 之类的同前缀版本。
GOT_TOKEN="${GOT_VERSION##* }"
[ "$GOT_TOKEN" = "v$VERSION" ] \
    || { echo "walgit binary reports '$GOT_VERSION', expected v$VERSION" >&2; exit 1; }

TRAY_BIN="${WALGIT_TRAY_BIN:-}"
if [ -z "$TRAY_BIN" ]; then
    # 与 release.yml/tray.yml 同形:--target-dir 钉到仓库根 target,避免
    # 因 cwd 不同把产物落到 tray-rs/target 而让 CI 找不到。
    ( cd "$ROOT" && cargo build --release --target-dir target \
        --manifest-path "$ROOT/deploy/tray/tray-rs/Cargo.toml" )
    TRAY_BIN="$ROOT/target/release/walgit-tray"
fi
[ -x "$TRAY_BIN" ] || { echo "missing tray binary: $TRAY_BIN" >&2; exit 1; }

APP="${TRAY_APP_DIR:-$HOME/Applications}/walgit-tray.app"
BIN_DIR="$APP/Contents/MacOS"
RES_DIR="$APP/Contents/Resources"
rm -rf "$APP"
mkdir -p "$BIN_DIR" "$RES_DIR"

cp "$TRAY_BIN" "$BIN_DIR/walgit-tray"
# 内嵌 Mach-O 先 ad-hoc 垫底;build-dmg.sh 会用 Developer ID 重签后公证。
cp "$WALGIT_BIN" "$RES_DIR/walgit"
codesign --force --sign - "$RES_DIR/walgit" 2>/dev/null || true
codesign --force --sign - "$BIN_DIR/walgit-tray" 2>/dev/null || true
cp release-install.sh "$RES_DIR/"
cp walgit.toml.template "$RES_DIR/walgit.toml"
for required in walgit release-install.sh walgit.toml; do
    [ -e "$RES_DIR/$required" ] || {
        echo "missing required app resource: $required" >&2
        exit 1
    }
done
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>com.walgit.tray</string>
    <key>CFBundleName</key><string>walgit-tray</string>
    <key>CFBundleExecutable</key><string>walgit-tray</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleIconFile</key><string>walgit</string>
    <key>LSUIElement</key><true/>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
# Dock 图标(icns)——如无现成 icns,app 会以通用图标显示,不影响功能。
if [ -f walgit.icns ]; then
    cp walgit.icns "$RES_DIR/walgit.icns"
fi

# Ad-hoc 整体签名:让独立 `build.sh` 产物也能通过 codesign 校验。
# 发布链(build-dmg.sh)随后会用 Developer ID 重新签名并公证。
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 \
    || { echo "ad-hoc codesign failed: $APP" >&2; exit 1; }
codesign --verify --deep --strict "$APP"
echo "built: $APP (version $VERSION, walgit from $WALGIT_BIN, tray $TRAY_BIN)"
echo "启动:open $APP   开机自启:系统设置 → 通用 → 登录项 → 添加本 app"
