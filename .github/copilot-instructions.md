# Repository instructions for AI coding agents

walgit is a git server (one binary in front of an object store) with a WAL-backed
storage engine, developed as an **agent-native collaboration platform**. Read the
architecture and operating manual before touching anything: `AGENTS.md` (constraints
§1, WAL design §2, principles §3, decisions §4, working rules §5, **agent protocol
§6**), then `GOAL.md` for what it is for, `CONTRIBUTING.md` for the workflow. These
outrank any generic convention.

## Working here

- **The canonical repository is walgit** (`origin = http://127.0.0.1:8081/gqf2008/walgit.git`);
  GitHub is a read-only mirror plus the release pipeline. Work units, PRs, reviews and the
  board are signed entries in walgit's D1 collaboration layer (`refs/collab/*`): follow the
  `walgit` skill's flow (`walgit collab entry --kind issue|status|patch|review|merge_result`),
  not GitHub labels/PRs. Never push branches to the GitHub remote by hand; never double-push.
- Work in a worktree off `origin/main` (walgit), one worktree per issue, and run the merge
  locally after the signed review approves: merge into `main`, push `origin`, then record
  `merge_result {"merged": true}` (card → `merged`; `status done` is the gated equivalent,
  `status closed` archives) in the thread.
- **Commits are Conventional Commits** (`fix(git): …`, one logical change each,
  message says why).
- **`just ci`** = warnings + clippy + test + test-cli + e2e + sim — everything a merge
  needs. Run the relevant tiers locally before pushing (Windows notes: `docs/WINDOWS.md`;
  the cli suites are ubuntu-only, seams at the justfile's `CLI_TESTS`).

## Reading CI

The deep matrix runs on the GitHub mirror's Actions and comments a summary on every PR
there. `clippy (known-red debt)` is expected-red (~1300 pre-existing hits, issue #1; keep
your increment at zero). Tests on the known-flaky list (§5) auto-rerun once. Anything else
red is a regression — investigate. (walgit's own decentralized CI, if configured for a repo,
reports through `ci-*` collab threads instead.)

## Safety

- `fail closed` is a hard contract (§1.3): auth, policy, integrity checks never
  silently degrade.
- Secrets and credentials never go in issues, PRs, or comments — see SECURITY.md
  for private reporting.
- Don't weaken lints, thresholds, or tests to make CI green.
