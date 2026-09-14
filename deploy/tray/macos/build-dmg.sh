#!/bin/bash
# build-dmg.sh — 构建并公证 macOS 发布 DMG。
#
# 用法：./build-dmg.sh [版本]
# 环境：
#   APPLE_ID / APPLE_TEAM_ID / APPLE_APP_PASSWORD
#                         CI 公证凭据；三者必须同时提供，优先于 NOTARY_PROFILE
#   NOTARY_PROFILE        notarytool profile，默认 voicecall-notary
#   NOTARY_KEYCHAIN       可选：profile 所在 keychain
#   NOTARY_S3_ACCELERATION=0  关闭 S3 acceleration（代理环境下更稳）
#   CODESIGN_KEYCHAIN     CI 临时钥匙串；同时提供 CODESIGN_KEYCHAIN_PASSWORD
#   WALGIT_IDENTITY       可选：Developer ID 身份
#
# 产物：dist/walgit-<版本>-<架构>.dmg
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
cd "$SCRIPT_DIR"

usage() {
    sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
}

check_tree() {
    local path="$1"
    local bad
    bad="$(find "$path" \( -name '._*' -o -name '.DS_Store' \) -print -quit)"
    if [ -n "$bad" ]; then
        echo "❌ AppleDouble/DS_Store metadata in $path: $bad" >&2
        return 1
    fi
}

check_zip() {
    local zip="$1"
    # 先确认 zip 本身可读:否则 unzip 失败的 stderr 会被 grep 的 `|| true`
    # 吞掉,损坏包反而“通过” AppleDouble 守卫(假绿)。
    if ! unzip -t "$zip" >/dev/null 2>&1; then
        echo "❌ not a readable zip: $zip" >&2
        return 1
    fi
    local bad
    bad="$(unzip -Z1 "$zip" | grep -E '(^|/)(\._|\.DS_Store)' || true)"
    if [ -n "$bad" ]; then
        echo "❌ AppleDouble/DS_Store entries in $zip: $bad" >&2
        return 1
    fi
}

check_version() {
    local binary="$1"
    local version="${2#v}"
    local got
    got="$("$binary" --version 2>&1 || true)"
    # 精确取完整版本 token:不接受 v0.5.0-beta 之类的同前缀版本。
    local token="${got##* }"
    if [ "$token" = "v$version" ]; then
        return 0
    fi
    echo "❌ binary reports '$got', expected v$version: $binary" >&2
    return 1
}

notary_mode() {
    local credentials=0
    [ -n "${APPLE_ID:-}" ] && credentials=$((credentials + 1))
    [ -n "${APPLE_TEAM_ID:-}" ] && credentials=$((credentials + 1))
    [ -n "${APPLE_APP_PASSWORD:-}" ] && credentials=$((credentials + 1))
    if [ "$credentials" -eq 3 ]; then
        echo direct
        return 0
    fi
    if [ "$credentials" -ne 0 ]; then
        echo "❌ APPLE_ID / APPLE_TEAM_ID / APPLE_APP_PASSWORD 必须同时提供" >&2
        return 1
    fi
    echo profile
}

notary_submit() {
    local file="$1"
    local mode
    mode="$(notary_mode)" || return 1
    local args
    if [ "$mode" = "direct" ]; then
        args=(submit "$file" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" \
            --password "$APPLE_APP_PASSWORD" --wait)
    else
        local profile="${NOTARY_PROFILE:-voicecall-notary}"
        args=(submit "$file" --keychain-profile "$profile" --wait)
        if [ -n "${NOTARY_KEYCHAIN:-}" ]; then
            args+=(--keychain "$NOTARY_KEYCHAIN")
        fi
    fi
    if [ "${NOTARY_S3_ACCELERATION:-1}" = "0" ]; then
        args+=(--no-s3-acceleration)
    fi
    local attempt
    for attempt in 1 2 3; do
        if xcrun notarytool "${args[@]}"; then
            return 0
        fi
        echo "notary submission failed (attempt $attempt/3); retrying" >&2
        sleep 5
    done
    return 1
}

unlock_codesign_keychain() {
    if [ -n "${CODESIGN_KEYCHAIN:-}" ]; then
        [ -n "${CODESIGN_KEYCHAIN_PASSWORD:-}" ] || {
            echo "❌ CODESIGN_KEYCHAIN 需要同时提供 CODESIGN_KEYCHAIN_PASSWORD" >&2
            return 1
        }
        security unlock-keychain -p "$CODESIGN_KEYCHAIN_PASSWORD" "$CODESIGN_KEYCHAIN"
    fi
}

case "${1:-}" in
    --check-version)
        check_version "${2:?binary}" "${3:?version}"
        exit 0 ;;
    --check-tree)
        check_tree "${2:?path}"
        exit 0 ;;
    --check-zip)
        check_zip "${2:?zip}"
        exit 0 ;;
    --check-notary-mode)
        notary_mode
        exit $? ;;
    -h|--help)
        usage
        exit 0 ;;
