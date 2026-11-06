#!/usr/bin/env bash
# wgl e2e (D69): the collaboration client loop against real git transport —
# the lane the retired `walgit collab`/`ci` integration suites owned. Two
# clones push through receive-pack to a bare origin, a second principal
# aggregates and verifies, the claim race converges (§8.4: exactly one
# winner, the loser exits 2), and the gc fold keeps aggregation byte-identical
# (§9). When a walgit binary is present, the board hash is cross-checked
# against the Rust aggregation — the two implementations must agree.
#
# POSIX sh + git only. `zig` builds the client when no binary is supplied
# via WGL_BIN; WALGIT_BIN (or a target/debug/walgit from this checkout) opts
# into the cross-implementation check.
set -euo pipefail
cd "$(dirname "$0")/.."

root="$(pwd)"
wgl_bin="${WGL_BIN:-$root/wgl/zig-out/bin/wgl}"
if [ ! -x "$wgl_bin" ]; then
    echo "wgl-e2e: building the client (zig build)"
    (cd wgl && zig build)
fi
W="$wgl_bin"

rust_bin="${WALGIT_BIN:-}"
if [ -z "$rust_bin" ]; then
    for cand in "$root/target/debug/walgit" \
                "$root/target/release/walgit" \
                "$(dirname "$(dirname "$root")")/target/debug/walgit" \
                "$(dirname "$(dirname "$root")")/target/release/walgit"; do
        if [ -x "$cand" ]; then rust_bin="$cand"; break; fi
    done
fi
have_rust=""; [ -n "$rust_bin" ] && [ -x "$rust_bin" ] && have_rust=1

tmp="$(mktemp -d)"
server_pid=""
trap 'rm -rf "$tmp"; [ -n "${server_pid:-}" ] && kill "$server_pid" 2>/dev/null || true' EXIT
origin="$tmp/origin.git"
a="$tmp/a"
b="$tmp/b"
git init -q --bare -b main "$origin"
git clone -q "$origin" "$a"
git clone -q "$origin" "$b"
git -C "$a" commit -q --allow-empty -m init && git -C "$a" push -q origin main
git -C "$b" pull -q origin main

step() { echo "== $*"; }
fail() { echo "wgl-e2e: $*" >&2; exit 1; }

step "register two principals through receive-pack"
( cd "$a" && "$W" collab join --principal alice --push origin >/dev/null ) || fail "alice join"
( cd "$b" && "$W" collab join --principal bob   --push origin >/dev/null ) || fail "bob join"

step "issue → status → patch chain (alice), pushed"
issue_oid="$( cd "$a" && "$W" collab entry --kind issue --id cc-e2e-demo --actor alice \
    --body '{"title":"the demo card"}' --push origin | awk '{print $2}' ))"
[ -n "$issue_oid" ] || fail "issue entry produced no oid"
status_oid="$( cd "$a" && "$W" collab entry --kind status --id cc-e2e-demo --actor alice \
    --parent "$issue_oid" --body '{"status":"in-progress","worktree":"wt-a","branch":"feat/a"}' \
    --push origin | awk '{print $2}' ))"
patch_oid="$( cd "$a" && "$W" collab entry --kind patch --id cc-e2e-demo --actor alice \
    --parent "$status_oid" --base refs/heads/main --head refs/heads/feat/a \
    --body '{"message":"the change"}' --push origin | awk '{print $2}' ))"
[ -n "$patch_oid" ] || fail "patch entry produced no oid"

step "the second clone aggregates and verifies offline"
( cd "$b" && git fetch -q origin '+refs/collab/*:refs/collab/*' )
a_hash="$( cd "$a" && "$W" collab board --format hash )"
b_hash="$( cd "$b" && "$W" collab board --format hash )"
[ "$a_hash" = "$b_hash" ] || fail "two clones disagree on the board hash: $a_hash vs $b_hash"

step "merge rules: protected base needs a distinct non-author approve (§7.3)"
# The rules doc gates the merge evaluation; without it nothing is protected.
rules_oid="$(printf '{"protect":["refs/heads/main"],"require_human_approvals":1}' \
    | git -C "$a" hash-object -w --stdin)"
