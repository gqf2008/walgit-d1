# walgit justfile — local dev and test targets.

# `timeout` is GNU coreutils: absent on macOS, where every wrapped recipe otherwise dies with
# `sh: timeout: command not found` (exit 127) before a single test runs — pushing contributors
# onto the broad `cargo test --workspace` that AGENTS.md forbids. Prefer timeout, then gtimeout
# (brew install coreutils), else run unwrapped: no watchdog is better than no tests.
t5 := `if command -v timeout >/dev/null 2>&1; then echo "timeout 300"; elif command -v gtimeout >/dev/null 2>&1; then echo "gtimeout 300"; else echo ""; fi`
t10 := `if command -v timeout >/dev/null 2>&1; then echo "timeout 600"; elif command -v gtimeout >/dev/null 2>&1; then echo "gtimeout 600"; else echo ""; fi`
t15 := `if command -v timeout >/dev/null 2>&1; then echo "timeout 900"; elif command -v gtimeout >/dev/null 2>&1; then echo "gtimeout 900"; else echo ""; fi`

# Default: show available targets.
default:
    @just --list

# Build the Vite SPA assets embedded by walgit-server.
web-build:
    cd web && pnpm install --frozen-lockfile && pnpm run build

# The web unit tests (vitest — MarkdownRenderer XSS boundary, issue #112).
web-test:
    cd web && pnpm install --frozen-lockfile && pnpm test

# macOS tray Release detection/package guards (Swift logic + AppleDouble checks).
tray-macos-test:
    deploy/tray/macos/test.sh

# Local dev = standalone: the server with every role (serve, maintain) at
# https://walgit.localhost:$PORT (default 8080) against local rustfs. Self-contained: starts rustfs (+ bucket) if
# it is not answering on :9000 and builds the SPA if web/dist is missing, then runs the server.
# `config` defaults to walgit.standalone.toml; point it at a real bucket by editing [store] there. The rustfs
# keys come from the environment (AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY; compose.yaml fixes them).
# Optional: export WALGIT__SERVER__AUTH__* (OIDC client, session secret) to try browser sign-in locally.
dev-local config="walgit.standalone.toml":
    #!/usr/bin/env bash
    set -euo pipefail
    export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-walgit-dev}" AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-walgit-dev-secret}"
    if ! curl -sf http://127.0.0.1:9000/minio/health/live >/dev/null 2>&1; then
        echo "rustfs not running on :9000 — starting it (just dev-store)"
        just dev-store
    fi
    if [ ! -f web/dist/index.html ]; then
        echo "web/dist missing — building the SPA (just web-build)"
        just web-build
    fi
    cargo build --release --bin walgit-server
    port="${PORT:-8080}"
    echo "→ https://walgit.localhost:${port}/  (PORT=${port}, config {{config}}, store rustfs :9000, cache /tmp/walgit)"
    exec ./target/release/walgit-server --config {{config}}

# Start rustfs (S3-compatible) for local dev via podman compose (rootless, no daemon group needed;
# `podman compose` drives compose.yaml through the docker-compose binary dev.yml installs).
# `podman compose` talks to the podman API socket; rootless nix podman has no systemd unit for it, so
# `podman system service` is started (detached, idle-timeout 0) when the socket is missing.
dev-store:
    #!/usr/bin/env bash
    set -euo pipefail
    # nix podman ships no /etc/containers: give the user a signature policy + registry search list once.
    cdir="${XDG_CONFIG_HOME:-$HOME/.config}/containers"; mkdir -p "$cdir"
    [ -f "$cdir/policy.json" ] || printf '{"default":[{"type":"insecureAcceptAnything"}]}\n' > "$cdir/policy.json"
    [ -f "$cdir/registries.conf" ] || printf 'unqualified-search-registries = ["docker.io"]\n' > "$cdir/registries.conf"
    sock="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/podman/podman.sock"
    if [ ! -S "$sock" ]; then
        echo "starting rootless podman API socket at $sock"
        mkdir -p "$(dirname "$sock")"
        setsid nohup podman system service --time=0 "unix://$sock" >/tmp/walgit-podman-service.log 2>&1 < /dev/null &
        for _ in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.2; done
        [ -S "$sock" ] || { echo "podman API socket did not appear; see /tmp/walgit-podman-service.log"; exit 1; }
    fi
    podman compose up -d rustfs
    echo "Waiting for rustfs to be healthy..."
    podman compose run --rm create-bucket
    echo "rustfs is running on http://127.0.0.1:9000 (console :9001)"
    echo "Credentials: walgit-dev / walgit-dev-secret"
    echo "Bucket: walgit-test"

