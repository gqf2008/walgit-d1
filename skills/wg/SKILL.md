---
name: wg
description: "Drive the D1 collaboration layer of a walgit host from the command line or over MCP: issues/status/reviews/patches/board/claims over refs/collab/*, the host-global principal registry, and the decentralized CI runner. Use when the user wants to file or move cards, claim work, review/approve, fold the inbox (gc), watch for collab events, register a principal or key, run .walgit/ci.toml tasks, or expose the collaboration surface to an MCP client — commands wg collab …, wg principal …, wg ci …, wg mcp (D69/D70)."
metadata:
  requires:
    bins: ["git", "wg"]
  platform: "macOS / Linux / Windows"
---

# wg — the D1 collaboration client

## 0. Install (prebuilt binaries)

Each release ships the `wg` client for every supported platform. The asset
names carry no version, so `releases/latest/download` is stable:

```sh
# Linux (static musl; x86_64 or aarch64)
curl -fsSL https://github.com/gqf2008/walgit-d1/releases/latest/download/wg-x86_64-linux.tar.gz | tar -xz -C ~/.local/bin wg

# macOS (aarch64 or x86_64) — signed and notarized by Apple; Gatekeeper
# checks the notarization ticket online on first launch.
curl -fsSL -o wg-macos.zip https://github.com/gqf2008/walgit-d1/releases/latest/download/wg-aarch64-macos.zip && unzip -o wg-macos.zip -d ~/.local/bin

# macOS installer package (aarch64 or x86_64) — installs wg at
# /usr/local/bin/wg, which is already on the default PATH; signed and
# notarized by Apple.
curl -fsSL -o wg-macos.pkg https://github.com/gqf2008/walgit-d1/releases/latest/download/wg-aarch64-macos.pkg && sudo installer -pkg wg-macos.pkg -target /

# Windows (x86_64): unzip wg-x86_64-windows.zip and put wg.exe on PATH
```

Swap the `aarch64` token for `x86_64` on Intel Macs. The `.pkg` installs only
`wg` (into `/usr/local/bin`); the DMG, `.deb`, and Windows installer carry
`wg` next to `walgit`, so installing the host's package is enough.
Every asset lives at <https://github.com/gqf2008/walgit-d1/releases/latest>; `wg --version` names the build.

`wg` is the standalone client for walgit's D1 collaboration layer
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
wg collab ls                        # thread ids
wg collab thread <id>               # chain-ordered entries + per-entry verification (JSON)
wg collab thread-heads              # thread id -> tip oid
wg collab pr <id>                   # PR view + merge-rule verdict (JSON)
wg collab board                     # the work-unit board (text|markdown|json|hash)
wg collab report                    # threads, PRs, verification health, CI runs (text|markdown|html)
```

The board definition is `.walgit/board.toml` at HEAD (`--board <file>`
previews an uncommitted one). Moving a card is an ordinary signed `status`
entry; the projection re-derives the columns.

## 2. Write (local ref + push)

```bash
wg collab join --principal alice --push origin     # create this worktree's identity + register
wg collab entry --kind issue --id cc-topic --actor alice \
    --body '{"title":"the card"}' --push origin     # prints "<ref> <oid>"; chain --parent <oid>
wg collab entry --kind status --id cc-topic --parent <oid> \
    --body '{"status":"in-progress","worktree":"wt","branch":"feat/x"}' --push origin
