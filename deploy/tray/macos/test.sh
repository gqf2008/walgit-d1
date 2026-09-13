#!/bin/bash
# macOS tray Release logic and package guard tests. Runs without starting the tray.
set -euo pipefail
cd "$(dirname "$0")"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/walgit-tray-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# 托盘主程序也要编译(此前 CI 只编译 ReleaseLogic,主程序坏了仍绿)。
swiftc -swift-version 5 -typecheck ReleaseLogic.swift walgit-tray.swift -framework AppKit

cp release_logic_test_main.swift "$TMP/main.swift"
swiftc -swift-version 5 ReleaseLogic.swift "$TMP/main.swift" -o "$TMP/release-logic-tests"
"$TMP/release-logic-tests"

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
    printf 'app-%s\n' "$2" >"$1/Contents/Resources/bundle-version"
}

stub_walgit() { # stub_walgit <deploy-dir> <version>
    cat >"$1/walgit" <<STUB
#!/bin/sh
[ "\${1:-}" = "--version" ] && echo "walgit v$2"
# WALGIT_TEST_PARTIAL_BOOTSTRAP:被核验调用时模拟新 app bootstrap「改了一半」
# ——更新 marker 与脚本、但二进制仍是旧版本;随后必须被回滚还原。
if [ -n "\${WALGIT_TEST_PARTIAL_BOOTSTRAP:-}" ]; then
    printf '\${WALGIT_TEST_PARTIAL_VERSION:-0.6.0}\n' > "\$(dirname "\$0")/.skeleton-version"
    printf 'run-\${WALGIT_TEST_PARTIAL_VERSION:-0.6.0}\n' > "\$(dirname "\$0")/run-walgit.sh"
fi
STUB
    chmod +x "$1/walgit"
}

stub_managed() { # stub_managed <deploy-dir> <version>
    printf 'run-%s\n' "$2" >"$1/run-walgit.sh"
    chmod +x "$1/run-walgit.sh"
}

# release_install_fixture <name> <old-version> <new-version> <success|rollback>
# 用一个必然不存在的 TRAY_PID 绕过真实托盘；WALGIT_UPDATE_SKIP_* 关闭
# 服务与 open 副作用。rollback 分支刻意不写新 marker/二进制，触发超时回滚。
release_install_fixture() {
    local name="$1"
    local old="$2"
    local new="$3"
    local expect="$4"
    local base="$TMP/fixture-$name"
    local deploy="$base/deploy"
    local dest="$base/Applications/walgit-tray.app"
    local mount="$base/mount"
    local dmg="$base/walgit-$new-arm64.dmg"
    rm -rf "$base"
    mkdir -p "$deploy" "$dest" "$mount/walgit-tray.app"
    pkginfo "$dest" "$old"
    stub_walgit "$deploy" "$old"
    printf '%s\n' "$old" >"$deploy/.skeleton-version"
    stub_managed "$deploy" "$old"
    : >"$dmg"
    pkginfo "$mount/walgit-tray.app" "$new"
    if [ "$expect" = success ]; then
        # 模拟新 app bootstrap 完成：marker 与部署二进制都已是新版本。
        printf '%s\n' "$new" >"$deploy/.skeleton-version"
        stub_walgit "$deploy" "$new"
    fi

    local partial=""
    [ "$expect" = success ] || partial="1"
    local rc=0
    WALGIT_DEPLOY_DIR="$deploy" \
    WALGIT_UPDATE_SKIP_SERVICE=1 \
    WALGIT_UPDATE_SKIP_OPEN=1 \
    WALGIT_UPDATE_BOOTSTRAP_WAIT=3 \
    WALGIT_UPDATE_TRAY_WAIT=3 \
    WALGIT_TEST_PARTIAL_BOOTSTRAP="$partial" \
    WALGIT_TEST_PARTIAL_VERSION="$new" \
        ./release-install.sh "$dmg" "$mount" "$dest" "$new" 999999 >/dev/null 2>&1 || rc=$?

    if [ "$expect" = success ]; then
        [ "$rc" = 0 ] || { echo "FAIL($name): expected success, rc=$rc" >&2; return 1; }
        grep -qx "app-$new" "$dest/Contents/Resources/bundle-version" \
            || { echo "FAIL($name): app not replaced" >&2; return 1; }
        ls "$dest".bak-* >/dev/null 2>&1 \
            || { echo "FAIL($name): no app backup kept" >&2; return 1; }
        grep -qx "$new" "$deploy/.skeleton-version" \
            || { echo "FAIL($name): deploy marker not on new version" >&2; return 1; }
    else
        [ "$rc" != 0 ] || { echo "FAIL($name): expected rollback failure" >&2; return 1; }
        grep -qx "app-$old" "$dest/Contents/Resources/bundle-version" \
            || { echo "FAIL($name): old app not restored" >&2; return 1; }
        # 备份发生在 partial-bootstrap 之前,回滚必须把四项都还原成旧值。
        grep -qx "$old" "$deploy/.skeleton-version" \
            || { echo "FAIL($name): deploy marker not restored" >&2; return 1; }
        "$deploy/walgit" --version | grep -q "v$old" \
            || { echo "FAIL($name): deploy binary not restored" >&2; return 1; }
        grep -qx "run-$old" "$deploy/run-walgit.sh" \
            || { echo "FAIL($name): run-walgit.sh not restored" >&2; return 1; }
    fi
}


