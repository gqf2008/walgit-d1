#!/bin/bash
# macOS tray/package tests. Runs without starting the real service.
#
# 托盘本体是跨平台 tray-rs(issue #183);这里测的是 macOS 打包链路:
# App Bundle 组装、AppleDouble/公证守卫、Release 安装/回滚、bootstrap 迁移。
# tray-rs 自己的纯逻辑(版本比较/release 解析/菜单文本)由 cargo test 覆盖。
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../../.. && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/walgit-tray-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# 无 WALGIT_TRAY_BIN(=本地跑)时**每次**都构建:只判"文件在不在"会拿旧产物
# 跑 fixture(实测踩过:改完源码后 WALGIT_DETECT_ONCE 没生效,托盘照常起
# 事件循环,测试挂死)。CI 传预构建产物,cargo 增量构建本身也很快。
TRAY_BIN="${WALGIT_TRAY_BIN:-}"
if [ -z "$TRAY_BIN" ]; then
    ( cd "$ROOT" && cargo build --release --target-dir "$ROOT/target" \
        --manifest-path "$ROOT/deploy/tray/tray-rs/Cargo.toml" )
    TRAY_BIN="$ROOT/target/release/walgit-tray"
fi
[ -x "$TRAY_BIN" ] || { echo "FAIL: missing tray binary $TRAY_BIN" >&2; exit 1; }

# issue #183 验收:构建/发布路径不得再出现 Swift 托盘(编译命令或源码名)。
if grep -rnE 'swiftc|walgit-tray\.swift|ReleaseLogic\.swift' \
    "$ROOT/.github/workflows" build.sh build-dmg.sh >/dev/null 2>&1; then
    echo "FAIL: Swift tray still referenced in the build/release path" >&2
    grep -rnE 'swiftc|walgit-tray\.swift|ReleaseLogic\.swift' \
        "$ROOT/.github/workflows" build.sh build-dmg.sh >&2
    exit 1
fi

free_port() {
    python3 - <<'PY'
import socket
s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()
PY
}

# tray-rs 单元测试(版本比较 / GitHub release 解析 / 菜单文本语义)。
cargo test --manifest-path "$ROOT/deploy/tray/tray-rs/Cargo.toml"

cat >"$TMP/walgit-good" <<'EOF'
#!/bin/sh
[ "${1:-}" = "--version" ] && echo "walgit v0.5.0"
EOF
cat >"$TMP/walgit-bad" <<'EOF'
#!/bin/sh
[ "${1:-}" = "--version" ] && echo "walgit v0.4.0"
EOF
chmod +x "$TMP/walgit-good" "$TMP/walgit-bad"
./build-dmg.sh --check-version "$TMP/walgit-good" 0.5.0
if ./build-dmg.sh --check-version "$TMP/walgit-bad" 0.5.0 >/dev/null 2>&1; then
    echo "FAIL: wrong version was accepted" >&2
    exit 1
fi

cat >"$TMP/walgit-prefix" <<'EOF'
#!/bin/sh
[ "${1:-}" = "--version" ] && echo "walgit v0.5.0-beta"
EOF
chmod +x "$TMP/walgit-prefix"
if ./build-dmg.sh --check-version "$TMP/walgit-prefix" 0.5.0 >/dev/null 2>&1; then
    echo "FAIL: same-prefix version v0.5.0-beta was accepted for 0.5.0" >&2
    exit 1
fi

mkdir -p "$TMP/tree"
touch "$TMP/tree/._bad"
if ./build-dmg.sh --check-tree "$TMP/tree" >/dev/null 2>&1; then
    echo "FAIL: AppleDouble tree was accepted" >&2
    exit 1
fi
rm "$TMP/tree/._bad"
./build-dmg.sh --check-tree "$TMP/tree"

mkdir -p "$TMP/zip-src"
touch "$TMP/zip-src/._bad"
(cd "$TMP/zip-src" && zip -q "$TMP/bad.zip" ._bad)
if ./build-dmg.sh --check-zip "$TMP/bad.zip" >/dev/null 2>&1; then
    echo "FAIL: AppleDouble zip was accepted" >&2
    exit 1
fi
rm -f "$TMP/zip-src/._bad"
touch "$TMP/zip-src/good"
(cd "$TMP/zip-src" && zip -q "$TMP/good.zip" good)
./build-dmg.sh --check-zip "$TMP/good.zip"

printf 'not a zip' >"$TMP/corrupt.zip"
if ./build-dmg.sh --check-zip "$TMP/corrupt.zip" >/dev/null 2>&1; then
    echo "FAIL: corrupt zip was accepted" >&2
    exit 1
fi

