#!/usr/bin/env bash
# Integration-suite completeness guard — issue #137 (server) generalized to
# every crate by issue #141. The filesystem is the truth, the justfile is the
# claim: every crates/*/tests/*.rs must sit in an execution lane, or the run
# that forgot it goes red here instead of the suite silently never running
# (the walgit-cli ci_e2e/collab_e2e orphanage #140's audit found on disk).
# D69: the collab/principal/ci families and their suites moved to the
# standalone wgl client; walgit-cli has no integration suites, and the wgl
# lanes (zig tests + the script e2e) are anchored here in their place.
#
# Lanes (all defined once, in the justfile; CI consumes them):
#   walgit-server  SERVER_TESTS, run by test-server-integration
#   wgl            zig unit/golden tests (wgl-test) + the transport e2e
#                  (wgl-e2e); the retired CLI_TESTS lane is gone
#   everything else  WILDCARD_TEST_PKGS, run by test-wildcard-crates: the whole
#                  tests/ directory is the lane, a new file runs the moment it
#                  is committed
# Non-suite files (shared modules, tiers with their own recipes) are named in
# the crate's *_TEST_EXEMPT — explicit, but read by this script, never a
# second copy of anything.
#
# The checks, and the failure each exists to catch:
#   ① every file in the truth set is in a lane                (new unregistered
#                                                             suite goes red)
#   ② a glob that matches nothing, an empty lane variable    (#140's vacuous-
#                                                             pass fix: green
#                                                             because there was
#                                                             nothing to see)
#   ③ the *_TESTS / PKGS variables are actually interpolated whole-line by the
#      recipes they claim to feed — a `{{tN}}` prefix glued in front of the
#      cargo line is the windows hazard those recipes' comments warn about, so
#      the match is anchored to the whole line, #140 style.
set -euo pipefail
cd "$(dirname "$0")/.."

die() { echo "suite-guard: $*" >&2; exit 1; }

# ① nested targets: `tests/<dir>/main.rs` is a cargo test target the top-level
# glob cannot see; a nested .rs that is not `mod.rs` either names such a target
# or is a dead file cargo ignores — both belong in this conversation. Flatten
# it, or extend this script with the shape you actually added.
nested="$(find crates/*/tests -mindepth 2 -name '*.rs' -not -name 'mod.rs' 2>/dev/null || true)"
[ -z "$nested" ] || die "nested test file(s) outside the guard's glob: $nested"

# ③ single-source consumption: each list variable must still be interpolated on
# the cargo line of its own recipe. Whole-line anchoring is load-bearing (see
# header ③) — and if the recipe disappears entirely, these greps are what turn
# the lane's removal loud.
grep -qE '^[[:space:]]*cargo test -p walgit-server \{\{SERVER_TESTS\}\}[[:space:]]*$' justfile \
    || die "test-server-integration no longer interpolates SERVER_TESTS on its own line — the single source became decorative"
grep -qE '^[[:space:]]*cd wgl && zig build test[[:space:]]*$' justfile \
    || die "wgl-test no longer runs `zig build test` on its own line — the single source became decorative"
grep -qE '^[[:space:]]*scripts/wgl-e2e.sh[[:space:]]*$' justfile \
    || die "wgl-e2e no longer runs the script on its own line — the single source became decorative"
grep -qE '^[[:space:]]*cargo test \{\{WILDCARD_TEST_PKGS\}\} --tests[[:space:]]*$' justfile \
    || die "test-wildcard-crates no longer interpolates WILDCARD_TEST_PKGS on its own line — the single source became decorative"

server_list="$(just --evaluate SERVER_TESTS)"
server_exempt="$(just --evaluate SERVER_TEST_EXEMPT)"
wild="$(just --evaluate WILDCARD_TEST_PKGS)"
# An empty lane variable makes the registration test below pass vacuously —
# `cargo test -p walgit-cli` with no --test flags is a different (broader,
# unregistered) invocation. A crate with integration suites always has a
# non-empty explicit lane.
[ -n "$server_list" ] || die "SERVER_TESTS evaluates empty — the server lane cannot register anything"
[ -n "$wild" ] || die "WILDCARD_TEST_PKGS evaluates empty — the wildcard lane is gone"

n_registered=0
for d in crates/*/tests; do
    [ -d "$d" ] || continue
    crate="$(basename "$(dirname "$d")")"
    # ② vacuous truth: a tests/ directory whose glob matches nothing is drift or
    # a rename, never a pass (140's fix, per directory now).
    [ -n "$(ls "$d"/*.rs 2>/dev/null)" ] \
        || die "crate $crate: tests/ exists but no top-level suite matches — vacuous lane, delete the dir or name a suite"
    case "$crate" in
        walgit-server) list="$server_list"; exempt="$server_exempt" ;;
        walgit-cli) die "walgit-cli grew an integration suite — register it in a lane like SERVER_TESTS (D69 moved the collab suites to wgl)" ;;
        *)
            case " $wild " in *" -p $crate "*) ;;
                *) die "crate $crate has integration suites but no lane — add it to WILDCARD_TEST_PKGS (the whole tests/ dir then runs on both legs) or give it an explicit *_TESTS like the server and the cli" ;;
            esac
            # wildcard lane: every top-level file runs; registration is the
            # crate membership above, checked once.
            for f in "$d"/*.rs; do n_registered=$((n_registered + 1)); done
            continue
            ;;
    esac
    for f in "$d"/*.rs; do
        n="$(basename "$f" .rs)"
        case " $exempt " in *" $n "*) continue ;; esac
        case " $list " in *" --test $n "*) n_registered=$((n_registered + 1)) ;;
            *) die "unregistered suite: $crate/tests/$n.rs — add --test $n to $crate's list in the justfile (SERVER_TESTS / CLI_TESTS), or its *_TEST_EXEMPT if it is not a suite" ;;
        esac
    done
done

# ② reverse vacuity: every named --test must exist on disk IN ITS OWN crate.
# cargo already dies on `no test target named`, but only where the lane RUNS —
# a typo in a lane the current leg skips would slither past. The --tests
# wildcard needs no such check: the directory is the list.
check_lane_names() {
    local crate="$1" entry
    shift
    for entry in "$@"; do
        [ "$entry" = "--test" ] && continue
        [ -f "crates/$crate/tests/$entry.rs" ] \
            || die "lane names --test $entry for $crate but crates/$crate/tests/$entry.rs is not on disk"
    done
}
# shellcheck disable=SC2086 # the lists are --test/name token pairs by contract
check_lane_names walgit-server $server_list

echo "suite-guard: $n_registered integration suite files have execution lanes"
