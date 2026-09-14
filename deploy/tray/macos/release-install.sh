#!/bin/bash
# release-install.sh — detached helper for macOS tray Release updates.
# Invoked as: release-install.sh <dmg> <mount> <app-dest> <version> <tray-pid>
#
# Program binaries live in the App Bundle; ~/.walgit is user state only.
# Upgrade replaces the app bundle (with rollback), then restarts the service
# from the restored/replaced bundle. Legacy managed copies under ~/.walgit are
# removed after the old service stops; config, cache, keys and credentials stay.
#
# 测试用覆盖（生产不设）：
#   WALGIT_DEPLOY_DIR / WALGIT_UPDATE_SKIP_SERVICE / WALGIT_UPDATE_SKIP_OPEN
#   WALGIT_UPDATE_HEALTHCHECK  探活命令(默认 curl)
#   WALGIT_UPDATE_TRAY_WAIT / WALGIT_UPDATE_BOOTSTRAP_WAIT / WALGIT_UPDATE_HEALTH_WAIT
#    —— 三段轮询次数(每段 0.1/0.5s)，仅测试用；未设按生产值。
set -euo pipefail

DMG="${1:?dmg}"
MOUNT="${2:?mount}"
APP_DEST="${3:?app destination}"
VERSION="${4#v}"
TRAY_PID="${5:?tray pid}"
DEPLOY="${WALGIT_DEPLOY_DIR:-$HOME/.walgit}"
LOG="$DEPLOY/tray.log"

TRAY_WAIT="${WALGIT_UPDATE_TRAY_WAIT:-300}"            # 0.1s/次 → 30s
BOOTSTRAP_WAIT="${WALGIT_UPDATE_BOOTSTRAP_WAIT:-120}"  # 0.5s/次 → 60s
HEALTH_WAIT="${WALGIT_UPDATE_HEALTH_WAIT:-30}"         # 0.5s/次 → 15s

log() {
    mkdir -p "$DEPLOY"
    printf '[%s] update v%s: %s\n' "$(date '+%H:%M:%S')" "$VERSION" "$*" >>"$LOG"
}
notify() {
    /usr/bin/osascript -e "display notification \"$1\" with title \"walgit 升级\"" >/dev/null 2>&1 || true
}
fail() {
    log "FAIL: $*"
    notify "$*"
    exit 1
}

cleanup_mount() {
    /usr/bin/hdiutil detach "$MOUNT" >/dev/null 2>&1 || true
}
trap cleanup_mount EXIT

[ -f "$DMG" ] || fail "安装包不存在: $DMG"
[ -d "$MOUNT/walgit-tray.app" ] || fail "挂载点缺少 walgit-tray.app: $MOUNT"

# Wait for the old tray to exit so replacing the bundle cannot race the process.
for _ in $(seq 1 "$TRAY_WAIT"); do
    kill -0 "$TRAY_PID" 2>/dev/null || break
    sleep 0.1
done
kill -0 "$TRAY_PID" 2>/dev/null && fail "旧托盘未退出"

healthcheck() { # healthcheck <url> [extra args...]
    local url="$1"; shift
    if [ -n "${WALGIT_UPDATE_HEALTHCHECK:-}" ]; then
        "$WALGIT_UPDATE_HEALTHCHECK" "$@" "$url"
    else
        curl -sf --max-time 2 "$url"
    fi
}

health_version() {
    local body
    body="$(healthcheck "$HEALTH_URL" --max-time 2 2>/dev/null || true)"
    printf '%s' "$body" | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'
}

# 与状态配置同源:自定义 [server].listen 时,预探活/重启后健康检查都要用
# 实际端口,否则会把「运行中」误判成停止后跳过启动。
listen_addr() {
    local l=""
    if [ -f "$DEPLOY/walgit.toml" ]; then
        l="$(awk -F'"' '/^[[:space:]]*listen[[:space:]]*=/{print $2; exit}' "$DEPLOY/walgit.toml" 2>/dev/null || true)"
    fi
    [ -n "$l" ] || l="127.0.0.1:8081"
    printf '%s' "$l"
}
HEALTH_URL="http://$(listen_addr)/healthz"

# 服务生命周期由 App Bundle 里的 walgit 二进制负责。
service() {
    local bin="$APP_DEST/Contents/Resources/walgit"
    [ -x "$bin" ] || return 1
    "$bin" service "$1" --config "$DEPLOY/walgit.toml" 2>&1
}

# 记录升级前服务是否在跑:只有它本来在跑,升级后才该把它带起来。
SERVICE_WAS_RUNNING=0
healthcheck "$HEALTH_URL" >/dev/null 2>&1 && SERVICE_WAS_RUNNING=1

# 0.5.x 的 launch 形态是 screen + run-walgit.sh，没有 pidfile；新
# `walgit service stop` 看不到它。端口仍被 walgit 占用时只 kill 映像名
# 匹配的监听进程，绝不按端口误杀别的服务。
legacy_stop() {
    local port pid comm
    port="${HEALTH_URL##*:}"
    port="${port%%/*}"
    command -v screen >/dev/null 2>&1 && screen -S "${WALGIT_SCREEN_SESSION:-walgit-server}" -X quit >/dev/null 2>&1 || true
    if command -v lsof >/dev/null 2>&1; then
        pid="$(lsof -tiTCP:"$port" -sTCP:LISTEN 2>/dev/null | head -1 || true)"
        if [ -n "$pid" ]; then
            comm="$(ps -p "$pid" -o comm= 2>/dev/null || true)"
            case "$comm" in
                *walgit*) kill "$pid" 2>/dev/null || true ;;
                *) log "legacy stop skipped: port $port held by $comm" ;;
            esac
        fi
    fi
}

