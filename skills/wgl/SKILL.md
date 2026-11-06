---
name: wgl
description: "Drive the D1 collaboration layer of a walgit host from the command line or over MCP: issues/status/reviews/patches/board/claims over refs/collab/*, the host-global principal registry, and the decentralized CI runner. Use when the user wants to file or move cards, claim work, review/approve, fold the inbox (gc), watch for collab events, register a principal or key, run .walgit/ci.toml tasks, or expose the collaboration surface to an MCP client — commands wgl collab …, wgl principal …, wgl ci …, wgl mcp (D69/D70)."
metadata:
  requires:
    bins: ["git", "wgl"]
  platform: "macOS / Linux / Windows"
---

# wgl — the D1 collaboration client

## 0. Install (prebuilt binaries)

Each release ships the `wgl` client for every supported platform. The asset
names carry no version, so `releases/latest/download` is stable:

```sh
# Linux (static musl; x86_64 or aarch64)
curl -fsSL https://github.com/gqf2008/walgit-d1/releases/latest/download/wgl-x86_64-linux.tar.gz | tar -xz -C ~/.local/bin wgl

# macOS (aarch64 or x86_64) — signed and notarized by Apple; Gatekeeper
# checks the notarization ticket online on first launch.
curl -fsSL -o wgl-macos.zip https://github.com/gqf2008/walgit-d1/releases/latest/download/wgl-aarch64-macos.zip && unzip -o wgl-macos.zip -d ~/.local/bin

# Windows (x86_64): unzip wgl-x86_64-windows.zip and put wgl.exe on PATH
```

The distro packages (`.deb`, the Windows installer, the macOS DMG) also carry
`wgl` next to `walgit` — installing the host's package is enough.
Every asset lives at <https://github.com/gqf2008/walgit-d1/releases/latest>; `wgl --version` names the build.

`wgl` is the standalone client for walgit's D1 collaboration layer
(`docs/D1_PROTOCOL.md`). The server hosts `refs/collab/*` like any ref;
**every command below reads those refs locally and verifies Ed25519
signatures offline** — writes are local refs pushed over plain git
transport, so there is no collaboration API call in the write lane.

Identity: one principal + key **per worktree** (`<git-dir>/walgit/keys/`,
0600; D58/D59/D62). Key files are 32 raw seed bytes as hex text; `--key`
takes a file path, never the key material.

## 1. Read (offline aggregation over the local refs)

```bash
git fetch origin '+refs/collab/inbox/*:refs/collab/inbox/*' '+refs/collab/meta/*:refs/collab/meta/*'
wgl collab ls                        # thread ids
wgl collab thread <id>               # chain-ordered entries + per-entry verification (JSON)
wgl collab thread-heads              # thread id -> tip oid
wgl collab pr <id>                   # PR view + merge-rule verdict (JSON)
wgl collab board                     # the work-unit board (text|markdown|json|hash)
wgl collab report                    # threads, PRs, verification health, CI runs (text|markdown|html)
```

The board definition is `.walgit/board.toml` at HEAD (`--board <file>`
previews an uncommitted one). Moving a card is an ordinary signed `status`
entry; the projection re-derives the columns.

## 2. Write (local ref + push)

```bash
wgl collab join --principal alice --push origin     # create this worktree's identity + register
wgl collab entry --kind issue --id cc-topic --actor alice \
    --body '{"title":"the card"}' --push origin     # prints "<ref> <oid>"; chain --parent <oid>
wgl collab entry --kind status --id cc-topic --parent <oid> \
    --body '{"status":"in-progress","worktree":"wt","branch":"feat/x"}' --push origin
wgl collab principal-register --principal alice --key <keyfile> --push origin   # rotate (destructive)
wgl collab principal-revoke  --principal alice --push origin                     # tombstone
wgl collab gc --actor alice --push origin            # fold the inbox into the signed snapshot (§9)
```

Write-side gates the server does not have: `status=done` needs the thread at
`needs-review` with a verified non-author approve (§7.5); a signature that
does not match the actor's registered key is refused before it enters the
append-only log. `--auto-fold` opportunistically folds past a threshold.

## 3. Claim a card (§8.4 — read → publish → re-read)

```bash
wgl collab claim --id <card> --worktree wt-x --branch feat/x --work 'plan' [--ttl 300]
echo $?   # 0 = you hold it, start work; 2 = stand down (another live claim), do NOT start; 1 = error
```

Duplicate claims are legal, duplicate execution is not. `--session <id>`
(else `KI_AGENT_ID`/`WALGIT_SESSION`) enables crash recovery scoped to one
session.

## 4. Watch (pull-only, §11.1)

```bash
wgl collab watch --remote origin --interval 10 [--once] [--exec '<cmd>']
```

Each new/changed ref runs `--exec` once with the raw entry JSON on stdin and
`WALGIT_COLLAB_{REF,KIND,THREAD,ACTOR,VERIFIED}` in the environment; a
failing `--exec` ends the watcher without advancing the state file
(at-least-once).

## 5. Principal registry (host-global, §4.4)

```bash
wgl principal register|rotate --url http://host:8081 --principal alice --key <file> --token "$WALGIT_TOKEN"
wgl principal list            --url http://host:8081
wgl principal revoke          --url http://host:8081 --principal alice --token ...
wgl collab principal-fetch [--remote origin]        # cache the host registry under refs/walgit/principals/*
```

Repo-local registrations (`refs/collab/meta/principals/*`) always override
the host registry.

## 6. CI runner (`docs/D1_CI_PROTOCOL.md`)

