# walgit — working rules

> Split out of `AGENTS.md` on 2026-11-05. These are the rules behind the
> charter's acceptance gates (`AGENTS.md` §3) and the test tiers CI runs.
> The no-backwards-compatibility banner applies to every change:

> **No backwards compatibility (pre-1.0).** We owe nothing to previous shapes of this system. Do not keep
> aliases, fallbacks, shims, deprecated routes, old config keys, old proto fields or migration code. When a
> decision changes the shape, **delete the old shape in the same change**: routes, clients, tests, docs, config
> keys. Data in the bucket is the one exception — WAL/manifest/log formats stay append-only and replayable,
> because the bucket is the repository; everything else is disposable. If you find yourself writing "still
> accepted for …" or "legacy", stop and remove the thing instead.

## Working rules

- **No backwards compatibility (pre-1.0, banner at top):** change the shape and delete the old one in the same
  commit — no aliases, shims, deprecated routes/keys/fields. Only bucket formats (WAL, manifest, log, checkpoint
  protos) stay append-only/replayable.
- **New user-visible surface ships with SKILL.md documentation.** Any new CLI subcommand/flag, MCP
  method/resource/flag, or user-visible workflow must update the public `web/SKILL.md` and the shipped ops skill
  `skills/walgit/SKILL.md` in the same change (or the ops skill must point to the web guide when the detail belongs
  there). `scripts/check-skill-covers-cli.sh` is the machine guard for CLI/MCP coverage; a PR that cannot explain
  the new surface there is not mergeable. The guard covers top-level CLI commands only (the `walgit mcp` flag half
  went with the adapter, D64); nested subcommands/flags and the MCP surface — tools, resource URIs,
  `resources/templates/list` — still require review discipline, because they are the host's `/mcp` contract, not
  a CLI to walk.
- Keep this file current: append decisions with a number and a date; never delete history, replace it with the
  decision that superseded it.
- **Never block the async runtime**: no blocking git/fs work (repack, midx, commit-graph, gix reopen, large
  reads/copies) on a tokio worker — `spawn_blocking`; every `refresh()` on an async path is `refresh_async()`. Pack
  materialization runs on the **bulk runtime** (`sync::on_bulk_runtime`). The runtime watchdog logs "async runtime
  stalled" with `inflight` and `tasks_running`: `inflight = 0` at a late tick ⇒ the platform paused the process,
  `inflight > 0` ⇒ a real stall — look at `lock_wait_max_ms` and `walgit_lock_wait_seconds{lock}`. Bulk bytes never
  share a transport with the control plane (`store.gcs.bulk_clients`, `bulk_concurrency`).
- **Correct is not sufficient.** Every protocol change (publish, sync, leases, checkpoints, bundles) is also
  judged on critical-path round trips against the bucket — read `docs/ROUNDTRIPS.md`, update its budget table, put
  before/after depth in the commit, keep verification on the failure path, assert request budgets in the sim.
- **Standalone first (D39):** a feature must work with walgit hit directly (no edge, in-process TLS, bytes
  streamed by walgit). Anything an edge takes over is announced per request in `X-Walgit-Capabilities`; never
  infer an edge from config, never hardcode a hostname in `crates/` or `web/`.
- **S3 and GCS are both first class.** Every store feature has both implementations and runs in the contract
  suite (`just test-s3` against rustfs, `just test-gcs <bucket>`); "GCS only" is a bug.
- **Use the rig before prod** (`just dev-store` → `walgit-server --config walgit.standalone.toml`). Per-repo
  settings (D24) with minute-scale slots compress a week of bundle behaviour into 30 minutes.
- No new auth paths (`docs/ARCHITECTURE.md` §1.3). No LIST on hot paths. No unbounded buffering of packs in memory. No full
  materialization above `cache.max_bytes` in budget mode. No silent long operations (make it a task, narrate it).
- Every immutable response: `immutable` + strong ETag + Range; every ref-dependent response: SWR + ETag.
- Before changing the wire/store formats: proto is append-only; manifests/log entries must stay replayable by
  old readers within the retention window.
- Web: pnpm + Vite, `pnpm run build` must pass oxlint/tsc. Config: `walgit.example.toml` documents every key;
  change it with the code.
