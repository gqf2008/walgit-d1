# walgit — architecture: constraints, the WAL, principles

> Split out of `AGENTS.md` on 2026-11-05. `AGENTS.md` is now the generic collaboration framework and `docs/CHARTER.md` the project-specific
> charter; this file is the system manual. Section numbers §1–§3 are preserved
> verbatim, so a reference like `docs/ARCHITECTURE.md §2.3` resolves to the
> same text it always did. Decision ids (`D1`…`D68`) live in
> `docs/DECISIONS.md`; working rules in `docs/WORKING_RULES.md`; where the
> repository lives and how releases ship in `docs/RELEASE.md`.

## 1. Constraints we design for

### 1.1 The machine: small, ephemeral, shared-nothing
- Assume an instance of **a few vCPU and tens of GiB of memory where the disk *is* that memory** (tmpfs).
  `cache.max_bytes` (default 20 GiB) is the budget for everything on disk. A monorepo's base pack alone can be
  30+ GB: **it cannot be materialized there, ever.** A host with a real SSD (`cache.mode = "disk"`) is the
  exception, placed by configuration, not the rule.
- **Instances are ephemeral and shared-nothing**: no stable identity, no node-to-node networking, no gossip.
  Explicit placement (D30) decides which hosts perform a repository's object work; refs-level reads remain
  available everywhere, including during a deploy.
- **CPU may be throttled between requests** on serverless platforms; background work there must be bounded
  (`wal.prefetch_max_bytes`).
- **Object store facts**: ~60–80 ms per GET, ~100 MB/s per connection (stripe for more), conditional GET ~15 ms;
  same-object overwrite is serialized (~1 write/s) — a single CAS'd object is a throughput cap.
- Operations that need real disk or hours of CPU (a full `repack -adb` of a monorepo, building its 30 GB weekly
  bundle) run on a host with an SSD using the same binary and the same WAL/lease protocol (`walgit compact
  --base`, `walgit import`, the `maintain` role with `maintenance.disk = "ssd"`).

### 1.2 What follows
- **Never fully materialize a big repo on a small instance.** Serve from the parts that fit: refs from the WAL
  (ref snapshot + log tail), pack *indexes* + bitmaps + commit-graph locally, pack *data* by range read from the
  bucket (remote reader) or a read-only bucket mount, recent small packs locally. Clone bytes come from static
  bundles, never through upload-pack.
- **An instance must become useful in seconds**, cold: refs in < 1 s (one manifest GET + snapshot + tail),
  objects for the web UI within the first request (remote reader), fetch remainders from local small packs.
- **Everything an instance computes that is immutable is cached for everyone**: in-process LRU and, where a
  second instance would otherwise recompute, the bucket itself (`cache/api/v1/*.json`). Wiping every instance
  loses nothing but warmth.
- **No silent waiting.** Long work is a *task* (id, log, lock, attachable stream) and is narrated to the
  client: SSE envelope for the web UI, sideband band-2 lines for git. "Cloning into… and then nothing" is a bug.

### 1.3 Security contract (`Config::validate` fails closed)
- Three auth modes (`server.auth.mode`): **`none`** (everyone is `anon` with write and admin — `validate` refuses unless `server.listen` is loopback),
  **`token`** (static tokens from the config, as `Authorization: Bearer` or an HTTP Basic password), **`oidc`**
  (any OpenID Connect issuer via discovery). In `oidc` mode `anonymous_read` must be false and an allowlist
  (`allowed_domains`/`allowed_emails`) must exist; three credentials are accepted — an ID token from the issuer
  (RS256/ES256, `iss`, `exp`, `aud` ∈ `audiences` ∪ {`oauth_client_id`}, `email_verified`), a **walgit access
  token** (`wgt_…`, HMAC-signed with `session_secret`, minted at `/_auth/tokens` by a signed-in browser, stateless,
  `access_token_ttl`; rotating the secret revokes all), and the HMAC **session cookie** set by `/_auth/login` →
  issuer → `/_auth/callback`. Static `tokens` work in `oidc` mode too (robots). Every path ends in the same
  allowlist and `write_domains`.