if [ "$SERVICE_WAS_RUNNING" = 1 ] && [ "${WALGIT_UPDATE_SKIP_SERVICE:-0}" != "1" ]; then
    service stop >/dev/null 2>&1 || true
    if healthcheck "$HEALTH_URL" >/dev/null 2>&1; then
        legacy_stop
    fi
fi

# v0.6.x 旧布局:这些曾是“部署骨架”托管文件,现已不属于状态目录。
# 只删精确的文件名;配置、cache、keys、日志、pidfile 与凭证不动。
for stale in walgit walgit-ensure run-walgit.sh .skeleton-version; do
    if [ -e "$DEPLOY/$stale" ]; then
        rm -f "$DEPLOY/$stale" || fail "清理旧布局失败: $DEPLOY/$stale"
        log "removed legacy state-file: $stale"
    fi
done

OLD_VERSION="unknown"
if [ -f "$APP_DEST/Contents/Info.plist" ]; then
    OLD_VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP_DEST/Contents/Info.plist" 2>/dev/null || echo unknown)"
fi
BACKUP="$APP_DEST.bak-$OLD_VERSION-$(date +%Y%m%d-%H%M%S)"
if [ -e "$APP_DEST" ]; then
    mv "$APP_DEST" "$BACKUP" || fail "备份旧 app 失败: $APP_DEST"
fi

# 新 app 由 `open` 启动后,回滚前必须先终止它,否则会把正在运行的新 bundle
# 移走,留下「新进程 + 旧 bundle」。
new_tray_pid() {
    pgrep -f "$APP_DEST/Contents/MacOS/walgit-tray" 2>/dev/null | head -1 || true
}
kill_new_tray() {
    local pid
    pid="$(new_tray_pid)"
    [ -n "$pid" ] || return 0
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.1
    done
    kill -9 "$pid" 2>/dev/null || true
}

rollback() {
    local why="$1"
    log "rollback: $why"
    kill_new_tray
    [ "${WALGIT_UPDATE_SKIP_SERVICE:-0}" != "1" ] && service stop >/dev/null 2>&1 || true

    local new_moved=0 restore_ok=0 open_ok=1 service_ok=1
    if [ ! -e "$APP_DEST" ]; then
        new_moved=1
    elif mv "$APP_DEST" "$APP_DEST.failed-$VERSION-$(date +%Y%m%d-%H%M%S)" 2>/dev/null; then
        new_moved=1
    fi
    if [ "$new_moved" = 1 ] && [ -e "$BACKUP" ] && mv "$BACKUP" "$APP_DEST" 2>/dev/null; then
        restore_ok=1
    fi
    if [ "$restore_ok" = 1 ] && [ "${WALGIT_UPDATE_SKIP_OPEN:-0}" != "1" ]; then
        open --env "WALGIT_DEPLOY_DIR=$DEPLOY" "$APP_DEST" >/dev/null 2>&1 || open_ok=0
    fi
    if [ "$restore_ok" = 1 ] && [ "$SERVICE_WAS_RUNNING" = 1 ] && [ "${WALGIT_UPDATE_SKIP_SERVICE:-0}" != "1" ]; then
        service start >/dev/null 2>&1 || service_ok=0
    fi
    if [ "$restore_ok" = 1 ] && [ "$open_ok" = 1 ] && [ "$service_ok" = 1 ]; then
        notify "$(printf '%s，已恢复旧版本' "$why")"
    else
        log "rollback incomplete: restore=$restore_ok open=$open_ok service=$service_ok"
        notify "$(printf '%s；自动回滚未完成，请重装旧版或手动恢复 %s' "$why" "$BACKUP")"
    fi
    exit 1
}

/usr/bin/ditto --norsrc --noextattr "$MOUNT/walgit-tray.app" "$APP_DEST" || rollback "替换 app 失败"
cleanup_mount
trap - EXIT
OPEN_FAILED=0
if [ "${WALGIT_UPDATE_SKIP_OPEN:-0}" != "1" ]; then
    if ! open --env "WALGIT_DEPLOY_DIR=$DEPLOY" "$APP_DEST" >/dev/null 2>&1; then
        OPEN_FAILED=1
    fi
fi

# App Bundle 是唯一程序来源:等新 bundle 内的 walgit 报出目标版本。
ok=0
for _ in $(seq 1 "$BOOTSTRAP_WAIT"); do
    app_v="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP_DEST/Contents/Info.plist" 2>/dev/null || true)"
    bin_v="$("$APP_DEST/Contents/Resources/walgit" --version 2>/dev/null || true)"
    bin_token="${bin_v##* }"
    if [ "$app_v" = "$VERSION" ] && [ "$bin_token" = "v$VERSION" ]; then
        ok=1
        break
    fi
    sleep 0.5
done
[ "$ok" = 1 ] || rollback "新 App Bundle 版本核验超限"
[ "$OPEN_FAILED" = 0 ] || rollback "无法启动新 app"

if [ "$SERVICE_WAS_RUNNING" = 1 ] && [ "${WALGIT_UPDATE_SKIP_SERVICE:-0}" != "1" ]; then
    service start >/dev/null 2>&1 || rollback "服务启动失败"
    for _ in $(seq 1 "$HEALTH_WAIT"); do
        if [ "$(health_version)" = "v$VERSION" ]; then
            log "SUCCESS: v$VERSION"
            notify "已升级到 v$VERSION"
            exit 0
        fi
        sleep 0.5
    done
    rollback "新服务健康检查未到 v$VERSION(仍跑旧版本?)"
fi

log "SUCCESS: v$VERSION"
notify "已升级到 v$VERSION"