wg collab principal-register --principal alice --key <keyfile> --push origin   # rotate (destructive)
wg collab principal-revoke  --principal alice --push origin                     # tombstone
wg collab gc --actor alice --push origin            # fold the inbox into the signed snapshot (§9)
```

Write-side gates the server does not have: `status=done` needs the thread at
`needs-review` with a verified non-author approve (§7.5); a signature that
does not match the actor's registered key is refused before it enters the
append-only log. `--auto-fold` opportunistically folds past a threshold.

## 3. Claim a card (§8.4 — read → publish → re-read)

```bash
wg collab claim --id <card> --worktree wt-x --branch feat/x --work 'plan' [--ttl 300]
echo $?   # 0 = you hold it, start work; 2 = stand down (another live claim), do NOT start; 1 = error
```

Duplicate claims are legal, duplicate execution is not. `--session <id>`
(else `KI_AGENT_ID`/`WALGIT_SESSION`) enables crash recovery scoped to one
session.

## 4. Watch (pull-only, §11.1)

```bash
wg collab watch --remote origin --interval 10 [--once] [--exec '<cmd>']
```

Each new/changed ref runs `--exec` once with the raw entry JSON on stdin and
`WALGIT_COLLAB_{REF,KIND,THREAD,ACTOR,VERIFIED}` in the environment; a
failing `--exec` ends the watcher without advancing the state file
(at-least-once).

## 5. Principal registry (host-global, §4.4)

```bash
wg principal register|rotate --url http://host:8081 --principal alice --key <file> --token "$WALGIT_TOKEN"
wg principal list            --url http://host:8081
wg principal revoke          --url http://host:8081 --principal alice --token ...
wg collab principal-fetch [--remote origin]        # cache the host registry under refs/walgit/principals/*
```

Repo-local registrations (`refs/collab/meta/principals/*`) always override
the host registry.

## 6. CI runner (`docs/D1_CI_PROTOCOL.md`)

```bash
wg ci validate                       # .walgit/ci.toml shape-check (V1–V11)
wg ci run --actor ci-runner --key <file> [--once] [--interval 15] [--task <name>]
wg ci status [--format text|markdown|json]
wg ci log [--run ci-…]               # the retained log blob, fetched on demand
wg ci artifacts --out dir [--run …]  # sha256-verified downloads
```

The runner polls ref tips, claims runs with signed `ci_claim` entries
(earliest-claimant convergence, TTL re-claim), executes each task in a
detached worktree with a cleared environment (`env_allow` + `WALGIT_CI_*`),
captures the output tail, uploads log/artifact blobs **before** the result
that references them, and publishes a signed `ci_result`.

## 7. MCP (stdio) — the collaboration surface for agents (D70)

The host serves no MCP endpoint (D72; it once carried the D1 tools and
`walgit://collab/*` resources, D64/D70). `wg mcp` serves them over **stdio
JSON-RPC** against a checkout's own `refs/collab/*` (D70). stdout carries one JSON-RPC message per line and
nothing else; logs go to stderr; stdin EOF exits 0. Requests are handled
serially (one worktree = one agent lane = one process).

```sh
wg mcp --repo <checkout> [--remote origin] \
        [--no-fetch] [--subscribe-interval 5] [--max-subscriptions 32]
```

`initialize`, `tools/list`, `tools/call`, `resources/list`,
`resources/templates/list`, `resources/read`, `resources/subscribe`,
`resources/unsubscribe` and `ping` are implemented; notifications are written
unprompted. A tool that ran and failed answers `isError: true` with a readable
message (a bad request is JSON-RPC `-32602`); a failed fetch or push is a tool
error, never a process exit. The MCP read fetch and a ref push are bounded at
two minutes: a timed-out transfer is that same tool error. Other wg commands
that push large blobs (`collab gc`'s snapshot, CI logs/artifacts) scale the
bound with the payload size (`gitx.pushTimeoutSeconds`: 120 s plus 1 s per
64 KiB), so a slow uplink does not turn a large fold into a failure.

### Tools

| tool | arguments | answer |
|---|---|---|
| `collab_board` | `fresh?` | the board projection (`.walgit/board.toml` at HEAD) |
| `collab_thread` | `id`, `fresh?` | `{id, entries:[{oid, principal, verified, entry, broken_refs}], pr}` |
| `collab_report` | `fresh?` | threads, PRs, CI runs, verification health |
| `ci_status` | `id?`, `fresh?` | `{repo, runs}` (the host's `/collab/ci` shape) |
| `collab_publish` | `entry` (object or JSON text) | `{ref, oid}` |
| `collab_register` | `force?` | `{ref, oid}` |

**Freshness.** Every read fetches `refs/collab/{inbox,meta}` first
(`git fetch --prune`, the same lane `wg collab claim` step 1 uses) and **fails
closed** if it cannot: a stale board is how two workers duplicate a card.
Pass `fresh:false` to read the cached refs; the result's `_meta` carries
`fresh` and `fetched_at` (the last successful fetch, absent if none). Those
fields never enter the body being compared. `--no-fetch` is a **hard gate**:
with it, no per-call `fresh:true` re-enables the network (an offline/air-gapped
process stays offline).

**Caller.** The principal is `<git-dir>/walgit/identity` — one per worktree
(D58/D59/D62). `collab_publish` refuses an entry whose `actor` is not that
principal; `collab_register` refuses when there is no identity. It also has
the `wg collab join` rotation guard: if the registry names a *different* key
for this principal, it refuses unless `force:true` — rotating makes every past
entry by that principal unverified, and the log is append-only. Prefer an
explicit `wg collab principal-register` for a real rotation. `wg mcp`
never signs for you and holds no key: build and sign with `wg collab entry`
(or any ed25519 signer over the canonical bytes, `docs/D1_PROTOCOL.md` §5.3).

`collab_publish` parses the entry the way the server's write path does
(`serde_json::from_value::<Entry>`: required/typed fields, `refs` object|null,
`version` in `u32` range, unknown fields ignored) and additionally requires an
`ed25519:` + 64-byte signature and the caller's own `actor`. For
`status=done` it fetches and applies the §7.5 gate (needs-review + a verified
non-author approve) — the same rule as `wg collab entry`. When `entry` is the
JSON **text**, those exact bytes become the blob, so the returned `oid` equals
the one `wg collab entry` wrote (no duplicate entry, and references to the
CLI's oid stay valid); an object argument is serialized compactly. It does
**not** verify the signature on
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

Thread ids in resource URIs are percent-decoded (`cc%2Dai` = `cc-ai`); an
invalid escape is not that resource. The local resource body has no WAL `seq`
(there is no WAL in the checkout) — the host's body carries one; compare the
rest. `resources/read` follows the same fresh policy (and `--no-fetch` is the
process-wide hard gate). `resources/subscribe` records the current version and a
background poller emits `notifications/resources/updated` with the `uri` when
it moves — best-effort, in-process, never a durable push (D55). The poller
fetches once per tick when freshness is on and does no refs work when nothing
is subscribed; an update means **re-read**, never "here is the diff". `resources/unsubscribe`, stdin EOF and a failing version
read all clean up the subscription (`list_changed` for the last case). The cap
is `--max-subscriptions` (a second `subscribe` beyond it is `-32602`), and the
tick is `--subscribe-interval` (>= 1 s, default 5 s).

**Verification and host-only keys.** `verified` (and therefore the `done`
gate) is computed from this checkout's `refs/walgit/principals/*` cache plus
`refs/collab/meta/principals/*`. A key registered only in the *host* registry
is invisible here until `wg collab principal-fetch`; if a verdict disagrees
with the host's aggregation, run that first.

### Error contract

- stdout: JSON-RPC only; diagnostics and git stderr go to stderr.
- One request line may be up to 2 MiB (the entry cap plus envelope/escaping);
  a longer line is drained, answered with one `-32700`, and the process keeps
  serving.
- `-32700` unparsable line, `-32600` not a request, `-32601` unknown method,
  `-32602` bad params/unknown argument, `-32603` a failed read (including a
  fetch failure).
- `tools/call` execution failures (`fetch failed`, `push refused`, unknown
  thread, rejected `done` transition) are `result.isError: true` — a model
  should read and act on the message.
- stdin EOF is a clean exit(0) after the poller stops.

## Failure handbook

- **"no project identity … run `wg collab join`"** — the checkout has no
  per-worktree identity yet.
- **"cannot sign as actor …"** — the key is not the one registered for that
  actor; sign as the right principal or rotate deliberately.
- **`git push` lease failures on gc** — a concurrent folder moved the
  snapshot; re-fetch and re-run (idempotent).
- **stand-down (exit 2)** is not an error — the card is held; do not start.