# Stop rustfs.
dev-store-stop:
    podman compose down

# --- tests -------------------------------------------------------------------
# Tiers (all hermetic: in-memory store, tempdir caches, real `git` binary):
#   test       fast tier, < 30 s: every unit/integration test not marked #[ignore]
#   test-wildcard-crates  the `--tests` lanes below, one list both CI legs read
#   test-cli   walgit-cli integration suites — ubuntu only, see CLI_TESTS
#   test-slow  benches/soak: #[ignore]d tests (20k-ref push, 466k-ref render, ...)
#   test-s3    store contract against local rustfs (just dev-store)
#   test-gcs   store contract against a real bucket (writes under a unique prefix)
# Which files exist under crates/*/tests is asserted complete by
# scripts/suite-guard.sh (every suite in an execution lane or a listed exemption;
# issue #137 for the server, #141 for all crates).

# The server integration suites — THE list (issue #137). Both CI legs run exactly
# this set: ubuntu through `test` below, windows through the "Server integration"
# step in ci.yml calling `test-server-integration`. The windows leg used to
# hand-copy an enumeration and drifted twice: c971024 added budgets here
# without ever touching ci.yml; 7f18675 (#136) edited both lists side by side and
# still missed them. The build-test job asserts this list is complete. Facts kept
# from the old windows-leg comment: setup_wizard's one unix-only case is
# #[cfg(unix)]-gated in its suite; budgets and the #134 five (setup_wizard,
# api_cli, follow, policy, policy_inbox — 21 tests, ~6 s on ubuntu) were evaluated
# platform-clean for windows in #137 (no cfg/shell/signal/file-mode seams; the
# same hermetic harness the windows-approved suites use since issue #2).
SERVER_TESTS := "--test web_api --test web_ui --test api_v1 --test static_http --test maintain --test routing_prefix --test lfs_upstream --test drain --test budgets --test setup_wizard --test api_cli --test follow --test policy --test policy_inbox"

# tests/ files that are not suites: harness.rs is the shared module the suites
# `mod` in (it has no #[test] of its own); e2e/sim are separate tiers with their
# own recipes. Read by scripts/suite-guard.sh. An entry here means NO leg runs
# the file — only files that genuinely are not suites belong on it.
SERVER_TEST_EXEMPT := "harness e2e sim"

# The walgit-cli integration suites (issue #141) — the first execution lane
# these files ever had; until now both CI legs only compiled them. Ubuntu-only,
# and the platform seam is stated per-file (verified on ab75c81, cited files
# unchanged through 65fe9a3; collab_e2e.rs cites refreshed for the D45 gc e2e,
# #160 — the new spawn is the same /dev/null seam), because "looks clean" is
# not the bar for a windows lane (AGENTS.md §5, issue #94 history):
#   - every CLI invocation in both suites passes `--config /dev/null`
#     (ci_e2e.rs:140,155,415; collab_e2e.rs:89,192,298,467,520) — D39 makes `NUL`
#     the windows form and the tests never branch, so each spawn exits 2;
#   - ci_e2e's task fixtures are POSIX-shell scripts, executed by the runner
#     through `cmd /C` on windows (ci_cmd.rs:1147-1150): `sleep 3` (ci_e2e.rs:402),
#     `sleep 30` (ci_e2e.rs:577) and `test "$CI_E2E_ALLOWED" = yes && …`
#     (ci_e2e.rs:507) do not exist in cmd;
#   - `collab watch --exec` spawns `sh -c` in the product itself
#     (collab_cmd.rs:1099) and the watch test's callback is
#     `cat > …/$WALGIT_COLLAB_*` (collab_e2e.rs:232) — no windows twin.
# The two git-driven collab suites (cli_full_collab_flow…, board_projection…)
# are windows-clean apart from the /dev/null seam; they stay in this single
# ubuntu-only lane rather than splitting the set — a windows lane for them is a
# product change (`NUL`/`sh` twins), not a test change, and belongs to its own
# work unit.
CLI_TESTS := "--test ci_e2e --test collab_e2e"

