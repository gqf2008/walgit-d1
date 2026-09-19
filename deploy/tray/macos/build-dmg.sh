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
#   WALGIT_BIN            可选：预构建 walgit；默认 target/release/walgit
#   WALGIT_TRAY_BIN       可选：预构建的 tray-rs 托盘；默认 target/release/walgit-tray
#   WALGIT_SKIP_BUILD=1   CI：跳过 web/cargo 构建，使用预构建 WALGIT_BIN
#   WALGIT_CLEAN_TARGET_AFTER_APP=1
#                         CI：app 组装后删除 $ROOT/target，给 DMG 腾空间
#   WALGIT_TEST_ROOT      可选：测试时覆盖 ROOT（仅配合 WALGIT_SKIP_BUILD）
#   WALGIT_HDIUTIL_BIN    可选：覆盖 hdiutil 路径（测试注入）
#   WALGIT_HDIUTIL_RETRY_DELAY
#                         可选：create 失败后的重试间隔秒数，默认 5
#
# 产物：dist/walgit-<版本>-<架构>.dmg
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="${WALGIT_TEST_ROOT:-$(cd "$SCRIPT_DIR/../../.." && pwd)}"
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

stale_dmg_mountpoint() {
    local image="$1"
    local hdiutil_bin="$2"
    local info

    # Only inspect the host when a volume named like our temporary DMG exists.
    # The image-path match below ensures we never detach an unrelated volume
    # that merely shares the same volume name.
    [ -d /Volumes/walgit ] || return 0
    command -v plutil >/dev/null 2>&1 || return 0
    command -v python3 >/dev/null 2>&1 || return 0
    info="$("$hdiutil_bin" info -plist 2>/dev/null)" || return 0
    printf '%s\n' "$info" | plutil -convert json -o - - 2>/dev/null \
        | python3 -c '
import json, sys
want = sys.argv[1]
try:
    images = json.load(sys.stdin).get("images", [])
except Exception:
    raise SystemExit(0)
for image in images:
    if image.get("image-path") != want:
        continue
    for entity in image.get("system-entities", []):
        if entity.get("mount-point") == "/Volumes/walgit":
            print("/Volumes/walgit")
            raise SystemExit(0)
' "$image" 2>/dev/null || true
}

detach_stale_dmg_mount() {
    local image="$1"
    local hdiutil_bin="$2"
    local mountpoint

    mountpoint="$(stale_dmg_mountpoint "$image" "$hdiutil_bin")"
    [ -n "$mountpoint" ] || return 0
    echo "detaching stale DMG mount from this build: $mountpoint" >&2
    "$hdiutil_bin" detach "$mountpoint" >&2 \
        || echo "warning: failed to detach $mountpoint; retrying create anyway" >&2
}

create_dmg_with_retry() {
    local stage="$1"
    local output="$2"
    local hdiutil_bin="${WALGIT_HDIUTIL_BIN:-hdiutil}"
    local retry_delay="${WALGIT_HDIUTIL_RETRY_DELAY:-5}"
    local attempt

    # Resource busy is transient on shared macOS runners. Keep the log instead
    # of swallowing stdout: a persistent cause must still be diagnosable.
    for attempt in 1 2 3; do
        if "$hdiutil_bin" create -volname walgit -srcfolder "$stage" -ov -format UDZO "$output" >&2; then
            return 0
        fi

        # hdiutil create is not resumable, so a failed attempt must not leave a
        # partial image behind for the next one.
        rm -f "$output"
        if [ "$attempt" -eq 3 ]; then
            echo "hdiutil create failed after 3 attempts: $output" >&2
            return 1
        fi

        echo "hdiutil create failed (attempt $attempt/3); retrying in ${retry_delay}s" >&2
        sync
        detach_stale_dmg_mount "$output" "$hdiutil_bin"
        sleep "$retry_delay"
    done
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
    --check-hdiutil-retry)
        create_dmg_with_retry "${2:?stage}" "${3:?output}"
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

# Swift 编译器已不在链路里(macOS 托盘 = tray-rs,issue #183):发布路径
# 不许再有 Swift 编译依赖——少一个工具就是少一个只在发布机上才炸的失败面。
for tool in cargo dot_clean ditto hdiutil plutil codesign security xcrun; do
    command -v "$tool" >/dev/null 2>&1 || { echo "missing tool: $tool" >&2; exit 1; }
done
notary_mode >/dev/null
IDENTITY="${WALGIT_IDENTITY:-}"
if [ -z "$IDENTITY" ]; then
    if [ -n "${CODESIGN_KEYCHAIN:-}" ]; then
        IDENTITY="$(security find-identity -v -p codesigning "$CODESIGN_KEYCHAIN" 2>/dev/null \
            | awk -F'"' '/Developer ID Application/ {print $2; exit}')"
    else
        IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
            | awk -F'"' '/Developer ID Application/ {print $2; exit}')"
    fi
fi
[ -n "$IDENTITY" ] || { echo "❌ Keychain 里没有 Developer ID Application 身份" >&2; exit 1; }
unlock_codesign_keychain

WORK="$(mktemp -d "${TMPDIR:-/tmp}/walgit-dmg.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

