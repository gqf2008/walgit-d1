# AGENTS.md — collaboration charter for this repository

This file is the **charter**: who does what, how work is dispatched, what it must pass before it
merges, and where every other document lives (§10). It is deliberately short. The system manual is
`docs/ARCHITECTURE.md`, the decision log is `docs/DECISIONS.md`, the detail behind each gate is
`docs/WORKING_RULES.md`, and release mechanics are `docs/RELEASE.md`.

Reading order: `GOAL.md` (what we want) → `docs/ARCHITECTURE.md` (what the system is) → this file
(how the team works) → `docs/DECISIONS.md` (why it is the way it is, consulted per decision).

> **No backwards compatibility (pre-1.0).** Change the shape and delete the old shape in the same
> change — no aliases, shims, deprecated routes/keys/fields. Only bucket formats (WAL, manifest, log,
> checkpoint protos) stay append-only and replayable. Full text: `docs/WORKING_RULES.md`.

## 0. Parameters

| Parameter | Value |
| --- | --- |
| Project | `walgit` — object-store-backed git host (Rust workspace + React SPA) |
| Repository root | the checkout you are in |
| Main branch (`<MAIN_BRANCH>`) | `main` |
| Member worktrees (`<WORKTREE_ROOT>`) | `.worktrees/<unit>` — a worktree is an identity, never removed (D62) |
| Member branches (`<BRANCH_PREFIX>`) | `feat/<thread>`, created inside the member's worktree, one per card |
| Build (`<BUILD_CMD>`) | `cargo build --workspace --all-targets`; SPA `just web-build` |
| Check (`<CHECK_CMD>`) | `just warnings`, `just clippy`; SPA `pnpm run build` (oxlint `--deny-warnings` + tsc) |
| Test (`<TEST_CMD>`) | `just test`; SPA `just web-test` |
| Integration (`<INTEGRATION_CMD>`) | `just wgl-e2e`, `just e2e`, `just sim`; everything at once is `just ci` |
| Review (`<REVIEW_CMD/流程>`) | leader review + a verified `approve` from a principal other than the author (`merge_rule_eval`) |
| Rhythm (`<TICK_INTERVAL>`) | no timers: `wgl collab watch --once` under a supervisor or a timer (D55) |
| Decision model (`<DECISION_MODEL>`) | none — deterministic gates are commands, judgment calls are the leader's |
| Tracker (`<TRACKER>`) | the D1 collaboration layer: `refs/collab/*`, read and written over `/mcp` (`docs/D1_PROTOCOL.md`) |

## 1. Roles

### Leader (one per project)

- Owns the decomposition: turns a goal into cards (`issue` entries) with **machine-checkable**
  acceptance criteria, and keeps the board honest.
- Dispatches each unowned card to a worker and supervises it (launch, timeout, budget, restart, log).
- Reviews the worker's patch, merges, pushes, and records `merge_result` plus `status: closed`.
- Escalates to the human for ambiguity, authorization or external input — never guesses.
- **Does not write the patch it reviews.** A leader that wrote a commit is not its reviewer; that
  card's review goes to another worker's principal, or the work waits for a second principal (D59).

### Worker (one per card)

- Works in **its own worktree and its own identity**: `.worktrees/<unit>`, branch `feat/<thread>`,
  key at `<git-dir>/walgit/keys/<principal>.ed25519` (D58/D59/D62). One card, one owner, one worktree.
- Claims before working: read the board, publish a `card_claim`, re-read the board; if someone else
  holds a live claim, stand down (`docs/D1_PROTOCOL.md` §8.4).
- Reports **every problem to its card** — an error, a red test, a missing dependency, an ambiguity —
  as a signed `comment`, or `status: blocked` when it halts the work (D62). The card is the only
  report channel: never the console alone, never a side chat.
- **Does not spawn sub-agents**, does not push to `main`, does not merge.

The set of surviving worktrees is the team's physical roster: retiring a principal is
`wgl collab principal-revoke`, never `git worktree remove` (D62).

## 2. Delivery flow

1. **File the card** — an `issue` root entry: title, body, owner, and acceptance criteria that a
   command (or a reviewer with the diff) can decide. Pure backlog with no owner is still written as a
   card with `status: blocked` / `needs-human`; a thread with no `status` entry shows as unassigned.
2. **Claim it** — `card_claim` with `{ttl?, worktree, branch, work}`; duplicate claims are legal,
   duplicate *execution* is not — the board's `owner` after your publish is the verdict.
3. **Work** in your own worktree/branch; keep `status` current (`in-progress`, `blocked`,
   `needs-human`), and put the live worktree/branch/work context on it.
4. **Hand to review** — `status: needs-review` with the branch and commit evidence on the card.
5. **Review** — a principal other than the author judges the diff against the card's acceptance and
   publishes a `review` with severity-tagged findings (`file:line` + failure scenario).
6. **Merge** — no Critical/Important finding **and** all acceptance green ⇒ the leader merges
   (local merge → push → one `merge_result` + `status: closed`). Minor does not block. Anything else
   routes back to the worker as `in-progress`, or to a fixer; genuine ambiguity goes to `needs-human`.

