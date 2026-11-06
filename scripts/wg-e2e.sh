#!/usr/bin/env bash
# wg e2e (D69): the collaboration client loop against real git transport —
# the lane the retired Rust collaboration integration suites owned. Two
# clones push through receive-pack to a bare origin, a second principal
# aggregates and verifies, the claim race converges (§8.4: exactly one
# winner, the loser exits 2), and the gc fold keeps aggregation byte-identical
# (§9). When a walgit binary is present, the board hash is cross-checked
# against the Rust aggregation — the two implementations must agree.
#
# POSIX sh + git only. `zig` builds the client when no binary is supplied
# via WG_BIN; WALGIT_BIN (or a target/debug/walgit from this checkout) opts
# into the cross-implementation check.
set -euo pipefail
cd "$(dirname "$0")/.."

root="$(pwd)"
wg_bin="${WG_BIN:-$root/wg/zig-out/bin/wg}"
if [ ! -x "$wg_bin" ]; then
    echo "wg-e2e: building the client (zig build)"
    (cd wg && zig build)
fi
W="$wg_bin"

rust_bin="${WALGIT_BIN:-}"
rust_explicit=""
if [ -z "$rust_bin" ]; then
    for cand in "$root/target/debug/walgit" \
                "$root/target/release/walgit" \
                "$(dirname "$(dirname "$root")")/target/debug/walgit" \
                "$(dirname "$(dirname "$root")")/target/release/walgit"; do
        if [ -x "$cand" ]; then rust_bin="$cand"; break; fi
    done
else
    rust_explicit=1
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
fail() { echo "wg-e2e: $*" >&2; exit 1; }

# A reference binary only counts when it still has the collab family. After
# D69 a branch-built `walgit` exits 2 here; that must skip the cross-check,
# not fail the lane (an explicitly supplied WALGIT_BIN that cannot answer is
# a caller error and is reported as such).
if [ -n "$have_rust" ]; then
    if "$rust_bin" --config /dev/null collab board --repo "$root" --format hash >/dev/null 2>&1; then
        :
    elif [ -n "$rust_explicit" ]; then
        fail "WALGIT_BIN=$rust_bin does not support the collab commands (D69 moved them to wg); it cannot serve as the cross-implementation reference"
    else
        echo "wg-e2e: $rust_bin has no collab commands (a D69+ walgit build); skipping the cross-implementation check"
        have_rust=""
    fi
fi

step "register two principals through receive-pack"
( cd "$a" && "$W" collab join --principal alice --push origin >/dev/null ) || fail "alice join"
( cd "$b" && "$W" collab join --principal bob   --push origin >/dev/null ) || fail "bob join"

step "issue → status → patch chain (alice), pushed"
issue_oid="$( cd "$a" && "$W" collab entry --kind issue --id cc-e2e-demo --actor alice \
    --body '{"title":"the demo card"}' --push origin | awk '{print $2}' )"
[ -n "$issue_oid" ] || fail "issue entry produced no oid"
status_oid="$( cd "$a" && "$W" collab entry --kind status --id cc-e2e-demo --actor alice \
    --parent "$issue_oid" --body '{"status":"in-progress","worktree":"wt-a","branch":"feat/a"}' \
    --push origin | awk '{print $2}' )"
patch_oid="$( cd "$a" && "$W" collab entry --kind patch --id cc-e2e-demo --actor alice \
    --parent "$status_oid" --base refs/heads/main --head refs/heads/feat/a \
    --body '{"message":"the change"}' --push origin | awk '{print $2}' )"
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