WALGIT_BIN="${WALGIT_BIN:-$ROOT/target/release/walgit}"
WALGIT_TRAY_BIN="${WALGIT_TRAY_BIN:-$ROOT/target/release/walgit-tray}"
if [ "${WALGIT_SKIP_BUILD:-0}" = "1" ]; then
    echo "== [1/8] build release binaries (skipped; using prebuilt) =="
else
    echo "== [1/8] build release binaries =="
    ( cd "$ROOT" && just web-build >/dev/null )
    WALGIT_BUILD_SHA="v$VERSION" cargo build --release --bin walgit --manifest-path "$ROOT/Cargo.toml"
    # 托盘 = 跨平台 tray-rs(独立 workspace,自己的 lock);--target-dir 与
    # tray.yml/release.yml 一致,产物落在仓库根 target/。
    ( cd "$ROOT" && cargo build --release --target-dir target \
        --manifest-path "$ROOT/deploy/tray/tray-rs/Cargo.toml" )
fi
check_version "$WALGIT_BIN" "$VERSION"
[ -x "$WALGIT_TRAY_BIN" ] || { echo "❌ missing tray binary: $WALGIT_TRAY_BIN" >&2; exit 1; }

echo "== [2/8] assemble app =="
APP_ROOT="$WORK/app"
mkdir -p "$APP_ROOT"
WALGIT_BIN="$WALGIT_BIN" WALGIT_TRAY_BIN="$WALGIT_TRAY_BIN" \
    TRAY_APP_DIR="$APP_ROOT" "$SCRIPT_DIR/build.sh" "$VERSION"
APP="$APP_ROOT/walgit-tray.app"
dot_clean -m "$APP" >/dev/null 2>&1 || true
check_tree "$APP"
/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist" | grep -Fx "$VERSION" >/dev/null
check_version "$APP/Contents/Resources/walgit" "$VERSION"

if [ "${WALGIT_CLEAN_TARGET_AFTER_APP:-0}" = "1" ]; then
    echo "== [2b/8] clean CI build tree =="
    echo "disk before CI cleanup:"
    df -h "$ROOT" "$WORK" || true
    du -sh "$ROOT/target" "$WORK" 2>/dev/null || true
    rm -rf "$ROOT/target"
    # The build is done; codesign/notarytool/hdiutil do not need cargo/rustup or
    # package-manager caches. Reclaim them before the DMG stage: the macOS
    # runner is otherwise close enough to full that hdiutil create can fail
    # after the app has been notarized (release run 35005976205, #204).
    if [ "${WALGIT_CLEAN_HOME_CACHES:-0}" = "1" ] && [ -n "${HOME:-}" ] && [ "$HOME" != "/" ]; then
        rm -rf             "$HOME/.cargo/registry"             "$HOME/.cargo/git"             "$HOME/.rustup/toolchains"             "$HOME/Library/Caches/pnpm"             "$HOME/Library/pnpm/store"             "$HOME/Library/Caches/Homebrew" 2>/dev/null || true
    fi
    echo "disk after CI cleanup:"
    df -h "$ROOT" "$WORK" || true
fi

echo "== [3/8] sign app =="
if [ -n "${CODESIGN_KEYCHAIN:-}" ]; then
    codesign --force --options runtime --timestamp --sign "$IDENTITY" \
        --keychain "$CODESIGN_KEYCHAIN" "$APP/Contents/Resources/walgit"
    codesign --force --deep --options runtime --timestamp --sign "$IDENTITY" \
        --keychain "$CODESIGN_KEYCHAIN" "$APP"
else
    codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP/Contents/Resources/walgit"
    codesign --force --deep --options runtime --timestamp --sign "$IDENTITY" "$APP"
fi
codesign --verify --deep --strict --verbose=2 "$APP"

echo "== [4/8] notarize app =="
APP_ZIP="$WORK/walgit-tray.zip"
ditto -c -k --keepParent --norsrc --noextattr "$APP" "$APP_ZIP"
check_zip "$APP_ZIP"
notary_submit "$APP_ZIP"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
spctl --assess --type execute --verbose=2 "$APP" 2>&1 | tail -1
# The notarization archive is no longer needed; freeing it before hdiutil
# avoids keeping a second copy of the app around during DMG creation.
rm -f "$APP_ZIP"

echo "== [5/8] assemble DMG =="
ARCH="$(uname -m)"
[ "$ARCH" = "arm64" ] || [ "$ARCH" = "x86_64" ] || ARCH="unknown"
STAGE="$WORK/dmg"
mkdir -p "$STAGE"
# Move instead of copy: the notarized app is no longer needed at its old path,
# and one app copy can be the difference between a successful hdiutil create
# and ENOSPC on the release runner.
mv "$APP" "$STAGE/walgit-tray.app"
ln -s /Applications "$STAGE/Applications"
check_tree "$STAGE"
TMP_DMG="$WORK/walgit-${VERSION}-${ARCH}.dmg"
df -h "$WORK" || true
create_dmg_with_retry "$STAGE" "$TMP_DMG"

echo "== [6/8] sign DMG =="
unlock_codesign_keychain
if [ -n "${CODESIGN_KEYCHAIN:-}" ]; then
    codesign --force --sign "$IDENTITY" --timestamp --keychain "$CODESIGN_KEYCHAIN" "$TMP_DMG"
else
    codesign --force --sign "$IDENTITY" --timestamp "$TMP_DMG"
fi

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