```bash
wgl ci validate                       # .walgit/ci.toml shape-check (V1–V11)
wgl ci run --actor ci-runner --key <file> [--once] [--interval 15] [--task <name>]
wgl ci status [--format text|markdown|json]
wgl ci log [--run ci-…]               # the retained log blob, fetched on demand
wgl ci artifacts --out dir [--run …]  # sha256-verified downloads
```

The runner polls ref tips, claims runs with signed `ci_claim` entries
(earliest-claimant convergence, TTL re-claim), executes each task in a
detached worktree with a cleared environment (`env_allow` + `WALGIT_CI_*`),
captures the output tail, uploads log/artifact blobs **before** the result
that references them, and publishes a signed `ci_result`.

## 7. MCP (stdio) — the collaboration surface for agents (D70)

The host's `/mcp` no longer carries the D1 tools or `walgit://collab/*`
resources; `wgl mcp` serves them over **stdio JSON-RPC** against a checkout's
own `refs/collab/*` (D70). stdout carries one JSON-RPC message per line and
nothing else; logs go to stderr; stdin EOF exits 0. Requests are handled
serially (one worktree = one agent lane = one process).

```sh
wgl mcp --repo <checkout> [--remote origin] \
        [--no-fetch] [--subscribe-interval 5] [--max-subscriptions 32]
```

`initialize`, `tools/list`, `tools/call`, `resources/list`,
`resources/templates/list`, `resources/read`, `resources/subscribe`,
`resources/unsubscribe` and `ping` are implemented; notifications are written
unprompted. A tool that ran and failed answers `isError: true` with a readable
message (a bad request is JSON-RPC `-32602`); a failed fetch or push is a tool
error, never a process exit. Ref fetches and pushes are bounded at two minutes:
a timed-out transfer is that same tool error.

### Tools

| tool | arguments | answer |
|---|---|---|
| `collab_board` | `fresh?` | the board projection (`.walgit/board.toml` at HEAD) |
| `collab_thread` | `id`, `fresh?` | `{id, entries:[{oid, principal, verified, entry, broken_refs}], pr}` |
| `collab_report` | `fresh?` | threads, PRs, CI runs, verification health |
| `ci_status` | `id?`, `fresh?` | `{repo, runs}` (the host's `/collab/ci` shape) |
| `collab_publish` | `entry` (object or JSON text) | `{ref, oid}` |
| `collab_register` | none | `{ref, oid}` |

**Freshness.** Every read fetches `refs/collab/{inbox,meta}` first
(`git fetch --prune`, the same lane `wgl collab claim` step 1 uses) and **fails
closed** if it cannot: a stale board is how two workers duplicate a card.
Pass `fresh:false` to read the cached refs; the result's `_meta` carries
`fresh` and `fetched_at` (the last successful fetch, absent if none). Those
fields never enter the body being compared.

**Caller.** The principal is `<git-dir>/walgit/identity` — one per worktree
(D58/D59/D62). `collab_publish` refuses an entry whose `actor` is not that
principal; `collab_register` refuses when there is no identity. `wgl mcp`
never signs for you and holds no key: build and sign with `wgl collab entry`
(or any ed25519 signer over the canonical bytes, `docs/D1_PROTOCOL.md` §5.3).

`collab_publish` checks the entry's structure (`kind`, `id`, `actor`, an
`ed25519:` + 64-byte signature, body object) and, for `status=done`, fetches
and applies the §7.5 gate (needs-review + a verified non-author approve) —
the same rule as `wgl collab entry`. It does **not** verify the signature on
write (an entry and the key registration may land in either order, and the git
lane cannot check it); aggregation verifies. Re-read the thread and require
`verified: true`. A raw `git push` bypasses every local check (§7.5): these are
discipline, not a security boundary; the write gate is `policy.json` on
receive-pack.

### Resources

- `walgit://collab/board` — board JSON; `_meta.version` is
  `b:<witness>[:claim:<expires_at>]` (the cards plus the earliest still-live
  claim, so a claim TTL lapse wakes you once).
- `walgit://collab/thread/<id>` — thread JSON; `_meta.version` is `h:<tip>`.

`resources/read` follows the same fresh policy (and `--no-fetch` applies
process-wide). `resources/subscribe` records the current version and a
background poller emits `notifications/resources/updated` with the `uri` when
it moves — best-effort, in-process, never a durable push (D55). The poller
fetches once per tick when freshness is on; an update means **re-read**, never
"here is the diff". `resources/unsubscribe`, stdin EOF and a failing version
read all clean up the subscription (`list_changed` for the last case). The cap
is `--max-subscriptions` (a second `subscribe` beyond it is `-32602`), and the
tick is `--subscribe-interval` (>= 1 s, default 5 s).

### Error contract

- stdout: JSON-RPC only; diagnostics and git stderr go to stderr.
- `-32700` unparsable line, `-32600` not a request, `-32601` unknown method,
  `-32602` bad params/unknown argument, `-32603` a failed read (including a
  fetch failure).
- `tools/call` execution failures (`fetch failed`, `push refused`, unknown
  thread, rejected `done` transition) are `result.isError: true` — a model
  should read and act on the message.
- stdin EOF is a clean exit(0) after the poller stops.

## Failure handbook

- **"no project identity … run `wgl collab join`"** — the checkout has no
  per-worktree identity yet.
- **"cannot sign as actor …"** — the key is not the one registered for that
  actor; sign as the right principal or rotate deliberately.
- **`git push` lease failures on gc** — a concurrent folder moved the
  snapshot; re-fetch and re-run (idempotent).
- **stand-down (exit 2)** is not an error — the card is held; do not start.