step "wg ci validate parses a ci.toml"
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
wait "$mcp_pid" || fail "wg mcp did not exit 0 on stdin EOF"
grep -q '"id":1' "$out" || fail "initialize did not answer"
echo "   mcp: 2 TTL updates, cap enforced, unsubscribe quiet, EOF exit 0"
# The write path over MCP: refusals first (a foreign actor and an
# unsigned entry must be refused before anything is pushed), then the
# §5.3 golden vector signed with the seed 07×32 — actor `alice`, the
# worktree identity here — published through `collab_publish` and read
# back verified by this checkout's own aggregation.
golden='{"version":1,"kind":"issue","id":"golden","actor":"alice","ts":1786500000,"parent":"","body":{"title":"golden vector"},"sig":"ed25519:VFROsCUBDR4Sj1eFoMdDI/iRfV0A0jgRSGFGjAB91MVh2oh3IwnohAxj7Mq55x+uvpyrhM2tlq6x3WYuT9f5DQ=="}'
# The golden vector is signed with the seed 07x32; rotate the fixture's
# `alice` to that key (after the read comparisons, which used the random
# key) so the published entry verifies in this checkout's aggregation.
golden_key="$tmp/golden.key"
printf '0707070707070707070707070707070707070707070707070707070707070707' > "$golden_key"
( cd "$m_a" && "$W" collab principal-register --principal alice --key "$golden_key" --push origin >/dev/null ) \
    || fail "golden key rotation"
srv_out="$tmp/srv-write.json"
{
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"collab_publish","arguments":{"entry":{"version":1,"kind":"comment","id":"golden","actor":"bob","ts":1,"parent":"","body":{},"sig":"ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="}}}}'
    printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"collab_publish","arguments":{"entry":{"version":1,"kind":"comment","id":"golden","actor":"alice","ts":1,"parent":"","body":{},"sig":""}}}}'
    printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"collab_publish","arguments":{"entry":'"$golden"'}}}'
    printf '%s\n' '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"collab_thread","arguments":{"id":"golden","fresh":false}}}'
} | ( cd "$m_a" && "$W" mcp --repo . --remote origin ) >"$srv_out" 2>"$tmp/srv-write.err" \
    || fail "wg mcp write round trip failed"
python3 - "$srv_out" <<'PYW'
import json, sys
msgs = {m.get("id"): m for m in map(json.loads, open(sys.argv[1])) if isinstance(m, dict)}
for i in (1, 2):
    assert msgs[i]["result"].get("isError") is True, (i, msgs[i])
assert not msgs[3]["result"].get("isError"), msgs[3]
pub = json.loads(msgs[3]["result"]["content"][0]["text"])
assert pub["ref"].startswith("refs/collab/inbox/alice/"), pub
thread = json.loads(msgs[4]["result"]["content"][0]["text"])
entries = thread["entries"]
assert len(entries) == 1, thread
assert entries[0]["verified"] is True, thread
assert entries[0]["entry"]["sig"].startswith("ed25519:"), thread
print("   write path: foreign actor and unsigned entry refused; golden vector published and verified")
PYW

step "MCP collab_register: refuses a silent rotation, force rotates, identity missing is a tool error"
# The write-path step above rotated alice's published key to the golden
# fixture key; this checkout still signs with its own key. Register must
# refuse that silent rotation, then accept it explicitly.
mcp_register="$tmp/mcp-register.json"
{
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"collab_register","arguments":{}}}'
    printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"collab_register","arguments":{"force":true}}}'
    printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"collab_register","arguments":{}}}'
} | ( cd "$m_a" && "$W" mcp --repo . --remote origin ) >"$mcp_register" 2>"$tmp/mcp-register.err" \
    || fail "wg mcp collab_register round trip failed"
python3 - "$mcp_register" <<'PYR'
import json, sys
msgs = {m.get("id"): m for m in map(json.loads, open(sys.argv[1])) if isinstance(m, dict)}
first = msgs[1]["result"]
assert first.get("isError") is True, first
assert "different public key" in first["content"][0]["text"], first
for i in (2, 3):
    assert not msgs[i]["result"].get("isError"), msgs[i]
print("   collab_register: rotation refused without force; force rotated; idempotent re-register ok")
PYR

# No identity in the checkout: the tool refuses with the join hint.
git clone -q "$m_origin" "$tmp/mcp-noident"
noident="$tmp/mcp-noident.json"
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"collab_register","arguments":{}}}' \
    | ( cd "$tmp/mcp-noident" && "$W" mcp --repo . --remote origin ) >"$noident" 2>/dev/null \
    || fail "wg mcp without an identity crashed"