esac

VERSION="${1:-$(git -C "$ROOT" describe --tags --abbrev=0 2>/dev/null || echo v0.0.0)}"
VERSION="${VERSION#v}"
case "$VERSION" in
    ''|*[!0-9A-Za-z.+-]*) echo "invalid version: $VERSION" >&2; exit 1 ;;
esac

for tool in cargo swiftc dot_clean ditto hdiutil plutil codesign security xcrun; do
    command -v "$tool" >/dev/null 2>&1 || { echo "missing tool: $tool" >&2; exit 1; }
done
notary_mode >/dev/null
IDENTITY="${WALGIT_IDENTITY:-}"
if [ -z "$IDENTITY" ]; then
    identity_keychains=()
    if [ -n "${CODESIGN_KEYCHAIN:-}" ]; then
        identity_keychains=("$CODESIGN_KEYCHAIN")
    fi
    IDENTITY="$(security find-identity -v -p codesigning "${identity_keychains[@]}" 2>/dev/null \
        | awk -F'"' '/Developer ID Application/ {print $2; exit}')"
fi
[ -n "$IDENTITY" ] || { echo "❌ Keychain 里没有 Developer ID Application 身份" >&2; exit 1; }
CODESIGN_KEYCHAIN_ARGS=()
if [ -n "${CODESIGN_KEYCHAIN:-}" ]; then
    CODESIGN_KEYCHAIN_ARGS=(--keychain "$CODESIGN_KEYCHAIN")
fi
unlock_codesign_keychain

WORK="$(mktemp -d "${TMPDIR:-/tmp}/walgit-dmg.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

echo "== [1/8] build release binary =="
( cd "$ROOT" && just web-build >/dev/null )
WALGIT_BUILD_SHA="v$VERSION" cargo build --release --bin walgit --manifest-path "$ROOT/Cargo.toml"
check_version "$ROOT/target/release/walgit" "$VERSION"

echo "== [2/8] assemble app =="
APP_ROOT="$WORK/app"
mkdir -p "$APP_ROOT"
WALGIT_BIN="$ROOT/target/release/walgit" TRAY_APP_DIR="$APP_ROOT" "$SCRIPT_DIR/build.sh" "$VERSION"
APP="$APP_ROOT/walgit-tray.app"
dot_clean -m "$APP" >/dev/null 2>&1 || true
check_tree "$APP"
/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist" | grep -Fx "$VERSION" >/dev/null
check_version "$APP/Contents/Resources/walgit" "$VERSION"

echo "== [3/8] sign app =="
codesign --force --options runtime --timestamp --sign "$IDENTITY" \
    "${CODESIGN_KEYCHAIN_ARGS[@]}" "$APP/Contents/Resources/walgit"
codesign --force --deep --options runtime --timestamp --sign "$IDENTITY" \
    "${CODESIGN_KEYCHAIN_ARGS[@]}" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

echo "== [4/8] notarize app =="
APP_ZIP="$WORK/walgit-tray.zip"
ditto -c -k --keepParent --norsrc --noextattr "$APP" "$APP_ZIP"
check_zip "$APP_ZIP"
notary_submit "$APP_ZIP"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
spctl --assess --type execute --verbose=2 "$APP" 2>&1 | tail -1

echo "== [5/8] assemble DMG =="
ARCH="$(uname -m)"
[ "$ARCH" = "arm64" ] || [ "$ARCH" = "x86_64" ] || ARCH="unknown"
STAGE="$WORK/dmg"
mkdir -p "$STAGE"
ditto --norsrc --noextattr "$APP" "$STAGE/walgit-tray.app"
ln -s /Applications "$STAGE/Applications"
check_tree "$STAGE"
TMP_DMG="$WORK/walgit-${VERSION}-${ARCH}.dmg"
hdiutil create -volname walgit -srcfolder "$STAGE" -ov -format UDZO "$TMP_DMG" >/dev/null

echo "== [6/8] sign DMG =="
unlock_codesign_keychain
codesign --force --sign "$IDENTITY" --timestamp \
    "${CODESIGN_KEYCHAIN_ARGS[@]}" "$TMP_DMG"

echo "== [7/8] notarize + staple DMG =="
notary_submit "$TMP_DMG"
xcrun stapler staple "$TMP_DMG"
xcrun stapler validate "$TMP_DMG"
spctl --assess --type open --context context:primary-signature -v "$TMP_DMG" 2>&1 | tail -1

echo "== [8/8] publish local artifact =="
mkdir -p "$SCRIPT_DIR/dist"
DMG="$SCRIPT_DIR/dist/walgit-${VERSION}-${ARCH}.dmg"
TMP_OUT="$DMG.tmp.$$"
ditto --norsrc --noextattr "$TMP_DMG" "$TMP_OUT"
mv -f "$TMP_OUT" "$DMG"
echo "✅ $DMG"
shasum -a 256 "$DMG"
