#!/bin/bash
# release-install.sh — detached helper for macOS tray Release updates.
# Invoked as: release-install.sh <dmg> <mount> <app-dest> <version> <tray-pid>
#
# 备份旧 app 与 ~/walgit 托管文件；替换失败或新版本健康检查失败时
# 两者一起回滚，避免出现「旧 app + 新部署骨架」的半更新状态。
#
# 测试用覆盖（生产不设）：
#   WALGIT_DEPLOY_DIR / WALGIT_UPDATE_SKIP_SERVICE / WALGIT_UPDATE_SKIP_OPEN
#   WALGIT_UPDATE_HEALTHCHECK  探活命令(默认 curl)
#   WALGIT_UPDATE_TRAY_WAIT / WALGIT_UPDATE_BOOTSTRAP_WAIT / WALGIT_UPDATE_HEALTH_WAIT
#    —— 三段轮询次数(每段 0.5s)，仅测试用；未设按生产值。
set -euo pipefail

DMG="${1:?dmg}"
MOUNT="${2:?mount}"
APP_DEST="${3:?app destination}"
VERSION="${4#v}"
TRAY_PID="${5:?tray pid}"
DEPLOY="${WALGIT_DEPLOY_DIR:-$HOME/walgit}"
LOG="$DEPLOY/tray.log"

TRAY_WAIT="${WALGIT_UPDATE_TRAY_WAIT:-300}"       # 0.1s/次 → 30s
BOOTSTRAP_WAIT="${WALGIT_UPDATE_BOOTSTRAP_WAIT:-120}"  # 0.5s/次 → 60s
HEALTH_WAIT="${WALGIT_UPDATE_HEALTH_WAIT:-30}"    # 0.5s/次 → 15s

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

# 服务健康检查返回的是 {"status":"ok","version":"v0.5.1"} —— 取出版本号,
# 用于判断升级后跑的是不是新二进制(#170:旧进程不会自己退出)。
health_version() {
    local body
    body="$(healthcheck "$HEALTH_URL" --max-time 2 2>/dev/null || true)"
    printf '%s' "$body" | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'
}

# 与部署配置同源:自定义 [server].listen 时,预探活/重启后健康检查都要用
# 实际端口,否则会把「运行中」误判成停止后跳过启动(服务静默停掉)。
listen_addr() {
    local l=""
    if [ -f "$DEPLOY/walgit.toml" ]; then
        l="$(awk -F'"' '/^[[:space:]]*listen[[:space:]]*=/{print $2; exit}' "$DEPLOY/walgit.toml" 2>/dev/null || true)"
    fi
    [ -n "$l" ] || l="127.0.0.1:8081"
    printf '%s' "$l"
}
HEALTH_URL="http://$(listen_addr)/healthz"
# 服务生命周期由 walgit 二进制负责(`walgit service …`):端口从部署的
# walgit.toml 读,不再需要把解析结果经环境变量传给一个 shell 脚本。
service() { "$DEPLOY/walgit" service "$1" --config "$DEPLOY/walgit.toml" 2>&1; }

# 记录升级前服务是否在跑:只有它本来在跑,升级后才该把它带起来。
# 用户主动停掉的服务不拉起(托盘里"停止服务"是明确意图)。
SERVICE_WAS_RUNNING=0
healthcheck "$HEALTH_URL" >/dev/null 2>&1 && SERVICE_WAS_RUNNING=1
if [ -x "$DEPLOY/walgit" ] && [ "${WALGIT_UPDATE_SKIP_SERVICE:-0}" != "1" ]; then
    service stop >/dev/null 2>&1 || true
fi

OLD_VERSION="unknown"
if [ -f "$APP_DEST/Contents/Info.plist" ]; then
    OLD_VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP_DEST/Contents/Info.plist" 2>/dev/null || echo unknown)"
fi
BACKUP="$APP_DEST.bak-$OLD_VERSION-$(date +%Y%m%d-%H%M%S)"
if [ -e "$APP_DEST" ]; then
    mv "$APP_DEST" "$BACKUP" || fail "备份旧 app 失败: $APP_DEST"
fi