Moving a card is one signed `status` entry, never a board write; the projection re-derives columns
from the entries.

## 3. Acceptance gates (before any merge)

```bash
just warnings      # zero rustc warnings
just clippy        # -D warnings
just test          # fast tier: lib/bins, wildcard crates, server integration
just wgl-test      # the wgl client's zig unit/golden tests
just wgl-e2e       # the wgl transport e2e (register→push→aggregate→claim→fold)
just e2e           # smart HTTP against real git
just sim           # fault-injected cluster over one truth store
just web-test      # SPA unit tests; plus `pnpm run build` for oxlint + tsc
```

A change touching `smart.rs`, receive/upload-pack, publish, sync, leases, checkpoints or bundles
also carries a round-trip note per `docs/ROUNDTRIPS.md`. A change touching the SPA also passes
`just web-build`. A release runs all of the above as `just ci`.

Blocking, on top of the commands:

- new behaviour has tests; a defect fix has a regression test;
- no deleted, skipped or weakened test to make the pipeline green;
- a user-visible surface (CLI command/flag, MCP tool/resource, workflow) updates `web/SKILL.md` and
  `skills/walgit/SKILL.md` in the same change, and `scripts/check-skill-covers-cli.sh` stays green;
- cross-crate contracts, config keys and protocol docs move with the code (`docs/CONTRACT.md`,
  `walgit.example.toml`);
- the clippy gate is part of the merge bar even while CI keeps its job `continue-on-error`
  (`docs/WORKING_RULES.md`).

## 4. Rhythm

There is no scheduler. The leader advances the board by reading it — `wgl collab watch` under a
supervisor, or `--once` under a timer — and by dispatching what is unowned. Instance state is local
and disposable; durable truth is `refs/collab/*`. A held, long-polling "wait until something
changes" request is not an option: refs-level polling is the observation surface (D55). If a pass
finds nothing new, report status; do not re-dispatch the same card.

## 5. Communication and permissions

- The card is the record. Decisions, blockers, red tests and exceptions go there as signed entries;
  process chatter stays out of it.
- Outward or irreversible acts need the human first: pushing to `github`, cutting a release tag,
  deleting a repository, forcing history (`docs/RELEASE.md` has the release path).
- **No credentials in the conversation, the repository, a card, a commit or a command line.** The
  tokens this project needs already live in their own places (the private `sqb-private` repository,
  the macOS Keychain, environment variables); reference them, never copy them.
- Deletion, force-push and history rewriting are confirmed before execution.

## 6. Tracker and cross-team work

The tracker is the D1 collaboration layer (`refs/collab/*`), not GitHub: the mirror's Issues, Wiki,
Projects and Discussions are disabled (D46). Read and write it over `/mcp` (`collab_board`,
`collab_thread`, `collab_publish`, …); `wgl collab …` is the scripting form of the same lanes (D69).

Cross-team or cross-repository work is a card too: background, expected result, acceptance,
owner/team, deadline and links in the body; state changes as `status` entries with a note. Releases
are a separate, human-driven act — see `docs/RELEASE.md` (D57: refs reach GitHub by hand, and the
mirror is never a resident loop).

## 7. Engineering discipline

The short version; `docs/WORKING_RULES.md` is the authority.

- Small commits, one logical change each, Conventional Commits, each commit buildable.
- `main` is protected: code lands through a member branch and a merge, never by a direct commit.
- Quality gates (§3) run before the merge; a red gate blocks, it is never rerouted.
- Review is by a different principal; findings name `file:line` and the failure scenario.
- Contracts (protocol, API, config keys, tool schemas) change together with their docs and examples;
  breaking ones need the human.
- New dependencies need a reason; no new auth paths; no `LIST` on a hot path; no silent long work.
- The bucket is the repository — everything else is a cache or a reader of the log
  (`docs/ARCHITECTURE.md` §3).

## 8. Dispatch template (the leader copies this onto a card)

```markdown
你是本项目团队成员（worker）。仓库：<REPO_ROOT>。
工作区/分支（已建好）：.worktrees/<unit>，分支 feat/<thread>。
先读 GOAL.md、docs/ARCHITECTURE.md，以及 docs/DECISIONS.md 中与本任务相关的条目。
只在该 worktree 内改动；不 push main、不合并、不派生下级代理；先 claim 再动手。

## 卡片
<thread id、标题、机器可判定的验收标准>

## 边界
<要做什么；明确不做什么；不重叠其他成员的文件>

## 验收
just warnings / just clippy / just test（改到 SPA 加 just web-test；改到 git 协议加 just e2e）。
新增行为有测试，修复缺陷有回归测试。

## 交付
先把状态和问题写进卡片（签名 comment / status），再报告：
分支、commit SHA、改动摘要、红→绿证据、运行的命令与结果、剩余风险。
```

## 9. Definition of Done

A branch may merge only when:

- [ ] the target behaviour is implemented and nothing outside the card's boundary moved;
- [ ] every gate in §3 is green (including the suites the diff's area demands);
- [ ] new/changed behaviour has tests; a defect has a regression test;
- [ ] docs, examples and config keys that the change touches are updated in the same change;
- [ ] no credential, no debug output on a standard channel, no hard-coded host;
- [ ] review completed by another principal, Critical/Important findings resolved or explicitly
      dispositioned;
- [ ] commits are small, conventional and each buildable;
- [ ] the card carries `merge_result` + `status: closed`, or the blocker is on the card.

## 10. Document map (one home per fact — link, don't duplicate)

| Doc | Who / when to read it |
|---|---|
| `GOAL.md` | Everyone, first. What walgit is for, the acceptance table, what we do not optimise for. |
| `docs/ARCHITECTURE.md` | Everyone. Constraints §1, the WAL §2, the principles a PR is judged against §3. |
| `AGENTS.md` (this) | Everyone. The charter: roles, delivery, gates, rhythm, tracker, document map. |
| `docs/DECISIONS.md` | Anyone changing a shape or asking "why is it like this". Every decision in force (D1–D68), append-only. |
| `docs/WORKING_RULES.md` | Anyone running a gate. Test tiers, known-flaky list, clippy/warnings bar, no-backwards-compat banner. |
| `docs/RELEASE.md` | Anyone pushing anywhere or cutting a release: remotes, credentials policy, CI signals, the GitHub mirror. |
| `README.md` | The introduction: why (the Cursor lineage), what it does, how it works briefly, running it, invariants. |
| `.github/copilot-instructions.md` | AI coding agents working in this repository (GitHub web/IDE): pointers, workflow, CI reading. |
| `docs/BUNDLE_URI_DESIGN.md` | Anyone touching bundles, the scheduler, base rebuilds, or big-repo clone/fetch UX. Design of record; normative config in §4. |
| `docs/WINDOWS.md` | Developing on a Windows host: prerequisites, test surface, `dev-store` equivalent, NTFS symlink coverage. |
| `docs/TRAY_LINUX_BACKEND.md` | Anyone touching the tray's Linux backend, the glib/gtk dependency chain, or the Dependabot glib advisory. |
| `docs/ROUNDTRIPS.md` | **Anyone touching a protocol that talks to the bucket** (publish, sync, checkpoints, compaction/leases, bundles, remote reader, store backends). Round trips are the cost model; correct is not sufficient. |
| `docs/spec/README.md` | Anyone running `just spec` or auditing the WAL contract: bounded TLA+ models, the contract map, and the open gaps. |
| `docs/POLICY.md` | Anyone touching receive-pack authorization or writing a repo policy. Normative rule language. |
| `docs/LFS.md` | Anyone touching LFS (`lfs.rs`, `lfs_upstream.rs`) or importing a repository whose LFS history lives elsewhere. |
| `docs/INTEGRITY.md` | Anyone touching import, the maintainer's `fsck`/`repair` units, or seeing `connectivity: missing object` on a push. |
| `docs/D1_CI_PROTOCOL.md` | Anyone touching the decentralized CI protocol: the `walgit ci` runner, `ci_claim`/`ci_result` entries, or the CI aggregation. |
| `docs/D1_PROTOCOL.md` | Anyone touching the D1 collaboration layer: identities, refs layout, entry schema/canonical signing, verification, aggregation, the board projection, the D45 fold, claims (§8.4), observation lanes. |
| `docs/D1_COLLAB_DESIGN.md` | Design background and progress history of the D1 layer (current normative rules are in `docs/D1_PROTOCOL.md`). |
| `docs/CONTRACT.md` | When you touch a crate boundary. The cross-crate contract; *extend, don't rename*; code wins where they differ. |
| `docs/reference/cursor-git-at-any-scale.md` | The source design, verbatim. Read once before touching WAL/publish/sync/placement. |
| `docs/patches/README.md` | Git client patches (bundle filter matching) and the gate for advertising filtered bundle families together. |
| `web/API.md` | UI/SDK authors and anyone changing `web/*.rs`. Wire contract, caching rules, SSE envelope, tasks, prefix-first lanes. |
| `web/sdk/README.md` | Users of `repos.js`. |
| `web/README.md` | Frontend engineers changing the React SPA, Vite build, SDK adapter, static assets, loading states. |
| `walgit.example.toml` | Every config key with its default and a comment. Change it with the code. |
| `walgit.standalone.toml` | The one-machine shape: `walgit-server --config walgit.standalone.toml` → `https://walgit.localhost:8080/`. |
| `deploy/nginx.conf.example` | An optional nginx in front; documents the `X-Accel-Redirect` byte-offload contract. |
| `deploy/tray/` | The tray app for a local deployment (macOS ships it inside the DMG): start/stop, update detection, click-to-upgrade. |
| `deploy/windows/` | The Windows installer (Inno Setup): per-user install, autostart; built into releases by `release.yml`. |
| `deploy/linux/` | The linux `.deb` assembly; built into releases by `release.yml`, validated on every PR by `ci.yml`. |
| `deploy/tray/macos/build-dmg.sh` | The macOS release DMG pipeline, run by `release.yml` and reproducible locally. |
| `Containerfile`, `flake.nix` | An OCI image; a Nix package, image and devshell. |