- Open at the application (no credential): `/healthz`, `/readyz`, `/repos.js`, `/repos.mjs`, `/SKILL.md`
  (D42, the AI-agent guide), `/_auth/*` (the
  sign-in flow itself) and **`/services/public/*`** (data-free; today `install.sh`, `ca.pem` and the shipped
  ops skill `skill/{SKILL.md,manifest.json,install.sh}` (D47); everything else
  under it 404; never reads repo data or takes a bearer — test `public_lane_serves_only_the_installer_without_auth`).
- **The server answers an invalid/expired credential with a real 401** — that is what makes git `erase` it from
  its helpers and ask again; the friendly 200 + in-band ERR is reserved for failures a retry cannot fix (account
  not allowed, verifier down). A 200 leaves git re-storing a dead token for its cache's lifetime. A **missing**
  credential is the same real 401 (git sends the credentials it holds — URL userinfo, helpers — only after a 401
  that offers Basic; a 403 makes a fresh push give up), while an authenticated identity that is not allowed stays
  403 (issue #79). The challenge is **channel-scoped**: git smart HTTP, LFS and bundle downloads answer
  `WWW-Authenticate: Basic …, Bearer …`; every browser-reachable surface answers `Bearer` only — a `Basic`
  challenge on a navigation or credentialed fetch pops the browser's native password dialog.
- **An edge announces what it took over, per request, in `X-Walgit-Capabilities`** (D39): `client-authorization`
  (the client's bearer travels in `X-Walgit-Authorization`; `Authorization` is the hop's own credential and is
  never read as the client's) and `accel-redirect` (static bytes by `X-Accel-Redirect`, honoured only when
  `server.accel_redirect = true` **and the TCP peer is loopback**). Hit directly, nothing is assumed.
- **The installer** (`crates/walgit-server/src/setup.rs`, POSIX sh, idempotent): git ≥ 2.46 + curl; pins a
  self-signed host's CA; takes the token from `$WALGIT_TOKEN`, an already stored one, or the terminal (no terminal:
  exit 2 with the two things to do); writes `~/.config/git/<host>-token` (0600) and the credential helper
  `<host>-credential-helper` (`get` → `authtype=Bearer`; `store` keeps what git hands it; `erase` on a 401 deletes
  the token and names `/_auth/tokens`); sets exactly `credential.https://<host>.helper` = `""` then ours,
  `transfer.bundleURI true`, `fetch.uriProtocols https`; unsets stale `fetch.bundleURI`/`extraHeader`; self-tests
  (`/api/v1/me`, or `ls-remote` of `?repo=`). Recipes come from one place (`setup::Recipes`,
  `/services/setup.json`); the Clone menu, API page, overview and git error help render them.
- **Bundle lists, two of them (D41)**: `bundles/list` is the *clone* list (fulls + chain; advertised in v2);
  **`bundles/catchup`** is the same list without the fulls and is what every recipe records in `fetch.bundleURI`
  — git's creationToken walk would otherwise download every full newer than the client's token.
  Blobless: `…/list?filter=blob:none` / `…/catchup?filter=blob:none`.
- **Host to host**: a front that forwards pushes to a broker presents `wal.push_broker_token` (or
  `WALGIT_BROKER_TOKEN`); the broker lists it in `tokens` and its principal in `trusted_forwarders`, so the end
  user travels in `X-Walgit-Principal`.
- Tests never write the user's global git config (private `GIT_CONFIG_GLOBAL`, `tests/lib-auth.sh`).

### 1.4 Requirements (the bar)
- Full git surface: smart HTTP v0/v2, ls-refs with prefixes, fetch with filter/shallow/deepen/sideband-all,
  receive-pack (atomic, delete, tags, push options, report-status-v2), bundle-uri (v2 command + static list),
  LFS batch/basic transfer, `<owner>/<repo>` namespaces. Every read on every instance is as fresh as a fetch:
  push acknowledged ⇒ the next request anywhere sees it.
- Cost must not scale with ref count on any hot path (O(1) `refs`, O(k) `resolve`, paged ref lists, prefix
  filtered advertisements) nor with pack size on a too-small instance.
- Clones of big repos are **static**: the server hands out URLs, the bucket/CDN moves the bytes. `bundles.require`
  rejects clones that skip bundle-uri, with the exact fix in the error text.
