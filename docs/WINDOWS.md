# Windows — build, test, dev-store on a Windows host

Context: **runbook** for developing walgit on Windows. The fork keeps the
platform first-class: CI runs a windows leg (compile all targets + zero-rustc-
warning gate + web unit tests + fast tier + server integration + sim + e2e,
`.github/workflows/ci.yml`), and the local workflow below is the
same surface a contributor gets.

## 1. Prerequisites

- **Rust**: `rustup` (the stable toolchain installs on
  first `cargo` use; `rustup show` in the repo root).
- **protoc**: `choco install protoc -y` (prost-build does not vendor it).
- **git for Windows**: required — the server shells out to real `git`
  (`multi-pack-index write`, `repack`, `index-pack`, …). Any recent build works.

  **Not every `git.exe` behaves the same.** The MSYS2 one
  (`\msys64\usr\bin\git.exe` — first on PATH for anyone who uses MSYS2, and what
  the walgit service actually picks up there) runs **bash-style brace expansion
  on its own arguments**: a spawned `rev-parse HEAD^{commit}` reaches git as
  `HEAD^commit`, so the source/tree endpoints 404 with `Not a valid object name
  <sha>^tree`. Never build a `^{tree}` / `^{commit}` suffix for a subprocess —
  use `git rev-list -1 <rev>` (peels a tag, prints nothing for a non-commit),
  `git log -1 --format=%T <rev>` (the root tree), `git cat-file -t|-e`, or
  `ls-tree <commit-ish>` directly. Fixed 2026-09-17, see the collab thread
  `cc-ai-win-git-argv`.
- **pnpm**: `corepack enable` or a standalone install; `just web-build` uses it.
- **just**: optional for a plain build; `just` itself is not installed by any
  package manager on Windows — grab a release binary from
  `just.systems` or `cargo install just`. (CI installs it via
  `taiki-e/install-action`.) Git Bash is the recommended shell for `just`
  recipes: they use GNU coreutils (`timeout`, `setsid`, …) that Git Bash ships.

Everything below assumes **Git Bash** (comes with git for Windows) unless
noted. `cargo` and `rustc` are the rustup proxies on PATH.

## 2. Build and test

```bash
just web-build        # SPA + SDK (pnpm install --frozen-lockfile + vite build)
just test             # fast hermetic tier (< 1 min)
just e2e              # smart-HTTP end-to-end against real git
just sim              # fault-injection simulation suite (seeds: WALGIT_SIM_SEEDS)
just ci               # warnings + clippy + test + test-cli + e2e + sim
```

Notes specific to Windows:

- `just` recipes wrap long commands in `timeout` (GNU coreutils) — Git Bash
  provides it; on a shell without it the recipes degrade to running without a
  watchdog.
- The e2e suite builds a `git` shim with `rustc` on the fly for the
  history-pack stall test (CreateProcess resolves only `.exe`, so a script
  cannot shadow `git`); a missing rustc makes that one test print the reason
  and skip.
- The server runs **without a console** (the tray starts it detached), so every
  `git` child would make Windows allocate a *new* console window — a black box
  flashing on screen per request. All server-side git spawns go through
  `walgit_git::git_command()` / `git_tokio_command()`, which pass
  `CREATE_NO_WINDOW`; spawning `git` any other way brings the windows back.
- git for Windows marks finished `pack-*.pack`/`pack-*.idx` **READ_ONLY**.
  Supersede deletes and repo teardown clear the attribute before removing
  (see `LocalRepo::remove_pack_file` and `Registry::delete`); if you ever hit
  `os error 5` deleting pack files outside those paths, clear the attribute
  first.

## 3. `just dev-store` on Windows (rustfs)

`just dev-store` is a nix-podman recipe (rootless, unix sockets) and does not
run on Windows. Two equivalents:

**Option A — podman-desktop / docker**: install either, then

```bash
podman compose up -d rustfs     # or: docker compose up -d rustfs
podman compose run --rm create-bucket
```

The compose file starts rustfs (S3-compatible) on `127.0.0.1:9000` with
credentials `walgit-dev / walgit-dev-secret` and bucket `walgit-test`.

**Option B — rustfs binary directly**: run a rustfs binary (any S3-compatible
target — memory, minio, local FS) on `:9000` and create the bucket:

```bash
rustfs --listen 127.0.0.1:9000 --backend memory &
# create the bucket with any S3 client (mc, aws cli, …):
#   walgit-test, walgit-dev / walgit-dev-secret
```

Then start the server with the standalone config:

```bash
cargo build --release --bin walgit-server
./target/release/walgit-server --config walgit.standalone.toml
```

(`walgit.standalone.toml` points the store at `http://127.0.0.1:9000` with
those credentials.)

## 4. NTFS symlinks (store-mount tests)

The `test_serve_level_links_base_from_store_mount` case (`crates/walgit-wal`
tests) links a base pack from a read-only bucket mount via a real NTFS
symbolic link. The test detects whether symlinks can be created and prints
`skipped: creating symlinks failed (Windows needs Developer Mode or
administrator rights)` when they cannot.

On a current Windows 10/11 (verified: 10.0.19045, non-admin, Developer Mode
off) ordinary users can create symlinks on NTFS without any setting, so the
case runs for real and is covered by the fork's windows leg. If you ever see
the skip message, enable Developer Mode:

> Settings → Privacy & security → For developers → Developer Mode: On

(the registry key is
`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock\AllowDevelopmentWithoutDevLicense = 1`).

The repo itself may live on any filesystem (tests use `%TEMP%`, NTFS by
default); a symlink-capable volume is only needed for the mount-link case.

## 5. What CI covers (fork, issue #2; lanes re-drawn by issues #137/#141)

- **ubuntu** build-test: `just warnings` (zero rustc warnings, all targets),
  `just test`, the CLI suites and `just sim` (clippy and e2e are separate jobs;
  `just ci` bundles them all for the laptop). Its "Integration suite lists are
  complete" step runs `scripts/suite-guard.sh`: every file in
  `crates/*/tests/` must sit in an execution lane of the justfile
  (`SERVER_TESTS`, `CLI_TESTS`, `WILDCARD_TEST_PKGS`) or a read-back exemption —
  a suite nobody runs goes red instead of staying silent (issues #137/#141).
- **windows** leg: compiles every target **and gates its rustc warnings**
  (same `warning_gate` pattern as ubuntu, read from the justfile — issue #141),
  runs `just web-test`, the fast tier (its crate set is the justfile's
  `WILDCARD_TEST_PKGS` — one list, both legs), the server integration suites
  (`SERVER_TESTS` — also both legs), sim and e2e. The leg calls `just` only for
  probe-free lines and plain cargo elsewhere — the job exists so platform seams
  drift loudly.
- **ubuntu-only by seam, not by speed**: the walgit-cli suites (`ci_e2e`,
  `collab_e2e`) hardcode `--config /dev/null` (the windows form is `NUL`,
  D39), run POSIX-shell task fixtures (`sleep`, `test "$V" = …`) that the
  windows runner would execute through `cmd /C`, and `collab watch --exec`
  spawns `sh -c` in the product itself — every seam is named with file:line at
  the justfile's `CLI_TESTS`. Making them windows-runnable is a product
  change (`NUL`/`sh` twins), not a test change.
- Known flaky on both platforms (rerun, not skip): `sim::base_rebuild…`
  (shared `TEST_ABORT_AFTER`; first-pass rate per AGENTS.md §5's entry — #138
  measured it on windows), `fetch_from_front_…` ~1 in 3 under the
  full e2e suite; both pass alone.