bootstrap_fixture() {
    local base="$TMP/bootstrap"
    local app="$base/walgit-tray.app"
    local res="$app/Contents/Resources"
    local deploy="$base/deploy"
    rm -rf "$base"
    mkdir -p "$app/Contents/MacOS" "$res" "$deploy"
    swiftc -swift-version 5 -framework AppKit walgit-tray.swift ReleaseLogic.swift \
        -o "$app/Contents/MacOS/walgit-tray" || { echo "FAIL(bootstrap): compile tray" >&2; return 1; }
    cat >"$res/walgit" <<'STUB'
#!/bin/sh
[ "${1:-}" = "--version" ] && echo "walgit v0.5.0"
STUB
    printf '#!/bin/sh\nexit 0\n' >"$res/run-walgit.sh"
    printf '#!/bin/sh\nexit 0\n' >"$res/walgit-ensure"
    chmod +x "$res/walgit" "$res/run-walgit.sh" "$res/walgit-ensure"
    printf '0.5.0\n' >"$res/skeleton.version"
    printf '[server]\nlisten = "127.0.0.1:8081"\n' >"$res/walgit.toml"

    # 旧部署骨架
    printf '#!/bin/sh\necho "walgit v0.4.0"\n' >"$deploy/walgit"
    printf 'old\n' >"$deploy/run-walgit.sh"
    printf 'old\n' >"$deploy/walgit-ensure"
    chmod +x "$deploy/walgit" "$deploy/run-walgit.sh" "$deploy/walgit-ensure"
    printf '0.4.0\n' >"$deploy/.skeleton-version"

    WALGIT_BOOTSTRAP_ONLY=1 WALGIT_DEPLOY_DIR="$deploy" WALGIT_CLI_LINK="$base/bin/walgit" \
        "$app/Contents/MacOS/walgit-tray" >/dev/null 2>&1 \
        || { echo "FAIL(bootstrap): tray exited nonzero" >&2; return 1; }
    grep -qx "0.5.0" "$deploy/.skeleton-version" \
        || { echo "FAIL(bootstrap): marker not upgraded" >&2; return 1; }
    "$deploy/walgit" --version | grep -qx "walgit v0.5.0" \
        || { echo "FAIL(bootstrap): walgit binary not upgraded" >&2; return 1; }
    grep -qx "#!/bin/sh" "$deploy/walgit-ensure" \
        || { echo "FAIL(bootstrap): walgit-ensure not upgraded" >&2; return 1; }

    # 自愈：删掉一个托管文件，marker 仍是同一版本，下一次 bootstrap 应补回。
    rm "$deploy/walgit-ensure"
    WALGIT_BOOTSTRAP_ONLY=1 WALGIT_DEPLOY_DIR="$deploy" WALGIT_CLI_LINK="$base/bin/walgit" \
        "$app/Contents/MacOS/walgit-tray" >/dev/null 2>&1 \
        || { echo "FAIL(bootstrap): self-heal exited nonzero" >&2; return 1; }
    [ -f "$deploy/walgit-ensure" ] \
        || { echo "FAIL(bootstrap): missing walgit-ensure not self-healed" >&2; return 1; }
    return 0
}