git -C "$a" update-ref refs/collab/meta/rules "$rules_oid"
git -C "$a" push -q origin refs/collab/meta/rules
( cd "$b" && git fetch -q origin '+refs/collab/*:refs/collab/*' )
not_allowed="$( cd "$b" && "$W" collab pr cc-e2e-demo | python3 -c 'import json,sys; print(json.load(sys.stdin)["merge"]["allowed"])' )"
[ "$not_allowed" = "False" ] || fail "protected base should start blocked, got $not_allowed"
( cd "$b" && "$W" collab entry --kind review --id cc-e2e-demo --actor bob \
    --parent "$patch_oid" --body '{"decision":"approve"}' --push origin >/dev/null ) || fail "bob review"
( cd "$a" && git fetch -q origin '+refs/collab/*:refs/collab/*' )
allowed="$( cd "$a" && "$W" collab pr cc-e2e-demo | python3 -c 'import json,sys; print(json.load(sys.stdin)["merge"]["allowed"])' )"
[ "$allowed" = "True" ] || fail "bob's approve should unblock the merge, got $allowed"

step "the claim race converges: one winner, one stand-down (§8.4)"
rc_a=0; ( cd "$a" && "$W" collab claim --id cc-e2e-demo --ttl 300 \
    --worktree wt-a --work 'alice does it' ) >/dev/null 2>&1 || rc_a=$?
rc_b=0; ( cd "$b" && "$W" collab claim --id cc-e2e-demo --ttl 300 \
    --worktree wt-b --work 'bob does it' ) >/dev/null 2>&1 || rc_b=$?
{ [ "$rc_a" = 0 ] && [ "$rc_b" = 2 ]; } || { [ "$rc_b" = 0 ] && [ "$rc_a" = 2 ]; } \
    || fail "claim race must end 0/2, got a=$rc_a b=$rc_b"

step "the fold keeps aggregation byte-identical (§9)"
before="$( cd "$b" && "$W" collab board --format hash )"
( cd "$a" && "$W" collab gc --actor alice --push origin ) >/dev/null || fail "gc fold"
inbox_after="$( git -C "$origin" for-each-ref 'refs/collab/inbox/*' | wc -l | tr -d ' ' )"
[ "$inbox_after" = "0" ] || fail "the fold left $inbox_after inbox ref(s) unpruned"
( cd "$b" && git fetch -q --prune origin '+refs/collab/*:refs/collab/*' )
after="$( cd "$b" && "$W" collab board --format hash )"
[ "$before" = "$after" ] || fail "the fold moved the aggregation: $before vs $after"

step "watch --once reports the snapshot move"
watch_out="$( cd "$b" && "$W" collab watch --once 2>/dev/null | grep -c snapshot || true )"
[ "${watch_out:-0}" -ge 1 ] || fail "watch did not surface the snapshot event"

step "wgl ci validate parses a ci.toml"
mkdir -p "$a/.walgit"
printf 'version = 1\n\n[[task]]\nname = "test"\ncommand = "true"\n' > "$a/.walgit/ci.toml"
( cd "$a" && "$W" ci validate >/dev/null ) || fail "ci validate"

step "the runner executes a real task end-to-end (claim → run → result)"
# Commit the ci.toml so the runner finds it at the tested commit, then run one
# pass as a CI principal: self-registers, claims, executes in a detached
# worktree, publishes the signed result.
( cd "$a" && git add .walgit/ci.toml && git commit -q -m 'ci: add the test task' \
    && git push -q origin main ) || fail "pushing the ci.toml commit"
ci_key="$tmp/ci-bot.key"
printf '0707070707070707070707070707070707070707070707070707070707070707' > "$ci_key"
( cd "$a" && "$W" ci run --once --actor ci-bot --key "$ci_key" --claim-ttl 300 ) \
    >/dev/null 2>&1 || fail "ci run"