# 公证凭据分派：CI 三件套走 direct，缺少任一必须拒绝，否则回退 profile。
notary_mode="$(APPLE_ID=id APPLE_TEAM_ID=team APPLE_APP_PASSWORD=pass ./build-dmg.sh --check-notary-mode)"
[ "$notary_mode" = "direct" ] || { echo "FAIL: expected direct notary mode, got $notary_mode" >&2; exit 1; }
notary_mode="$(env -u APPLE_ID -u APPLE_TEAM_ID -u APPLE_APP_PASSWORD ./build-dmg.sh --check-notary-mode)"
[ "$notary_mode" = "profile" ] || { echo "FAIL: expected profile notary mode, got $notary_mode" >&2; exit 1; }
if APPLE_ID=id APPLE_TEAM_ID=team env -u APPLE_APP_PASSWORD ./build-dmg.sh --check-notary-mode >/dev/null 2>&1; then
    echo "FAIL: partial direct notary credentials were accepted" >&2
    exit 1
fi
if grep -Eq 'identity_keychains|CODESIGN_KEYCHAIN_ARGS' build-dmg.sh; then
    echo "FAIL: Bash 3.2 incompatible empty-array pattern reintroduced" >&2
    exit 1
fi

# Full local-path smoke under macOS Bash 3.2: no CODESIGN_KEYCHAIN, fake
# signing/notary tools, prebuilt binary. This is the path that used to abort
# on an empty array before any signing happened.
# Seed the two cleanup opt-ins: repository target and HOME caches.
cleanup_seed() {
    local root="$1"
    local home="$2"
    rm -rf "$root/target"
    mkdir -p "$root/target" \
        "$home/.cargo/registry" "$home/.cargo/git" \
        "$home/.rustup/toolchains" \
        "$home/Library/Caches/pnpm" "$home/Library/pnpm/store" \
        "$home/Library/Caches/Homebrew"
    : >"$root/target/sentinel"
}

cleanup_check_home() {
    local home="$1"
    local want="$2"
    local path
    for path in \
        "$home/.cargo/registry" \
        "$home/.cargo/git" \
        "$home/.rustup/toolchains" \
        "$home/Library/Caches/pnpm" \
        "$home/Library/pnpm/store" \
        "$home/Library/Caches/Homebrew"; do
        if [ "$want" = present ] && [ ! -e "$path" ]; then
            echo "FAIL: local HOME cache was cleaned without the CI opt-in: $path" >&2
            return 1
        fi
        if [ "$want" = absent ] && [ -e "$path" ]; then
            echo "FAIL: CI HOME cache cleanup did not run: $path" >&2
            return 1
        fi
    done
}

bash_compat_smoke() {
    local base="$TMP/bash-compat"
    local fake="$base/bin"
    local home built
    mkdir -p "$fake"
    cat >"$base/walgit" <<'EOF'
#!/bin/sh
[ "${1:-}" = "--version" ] && echo "walgit v0.0.0-ci"
EOF
    chmod +x "$base/walgit"
    cat >"$base/tray" <<'EOF'
#!/bin/sh
exit 0
EOF
    chmod +x "$base/tray"
    printf '#!/bin/sh
exit 0
' >"$fake/codesign"
    printf '#!/bin/sh
exit 0
' >"$fake/spctl"
    printf '#!/bin/sh
exit 0
' >"$fake/xcrun"
    cat >"$fake/hdiutil" <<'EOF'
#!/bin/sh
for arg in "$@"; do
  case "$arg" in *.dmg) : >"$arg"; break ;; esac