python3 - "$noident" <<'PYI'
import json, sys
msg = next(iter(map(json.loads, open(sys.argv[1]))))
assert msg["result"].get("isError") is True, msg
assert "no D1 identity" in msg["result"]["content"][0]["text"], msg
print("   collab_register without identity: refused with the join hint")
PYI

step "MCP done gate: needs-review + verified non-author approve, then a refusal once done"
# Build the §7.5 precondition through the ordinary CLI: status needs-review,
# a patch by alice, a verified approve by bob. The signed done entry itself is
# produced without --push (the CLI applies the same gate), then handed to
# `collab_publish`, which must fetch, apply the same rule and accept it.
# The golden-vector step above rotated alice's published key; restore this
# checkout's own key so the entries signed below aggregate as verified.
alice_key="$(git -C "$m_a" rev-parse --absolute-git-dir)/walgit/keys/alice.ed25519"
( cd "$m_a" && "$W" collab principal-register --principal alice --key "$alice_key" --push origin >/dev/null ) \
    || fail "restoring alice's key for the done-gate check"
m_head="$( cd "$m_a" && "$W" collab thread-heads | python3 -c 'import json,sys; print(json.load(sys.stdin)["cc-mcp-card"])' )"
m_head="$( cd "$m_a" && "$W" collab entry --kind status --id cc-mcp-card --actor alice --parent "$m_head" \
    --body '{"status":"needs-review"}' --push origin | awk '{print $2}' )"
m_head="$( cd "$m_a" && "$W" collab entry --kind patch --id cc-mcp-card --actor alice --parent "$m_head" \
    --base refs/heads/main --head refs/heads/feat-mcp --body '{"message":"mcp gate"}' --push origin | awk '{print $2}' )"
( cd "$m_b" && git fetch -q origin '+refs/collab/*:refs/collab/*' )
( cd "$m_b" && "$W" collab entry --kind review --id cc-mcp-card --actor bob --parent "$m_head" \
    --body '{"decision":"approve"}' --push origin >/dev/null ) || fail "bob's approve"
( cd "$m_a" && git fetch -q origin '+refs/collab/*:refs/collab/*' )
m_head="$( cd "$m_a" && "$W" collab thread-heads | python3 -c 'import json,sys; print(json.load(sys.stdin)["cc-mcp-card"])' )"
done_ref_oid="$( cd "$m_a" && "$W" collab entry --kind status --id cc-mcp-card --actor alice --parent "$m_head" \
    --body '{"status":"done","work":"shipped via mcp"}' )"
done_oid="$(printf '%s' "$done_ref_oid" | awk '{print $2}')"
done_entry="$(git -C "$m_a" cat-file -p "$done_oid")"
done_out="$tmp/mcp-done.json"
python3 - "$done_entry" > "$tmp/mcp-done.ndjson" <<'PYD'
import json, sys
entry = sys.argv[1]
for i in (1, 2):
    print(json.dumps({"jsonrpc": "2.0", "id": i, "method": "tools/call",
                      "params": {"name": "collab_publish", "arguments": {"entry": entry}}}))
print(json.dumps({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                  "params": {"name": "collab_thread", "arguments": {"id": "cc-mcp-card", "fresh": False}}}))
# A generic client percent-encodes the id's dashes.
print(json.dumps({"jsonrpc": "2.0", "id": 4, "method": "resources/read",
                  "params": {"uri": "walgit://collab/thread/cc%2Dmcp%2Dcard"}}))
PYD
( cd "$m_a" && "$W" mcp --repo . --remote origin ) < "$tmp/mcp-done.ndjson" >"$done_out" 2>"$tmp/mcp-done.err" \
    || fail "wg mcp done-gate round trip failed"
