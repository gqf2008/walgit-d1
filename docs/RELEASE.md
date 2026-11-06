# walgit — where the repository lives, and how releases ship

> Split out of `AGENTS.md` on 2026-11-05 (the old "Where this repository lives",
> §6.3 and §6.4). Read it before pushing anywhere or cutting a release.

## Where this repository lives

- **Canonical: walgit** — `origin = http://127.0.0.1:8081/gqf2008/walgit.git`. Issues, PRs, reviews and the
  board are signed entries in the D1 collaboration layer (`refs/collab/*`, `wgl collab ...`), not GitHub.
- **GitHub is the release pipeline** — `github = gqf2008/walgit-d1`. **Releases go through GitHub Actions, and
  their refs get there by hand**: after `main` and the tag are on walgit, `git push github main` and
  `git push github vX.Y.Z` — nothing else carries them, and pushing the tag is what fires
  `.github/workflows/release.yml` (build linux/windows/macOS, changelog from Conventional Commits, GitHub
  Release). The `~/.walgit/sync-to-github.sh` mirror is **not** part of the
  release path: **never run it as a resident loop** — not a `screen`/watch loop, not a
  launchd/Task-Scheduler/cron job (one was installed 2026-09-29 and removed 2026-09-30; D57): a loop makes
  releases land without anyone deciding to publish them, and it pushes branch deletions while nobody is
  watching. Outside that release path nothing is pushed and nothing is mirrored by default: feature branches
  stay on walgit (below: Releases and Actions keep working; the pre-2026-09-16 PR history stays as history).
- **Release push credentials are not in this repository.** Only the `gqf2008` account can push to
  `gqf2008/walgit-d1`; its token lives in the private `sqb-private` repository at
  `secrets/keychain/github-gqf2008-oauth.txt` (also served by the macOS Keychain entry
  `GitHub - https://api.github.com`). The normally active `gh` account (`aerodesk-io`) has only `pull` on
  the mirror, so `403 Permission … denied to aerodesk-io` on a release push means "authenticate as
  gqf2008", not "retry". Never copy the token itself into this repository, an issue, a commit or a
  command line.
- The deep CI matrix still runs on the GitHub mirror's Actions (its secrets live there); walgit holds the
  code and the collaboration history. The mirror's **Issues, Wiki, Projects and Discussions are disabled**
  (2026-09-16): for collaboration it keeps only PR history (by project policy — GitHub does not enforce it)
  and Releases, while Actions/Security keep their CI/release jobs. `fork` (`gqf2008/walgit-1`) and `upstream`
  (`tobi/walgit`) are historical references — never push to them.
- The mirror tool is `~/.walgit/sync-to-github.sh`. It is **not** how releases ship and it is not to be run as
  a loop (D57); it mirrors every `refs/heads/*` and `refs/tags/*`, never `refs/collab/*`, and deletes GitHub
  branches walgit no longer has (GitHub's default branch excepted), which is exactly why it must stay a
  deliberate, watched act. If it is ever run for a one-off repair, run it by hand, never two syncs at once
  (there is no lock, and concurrent runs race the same refs).

## Reading CI signals

The CI workflow posts a **summary comment** on every PR (posted by the `summary` job in
`ci.yml`); treat it as authoritative, not raw job colors:

- **Clippy is a real signal** (issue #1 debt cleared 2026-09-03): a red `clippy (known-red
  debt)` job means the PR itself introduced warnings — fix before merging. The job's
  `continue-on-error` is historical and stays only until the ruleset requires the check.
- **Expected flaky — rerun once**: tests on the known-flaky list (`docs/WORKING_RULES.md`) may fail once;
  the sim/e2e/fast-tier/server-integration steps auto-rerun once (the windows leg's
  fast-hermetic and server-integration steps included, issue #141), and a second failure
  is a regression.
  A summary line reading `real red (already rerun once): <step>` means the failing step
  already survived its built-in retry: rerun by hand, don't treat it as a flake.
- **Real red**: anything else — investigate before merging.

## Governance and releases

- The **GitHub mirror's** `main` is protected by a ruleset — live shape, verified 2026-09-30:
  `pull_request` (0 required approvals), `deletion`, `non_fast_forward`, `required_linear_history`,
  and **no required status checks**; the repository-admin role carries `bypass_mode: always`, which is
  what lets a release push `main` directly. The canonical `main` lives in walgit and is moved by a
  signed collab `merge_result` plus a direct push to `origin`; GitHub's copy follows by hand
  (`git push github main`, D57).
- Design decisions and Q&A: historically GitHub **Discussions** (Announcements/General/
  Ideas/Polls/Q&A/Show and tell); discussions are disabled on the mirror now — put the "why"
  in the wgl collab thread (`comment` entries) instead.
- Vulnerabilities: private reporting (SECURITY.md) + Dependabot security updates +
  CodeQL scanning on PRs (results in the Security tab).
- Releases: tag `v*` triggers `release.yml` (builds the linux `.deb`, the Windows
  installer, and the signed/notarized macOS DMG, changelog from Conventional
  Commits, attaches to `gh release`; `workflow_dispatch` runs the same packaging
  path as a signing dry run and skips publication; re-pushing an existing tag
  re-attaches fresh artefacts). One installer per platform, no raw binaries
  (issue #108). Versioning is semantic; milestone `v0.1` is the first release
  target. A release is also a work unit: create a `batch` issue with the release
  checklist.

- **D67** **Agents are told one interface: MCP. The HTTP API stays, undocumented in SKILL.md
  (2026-11-01, cc-ai-mcp-only-interface).** The host keeps its JSON API (`/{o}/{r}/api/…`) — it is
  the browser's lane, the SDK's lane, and what `/mcp` projects (D64) — but the agent-facing documents
  (`web/SKILL.md`, `skills/walgit/SKILL.md`) no longer describe it as a way to *interact*: no
  endpoint paths, no query recipes, no "POST to …" instructions. An agent reads, writes, watches and
  reports over `/mcp`; `git` moves bytes; the `walgit` binary is for what a client cannot do from
  outside — minting an identity, signing an entry when no signer is available, the fold, host
  operations. Both SKILL.md files therefore state the same thing at their head: *git + MCP are the
  interfaces, the CLI is for local/privileged acts*. The normative protocol docs (`docs/D1_PROTOCOL.md`,
  `docs/D1_CI_PROTOCOL.md`), `web/API.md` and the SPA's API page keep documenting the HTTP surface —
  "undocumented in the agent guide" is a documentation boundary, never a deprecation, and no endpoint
  may be removed on the strength of this decision: an MCP tool is a projection of an endpoint that
  must keep working for browsers, edges and scripts. The one CLI group kept in `web/SKILL.md` in
  fact-shaped form is `walgit repo …`, for a shell or CI job that is not an MCP client, and it is
  explicitly "same answers, prefer MCP".