release_install_fixture success 0.4.0 0.5.0 success
release_install_fixture rollback 0.4.0 0.6.0 rollback
# #170 菜单版本语义:upgrade 行显示托盘版本,服务版本单独标注。
# 修复前它把服务版本当 "版本/当前" 打印,于是「已是最新」旁边会印旧的服务版本。
menu_fixture() {
    local app="$TMP/menu/walgit-tray.app"
    local tray_bin="$app/Contents/MacOS/walgit-tray"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    swiftc -swift-version 5 -framework AppKit walgit-tray.swift ReleaseLogic.swift \
        -o "$tray_bin" || { echo "FAIL(menu): compile tray" >&2; return 1; }
    local out
    # healthz 版本解析:必须精确,不能把 v0.5.10 当 v0.5.1;容忍键值间空格。
    case "$(WALGIT_HEALTH_TEST='{"status":"ok","version":"v0.5.10"}' "$tray_bin")" in
        v0.5.10) ;;
        *) echo "FAIL(menu): healthz version parse wrong" >&2; return 1 ;;
    esac
    # 正控目标:下面这条在把 healthVersion 改回子串匹配时必须红
    case "$(WALGIT_HEALTH_TEST='{ "status": "ok", "version": "v0.5.1" }' "$tray_bin")" in
        v0.5.1) ;;
        *) echo "FAIL(menu): spaced healthz JSON not parsed" >&2; return 1 ;;
    esac
    # idle:检查前的版本行
    out="$(WALGIT_MENU_TEST=0.5.1 WALGIT_MENU_SERVICE=v0.5.0 WALGIT_MENU_STATE=idle "$tray_bin")"
    case "$out" in
        *"版本 0.5.1"*"服务 0.5.0"*"检查更新…"*) ;;
        *) echo "FAIL(menu): idle line wrong: $out" >&2; return 1 ;;
    esac
    out="$(WALGIT_MENU_TEST=0.5.1 WALGIT_MENU_SERVICE=v0.5.0 WALGIT_MENU_STATE=latest "$tray_bin")"
    case "$out" in
        *"版本 0.5.1"*"已是最新"*) ;;
        *) echo "FAIL(menu): upgrade line wrong: $out" >&2; return 1 ;;
    esac
    case "$out" in
        *"版本 0.5.0"*) echo "FAIL(menu): still prints service version as current: $out" >&2; return 1 ;;
    esac
    case "$out" in
        *"服务 0.5.0"*) ;;
        *) echo "FAIL(menu): service version not shown separately: $out" >&2; return 1 ;;
    esac
    out="$(WALGIT_MENU_TEST=0.5.0 WALGIT_MENU_SERVICE=v0.5.0 WALGIT_MENU_STATE=available WALGIT_MENU_RELEASE=0.5.1 "$tray_bin")"
    case "$out" in
        *"下载并升级到 v0.5.1"*"当前 0.5.0"*) ;;
        *) echo "FAIL(menu): available line wrong: $out" >&2; return 1 ;;
    esac
    # 无 release → 源码升级分支
    out="$(WALGIT_MENU_TEST=0.5.0 WALGIT_MENU_SERVICE=v0.5.0 WALGIT_MENU_STATE=available WALGIT_MENU_SOURCE=abc1234 "$tray_bin")"
    case "$out" in
        *"从源码升级到 abc1234"*"当前 0.5.0"*) ;;
        *) echo "FAIL(menu): source line wrong: $out" >&2; return 1 ;;
    esac
    # 服务停着时不印空"服务"
    out="$(WALGIT_MENU_TEST=0.5.1 WALGIT_MENU_SERVICE="" WALGIT_MENU_STATE=latest "$tray_bin")"
    case "$out" in
        *"服务 "*) echo "FAIL(menu): empty service label: $out" >&2; return 1 ;;
    esac
    # installing / checking / failed 的文案
    out="$(WALGIT_MENU_TEST=0.5.1 WALGIT_MENU_STATE=installing WALGIT_MENU_BUSY=下载中… "$tray_bin")"
    case "$out" in
        *"升级中…"*"下载中…"*) ;;
        *) echo "FAIL(menu): installing line wrong: $out" >&2; return 1 ;;
    esac
    out="$(WALGIT_MENU_TEST=0.5.1 WALGIT_MENU_STATE=checking "$tray_bin")"
    case "$out" in
        *"正在检查更新…"*) ;;
        *) echo "FAIL(menu): checking line wrong: $out" >&2; return 1 ;;
    esac
    out="$(WALGIT_MENU_TEST=0.5.1 WALGIT_MENU_STATE=failed "$tray_bin")"
    case "$out" in
        *"上次升级失败"*) ;;
        *) echo "FAIL(menu): failed line wrong: $out" >&2; return 1 ;;
    esac
    return 0
}