- Test tiers: `just test` (fast, < 1 min: lib/bins, the wildcard crates, the server integration set),
  `just wgl-test` + `just wgl-e2e` (the wgl client lanes, D69), `just e2e`, `just sim`, `just warnings`, `just clippy` (the
  `[workspace.lints]` set, `-D warnings`), `just ci` = all of them; `just spec` / `just spec-fragments` / `just spec-full`
  run the bounded TLA+/TLC contract checks in `docs/spec/` (not part of `just ci`; CI runs the
  four-model fragment subset in its own bounded job); the **simulation
  suite** `cargo test -p walgit-server --test sim` (fault links per instance over one truth store: crash,
  partition, stale, lost response, orphan scenarios + randomized seeds `WALGIT_SIM_SEEDS`/`WALGIT_SIM_SEED`);
  CI runs sim on both ubuntu (build-test) and the windows leg, e2e on ubuntu and the windows leg,
  and the server integration suites and the wildcard crates on both — one list per lane in the
  justfile (`SERVER_TESTS`, `WILDCARD_TEST_PKGS`; issues #137/#141). The **walgit-cli suites
  (`CLI_TESTS`) ran on ubuntu only** — retired by D69: the collaboration command
  families moved to the standalone wgl client, whose lanes are `wgl-test` (zig
  unit/golden) and `wgl-e2e` (transport end-to-end; cross-checks the board hash
  against the Rust aggregation when a walgit binary is present). The old seam
  arguments live at the justfile
  (issue #141). `scripts/suite-guard.sh` (a CI step) asserts every `crates/*/tests/*.rs` is in a lane
  or a listed exemption; the windows leg also runs `just web-test` and greps the same
  `warning_gate` pattern ubuntu's `just warnings` uses (issue #141).
  `just test-slow` (ignored benches); `tests/e2e.sh` against a running server (`WALGIT_E2E_BASE_URL`,
  `WALGIT_TOKEN`). Never `cargo test --workspace --no-fail-fast` in a session; wrap ad-hoc cargo in `timeout`.
- Known flaky (find the cause, not the assertion):
  `sim::base_rebuild_resumes_after_a_kill_between_any_two_phases` (shared `TEST_ABORT_AFTER`).
  Its "~1 in 7" was counted on **job conclusions**: the sim step's own rerun swallowed the
  first-pass reds. #138 measured 4 reds in 24 h on windows first pass, and inside one job both
  attempts can fail identically — the race is with this instance's own post-publish commit-graph
  writer, not with another test, so "passes alone" is a machine-state coincidence, not a verdict.
  Root cause #138, fixed by #139 (`copy_tree` now skips transient `.tmp` names, tolerates
  `NotFound` inside the commit-graph state only, and puts the source path in every other error).
  Delete this entry after ~1 week of windows-leg **first-pass** green — the per-attempt metric,
  not the job conclusion that misled us here.
  Windows-only timing cases (issue #94; assertions widened for windows; the walgit-wal CI step
  reruns once and the sim suite already reruns once):
  `walgit_wal::tests::test_refs_sync_never_waits_behind_a_long_read_guard` and
  `sim::liveness_stale_instance_cannot_starve_the_core`. `reads_after_an_acknowledged_push_never_show_the_previous_tip` is
  hardened (PR #28): a sustained run of ≥3 consecutive identical foreign tips is a hard failure with a
  full DIAG dump (contenders/prev_winner/base/winner/after/seen) — a red means the read-side race of
  issue #4 re-fired; attach the dump to issue #4, never rerun-and-forget. 1–2 stray foreign samples
  print the same dump without failing. (2026-09-09, #146: the test's contender loop used to clone and
  spawn serially, so contenders formed a fast-forward chain and the foreign judgement false-positived
  on the chain's committed intermediate tips — run 34225650473's "c549696" red was that
  misjudgement, not a server race; adjudication on issue #4. The loop is now two-phase: all six
  contenders cloned from the same base before any push spawns, with exactly-one-winner asserted, so a
  foreign red is again unambiguous server evidence.)
- **The clippy gate is green** (issue #1 debt cleared 2026-09-03): `just clippy`
  (`cargo clippy --workspace --all-targets -- -D warnings`) exits 0. New warnings are a
  regression — fix them in the PR; never by weakening the lint table. The `clippy` CI job
  keeps `continue-on-error` until the branch ruleset adds it to required checks
  (maintainer follow-up); a red there is treated as a blocker by convention.
