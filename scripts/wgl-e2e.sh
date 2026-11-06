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
    for cand in "$root/target/debug/walgit" "$root/target/release/walgit"                 "$(dirname "$root")/walgit/target/debug/walgit"                 "$(dirname "$(dirname "$root")")/walgit/target/release/walgit"; do
        if [ -x "$cand" ]; then rust_bin="$cand"; break; fi
    done
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

step "cross-implementation: the Rust aggregation must agree on the board hash"
if [ -n "$have_rust" ]; then
    rust_hash="$( "$rust_bin" --config /dev/null collab board --repo "$b" --format hash 2>/dev/null )"
    [ "$rust_hash" = "$after" ] || fail "wgl and walgit disagree on the board hash: $after vs ${rust_hash:-<none>}"
    echo "   wgl == walgit: $after"
else
    echo "   (no walgit binary found; skipping the cross-check)"
fi

echo "wgl-e2e: ok"