# #170 helper 路径:升级前的服务状态决定升级后要不要动服务。
#  (a) 升级前在跑 → 必须重启并确认 /healthz 到 v<new>;
#  (b) 升级前停着 → 不得拉起(托盘里"停止服务"是明确意图)。
update_service_fixture() {
    local mode="$1"           # running | stopped
    local force_rollback="${2:-0}"
    local base="$TMP/update-$mode-$force_rollback"
    local deploy="$base/deploy"
    local dest="$base/Applications/walgit-tray.app"
    local mount="$base/mount"
    local calls="$base/ensure.calls"
    local port
    port="$(python3 - <<'PYPORT'
import socket
s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()
PYPORT
)"
    rm -rf "$base"
    mkdir -p "$deploy" "$mount/walgit-tray.app/Contents/Resources" "$dest/Contents/Resources" "$base/bin"
    : >"$base/x.dmg"
    printf 'listen = "127.0.0.1:%s"\n' "$port" >"$deploy/walgit.toml"
    pkginfo "$dest" "0.5.0"
    # force_rollback:挂载点里的 app 版本与请求升级的版本不符 → 必然回滚
    if [ "$force_rollback" = 1 ]; then
        pkginfo "$mount/walgit-tray.app" "0.9.9"
    else
        pkginfo "$mount/walgit-tray.app" "0.5.1"
    fi
    printf '0.5.1\n' >"$deploy/.skeleton-version"
    stub_walgit "$deploy" "0.5.1"
    stub_managed "$deploy" "0.5.1"
    printf '#!/bin/sh\nexit 0\n' >"$deploy/run-walgit.sh"; chmod +x "$deploy/run-walgit.sh"

    # 假服务:从 $base/version 读版本返回
    cat >"$base/server.py" <<'PYS'
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
    printf 'v0.5.0\n' >"$base/version"
    local srv_pid=""
    if [ "$mode" = running ]; then
        python3 "$base/server.py" "$port" "$base/version" >/dev/null 2>&1 &
        srv_pid=$!
        sleep 0.8
    fi

    # 服务生命周期现在是 `walgit service <verb>`:stub 记录 service 子命令的
    # 动词;start/restart 时把版本推进到 v0.5.1(模拟重启到新二进制)。
    cat >"$deploy/walgit" <<STUB