done
exit 0
EOF
    chmod +x "$fake"/*

    # Cleanup disabled: both the target and the HOME caches must survive.
    home="$base/home-disabled"
    cleanup_seed "$base/root" "$home"
    PATH="$fake:$PATH" HOME="$home" WALGIT_SKIP_BUILD=1 WALGIT_TEST_ROOT="$base/root" \
        WALGIT_BIN="$base/walgit" WALGIT_TRAY_BIN="$base/tray" \
        WALGIT_IDENTITY='Developer ID Application: Test' \
        NOTARY_PROFILE=test /bin/bash ./build-dmg.sh 0.0.0-ci >/dev/null 2>&1
    built="$(ls dist/walgit-0.0.0-ci-*.dmg 2>/dev/null | head -1)"
    [ -n "$built" ] || { echo "FAIL: bash-compat smoke produced no DMG" >&2; return 1; }
    [ -e "$base/root/target/sentinel" ] \
        || { echo "FAIL: target cleaned without the CI opt-in" >&2; return 1; }
    cleanup_check_home "$home" present
    rm -f "$built"

    # CI target cleanup enabled, but HOME cache cleanup deliberately not set:
    # the target goes away, the developer's global cache does not.
    home="$base/home-local-cleanup"
    cleanup_seed "$base/root" "$home"
    PATH="$fake:$PATH" HOME="$home" WALGIT_SKIP_BUILD=1 WALGIT_TEST_ROOT="$base/root" \
        WALGIT_CLEAN_TARGET_AFTER_APP=1 WALGIT_BIN="$base/walgit" \
        WALGIT_TRAY_BIN="$base/tray" \
        WALGIT_IDENTITY='Developer ID Application: Test' NOTARY_PROFILE=test \
        /bin/bash ./build-dmg.sh 0.0.0-ci >/dev/null 2>&1
    built="$(ls dist/walgit-0.0.0-ci-*.dmg 2>/dev/null | head -1)"
    [ -n "$built" ] || { echo "FAIL: cleanup smoke produced no DMG" >&2; return 1; }
    [ ! -e "$base/root/target/sentinel" ] \
        || { echo "FAIL: CI target cleanup did not run" >&2; return 1; }
    cleanup_check_home "$home" present
    rm -f "$built"

    # Full CI cleanup: the dedicated HOME-cache switch removes the fixture
    # caches. This is the only path that may touch $HOME caches.
    home="$base/home-ci-cleanup"
    cleanup_seed "$base/root" "$home"
    PATH="$fake:$PATH" HOME="$home" WALGIT_SKIP_BUILD=1 WALGIT_TEST_ROOT="$base/root" \
        WALGIT_CLEAN_TARGET_AFTER_APP=1 WALGIT_CLEAN_HOME_CACHES=1 \
        WALGIT_BIN="$base/walgit" WALGIT_TRAY_BIN="$base/tray" \
        WALGIT_IDENTITY='Developer ID Application: Test' NOTARY_PROFILE=test \
        /bin/bash ./build-dmg.sh 0.0.0-ci >/dev/null 2>&1
    built="$(ls dist/walgit-0.0.0-ci-*.dmg 2>/dev/null | head -1)"
    [ -n "$built" ] || { echo "FAIL: full cleanup smoke produced no DMG" >&2; return 1; }
    [ ! -e "$base/root/target/sentinel" ] \
        || { echo "FAIL: full CI target cleanup did not run" >&2; return 1; }
    cleanup_check_home "$home" absent
    rm -f "$built"
    return 0
}
# The real app bundle must contain exactly the new contract: program in
# Resources, no managed copies under ~/.walgit.
layout_fixture() {
    local base="$TMP/layout"
    local app="$base/walgit-tray.app"
    mkdir -p "$base/bin"
    cat >"$base/walgit" <<'EOF'
#!/bin/sh
[ "${1:-}" = "--version" ] && echo "walgit v0.5.0"
EOF
    chmod +x "$base/walgit"
    cat >"$base/bin/codesign" <<'EOF'
#!/bin/sh
exit 0
EOF
    chmod +x "$base/bin/codesign"
    PATH="$base/bin:$PATH" WALGIT_BIN="$base/walgit" WALGIT_TRAY_BIN="$TRAY_BIN" \
        TRAY_APP_DIR="$base" ./build.sh 0.5.0 >/dev/null
    # #197：菜单栏状态项被遮挡时，Dock 是唯一入口 —— 这条键必须一直是 false，
    # 否则应用会退回"纯菜单栏"，图标一被挤掉就彻底找不到。
    [ "$(/usr/libexec/PlistBuddy -c 'Print :LSUIElement' "$app/Contents/Info.plist" 2>/dev/null)" = "false" ] \
        || { echo "FAIL(layout): LSUIElement must be false (a Dock icon is the fallback entry point)" >&2; return 1; }
    [ -x "$app/Contents/Resources/walgit" ] || { echo "FAIL(layout): missing bundled walgit" >&2; return 1; }
    [ -x "$app/Contents/Resources/release-install.sh" ] || { echo "FAIL(layout): missing release-install.sh" >&2; return 1; }
    [ -f "$app/Contents/Resources/walgit.toml" ] || { echo "FAIL(layout): missing state template" >&2; return 1; }
    for stale in walgit-ensure run-walgit.sh skeleton.version; do
        if [ -e "$app/Contents/Resources/$stale" ]; then
            echo "FAIL(layout): stale resource $stale" >&2
            return 1
        fi
    done
    return 0
}

# Bootstrap only initializes user state; it must never copy the program into
# ~/.walgit and must migrate old managed files without touching cache/config.
bootstrap_fixture() {
    local base="$TMP/bootstrap"
    local app="$base/walgit-tray.app"
    local res="$app/Contents/Resources"
    local state="$base/state"
    local port
    port="$(free_port)"
    mkdir -p "$app/Contents/MacOS" "$res" "$state/cache"
    pkginfo "$app" 0.5.0
    cp "$TRAY_BIN" "$app/Contents/MacOS/walgit-tray"
    cat >"$res/walgit" <<'EOF'
#!/bin/sh
case "${1:-}" in
  --version) echo "walgit v0.5.0" ;;
  service) exit 0 ;;
esac
EOF
    chmod +x "$res/walgit"
    printf '[server]\nlisten = "127.0.0.1:%s"\n' "$port" >"$res/walgit.toml"
    printf 'keep\n' >"$state/cache/keep"
    for stale in walgit walgit-ensure run-walgit.sh .skeleton-version; do
        printf 'old\n' >"$state/$stale"
        chmod +x "$state/$stale"
    done

    WALGIT_BOOTSTRAP_ONLY=1 WALGIT_STATE_DIR="$state" WALGIT_DEPLOY_DIR="$state" \
        WALGIT_CLI_LINK="$base/bin/walgit" \
        "$app/Contents/MacOS/walgit-tray" >/dev/null 2>&1
    [ -f "$state/walgit.toml" ] || { echo "FAIL(bootstrap): config not initialized" >&2; return 1; }
    grep -q "listen = \"127.0.0.1:$port\"" "$state/walgit.toml" \
        || { echo "FAIL(bootstrap): config template not copied" >&2; return 1; }
    # Old 0.5.x helpers still check these three paths during an in-app upgrade.
    [ "$(readlink "$state/walgit")" = "$res/walgit" ] \
        || { echo "FAIL(bootstrap): legacy upgrade symlink missing" >&2; return 1; }
    [ "$(cat "$state/.skeleton-version")" = "0.5.0" ] \
        || { echo "FAIL(bootstrap): legacy upgrade marker missing" >&2; return 1; }
    [ -x "$state/walgit-ensure" ] \
        || { echo "FAIL(bootstrap): legacy ensure shim missing" >&2; return 1; }
    "$state/walgit-ensure" stop >/dev/null 2>&1 \
        || { echo "FAIL(bootstrap): legacy ensure shim not executable" >&2; return 1; }
    [ -f "$state/cache/keep" ] || { echo "FAIL(bootstrap): cache touched" >&2; return 1; }
    [ "$(readlink "$base/bin/walgit")" = "$res/walgit" ] \
        || { echo "FAIL(bootstrap): CLI link does not target bundle binary" >&2; return 1; }
    return 0
}

# #193: an unwritable/disabled default CLI link must fall back to the user
# entry instead of writing a spurious Permission denied error every launch.
# Explicit WALGIT_CLI_LINK keeps the old fail-loud behavior.
cli_link_fallback_fixture() {
    local base="$TMP/cli-link-fallback"
    local app="$base/app/walgit-tray.app"
    local res="$app/Contents/Resources"
    local state="$base/state"
    local state_explicit="$base/state-explicit"
    local home="$base/home"
    local bad_parent="$base/not-a-dir"
    local fallback="$home/.local/bin/walgit"
    mkdir -p "$app/Contents/MacOS" "$res" "$home/.local/bin"
    pkginfo "$app" 0.5.0
    cp "$TRAY_BIN" "$app/Contents/MacOS/walgit-tray"
    cat >"$res/walgit" <<'EOF'
#!/bin/sh
case "${1:-}" in
  --version) echo "walgit v0.5.0" ;;
  service) exit 0 ;;
esac
EOF
    chmod +x "$res/walgit"
    printf 'not a directory\n' >"$bad_parent"

    HOME="$home" WALGIT_BOOTSTRAP_ONLY=1 WALGIT_STATE_DIR="$state" \
        WALGIT_DEPLOY_DIR="$state" \
        WALGIT_CLI_LINK_PRIMARY="$bad_parent/walgit" \
        WALGIT_CLI_LINK_FALLBACK="$fallback" \
        "$app/Contents/MacOS/walgit-tray" >/dev/null 2>&1 || {
            echo "FAIL(cli-link-fallback): bootstrap exited non-zero" >&2
            return 1
        }
    [ "$(readlink "$fallback")" = "$res/walgit" ] \
        || { echo "FAIL(cli-link-fallback): user fallback link missing" >&2; return 1; }
    if grep -q 'Permission denied' "$state/tray.log"; then
        echo "FAIL(cli-link-fallback): spurious Permission denied log" >&2
        return 1
    fi

    HOME="$home" WALGIT_BOOTSTRAP_ONLY=1 WALGIT_STATE_DIR="$state_explicit" \
        WALGIT_DEPLOY_DIR="$state_explicit" \
        WALGIT_CLI_LINK="$bad_parent/walgit" \
        "$app/Contents/MacOS/walgit-tray" >/dev/null 2>&1 || true
    grep -q 'CLI 软链失败' "$state_explicit/tray.log" \
        || { echo "FAIL(cli-link-fallback): explicit failure must stay loud" >&2; return 1; }
    return 0
}

# Legacy installs used ~/walgit for both program and state. The bridge must
# migrate user state to ~/.walgit while leaving the old helper's version/config
# paths intact until it finishes.
legacy_migration_fixture() {
    local base="$TMP/legacy-migration"
    local home="$base/home"
    local old="$home/walgit"
    local new="$home/.walgit"
    local app="$base/walgit-tray.app"
    local res="$app/Contents/Resources"
    local port
    port="$(free_port)"
    mkdir -p "$app/Contents/MacOS" "$res" "$old/keys" "$old/cache"
    pkginfo "$app" 0.5.0
    cp "$TRAY_BIN" "$app/Contents/MacOS/walgit-tray"
    cat >"$res/walgit" <<'EOF'
#!/bin/sh
[ "${1:-}" = "--version" ] && echo "walgit v0.5.0"
exit 0
EOF
    chmod +x "$res/walgit"
    printf '[server]\nlisten = "127.0.0.1:%s"\n' "$port" >"$res/walgit.toml"
    printf 'listen = "127.0.0.1:%s"\n' "$port" >"$old/walgit.toml"
    printf 'creds\n' >"$old/.r2-credentials"
    printf 'key\n' >"$old/keys/k"
    printf 'cache\n' >"$old/cache/c"
    printf 'old\n' >"$old/walgit"
    printf 'old\n' >"$old/walgit-ensure"
    printf 'old\n' >"$old/run-walgit.sh"
    printf '0.4.0\n' >"$old/.skeleton-version"

    HOME="$home" WALGIT_STATE_DIR="$new" WALGIT_LEGACY_DIR="$old"     WALGIT_BOOTSTRAP_ONLY=1 WALGIT_DEPLOY_DIR="$old" \
        WALGIT_CLI_LINK="$base/bin/walgit" \
        "$app/Contents/MacOS/walgit-tray" >/dev/null 2>&1

    [ -f "$new/walgit.toml" ] || { echo "FAIL(legacy-migration): config not copied" >&2; return 1; }
    [ -f "$new/.r2-credentials" ] || { echo "FAIL(legacy-migration): credentials not copied" >&2; return 1; }
    [ -f "$new/keys/k" ] || { echo "FAIL(legacy-migration): keys not copied" >&2; return 1; }
    [ -f "$old/keys/k" ] || { echo "FAIL(legacy-migration): old keys moved" >&2; return 1; }
    [ -f "$old/cache/c" ] || { echo "FAIL(legacy-migration): old cache moved" >&2; return 1; }
    [ -f "$old/walgit.toml" ] || { echo "FAIL(legacy-migration): old config not retained" >&2; return 1; }
    [ "$(readlink "$old/walgit")" = "$res/walgit" ] \
        || { echo "FAIL(legacy-migration): bridge symlink missing" >&2; return 1; }
    [ -x "$old/walgit-ensure" ] || { echo "FAIL(legacy-migration): bridge shim missing" >&2; return 1; }
    return 0
}

# Manual drag-to-Applications does not inject WALGIT_DEPLOY_DIR. The new app
# must still discover ~/walgit, stop its service, copy state and restart from
# the new bundle.
manual_legacy_migration_fixture() {
    local base="$TMP/manual-legacy"
    local home="$base/home"
    local old="$home/walgit"
    local new="$home/.walgit"
    local app="$base/walgit-tray.app"
    local res="$app/Contents/Resources"
    local version_file="$base/version"
    local port
    port="$(free_port)"
    mkdir -p "$app/Contents/MacOS" "$res" "$old"
    pkginfo "$app" 0.5.0
    cp "$TRAY_BIN" "$app/Contents/MacOS/walgit-tray"
    cat >"$res/walgit" <<STUB
#!/bin/sh
case "\${1:-}" in
  --version) echo "walgit v0.5.0" ;;
  service) [ "\${2:-}" = start ] && printf 'v0.5.0\n' >"$version_file"; exit 0 ;;
esac
STUB
    chmod +x "$res/walgit"
    printf '[server]\nlisten = "127.0.0.1:%s"\n' "$port" >"$res/walgit.toml"
    printf '[server]\nlisten = "127.0.0.1:%s"\n' "$port" >"$old/walgit.toml"
    printf 'v0.4.0\n' >"$version_file"

    cat >"$base/health.py" <<'PYS'
import os, socket, sys
port = int(sys.argv[1]); vfile = sys.argv[2]
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port)); srv.listen(5)
while True:
    try: c, _ = srv.accept()
    except OSError: break
    try:
        c.recv(4096)
        v = open(vfile).read().strip() if os.path.exists(vfile) else "v0.0.0"
        body = '{"status":"ok","version":"%s"}' % v
        c.sendall(("HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % len(body)).encode() + body.encode())
    except OSError: pass
    finally: c.close()
PYS
    python3 "$base/health.py" "$port" "$version_file" >/dev/null 2>&1 &
    local srv_pid=$!
    sleep 0.5

    WALGIT_STATE_DIR="$new" WALGIT_LEGACY_DIR="$old" WALGIT_DEPLOY_DIR="$new" \
    WALGIT_BOOTSTRAP_ONLY=1 WALGIT_CLI_LINK="$base/bin/walgit" \
        "$app/Contents/MacOS/walgit-tray" >/dev/null 2>&1 || true
    ( kill "$srv_pid" 2>/dev/null; wait "$srv_pid" 2>/dev/null ) || true

    [ -f "$new/walgit.toml" ] || { echo "FAIL(manual-legacy): config not migrated" >&2; return 1; }
    [ -f "$old/walgit.toml" ] || { echo "FAIL(manual-legacy): old config lost" >&2; return 1; }
    [ "$(cat "$version_file")" = "v0.5.0" ] \
        || { echo "FAIL(manual-legacy): service not restarted from new bundle" >&2; return 1; }
    return 0
}

# Release 检测自检:无 GUI 也要能验证「菜单指向哪个版本」(macOS Release 通道)。
# WALGIT_DETECT_ONCE=1 让托盘跑一次检测后打印菜单行退出,WALGIT_RELEASE_FIXTURE
# 用本地 JSON 代替 GitHub API,检查结果同时落在 tray.log。
release_detect_fixture() {
    local base="$TMP/release-detect"
    local app="$base/walgit-tray.app"
    local res="$app/Contents/Resources"
    local state="$base/state"
    local arch
    case "$(uname -m)" in
        arm64) arch=arm64 ;;
        x86_64) arch=x86_64 ;;
        *) echo "skip(release-detect): unsupported arch" >&2; return 0 ;;
    esac
    mkdir -p "$app/Contents/MacOS" "$res" "$state"
    pkginfo "$app" 0.5.0
    cp "$TRAY_BIN" "$app/Contents/MacOS/walgit-tray"
    printf '#!/bin/sh\n[ "${1:-}" = "--version" ] && echo "walgit v0.5.0"\n' >"$res/walgit"
    chmod +x "$res/walgit"
    # 空端口:别让 fixture 读到开发机上真实在跑的服务(菜单行要可预期)。
    local port
    port="$(free_port)"
    printf '[server]\nlisten = "127.0.0.1:%s"\n' "$port" >"$state/walgit.toml"

    write_release_fixture() { # <file> <version>
        printf '{"tag_name":"v%s","assets":[{"name":"walgit-%s-%s.dmg","browser_download_url":"https://example.invalid/w.dmg","digest":"sha256:%s"}]}\n' \
            "$2" "$2" "$arch" "$(printf 'a%.0s' $(seq 1 64))" >"$1"
    }

    write_release_fixture "$base/newer.json" 0.9.9
    local out
    out="$(WALGIT_STATE_DIR="$state" WALGIT_RELEASE_FIXTURE="$base/newer.json" \
        WALGIT_DETECT_ONCE=1 "$app/Contents/MacOS/walgit-tray")"
    case "$out" in
        *"⬆️ 下载并升级到 v0.9.9"*"当前 0.5.0"*) ;;
        *) echo "FAIL(release-detect): menu line wrong: $out" >&2; return 1 ;;
    esac
    grep -q "detect: app=0.5.0 release=v0.9.9 → available" "$state/tray.log" \
        || { echo "FAIL(release-detect): detection not logged" >&2; return 1; }

    # 同版本 → 已是最新(不能自己提示升级到自己)。
    write_release_fixture "$base/same.json" 0.5.0
    out="$(WALGIT_STATE_DIR="$state" WALGIT_RELEASE_FIXTURE="$base/same.json" \
        WALGIT_DETECT_ONCE=1 "$app/Contents/MacOS/walgit-tray")"
    case "$out" in
        *"已是最新"*) ;;
        *) echo "FAIL(release-detect): same version not up-to-date: $out" >&2; return 1 ;;
    esac

    # 旧版本 → 已是最新(降级提示是 bug)。
    write_release_fixture "$base/older.json" 0.4.0
    out="$(WALGIT_STATE_DIR="$state" WALGIT_RELEASE_FIXTURE="$base/older.json" \
        WALGIT_DETECT_ONCE=1 "$app/Contents/MacOS/walgit-tray")"
    case "$out" in
        *"已是最新"*) ;;
        *) echo "FAIL(release-detect): older release offered: $out" >&2; return 1 ;;
    esac
    return 0
}

# #200：点 Dock 图标 → reopen 钩子（注入到 winit 的 delegate 类）→ 打开 Web UI。
# CI 没有 Dock 可点，所以让托盘用 `WALGIT_REOPEN_SELFTEST=1` 直接调那个选择子：
# 走的正是 AppKit 点击时调用的同一个 IMP，调用完即退出（不需要窗口服务器）。
reopen_hook_fixture() {
    local base="$TMP/reopen"
    local app="$base/walgit-tray.app"
    local res="$app/Contents/Resources"
    local state="$base/state"
    mkdir -p "$app/Contents/MacOS" "$res" "$state"
    pkginfo "$app" 0.6.4
    cp "$TRAY_BIN" "$app/Contents/MacOS/walgit-tray"
    printf '#!/bin/sh\nexit 0\n' >"$res/walgit"
    chmod +x "$res/walgit"
    local port
    port="$(free_port)"
    printf '[server]\nlisten = "127.0.0.1:%s"\n' "$port" >"$state/walgit.toml"

    # Bounded: without the hook the selftest never runs, the tray enters its GUI
    # loop and would hang the fixture forever (found the hard way) — so the run
    # gets a deadline and "still alive" is a failure, not a wait.
    local out
    # `WALGIT_OPEN_URL_FILE`: the hook's action (open the Web UI) writes the URL
    # there instead of launching a browser — the fixture asserts *that*, so
    # deleting `open_web()` from the hook cannot stay green, and CI never pops a
    # browser window.
    WALGIT_STATE_DIR="$state" WALGIT_CLI_LINK="$base/bin/walgit" \
        WALGIT_REOPEN_SELFTEST=1 WALGIT_OPEN_URL_FILE="$base/open-url.txt" \
        "$app/Contents/MacOS/walgit-tray" >"$base/out.txt" 2>&1 &
    local pid=$!
    local waited=0
    while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 60 ]; do
        sleep 0.25
        waited=$((waited + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
        kill -9 "$pid" 2>/dev/null
        echo "FAIL(reopen): selftest did not exit within 15s (hook not installed?)" >&2
        return 1
    fi
    wait "$pid" 2>/dev/null || true
    out="$(cat "$base/out.txt")"
    case "$out" in
        *"reopen-selftest handled=false"*) ;;
        *) echo "FAIL(reopen): selftest did not invoke the hook: $out" >&2; return 1 ;;
    esac
    grep -q "dock: reopen hook installed=true" "$state/tray.log" \
        || { echo "FAIL(reopen): hook not installed on the delegate class" >&2; return 1; }
    # The action itself: the hook must have asked for the Web UI URL. (A log line
    # alone stayed green even with `open_web()` deleted — review finding.)
    grep -Fqx "http://127.0.0.1:$port/" "$base/open-url.txt" \
        || { echo "FAIL(reopen): the hook did not open the Web UI: $(cat "$base/open-url.txt" 2>/dev/null)" >&2; return 1; }
    return 0
}

pkginfo() { # pkginfo <app> <version>
    mkdir -p "$1/Contents/Resources"
    cat >"$1/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>com.walgit.tray.test</string>
    <key>CFBundleShortVersionString</key><string>$2</string>
    <key>CFBundleVersion</key><string>$2</string>
</dict>
</plist>
PLIST
}

make_app_binary() { # make_app_binary <app> <version>
    cat >"$1/Contents/Resources/walgit" <<STUB
#!/bin/sh
case "\${1:-}" in
  --version) echo "walgit v$2" ;;
  service) exit 0 ;;
esac
STUB
    chmod +x "$1/Contents/Resources/walgit"
}

release_install_fixture() { # release_install_fixture <success|rollback>
    local expect="$1"
    local base="$TMP/release-$expect"
    local state="$base/state"
    local dest="$base/Applications/walgit-tray.app"
    local mount="$base/mount"
    local port
    port="$(free_port)"
    mkdir -p "$state" "$mount/walgit-tray.app/Contents/Resources"
    printf '[server]\nlisten = "127.0.0.1:%s"\n' "$port" >"$state/walgit.toml"
    printf 'keep\n' >"$state/keep"
    for stale in walgit walgit-ensure run-walgit.sh .skeleton-version; do
        printf 'old\n' >"$state/$stale"
    done
    pkginfo "$dest" 0.5.0
    make_app_binary "$dest" 0.5.0
    pkginfo "$mount/walgit-tray.app" 0.5.1
    if [ "$expect" = success ]; then
        make_app_binary "$mount/walgit-tray.app" 0.5.1
    else
        # App bundle says 0.5.1 but its program binary is wrong: version guard
        # must roll the whole app back.
        make_app_binary "$mount/walgit-tray.app" 9.9.9
    fi
    : >"$base/x.dmg"

    local rc=0
    WALGIT_DEPLOY_DIR="$state" \
    WALGIT_UPDATE_SKIP_SERVICE=1 \
    WALGIT_UPDATE_SKIP_OPEN=1 \
    WALGIT_UPDATE_TRAY_WAIT=1 \
    WALGIT_UPDATE_BOOTSTRAP_WAIT=2 \
        ./release-install.sh "$base/x.dmg" "$mount" "$dest" 0.5.1 999999 >/dev/null 2>&1 || rc=$?

    if [ "$expect" = success ]; then
        [ "$rc" = 0 ] || { echo "FAIL(release-success): rc=$rc" >&2; return 1; }
        [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$dest/Contents/Info.plist")" = "0.5.1" ] \
            || { echo "FAIL(release-success): app not replaced" >&2; return 1; }
        ls "$dest".bak-* >/dev/null 2>&1 \
            || { echo "FAIL(release-success): no app backup" >&2; return 1; }
    else
        [ "$rc" != 0 ] || { echo "FAIL(release-rollback): expected failure" >&2; return 1; }
        [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$dest/Contents/Info.plist")" = "0.5.0" ] \
            || { echo "FAIL(release-rollback): old app not restored" >&2; return 1; }
        ls "$dest".failed-* >/dev/null 2>&1 \
            || { echo "FAIL(release-rollback): failed app not kept" >&2; return 1; }
    fi
    [ -f "$state/walgit.toml" ] || { echo "FAIL(release-$expect): config lost" >&2; return 1; }
    [ -f "$state/keep" ] || { echo "FAIL(release-$expect): user file lost" >&2; return 1; }
    for stale in walgit walgit-ensure run-walgit.sh .skeleton-version; do
        [ ! -e "$state/$stale" ] || { echo "FAIL(release-$expect): stale $stale remains" >&2; return 1; }
    done
    return 0
}

make_service_app_binary() { # make_service_app_binary <app> <version> <state> <version-file>
    cat >"$1/Contents/Resources/walgit" <<STUB
#!/bin/sh
case "\${1:-}" in
  --version) echo "walgit v$2" ;;
  service)
    echo "\${2:-}" >>"$3/service.calls"
    case "\${2:-}" in
      start) printf 'v$2\n' >"$4" ;;
      stop) printf 'v0.0.0\n' >"$4" ;;
    esac
    ;;
esac
STUB
    chmod +x "$1/Contents/Resources/walgit"
}

release_service_fixture() {
    local base="$TMP/release-service"
    local state="$base/state"
    local dest="$base/Applications/walgit-tray.app"
    local mount="$base/mount"
    local version_file="$base/version"
    local port
    port="$(free_port)"
    mkdir -p "$state" "$mount/walgit-tray.app/Contents/Resources"
    printf '[server]\nlisten = "127.0.0.1:%s"\n' "$port" >"$state/walgit.toml"
    printf 'v0.5.0\n' >"$version_file"
    pkginfo "$dest" 0.5.0
    make_service_app_binary "$dest" 0.5.0 "$state" "$version_file"
    pkginfo "$mount/walgit-tray.app" 0.5.1
    make_service_app_binary "$mount/walgit-tray.app" 0.5.1 "$state" "$version_file"
    : >"$base/x.dmg"

    cat >"$base/health.py" <<'PYS'
import os, socket, sys
port = int(sys.argv[1]); vfile = sys.argv[2]
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port)); srv.listen(5)
while True:
    try: c, _ = srv.accept()
    except OSError: break
    try:
        c.recv(4096)
        v = open(vfile).read().strip() if os.path.exists(vfile) else "v0.0.0"
        body = '{"status":"ok","version":"%s"}' % v
        c.sendall(("HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % len(body)).encode() + body.encode())
    except OSError: pass
    finally: c.close()
PYS
    python3 "$base/health.py" "$port" "$version_file" >/dev/null 2>&1 &
    local srv_pid=$!
    sleep 0.5

    local rc=0
    WALGIT_DEPLOY_DIR="$state" \
    WALGIT_UPDATE_SKIP_OPEN=1 \
    WALGIT_UPDATE_TRAY_WAIT=1 \
    WALGIT_UPDATE_BOOTSTRAP_WAIT=2 \
    WALGIT_UPDATE_HEALTH_WAIT=3 \
        ./release-install.sh "$base/x.dmg" "$mount" "$dest" 0.5.1 999999 >/dev/null 2>&1 || rc=$?
    ( kill "$srv_pid" 2>/dev/null; wait "$srv_pid" 2>/dev/null ) || true

    [ "$rc" = 0 ] || { echo "FAIL(release-service): rc=$rc" >&2; return 1; }
    grep -qx stop "$state/service.calls" || { echo "FAIL(release-service): old service not stopped" >&2; return 1; }
    grep -qx start "$state/service.calls" || { echo "FAIL(release-service): new service not started" >&2; return 1; }
    return 0
}

bash_compat_smoke
layout_fixture
bootstrap_fixture
cli_link_fallback_fixture
legacy_migration_fixture
manual_legacy_migration_fixture
release_detect_fixture
reopen_hook_fixture
release_install_fixture success
release_install_fixture rollback
release_service_fixture
bash -n release-install.sh
echo "tray macos tests: ok"