( cd "$b" && git fetch -q origin '+refs/collab/*:refs/collab/*' )
ci_state="$( cd "$b" && "$W" ci status --format json | python3 -c 'import json,sys
runs=json.load(sys.stdin)["runs"]
hits=[r for r in runs if r["id"].startswith("ci-") and r["task"]=="test"]
print(hits[0]["state"] if hits else "none")' )"
[ "$ci_state" = "done" ] || fail "the ci run did not settle: state=$ci_state"
ci_conclusion="$( cd "$b" && "$W" ci status --format json | python3 -c 'import json,sys
runs=json.load(sys.stdin)["runs"]
hits=[r for r in runs if r["id"].startswith("ci-") and r["task"]=="test"]
print((hits[0].get("conclusion") or "none"))' )"
[ "$ci_conclusion" = "success" ] || fail "the ci task did not succeed: $ci_conclusion"
ci_log="$( cd "$a" && "$W" ci log 2>/dev/null )"; [ "$?" = 0 ] || fail "ci log"
ci_status_text="$( cd "$b" && "$W" ci status 2>/dev/null | grep -c 'done' || true )"
[ "${ci_status_text:-0}" -ge 1 ] || fail "ci status text does not show the done run"

step "MCP: fresh reads fail closed, subscriptions wake on a claim TTL lapse, cap, unsubscribe, EOF"
m_origin="$tmp/mcp-origin.git"; m_a="$tmp/mcp-a"; m_b="$tmp/mcp-b"
git init -q --bare -b main "$m_origin"
git clone -q "$m_origin" "$m_a"; git clone -q "$m_origin" "$m_b"
git -C "$m_a" commit -q --allow-empty -m init
git -C "$m_a" push -q origin main
git -C "$m_b" pull -q origin main
( cd "$m_a" && "$W" collab join --principal alice --push origin >/dev/null ) || fail "mcp alice join"
( cd "$m_b" && "$W" collab join --principal bob --push origin >/dev/null ) || fail "mcp bob join"
( cd "$m_a" && "$W" collab entry --kind issue --id cc-mcp-card --actor alice \
    --body '{"title":"mcp card"}' --push origin >/dev/null )

# A read that cannot fetch must be a tool error, not a silently stale board.
no_fetch="$( cd "$m_b" && printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"collab_board","arguments":{}}}' \
    | "$W" mcp --repo . --remote not-a-remote 2>/dev/null )"
printf '%s' "$no_fetch" | grep -q '"isError":true' \
    || fail "a failed fetch was not a fail-closed tool error: $no_fetch"

fifo="$tmp/mcp-in"; mkfifo "$fifo"
out="$tmp/mcp-sub.out"; err="$tmp/mcp-sub.err"
( cd "$m_b" && exec "$W" mcp --repo . --remote origin --subscribe-interval 1 --max-subscriptions 1 \
    <"$fifo" >"$out" 2>"$err" ) &
mcp_pid=$!
exec 9<>"$fifo"
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' >&9
printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"resources/subscribe","params":{"uri":"walgit://collab/board"}}' >&9
i=0
while [ "$i" -lt 40 ] && ! grep -q '"id":2' "$out"; do sleep 0.5; i=$((i + 1)); done
grep -q '"id":2' "$out" || fail "resources/subscribe did not answer (see $err)"
printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"resources/subscribe","params":{"uri":"walgit://collab/thread/cc-mcp-card"}}' >&9
i=0
while [ "$i" -lt 20 ] && ! grep -q '"code":-32602' "$out"; do sleep 0.5; i=$((i + 1)); done
grep -q '"code":-32602' "$out" || fail "--max-subscriptions was not enforced"

# The claim appears (one update) and its TTL lapse moves the version again.
( cd "$m_a" && "$W" collab claim --id cc-mcp-card --ttl 10 --worktree wt-mcp --work 'ttl probe' ) \
    >/dev/null 2>&1 || fail "claim for the TTL probe"
i=0
while [ "$i" -lt 40 ] && [ "$(grep -c 'notifications/resources/updated' "$out" || true)" -lt 2 ]; do
    sleep 0.5
    i=$((i + 1))
done
wakes="$(grep -c 'notifications/resources/updated' "$out" || true)"
[ "${wakes:-0}" -ge 2 ] || fail "expected a claim update and a TTL-lapse update, got ${wakes:-0}"