#!/bin/sh
if [ "\${1:-}" = "--version" ]; then echo "walgit v0.5.1"; exit 0; fi
if [ "\${1:-}" = "service" ]; then
  echo "\${2:-}" >>"$calls"
  case "\${2:-start}" in
    start|restart) printf 'v0.5.1\n' >"$base/version" ;;
  esac
fi
exit 0
STUB
    chmod +x "$deploy/walgit"
    printf '#!/bin/sh\nexit 0\n' >"$deploy/walgit-ensure"; chmod +x "$deploy/walgit-ensure"
    : >"$calls"

    local rc=0
    WALGIT_DEPLOY_DIR="$deploy" WALGIT_UPDATE_SKIP_OPEN=1 \
    WALGIT_UPDATE_BOOTSTRAP_WAIT=4 WALGIT_UPDATE_TRAY_WAIT=2 WALGIT_UPDATE_HEALTH_WAIT=6 \
        ./release-install.sh "$base/x.dmg" "$mount" "$dest" 0.5.1 999999 >/dev/null 2>&1 || rc=$?

    local got=""
    if [ -n "$srv_pid" ]; then
        got="$(curl -sf --max-time 2 "http://127.0.0.1:$port/healthz" 2>/dev/null || true)"
        ( kill "$srv_pid" 2>/dev/null; wait "$srv_pid" 2>/dev/null ) || true
    fi
    local leftover
    leftover="$(lsof -tiTCP:"$port" -sTCP:LISTEN 2>/dev/null || true)"
    [ -n "$leftover" ] && ( kill $leftover 2>/dev/null ) || true

    if [ "$mode" = running ]; then
        if [ "$force_rollback" = 1 ]; then
            # 在跑 + 强制回滚:必须先 stop(再恢复旧版),不得把服务落在停止状态。
            [ "$rc" != 0 ] || { echo "FAIL(rollback-running): expected failure" >&2; return 1; }
            grep -qx stop "$calls" || { echo "FAIL(rollback-running): not stopped" >&2; return 1; }
            grep -qx start "$calls" || { echo "FAIL(rollback-running): running service not restored" >&2; return 1; }
            return 0
        fi
        [ "$rc" = 0 ] || { echo "FAIL(update-running): rc=$rc" >&2; cat "$deploy/tray.log" >&2; return 1; }
        grep -qx stop "$calls" || { echo "FAIL(update-running): not stopped" >&2; return 1; }
        grep -qx start "$calls" || { echo "FAIL(update-running): not restarted" >&2; return 1; }
        case "$got" in
            *'"version":"v0.5.1"'*) ;;
            *) echo "FAIL(update-running): healthz still $got" >&2; return 1 ;;
        esac
    else
        # 先查"有没有被拉起"再查 rc:无条件重启会连带 rollback,rc 非 0 会
        # 掩盖真正的违规(用户停着的服务被偷偷启动)。force_rollback=1 时
        # 升级必然失败,同样不得借"恢复旧版本"之名把服务拉起来。
        if grep -qx start "$calls"; then
            echo "FAIL(update-stopped): started a service the user had stopped" >&2
            return 1
        fi
        if [ "$force_rollback" = 1 ]; then
            [ "$rc" != 0 ] || { echo "FAIL(update-stopped-rollback): expected failure" >&2; return 1; }
        else
            [ "$rc" = 0 ] || { echo "FAIL(update-stopped): rc=$rc" >&2; return 1; }
        fi
    fi
    return 0
}