# ~/walgit 托管文件(present 才备份)，与 app 一起回滚。
DEPLOY_BACKUP=""
DEPLOY_MANAGED=""
for f in walgit run-walgit.sh .skeleton-version; do
    [ -e "$DEPLOY/$f" ] || continue
    [ -n "$DEPLOY_BACKUP" ] || DEPLOY_BACKUP="$(mktemp -d "${TMPDIR:-/tmp}/walgit-deploy-bak.XXXXXX")"
    cp -p "$DEPLOY/$f" "$DEPLOY_BACKUP/$f" 2>/dev/null || true
    DEPLOY_MANAGED="$DEPLOY_MANAGED $f"
done

restore_deploy_files() {
    [ -n "$DEPLOY_BACKUP" ] || return 0
    local f
    for f in $DEPLOY_MANAGED; do
        cp -p "$DEPLOY_BACKUP/$f" "$DEPLOY/$f" 2>/dev/null || true
    done
}

# 新 app 由 `open` 启动后,其 bootstrap 可能已经跑起来;回滚前必须先终止
# 它,否则会把正在运行的新 bundle 移走,留下「新进程 + 旧 bundle」。
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
    if [ -e "$APP_DEST" ]; then
        mv "$APP_DEST" "$APP_DEST.failed-$VERSION-$(date +%Y%m%d-%H%M%S)" 2>/dev/null || true
    fi
    if [ -e "$BACKUP" ]; then
        mv "$BACKUP" "$APP_DEST" 2>/dev/null || true
    fi
    restore_deploy_files
    [ "${WALGIT_UPDATE_SKIP_OPEN:-0}" != "1" ] && open --env "WALGIT_DEPLOY_DIR=$DEPLOY" "$APP_DEST" >/dev/null 2>&1 || true
    # 只恢复"升级前本来在跑"的服务;用户主动停掉的不要借回滚之名拉起。
    if [ "$SERVICE_WAS_RUNNING" = 1 ] && [ -x "$DEPLOY/walgit" ] && [ "${WALGIT_UPDATE_SKIP_SERVICE:-0}" != "1" ]; then
        service start >/dev/null 2>&1 || true
    fi
    notify "$(printf '%s，已恢复旧版本' "$why")"
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

# The new app's bootstrap updates the managed ~/walgit files. Wait until the
# marker and the deployment binary agree with the new bundle before restart.
ok=0
for _ in $(seq 1 "$BOOTSTRAP_WAIT"); do
    app_v="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP_DEST/Contents/Info.plist" 2>/dev/null || true)"
    marker="$(cat "$DEPLOY/.skeleton-version" 2>/dev/null || true)"
    bin_v="$("$DEPLOY/walgit" --version 2>/dev/null || true)"
    bin_token="${bin_v##* }"
    if [ "$app_v" = "$VERSION" ] && [ "$marker" = "$VERSION" ] && [ "$bin_token" = "v$VERSION" ]; then
        ok=1
        break
    fi
    sleep 0.5
done
[ "$ok" = 1 ] || rollback "新版本启动或部署骨架更新时间超限"
[ "$OPEN_FAILED" = 0 ] || rollback "无法启动新 app"

if [ "$SERVICE_WAS_RUNNING" = 1 ] && [ -x "$DEPLOY/walgit" ] && [ "${WALGIT_UPDATE_SKIP_SERVICE:-0}" != "1" ]; then
    service start >/dev/null 2>&1 || rollback "服务启动失败"
    for _ in $(seq 1 "$HEALTH_WAIT"); do
        if [ "$(health_version)" = "v$VERSION" ]; then
            [ -n "$DEPLOY_BACKUP" ] && rm -rf "$DEPLOY_BACKUP"
            log "SUCCESS: v$VERSION"
            notify "已升级到 v$VERSION"
            exit 0
        fi
        sleep 0.5
    done
    rollback "新服务健康检查未到 v$VERSION(仍跑旧版本?)"
fi

[ -n "$DEPLOY_BACKUP" ] && rm -rf "$DEPLOY_BACKUP"
log "SUCCESS: v$VERSION"
notify "已升级到 v$VERSION"
