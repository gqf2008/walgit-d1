---
name: wgl
description: "Drive the D1 collaboration layer of a walgit host from the command line: issues/status/reviews/patches/board/claims over refs/collab/*, the host-global principal registry, and the decentralized CI runner. Use when the user wants to file or move cards, claim work, review/approve, fold the inbox (gc), watch for collab events, register a principal or key, or run .walgit/ci.toml tasks — commands wgl collab …, wgl principal …, wgl ci … (D69)."
metadata:
  requires:
    bins: ["git", "wgl"]
  platform: "macOS / Linux / Windows"
---

# wgl — the D1 collaboration client

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

## Failure handbook

- **"no project identity … run `wgl collab join`"** — the checkout has no
  per-worktree identity yet.
- **"cannot sign as actor …"** — the key is not the one registered for that
  actor; sign as the right principal or rotate deliberately.
- **`git push` lease failures on gc** — a concurrent folder moved the
  snapshot; re-fetch and re-run (idempotent).
- **stand-down (exit 2)** is not an error — the card is held; do not start.