bootstrap_restart_fixture() {
    local base="$TMP/bootrestart"
    local app="$base/walgit-tray.app"
    local res="$app/Contents/Resources"
    local deploy="$base/deploy"
    local port
    port="$(python3 - <<'PYPORT'
import socket
s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()
PYPORT
)"
    rm -rf "$base"
    mkdir -p "$app/Contents/MacOS" "$res" "$deploy" "$base/bin"
    swiftc -swift-version 5 -framework AppKit walgit-tray.swift ReleaseLogic.swift \
        -o "$app/Contents/MacOS/walgit-tray" || { echo "FAIL(bootrestart): compile" >&2; return 1; }

    # bundle 里是 v0.5.1
    # 服务重启现在是 `walgit service restart`（脚本不再是负责人）：stub 必须
    # 既回答 --version，也实现 restart 的副作用（把假服务切到新版本）。
    cat >"$res/walgit" <<STUB
#!/bin/sh
case "\${1:-}" in
  --version) echo "walgit v0.5.1" ;;
  service) printf 'v0.5.1\n' >"$base/version" ;;
esac
STUB
    chmod +x "$res/walgit"
    printf '#!/bin/sh\nexit 0\n' >"$res/run-walgit.sh"; chmod +x "$res/run-walgit.sh"
    # bundle 自带的 walgit-ensure 就是"重启"语义:写新版本号,让假服务随之更新。
    printf '#!/bin/sh\nprintf "v0.5.1\\n" >"%s/version"\n' "$base" >"$res/walgit-ensure"
    chmod +x "$res/walgit-ensure"
    printf '0.5.1\n' >"$res/skeleton.version"
    printf '[server]\nlisten = "127.0.0.1:%s"\n' "$port" >"$res/walgit.toml"

    # 部署里是 v0.5.0 + marker 0.5.0
    cat >"$deploy/walgit" <<STUB
#!/bin/sh
case "\${1:-}" in
  --version) echo "walgit v0.5.0" ;;
  service) printf 'v0.5.1\n' >"$base/version" ;;
esac
STUB
    chmod +x "$deploy/walgit"
    printf 'old\n' >"$deploy/run-walgit.sh"; chmod +x "$deploy/run-walgit.sh"
    printf 'old\n' >"$deploy/walgit-ensure"
    printf '0.5.0\n' >"$deploy/.skeleton-version"
    printf '[server]\nlisten = "127.0.0.1:%s"\n' "$port" >"$deploy/walgit.toml"

    # 假服务:从 $base/version 读版本返回
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
    printf 'v0.5.0\n' >"$base/version"
    python3 "$base/health.py" "$port" "$base/version" >/dev/null 2>&1 &
    local srv_pid=$!
    sleep 0.8

    # 替换后的 walgit-ensure:重启假服务并把版本标记为新
    printf '#!/bin/sh\nprintf "v0.5.1\\n" >"%s/version"\n' "$base" >"$deploy/walgit-ensure"
    chmod +x "$deploy/walgit-ensure"
    : >"$base/ensure.calls"

    WALGIT_BOOTSTRAP_ONLY=1 WALGIT_DEPLOY_DIR="$deploy" WALGIT_CLI_LINK="$base/bin/walgit" \
        "$app/Contents/MacOS/walgit-tray" >/dev/null 2>&1 \
        || { ( kill "$srv_pid" 2>/dev/null ); echo "FAIL(bootrestart): tray rc" >&2; return 1; }

    local got
    got="$(curl -sf --max-time 2 "http://127.0.0.1:$port/healthz" 2>/dev/null || true)"
    ( kill "$srv_pid" 2>/dev/null; wait "$srv_pid" 2>/dev/null ) || true
    case "$got" in
        *'"version":"v0.5.1"'*) ;;
        *) echo "FAIL(bootrestart): service not restarted to v0.5.1: $got" >&2; return 1 ;;
    esac
    return 0
}

bootstrap_restart_fixture
menu_fixture
bootstrap_fixture
update_service_fixture running
update_service_fixture stopped
# 回滚:在跑的必须恢复;停着的不许被拉起
update_service_fixture running 1
update_service_fixture stopped 1

bash -n release-install.sh
echo "tray macos tests: ok"