- Acceptance for the monorepo: cold instance serves `info/refs`/ls-refs/web `refs` in < 1 s; web UI renders
  main's tree/blob/commits without packs on disk; `git clone` (bundle-uri) + `git fetch` work against a tmpfs
  host; `clone --filter=blob:none --depth=1 --sparse --single-branch` in seconds (reference: 2075 s → 8 s).
  Bounded/CI clones must pass `-c transfer.bundleURI=false`: git downloads the advertised full bundle first
  otherwise (the server cannot see the filter at bundle-uri time).

---

## 2. The WAL — source of truth, and how we squeeze value out of it

### 2.1 Objects and the commit point (`repos/<owner>/<repo>/…`)
| Object | Role |
|---|---|
| `manifest.pb` | Tiny, **CAS-rewritten**: `head_seq`, live pack set `PackRef[]` (checksum, sizes, tier, has_rev/bitmap), log segments, checkpoint pointer, settings inline, `revision`, `updated_at`. **The linearization point.** Nothing is visible before its CAS; everything after is idempotent and replayable. |
| `log/<first_seq>.pb` | Immutable, uvarint-framed `LogEntry` frames: PUSH / REF_UPDATE (ref transaction + pack pointer), COMPACT (new pack, `supersedes[]`), CHECKPOINT, SETTINGS. Strictly increasing `seq`. One small object per publish batch. |
| `wal/<checksum>.pack/.idx/.rev/.bitmap/.commit-graph` | Immutable packs, content-addressed by pack checksum: push packs (tier 0), compaction outputs (tier 1), the base (tier 2, bitmap'd), plus the side-files a reader needs. A pack a COMPACT entry dropped also carries a marker under `wal/_superseded/<checksum>` (when it left the live set; its own prefix so GC's listing never walks pack objects) — the manifest keeps only the live set, and the superseding entry is eventually folded into a checkpoint, so the marker is what lets bucket GC age a pack. |
| `checkpoints/<seq>/checkpoint.pb`, `refs.pb` | Folded state at `seq`: live pack set + full `RefSnapshot`. Cold start = snapshot + tail, never full replay. |
| `bundles/list.pb`, `bundles/<strategy>/…` | bundle-uri artefacts + CAS'd list. |
| `leases/<name>.pb` | CAS lease with TTL heartbeat: `compact`, `bundle:<strategy>`, `gc` (the GC pass renews it for its whole run; `ReclaimingPack` carries the listing pass's `owner`+`token` fence, `since`, and an absolute `fence_until`; every destructive step re-checks the fence, and a later pass only recovers *another* owner's claim once `fence_until` has passed — legacy claims without it fall back to `since + max(3×lease_ttl, 5min)`). The only cross-instance mutex. |
| `cache/api/v1/<sha1>.json` | Shared render cache of immutable web API answers. |
| `policy.json` | Per-repo push policy (rule language, not on the WAL). `docs/POLICY.md`. Missing = allow-all. |
| `fsck.pb` | Last connectivity audit (`FsckReport`), written by the maintainer's `fsck` unit, consumed by `repair` (`docs/INTEGRITY.md`). |
| `gc.pb` | Last bucket-GC pass (`GcReport`: when, how many packs, how many bytes) — the maintainer's plan reads it to decide whether the `gc` unit is due (`maintenance.gc_interval`). |
| `lfs/objects/<aa>/<bb>/<oid>` | LFS objects (sha256-addressed, immutable). Missing ones can be read through from `upstream.lfs` and persisted (`docs/LFS.md`). |
Schema `crates/walgit-proto/proto/walgit/v1/wal.proto`; GCS (REST/JSON API data, gRPC metadata —
the "gRPC" blanket label was the #133 audit's issue #135 fix), S3 (AWS SDK) and in-memory stores share
one contract suite (`crates/walgit-store/tests/contract.rs`, incl. compose).

### 2.2 Write path
receive-pack (ours, `walgit-git/src/receive.rs`) → index the pack locally (`git index-pack --stdin --fix-thin
--keep --rev-index --threads=0`, `--fsck-objects` when `wal.fsck_objects`) in a per-ingest scratch git dir (a
rejected push leaves nothing behind) → connectivity per config (`spawn_blocking`) → **push policy**
(`docs/POLICY.md`) → `pack PUT ∥ idx PUT ∥ log PUT` → **manifest CAS** (group commit per repo per instance,
`wal.batch_window`) → commit local ref txn → `ok` to the client. On 412: refetch, re-validate every old value
(moved ref ⇒ `ng`), re-seq, retry with jittered backoff. **Who writes (D28): the host that maintains a repo.**
Hosts that maintain nothing may forward receive-pack to a **push broker** (`wal.push_broker_url`) so one warm
writer batches the CAS; fallback to the local path if the broker is down. Publish is CAS-safe, so disjoint writer
sets are correct by construction; the broker is an optimization, never a dependency. Never ACK before the bucket
ACKs. **Upstream follow** (`walgit_server::follow`, D33) is the second writer shape: refs in a repo's `[upstream]
follow` are brought up to `upstream.git`'s by the maintaining host every `maintenance.follow_interval` through the
same ingest → connectivity → fast-forward → `publish_push` path, `principal = upstream`.

### 2.3 Read path — sync levels (`RepoHandle::sync_*`, `walgit-wal/src/handle.rs`, `sync.rs`)
Every request: conditional GET of `manifest.pb` (skippable for `wal.freshness_ttl`) → 304 serve / 200 apply.
| Level | Brings | Used by |
|---|---|---|
| **Refs** | checkpoint `RefSnapshot` + every log entry's ref txn → `packed-refs`. No packs. | `info/refs`, `ls-refs`, `bundle-uri`, web `refs`/`resolve`/overview, read_log |
| **Serve** (`sync()`) | Refs + the pack set as the instance can hold it: tiers < 2 and the **history pack** (D18) local; a tier-2 base as side-files (idx/rev/bitmap/commit-graph layer) + `pack-<sha>.pack` linked into a read-only bucket mount (`cache.store_mount`) or **remote-served** without one; midx over history + base | upload-pack, receive-pack, bundle builds, prewarm |
| **Full** (`sync_full()`) | Refs + every live pack local (striped parallel range downloads); only for repos that fit | base rebuilds (`compact --base`), geometric compaction |
| **Objects** | Serve when the pack set fits `cache.max_bytes`, else Refs + **remote reader** → `ObjectAccess::{Local,Remote}` | web API object endpoints |
Long syncs register as tasks (`materialize`, `remote-index`) and stream progress; pack work runs on the **bulk
runtime** and never takes the refs phase's lock (D19). `check_fits` refuses to pull a pack set that cannot fit
(TooLarge → 500 / pkt ERR with the bundle-uri fix; it is not retryable). Local disk is a bounded LRU (`cache.max_bytes`,
`evict_idle_after`) in budget mode, watermark eviction in disk mode (D25).

### 2.4 Getting the most out of the WAL (the strategies)
- **Refs-first everything.** Ref advertisement, peeled tags (`RefUpdate.new_peeled` recorded by the writer so
  replicas advertise `^{}` without objects), web `refs`/`resolve` — all from snapshot + tail, no objects.
- **Remote-served bases + gix fetch engine**: at Serve level with no store mount and a pack set that does not fit,
  tier-2 bases are remote-served (`PackPlan::Remote`: commit-graph layer local, nothing else); `git fetch` then
  runs the gix engine (`upload_gix.rs`): commit-graph walk, tree diff, base objects faulted by range, streamed.
- **Remote reader for too-large repos** (`walgit-wal/src/remote.rs`): pack indexes local, data by 1 MiB range
  reads through a process-wide block LRU (`cache.remote_block_bytes`), incremental inflate, iterative delta
  resolution with a decoded-object LRU. The web API faults exactly the objects a git command will touch into the
  loose store (`web/objects.rs`) and runs unchanged `git` renderers. The same contract test runs with a 1-byte cache.
- **Side-files published with packs**: `.idx`, `.rev`, `.bitmap`, and for tier-2 bases a **split commit-graph
  layer** readers install as their chain base. Every pack writer produces `.rev` (`pack.writeReverseIndex`; git
  ≥ 2.47 on the server); a published pack ≥ 250 k objects without one gets it from the maintainer (`rev-index`
  unit) — without it git rebuilds the reverse index in memory on every `pack-objects` (2.85 s per fetch on a
  60 M-object repository).
- **Checkpoints fold the log** when `wal.snapshot_every_entries` OR `wal.checkpoint_interval` OR
  `wal.checkpoint_tail_bytes` fires — the checkpoint is the unit of serving state. Writing one is refs-level work,
  so each `maintain` host checkpoints its own repositories.
- **Provenance for free**: every push and repack is a log entry; `walgit wal ls|show|materialize --at-seq`.
- **A checkpoint trims the manifest, never the bucket**: it drops folded segments from `log_segments` (the
  manifest stays tiny) while the objects live until `maintenance.retention_wal`, so a window below the listed
  tail is read with **one `LIST log/`** (`log_reader.rs::segments_below`; the segment name carries its first
  seq, so the listing bounds the candidates) and a segment retention already reclaimed is skipped, not fatal.
  The un-folded tail keeps the LIST-free fast path.
- **Never LIST on a hot path**; 404s are free; probe, don't list. Immutable objects get
  `Cache-Control: public, max-age=31536000, immutable` + strong ETag + Range everywhere (D10 static contract).

### 2.5 Compaction (WAL + git), leader by lease
- Assigned maintainers run **geometric** folding of fresh packs (tier 0 → tier 1, `git repack -d --geometric
  --write-midx`) under `leases/compact.pb`; the result is a COMPACT entry; followers download the new pack and
  drop superseded ones after in-flight readers finish. Triggers: `compaction.trigger_packs`, `trigger_bytes` —
  and at least **two** fresh packs (one pack folds into itself).
- **The base (tier 2, one pack + bitmap) is rebuilt only when the weekly bundle is built, and only on an ssd
  host** — the maintainer's `BaseRebuild` unit, followed by the weekly slot's compose
  (`docs/BUNDLE_URI_DESIGN.md §5`; `walgit compact --base` is the manual form). The rebuild runs in a scratch copy
  under `<cache.dir>/_rebuild/` that survives the process and resumes after a restart iff the WAL head has not
  moved (§5a); the serving copy is never rewritten. Invariant that makes serving cheap: *everything newer than the
  base lives in small/medium packs that fit on every instance.*
- **A fold never touches the base or a history pack** (`--keep-pack`), **a base is rebuilt only by the weekly
  unit / `compact --base`**, and **a rebuild supersedes every other live pack** by the manifest, not by what git
  happened to delete.
- **A tier-2 pack carries its own witness checkpoint (#195).** Bundle compose replays the base's refs with
  `refs_at_seq(base_seq)`, and the reader used to consult *only* the live checkpoint — so the first fold after a
  rebuild made the base unreplayable (`refs at seq X are not replayable: log folded up to Y and no checkpoint at or
  before`). The tier-2 publish choke point (`publish_compact_impl`, shared by base rebuild, `walgit import` and
  `wal add-pack --tier 2`) writes an **exact checkpoint at the pack's own seq** (`write_witness_checkpoint`:
  the `checkpoints/<seq>/{checkpoint.pb,refs.pb}` pair, create-if-absent, **no manifest CAS** — it does not move
  the live fold; a failed pair write fails the publish unit so it retries). When the live checkpoint has moved past
  the cut, the reader lists `checkpoints/` and accepts an exact witness, or an older complete witness only when the
  manifest proves every log segment in `(witness.seq, cut]` is present; a folded gap returns the original
  not-replayable error instead of silently serving older refs. That invariant — *tier-2 publish ⇒ exact witness at
  its seq* — is also what makes folded WAL objects reclaimable at all (see the GC bullet below).
- Superseded packs are retained `compaction.retention_superseded` (provenance window) then GC'd by the maintainer's `gc` unit: it lists the markers and deletes a pack + its side-files + the marker once the marker is older than the window and the pack is not live (re-checked against a fresh manifest). Before that scan the same unit **reconciles marker-less orphans** — packs superseded before markers existed, uploads abandoned before the manifest CAS, a crash between the object PUT and the log entry — by listing `wal/` and stamping what the fresh manifest does not reference (`superseded_at = now`, so an orphan's reclamation only ever starts later; ≤ `GC_MAX_MARKERS_PER_UNIT` per pass, and the unit stays due until the reconcile is complete). The marker set is therefore self-healing, not a one-off backfill. The same unit also **reclaims folded WAL objects** (#175): log segments below `manifest.min_seq` and checkpoint directories that are neither the live checkpoint nor the newest one at or before a live base pack's seq (bundle compose replays that base's refs from it) are deleted once `maintenance.retention_wal` has passed. The age comes from each candidate's own durable content — a log segment's newest `LogEntry.created_at`, a checkpoint's `created_at` (falling back to `refs.pb`'s snapshot time for a crash remnant) — not from the live checkpoint, whose churn in an active repository would otherwise keep every older folded object inside the window forever (#206). Missing/invalid timestamps fail safe by keeping the object. Three fences keep that deletion safe: only checkpoint directories **strictly older than the live one** are candidates (a checkpoint is PUT before its manifest CAS, so a newer directory may be an in-flight write); the checkpoint writer itself never CASes a checkpoint older than the live one (`checkpoint.rs` retry loop), so "older than live" really is superseded; and the whole pass **fails closed** unless every live base pack has an *exact, complete* checkpoint at its seq (`checkpoint.pb` + `refs.pb` both present) — without that witness we cannot name the log segments a rebuild would replay (#195), so folded segments and older checkpoints are all kept. Only marked packs are candidates, and a publisher refuses to adopt a checksum GC has listed in `Manifest.reclaiming` (`WalError::Reclaiming`) — the listing is CAS'd in *before* any object is deleted and cleared after, so it is the ordering record (the marker is the age record, not the lock). That closes the window where a publisher re-adopting a byte-identical pack could CAS it live between GC's live-check and GC's delete. Bounded per pass (`GC_MAX_PACKS_PER_UNIT`, default 32).

### 2.5b Self-healing by construction (D22)
Everything the maintainer produces — checkpoints, bundles per slot, compactions, retention — is a **pure function
of (config, WAL state)**. The maintainer does not run schedules; it computes the *desired state* every pass and
performs **one bounded unit of the most important missing work** (checkpoint → repair → missing weekly → missing
dailies oldest-first → missing hourlies → compaction → rev-index → gc → fsck audit), as a task, under a lease. An outage
of any length leaves no permanent hole; a deleted or corrupt artefact is "missing" and rebuilt identically; config
changes take effect by re-planning; there are no one-off backfill scripts.

### 2.6 Bundles (bundle-uri): move clone bytes to static files — **the north star** (`docs/BUNDLE_URI_DESIGN.md`)
- Strategies (config, D21/D22): **weekly full** (for a big repo = base pack ∘ header via store-side compose — GCS
  natively, S3 by multipart `UploadPartCopy`; no disk, no index-pack), **daily incremental** chained on the previous
  daily, **hourly incremental** on the newest daily, each on a calendar slot (6-field UTC cron `schedule`; the fire
  time is the as-of instant), `creationToken = slot epoch`, main-only refs for `bundles.main_only` repos;
  `bundles/list.pb` CAS'd.
- Served as static objects (`/{o}/{r}.git/bundles/...`, ETag/Range, immutable) by walgit or offloaded to an edge
  (`X-Accel-Redirect`, `deploy/nginx.conf.example`), or as presigned store URLs (`serve_via = "signed_url"`);
  advertised in the v2 capability list, at `bundles/list` and on band 2 of every narrated fetch. `bundles.require`
  refuses *unbounded* zero-have fetches of listed repos with the exact fix (D17).
- Builds run on the **maintainer** under `leases/bundle:<strategy>` (Serve sync first; unresolvable tips skipped
  with a notice); weekly full is a compose.
- **Blobless family** (design §6b): strategies with `filter = "blob:none"` — weekly-history = header ∘ the D18
  history pack, incrementals `--filter=blob:none` — advertised ONLY at `bundles/list?filter=blob:none`.

### 2.7 Tasks, progress, narration (`walgit-wal/src/tasks.rs`, `crates/walgit-server/src/sse.rs`, `smart.rs`)
Any long work = a task: unique id, per-instance log (`GET …/tasks`), `(repo, kind)` lock (a second start joins),
replayable packet stream (`GET …/tasks/{id}`). Packets: `notice`, `progress {label,done,total?,unit,percent?}`,
`task`, terminal `result` | `error`. Web: any JSON endpoint that cannot answer immediately streams the SSE
envelope when the client accepts `text/event-stream`; fast answers stay plain cacheable JSON. Git: v2 `fetch`
advertises `sideband-all` and narrates on band 2 (`remote: * …`). Startup: `cache.prewarm` repos run a `prewarm`
task; `/readyz` is 503 until done or `prewarm_ready_timeout`. `no-progress` is honoured.

---
## 3. Principles (what a PR is judged against)

One idea: **the bucket is the repository; everything else is a cache or a reader of the log.** Ten consequences.
A change that feels natural on a conventional git host (a table, a queue, a cache server, a webhook from the push
path, a bigger disk) is usually a violation here. A violation means the principle is wrong — amend it with a
decision in `docs/DECISIONS.md` — or the PR is; never "fix later".

| # | Principle | The tell in a PR | The question to answer |
|---|---|---|---|
| **I** | **No state outside the object store.** Disk and memory are caches. | A database, Redis, SQLite, a file that must survive a restart, an env var that encodes data. | "If every instance is wiped now, what is lost?" — must be "warmth". |
| **II** | **The manifest CAS is the only commit point.** Immutable objects are never overwritten (`PutMode::Overwrite` only on the manifest, bundle list, leases, fsck.pb, gc.pb, maintainer heartbeats, render cache). | A second "commit" (a flag file, a list update that makes data visible), an ACK before the bucket's. | "What does a client on another instance see between the PUT and the CAS?" — nothing new. |
| **III** | **Side effects are readers of the WAL, never steps of a write.** Mirrors, notifications and other side effects tail the log from a durable cursor. | A webhook/HTTP call from `receive.rs`, `publish.rs`, `follow.rs`, `smart.rs`. | "If this side effect fails, does the push?" — no. "Is it replayable from the cursor?" — yes. |
| **IV** | **Every read revalidates; there is no eventually.** | A cache that outlives the manifest's generation, a TTL invented for a repo-scoped answer, a read that skips `sync_*`. | "After `push` returns `ok`, can any instance serve the old state?" (`cargo test -p walgit-server --test sim`). |
| **V** | **Serve from the parts that fit; never a bigger box, never a hard-coded host.** | "Just download the pack", a path that assumes the full pack set is local, a hostname in `crates/` or `web/`. | "What happens to this code on a 20 GiB tmpfs with a 32 GB base pack? Which sync level does it need?" |
| **VI** | **Never block the async runtime; bulk bytes never share a lane with the control plane.** | `Command::new(...).output()` or `std::fs` big reads in an `async fn` outside `spawn_blocking`/the bulk runtime; a queued writer on `RepoHandle::rw` from an install path. | "On which thread does this run, and what holds `sync_mutex`/`rw` while it does?" (e2e `blocking_work_in_the_install_path_does_not_stall_requests`). |
| **VII** | **No LIST on a hot path; count the round trips.** | A `.list(` in request handling; a new GET "just to check"; a protocol change without a `docs/ROUNDTRIPS.md` row. | Before/after depth in the commit; the sim asserts request budgets (`FaultStore::stats().ops`). |
| **VIII** | **Standalone first; the edge announces, the app never assumes.** | Reading an `X-Walgit-*` request header without checking `X-Walgit-Capabilities`; a feature only testable behind nginx. | "Does this work on `walgit.standalone.toml` with nothing in front?" |
| **IX** | **No silent waiting.** | A new op that blocks a handler, a loop without a task, a `sleep` a client would wait on. | "Where does the client see this taking time?" — a task kind, a progress packet or a band-2 line. |
| **X** | **Keep walgit small.** Upstream `git` does git things; gix only where measured faster and correct; one config file, one auth story, one SDK; scope is `GOAL.md §4`. Proto append-only. | A new dependency without a why; a reimplementation of something `git` does; an alias/shim/"legacy" branch; a removed proto field. | "Which line of GOAL §4 is this for?" |

---