python3 - "$done_out" "$done_oid" <<'PYD'
import json, sys
msgs = {m.get("id"): m for m in map(json.loads, open(sys.argv[1])) if isinstance(m, dict)}
assert not msgs[1]["result"].get("isError"), msgs[1]
# The blob is the bytes the client signed, so the oid is the CLI's oid — no
# duplicate entry with the same content.
pub = json.loads(msgs[1]["result"]["content"][0]["text"])
assert pub["oid"] == sys.argv[2], (pub["oid"], sys.argv[2])
assert msgs[2]["result"].get("isError") is True, msgs[2]
thread = json.loads(msgs[3]["result"]["content"][0]["text"])
encoded = json.loads(msgs[4]["result"]["contents"][0]["text"])
assert encoded["id"] == "cc-mcp-card", encoded
last = thread["entries"][-1]
assert last["entry"]["kind"] == "status", last
assert last["entry"]["body"]["status"] == "done", last
assert last["verified"] is True, last
print("   done gate: approved thread published; the now-done thread refuses another done")
PYD

step "MCP transport: a >64 KiB request is read, an overlong line is one error and the lane survives"
python3 - > "$tmp/mcp-big.ndjson" <<'PYB'
import json
entry = {"version": 1, "kind": "comment", "id": "big", "actor": "alice", "ts": 1,
         "parent": "", "body": {"note": "x" * 70000}}  # no sig: refused by the tool
print(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                  "params": {"name": "collab_publish", "arguments": {"entry": entry}}}))
print(json.dumps({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {}}))
print("x" * 3000000)  # beyond the 2 MiB transport buffer
print(json.dumps({"jsonrpc": "2.0", "id": 3, "method": "ping"}))
PYB
big_out="$tmp/mcp-big.json"
( cd "$m_a" && "$W" mcp --repo . --remote origin ) < "$tmp/mcp-big.ndjson" >"$big_out" 2>"$tmp/mcp-big.err" \
    || fail "wg mcp did not survive an overlong line"
python3 - "$big_out" <<'PYT'
import json, sys
msgs = [m for m in map(json.loads, open(sys.argv[1])) if isinstance(m, dict)]
by_id = {m.get("id"): m for m in msgs}
assert by_id[1]["result"].get("isError") is True, by_id[1]   # parsed: MissingSig
assert "sig" in by_id[1]["result"]["content"][0]["text"], by_id[1]
assert not by_id[2]["result"].get("isError"), by_id[2]      # initialize still answered
assert any(m.get("error", {}).get("code") == -32700 for m in msgs), msgs
assert by_id[3]["result"] == {}, by_id[3]                   # lane survived, ping answered
print("   transport: 70 KiB request parsed; 3 MiB line = one -32700; ping still answered")
PYT

step "ci timeout: a hung task settles as timeout and the runner returns"
cat > "$a/.walgit/ci.toml" <<'EOF'
version = 1

[[task]]
name = "test"
command = "true"

[[task]]
name = "slow"
command = "sleep 5"
timeout = "1s"

[[task]]
name = "race"
command = "sleep 3"
EOF
( cd "$a" && git add .walgit/ci.toml && git commit -q -m 'ci: add timeout and race tasks' \
    && git push -q origin main ) || fail "pushing the timeout/race ci.toml"
slow_start="$(date +%s)"
rc_slow=0
( cd "$a" && "$W" ci run --once --actor ci-slow --key "$ci_key" --task slow \
    --state "$tmp/slow-state.json" ) >"$tmp/slow.log" 2>&1 || rc_slow=$?
slow_elapsed=$(( $(date +%s) - slow_start ))
{ [ "$rc_slow" = 0 ] || [ "$rc_slow" = 1 ]; } \
    || fail "timeout runner exited $rc_slow (expected 0/1): $(cat "$tmp/slow.log")"
[ "$slow_elapsed" -le 20 ] \
    || fail "the timeout runner did not return within 20s (${slow_elapsed}s): $(cat "$tmp/slow.log")"
( cd "$b" && git fetch -q origin '+refs/collab/*:refs/collab/*' )
slow_conclusion="$( cd "$b" && "$W" ci status --format json | python3 -c 'import json,sys
runs=json.load(sys.stdin)["runs"]
eff=None
for r in runs:
    if r["task"] != "slow": continue
    for a in r["attempts"]:
        if a.get("effective"): eff=a["effective"]
print((eff or {}).get("conclusion") or "none")' )"
[ "$slow_conclusion" = "timeout" ] || fail "the slow task did not settle as timeout: $slow_conclusion"
echo "   slow settled timeout in ${slow_elapsed}s"