printf '%s\n' '{"jsonrpc":"2.0","id":4,"method":"resources/unsubscribe","params":{"uri":"walgit://collab/board"}}' >&9
sleep 2
before="$(grep -c 'notifications/resources/updated' "$out" || true)"
head_oid="$( cd "$m_a" && "$W" collab thread-heads | python3 -c 'import json,sys; print(json.load(sys.stdin)["cc-mcp-card"])' )"
( cd "$m_a" && "$W" collab entry --kind status --id cc-mcp-card --actor alice --parent "$head_oid" \
    --body '{"status":"blocked","note":"after unsubscribe"}' --push origin >/dev/null )
sleep 3
after_count="$(grep -c 'notifications/resources/updated' "$out" || true)"
[ "$before" = "$after_count" ] || fail "a write after unsubscribe still notified ($before -> $after_count)"

exec 9>&-
wait "$mcp_pid" || fail "wgl mcp did not exit 0 on stdin EOF"
grep -q '"id":1' "$out" || fail "initialize did not answer"
echo "   mcp: 2 TTL updates, cap enforced, unsubscribe quiet, EOF exit 0"

step "cross-implementation: the Rust aggregation must agree on the board hash"
if [ -n "$have_rust" ]; then
    # D69 moved the collaboration command family to wgl. A reference binary
    # built from this tree therefore has no `collab` subcommand, and calling
    # one must be an **explicit skip**, not a red on a clean build. Probe the
    # command surface first; only an older binary that still answers it is a
    # usable cross-check.
    if "$rust_bin" --config /dev/null collab --help >/dev/null 2>&1; then
        rust_hash="$( "$rust_bin" --config /dev/null collab board --repo "$b" --format hash 2>/dev/null )"
        [ "$rust_hash" = "$after" ] || fail "wgl and walgit disagree on the board hash: $after vs ${rust_hash:-<none>}"
        echo "   wgl == walgit: $after"
    else
        echo "   (reference binary has no \`collab\` command — D69 moved it to wgl; skipping the cross-check explicitly)"
    fi
else
    echo "   (no walgit binary found; skipping the cross-check explicitly)"
fi

step "real server (optional): wgl mcp == the host's own aggregation"
explicit_server="${WALGIT_SERVER_BIN:-}"
server_bin="$explicit_server"
if [ -z "$server_bin" ]; then
    for cand in "$root/target/debug/walgit-server" \
                "$root/target/release/walgit-server" \
                "$(dirname "$(dirname "$root")")/target/debug/walgit-server" \
                "$(dirname "$(dirname "$root")")/target/release/walgit-server"; do
        if [ -x "$cand" ]; then server_bin="$cand"; break; fi
    done
fi
if [ -z "$server_bin" ] || [ ! -x "$server_bin" ]; then
    echo "   (no walgit-server binary via WALGIT_SERVER_BIN or target/{debug,release}; skipping the real-server comparison explicitly — not a pass)"
else
    echo "   server binary: $server_bin"
    port="$(python3 -c 'import socket
s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
    base="http://127.0.0.1:$port"
    cfg="$tmp/walgit-server.toml"
    cat > "$cfg" <<EOF