# tests/ files in walgit-cli that are not suites (read by scripts/suite-guard.sh).
# Empty today: both files are suites. An entry means NO leg runs the file.
CLI_TEST_EXEMPT := ""

# Every crate whose whole tests/ directory runs through a single `--tests`
# invocation — no per-suite registration (a file added there runs the moment it
# is committed). Both legs consume this one list: ubuntu via `test` below,
# windows via the "Fast hermetic tier" step in ci.yml (issue #141 retired that
# leg's hand-enumerated `-p` lines; the walgit-wal timing case, issue #94, is
# covered by the step's set-level rerun, the Server-integration precedent).
WILDCARD_TEST_PKGS := "-p walgit-store -p walgit-git -p walgit-wal -p walgit-bundle"

# The probe invariant: just evaluates EVERY top-level backtick variable when any
# recipe runs (probed on just 1.48.0), so the windows leg already loads t5/t10/t15
# today via `just web-build` and must keep loading them — the probes always exit 0.
# USING a probe's value is the separate hazard: where just takes git-bash's sh,
# the probe's `command -v timeout` finds System32's timeout.exe (not coreutils —
# `timeout 300 cargo` is a hard error there), so a timeout-wrapped recipe line must
# stay unreachable from windows. Hang protection on the windows leg is the CI
# step's timeout-minutes; ubuntu wraps the call in `test` below with t10 — one
# 600 s budget for all 14 suites, the sum of the old two t5 lines, because 300 s
# has tripped this tier under CI load (exit 124; see the note at ci.yml's
# fast-tier step).

# Run the server integration set, unwrapped: the body never references {{tN}}.
test-server-integration:
    cargo test -p walgit-server {{SERVER_TESTS}}

# The walgit-cli integration set, unwrapped (same no-probe invariant; scripts/
# suite-guard.sh anchors this exact line). Ubuntu-only in CI — see CLI_TESTS.
test-cli:
    cargo test -p walgit-cli {{CLI_TESTS}}

# The wildcard crates' integration suites, unwrapped; both legs call this
# (issue #141). Anchored by scripts/suite-guard.sh — keep the line shape.
test-wildcard-crates:
    cargo test {{WILDCARD_TEST_PKGS}} --tests

# Fast hermetic tier (< 30 s): every test not marked #[ignore].
# Fast tier (default, < 1 min): unit tests + the quick integration suites.
# Never run `cargo test --workspace --no-fail-fast` interactively: a single
# hung test blocks for the whole timeout. Use `just e2e` / `just ci` below.
test:
    {{t5}} cargo test --workspace --lib --bins
    {{t5}} just test-wildcard-crates
    {{t10}} just test-server-integration

# Smart-HTTP end-to-end against real git (≈ 20 s) — run when touching smart.rs/receive/upload-pack/wal.
e2e *ARGS:
    {{t10}} cargo test -p walgit-server --test e2e {{ARGS}}

# The rustc zero-warning gate pattern — the ONE source both legs read
# (ubuntu's `warnings` recipe interpolates it below; windows' ci.yml compile
# step takes it via `just --evaluate warning_gate`). #137's lesson applied to
# the gate itself: a second hand-copied enumeration of this lint list is
# exactly what drifted before.
warning_gate := "^warning: (unused|function|variable|field|method|struct|enum|never|dead|irrefutable|unreachable|value assigned|deprecated|trait|type|constant|static|associated)"