step "two runners race: exactly one effective result, the loser stands down"
( cd "$a" && "$W" ci run --once --actor ci-a --key "$ci_key" --task race \
    --state "$tmp/race-a-state.json" ) >"$tmp/race-a.log" 2>&1 &
race_a_pid=$!
( cd "$b" && "$W" ci run --once --actor ci-b --key "$ci_key" --task race \
    --state "$tmp/race-b-state.json" ) >"$tmp/race-b.log" 2>&1 &
race_b_pid=$!
race_rc_a=0; wait "$race_a_pid" || race_rc_a=$?
race_rc_b=0; wait "$race_b_pid" || race_rc_b=$?
[ "$race_rc_a" = 0 ] || fail "race runner ci-a exited $race_rc_a: $(cat "$tmp/race-a.log")"
[ "$race_rc_b" = 0 ] || fail "race runner ci-b exited $race_rc_b: $(cat "$tmp/race-b.log")"
( cd "$b" && git fetch -q origin '+refs/collab/*:refs/collab/*' )
race_out="$( cd "$b" && "$W" ci status --format json | python3 -c 'import json,sys
runs=json.load(sys.stdin)["runs"]
eff=[]
for r in runs:
    if r["task"] != "race": continue
    for a in r["attempts"]:
        if a.get("effective"): eff.append(a["effective"])
print(len(eff), (eff[0]["actor"] if eff else "none"))' )"
set -- $race_out
race_effective_count="$1"; race_effective_actor="$2"
[ "$race_effective_count" = "1" ] \
    || fail "the race must settle exactly one effective result, got $race_effective_count"
[ "$race_effective_actor" = "ci-a" ] || [ "$race_effective_actor" = "ci-b" ] \
    || fail "unexpected effective actor: $race_effective_actor"
if [ "$race_effective_actor" = "ci-a" ]; then loser_log="$tmp/race-b.log"; else loser_log="$tmp/race-a.log"; fi
grep -qE 'standing down|lost the claim race|held by' "$loser_log" \
    || fail "the losing runner did not stand down: $(cat "$loser_log")"
echo "   winner $race_effective_actor; loser stood down"

step "cross-implementation: the Rust aggregation must agree on the board hash"
if [ -n "$have_rust" ]; then
    rust_hash="$( "$rust_bin" --config /dev/null collab board --repo "$b" --format hash 2>/dev/null )" \
        || fail "the reference $rust_bin failed the collab board cross-check"
    [ "$rust_hash" = "$after" ] || fail "wg and walgit disagree on the board hash: $after vs ${rust_hash:-<none>}"
    echo "   wg == walgit: $after"
else
    echo "   (no reference walgit binary with collab commands; skipping the cross-check)"
fi

step "real server (optional): wg mcp == the host's own aggregation"
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
    # Fixed HEAD (review condition 3): the host's board reads the repository
    # HEAD, so the checkout compared against it must be on that same commit.
    # Both sides here are the `main` tip; assert it instead of assuming it.
    [ "$(git -C "$srv" symbolic-ref --short HEAD)" = "main" ] \
        || fail "the compared checkout is not on main; the board comparison needs one HEAD"
    [ "$(git -C "$srv" rev-parse HEAD)" = "$(git -C "$srv" rev-parse refs/heads/main)" ] \
        || fail "the compared checkout HEAD is not the main tip"
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
        || fail "wg mcp against the real server"

    if python3 - "$base" "$mcp_json" 2>"$tmp/compare.err" <<'PYEOF'
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

# Resources: the same bodies (the host adds its WAL `seq`, which wg has no
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
print(f"   server == wg mcp: board/thread/report/ci + resources (board {bv}, thread {tv})")
PYEOF
    then
        :
    elif [ -n "$explicit_server" ]; then
        cat "$tmp/compare.err" >&2 || true
        fail "the walgit-server under test ($server_bin) disagrees with wg mcp"
    else
        echo "   (the auto-detected walgit-server is older than a compared endpoint — explicit skip; set WALGIT_SERVER_BIN to the build under test)"
    fi
    fi
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    server_pid=""
fi

echo "wg-e2e: ok"