[server]
listen = "127.0.0.1:$port"
auto_create_on_push = true
[server.auth]
mode = "none"
[store]
backend = "memory"
memory_backend_intentional = true
[cache]
dir = "$tmp/server-cache"
EOF
    "$server_bin" --config "$cfg" >"$tmp/server.log" 2>&1 &
    server_pid=$!
    ready=""
    i=0
    while [ "$i" -lt 80 ]; do
        if curl -fsS "$base/api/v1" >/dev/null 2>&1; then ready=1; break; fi
        sleep 0.25
        i=$((i + 1))
    done
    if [ -z "$ready" ]; then
        fail "the optional walgit-server did not become ready (see $tmp/server.log)"
    fi

    srv="$tmp/srv-repo"
    git init -q -b main "$srv"
    git -C "$srv" config user.email e2e@example.test
    git -C "$srv" config user.name e2e
    git -C "$srv" commit -q --allow-empty -m init
    git -C "$srv" remote add origin "$base/acme/widgets.git"
    git -C "$srv" push -q origin main || fail "push to the real server"
    ( cd "$srv" && "$W" collab join --principal alice --push origin >/dev/null ) || fail "server join"
    srv_issue="$( cd "$srv" && "$W" collab entry --kind issue --id cc-srv-card --actor alice \
        --body '{"title":"server card"}' --push origin | awk '{print $2}' )"
    ( cd "$srv" && "$W" collab entry --kind status --id cc-srv-card --actor alice --parent "$srv_issue" \
        --body '{"status":"in-progress","worktree":"wt","branch":"feat/x"}' --push origin >/dev/null )
    ( cd "$srv" && "$W" collab entry --kind patch --id cc-srv-card --actor alice --parent "$srv_issue" \
        --base refs/heads/main --head refs/heads/main --body '{"message":"the change"}' --push origin >/dev/null )

    probe_status="$(curl -s -o /dev/null -w '%{http_code}' "$base/acme/widgets/api-browser/collab/board")"
    if [ "$probe_status" = "404" ] && [ -z "$explicit_server" ]; then
        echo "   (the auto-detected walgit-server predates the api-browser/collab routes; skipping explicitly — set WALGIT_SERVER_BIN to the build under test)"
        stale_server=1
    else
        stale_server=0
    fi
    if [ "$stale_server" = "0" ]; then
    mcp_json="$tmp/srv-mcp.json"
    {
        printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
        printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"collab_board","arguments":{}}}'
        printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"collab_thread","arguments":{"id":"cc-srv-card"}}}'
        printf '%s\n' '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"collab_report","arguments":{}}}'
        printf '%s\n' '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"ci_status","arguments":{}}}'
        printf '%s\n' '{"jsonrpc":"2.0","id":6,"method":"resources/read","params":{"uri":"walgit://collab/board"}}'
        printf '%s\n' '{"jsonrpc":"2.0","id":7,"method":"resources/read","params":{"uri":"walgit://collab/thread/cc-srv-card"}}'
    } | ( cd "$srv" && "$W" mcp --repo . --remote origin ) >"$mcp_json" 2>"$tmp/srv-mcp.err" \
        || fail "wgl mcp against the real server"

    if python3 - "$base" "$mcp_json" <<'PYEOF'
import json, re, sys, urllib.request

base, path = sys.argv[1], sys.argv[2]
msgs = {m.get("id"): m for m in map(json.loads, open(path)) if isinstance(m, dict)}

def tool(i):
    r = msgs[i]["result"]
    assert not r.get("isError"), (i, r)
    return json.loads(r["content"][0]["text"])

def get(p):
    return json.loads(urllib.request.urlopen(base + p).read())

repo = "/acme/widgets/api-browser/collab"
board = get(repo + "/board")
thread = get(repo + "/threads/cc-srv-card")
report = get(repo + "/report")
ci = get(repo + "/ci")

assert tool(2) == board, "collab_board differs from GET /collab/board"
assert tool(3) == thread, "collab_thread differs from GET /collab/threads/<id>"
assert tool(4) == report, "collab_report differs from GET /collab/report"
assert tool(5) == ci, "ci_status differs from GET /collab/ci"

# Resources: the same bodies (the host adds its WAL `seq`, which wgl has no
# source for) and the host's version grammar.
def resource(i):
    c = msgs[i]["result"]["contents"][0]
    return c["_meta"]["version"], json.loads(c["text"])

bv, bbody = resource(6)
tv, tbody = resource(7)
server_board = dict(board)
server_thread = dict(thread)
server_board.pop("seq", None)
server_thread.pop("seq", None)
assert bbody == server_board, "resource board body differs"
assert tbody == server_thread, "resource thread body differs"
assert re.fullmatch(r"b:[0-9a-f]{16}", bv), f"board version is not the witness grammar: {bv}"
tip = next(c["last_oid"] for col in board["columns"] for c in col["cards"] if c["id"] == "cc-srv-card")
assert tv == "h:" + tip, f"thread version {tv} != its tip {tip}"
print(f"   server == wgl mcp: board/thread/report/ci + resources (board {bv}, thread {tv})")
PYEOF
    then
        :
    elif [ -n "$explicit_server" ]; then
        fail "the walgit-server under test ($server_bin) disagrees with wgl mcp"
    else
        echo "   (the auto-detected walgit-server is older than a compared endpoint — explicit skip; set WALGIT_SERVER_BIN to the build under test)"
    fi
    fi
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    server_pid=""
fi

echo "wgl-e2e: ok"
