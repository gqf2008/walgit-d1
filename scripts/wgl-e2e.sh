#!/usr/bin/env bash
# wgl e2e (D69): the collaboration client loop against real git transport —
# the lane the retired Rust collaboration integration suites owned. Two
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
trap 'rm -rf "$tmp"' EXIT
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

# A reference binary only counts when it still has the collab family. After
# D69 a branch-built `walgit` exits 2 here; that must skip the cross-check,
# not fail the lane (an explicitly supplied WALGIT_BIN that cannot answer is
# a caller error and is reported as such).
if [ -n "$have_rust" ]; then
    if "$rust_bin" --config /dev/null collab board --repo "$root" --format hash >/dev/null 2>&1; then
        :
    elif [ -n "$rust_explicit" ]; then
        fail "WALGIT_BIN=$rust_bin does not support the collab commands (D69 moved them to wgl); it cannot serve as the cross-implementation reference"
    else
        echo "wgl-e2e: $rust_bin has no collab commands (a D69+ walgit build); skipping the cross-implementation check"
        have_rust=""
    fi
fi

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
    [ "$rust_hash" = "$after" ] || fail "wgl and walgit disagree on the board hash: $after vs ${rust_hash:-<none>}"
    echo "   wgl == walgit: $after"
else
    echo "   (no reference walgit binary with collab commands; skipping the cross-check)"
fi

echo "wgl-e2e: ok"