# Zero rustc warnings, workspace-wide, all targets (tests, benches, examples).
# Done by grepping the normal build instead of RUSTFLAGS=-D warnings, which would
# change every crate's fingerprint and force full rebuilds in every shell.
warnings:
    #!/usr/bin/env bash
    set -uo pipefail
    # A command substitution that fails does NOT abort under `set -uo pipefail` (no -e), so a
    # workspace that does not compile used to fall through to "no rustc warnings" and exit 0 —
    # the preflight passing on a broken tree. Check the build's status before grepping it.
    if ! out="$({{t15}} cargo build --workspace --all-targets 2>&1)"; then
        printf '%s\n' "$out"
        echo; echo "cargo build failed — fix the errors above"; exit 1
    fi
    # CI sets CARGO_TERM_COLOR=always, which prefixes every diagnostic with ANSI
    # escapes — an anchored `^warning:` then never matches and this gate passes on
    # a warning-bearing tree (issue #29). Strip the escapes before matching; the
    # ESC is embedded as a bash $'…' literal so BSD and GNU sed both take it.
    plain="$(printf '%s\n' "$out" | sed $'s/\x1b\\[[0-9;]*m//g')"
    if printf '%s\n' "$plain" | grep -qE "{{warning_gate}}"; then
        printf '%s\n' "$plain" | grep -E '^warning' -A4 | grep -vE '^warning: `walgit-[a-z]+`'
        echo; echo "rustc warnings present — fix them (just warnings is part of just ci and the deploy preflight)"; exit 1
    fi
    echo "no rustc warnings"

# Verify the release changelog filter handles Conventional Commits breaking forms.
check-release-changelog-filter:
    scripts/check-release-changelog-filter.sh

# Clippy, workspace-wide, all targets, warnings are errors. The lint set lives in
# [workspace.lints] in Cargo.toml; test code is exempt from the panic-path restriction
# lints via clippy.toml (allow-unwrap-in-tests etc.).
clippy:
    {{t15}} cargo clippy --workspace --all-targets -- -D warnings

# Everything that must be green before a merge (what CI runs). A body, not a
# dependency list, so the cli tier gets its timeout guard like its siblings
# (`just ci` has never been windows-runnable — `warnings` uses the {{t15}}
# probe; the ubuntu-only verdict for test-cli is argued at CLI_TESTS).
ci:
    just warnings
    just check-release-changelog-filter
    just clippy
    just test
    {{t10}} just test-cli
    just e2e
    just sim

# Simulation suite: fault-injected cluster over one truth store
# (seeds: WALGIT_SIM_SEEDS / WALGIT_SIM_SEED).
sim:
    {{t5}} cargo test -p walgit-server --test sim

# Slow tier: #[ignore]d benches/soaks (20k-ref push, 466k-ref render, ...).
test-slow:
    cargo test --workspace -- --ignored --nocapture

# Store contract against a real GCS bucket (unique prefix per run, cleaned up).
test-gcs bucket:
    WALGIT_TEST_GCS_BUCKET={{bucket}} cargo test -p walgit-store --features gcs --test contract -- gcs_contract --nocapture

# Run walgit-store contract tests against memory only.
store-test:
    cargo test -p walgit-store --test contract -- memory_contract

# Run walgit-store contract tests against rustfs (requires `just dev-store` first).
# Store contract against local rustfs (run `just dev-store` first).
test-s3: store-test-s3

store-test-s3:
    WALGIT_TEST_S3_ENDPOINT=http://127.0.0.1:9000 \
    WALGIT_TEST_BUCKET=walgit-test \
    AWS_ACCESS_KEY_ID=walgit-dev \
    AWS_SECRET_ACCESS_KEY=walgit-dev-secret \
    cargo test -p walgit-store --test contract -- --nocapture

# Run all walgit-store tests (memory + S3 if env set).
store-test-all:
    cargo test -p walgit-store
