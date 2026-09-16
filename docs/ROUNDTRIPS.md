# Round trips are the cost model

Context: **who needs to read this, and when.** Anyone (human or agent) touching a protocol that talks to the
bucket: the publish path (`walgit-wal/src/publish.rs`), sync levels and freshness (`sync.rs`, `handle.rs`),
checkpoints, compaction and its lease (`walgit-server/src/ops.rs`, `coord.rs`), bundles (`walgit-bundle`),
the remote reader, LFS, or the `ObjectStore` backends themselves — and anyone writing or triaging simulation
scenarios (`crates/walgit-server/tests/sim.rs`), because a sim fix that is correct but adds a happy-path request
is a regression, not a fix. Read it *before* designing the change, use §5 when writing the commit/PR, and
keep §2 current. It exists because walgit runs on a tmpfs host instances that own nothing but a bucket: every
user-visible latency is a sum of sequential GCS requests, and the manifest is a single CAS'd object with a hard
write-rate cap — so the number and shape of round trips *is* the performance design. Referenced from AGENTS.md
(read order and §5 working rules) and from the sim harness briefs.

**Correct is necessary, not sufficient.** walgit's only durable primitive is a bucket with ~60–80 ms per
request, one serialized overwrite per second per object, and no transactions beyond single-object CAS.
Every protocol we design on top (publish, sync, compaction, checkpoints, bundles, leases) is judged on two
axes at once:

1. **Safety/liveness** — the simulation suite (`crates/walgit-server/tests/sim.rs`) and the contract tests.
2. **Critical-path round trips** — how many *sequential* bucket requests a user-visible operation needs,
   and how many requests in total (cost, contention on CAS'd objects).

A change that is correct but adds a sequential round trip to a hot path is a regression. A fix for a
liveness bug that moves work onto the *failure* path and leaves the happy path at the same depth is the
right shape. This document is the thinking tool; apply it to every protocol change and say so in the commit.

## 1. The numbers (GCS measurements)
| Primitive | Cost | Notes |
|---|---|---|
| GET / PUT small object | 60–80 ms p50/p99 | one request = one round trip |
| Conditional GET (`If-None-Match`) → 304 | 15–18 ms | the cheapest "is anything new?" |
| HEAD | ≈ GET | never add one on a happy path (`download_object` skips it when `PackRef` already has the size) |
| 404 | free | probe, don't list |
| LIST | slow, paged, eventually-ish | never on a hot path (rule in AGENTS §5) |
| CAS overwrite of one object | serialized, ~1 write/s | a CAS'd object is a throughput cap; 412 is the normal contention signal |
| Range read of a big object | ~100 MB/s per connection | stripe for more; bulk bytes on their own pool |
| tier-2 publish (#195) | the publish's own rounds, plus **2 serial create-if-absent PUTs** (`checkpoints/<seq>/refs.pb` then `checkpoint.pb`) for the witness — no manifest CAS, so it adds no CAS contention with the live fold; a failed witness write fails the publish unit and is retried. Replay (`refs_at_seq(base_seq)`) is a checkpoint+tail replay when the live checkpoint is old enough; after a fold passes the cut, the reader adds **1 LIST of `checkpoints/`** plus a pair of GETs, and accepts the witness only when it is exact or the `(witness.seq, cut]` log tail is contiguous. Bundle compose pays this once; the maintainer planner pays it only when a full slot is missing, and the web `upcoming` plan renders the same proof (refs-level, no pack data, but not a zero-round read) |
| `gc` op (#175) | lease GET+PUT (+ heartbeat CASes every `lease_ttl/3`) + 1 LIST of `wal/_superseded/` + 1 LIST of `wal/` + ≤ `GC_MAX_MARKERS_PER_UNIT` (512) conditional marker PUTs + 1 LIST of `log/` + 1 LIST of `checkpoints/` + per WAL candidate 1 GET to read its own age (a log segment's newest entry time; a checkpoint's `created_at`, plus 1 refs GET when `checkpoint.pb` is missing) + ≤ `GC_MAX_WAL_OBJECTS_PER_UNIT` (512) DELETEs of folded log segments and non-live checkpoint objects + per marker 1 GET + per claim 1 GET + ≤ 32 × (per object 1 HEAD + 1 manifest GET re-checking the claim fence + 1 conditional DELETE) + per retired marker 1 more manifest GET + 2 manifest CAS + conditional marker DELETE + lease GET+DELETE | refs-level (never reads pack data); the bound counts *reclaimable* packs (live markers are dropped first), so a pass is bounded and `complete=false` keeps the unit due until markers drain. The WAL age GETs are deliberate: the store contract exposes no object timestamps and the live checkpoint is recreated by healthy repos, so only the candidate's own immutable content can prove it is past `retention_wal` (#206). The second LIST reconciles orphans that never got a marker (#177): it walks `wal/` once per `gc` pass (≥ `maintenance.gc_interval`, 24 h by default — not a hot path) and stamps ≤ 512 of them, keeping the unit due until the reconcile completes. A superseding COMPACT adds one concurrent PUT per superseded pack (skipping checksums GC has claimed), issued before its manifest CAS |
| Compose | 1 request, no data transfer | ≤ 32 sources |

## 2. Budgets to defend (happy path, sequential depth → total requests)
| Operation | Depth | Requests | Where |
|---|---|---|---|
| Any read (`info/refs`, ls-refs, web refs/resolve) | 1 cond GET (or 0 within `freshness_ttl`) | 1 | `sync.rs::freshness_check` |
| Cold Refs sync | 1 manifest GET → 1 round (checkpoint refs ∥ log tail segments) | no checkpoint: 1 + tail (2 with one segment); checkpoint: 2 + tail | `registry.rs::open`, `sync.rs` |
| Push request / publish (`process_batch`) | 1 freshness GET → pack PUT ∥ idx PUT ∥ log PUT (1 round) → manifest CAS (1 round) | request: 5; already-synced publish: 4 | `publish.rs` |
| Compaction publish | same shape as push | — | `publish.rs::publish_compact_impl` |
| Large pack upload (S3/R2 `PutBody::File` > `store.multipart_threshold`) | 1 HEAD pre-check (only for `PutMode::Create`) → 1 CreateMultipartUpload → ⌈len/part_size⌉ sequential UploadPart → 1 CompleteMultipartUpload; each part is one SDK request, so a 135 MiB pack at 32 MiB parts costs 3 + 5 = 8 requests instead of one single-shot PUT that cannot finish inside the request timeout (and cannot exceed S3's 5 GiB `PutObject` cap at all) | 3 + parts | `s3.rs::put` / `multipart_put` |
| Checkpoint | 1 cond GET (freshness) → refs PUT ∥ checkpoint PUT → manifest CAS | 3 rounds, 4 requests (was 6/6 until 2026-08-22: a bundle-list GET before the checkpoint PUT and a log GET for provenance times sat in the chain; times now come from the writer's own applied state, `bundle_key` is no longer looked up) | `checkpoint.rs` |
| Settings publish (D24) | refs sync (conditional GET) → log slot PUT → manifest CAS; readers pay nothing extra (settings ride inline on the manifest) | 3 rounds; read: 0 | `publish.rs::publish_settings_impl` |
| Lease acquire | 1 GET → 1 CAS put (or 1 Create when absent) | 2 | `coord.rs::try_acquire` |
| Publish, local commit (2026-08-23) | unchanged in round trips: after the manifest CAS the ref txns are applied to the local copy **before** the new manifest version is advertised, both under `sync_mutex` (the refs phase of every sync); the reverse order let a reader cache the old refs under the new version, and without the lock a concurrent sync replayed the same entry (two `update-ref`, a lock collision). A landed CAS is answered `ok` whatever the local apply does — the next sync replays (one conditional GET that then returns 200, no extra write). | 0 extra | `publish.rs::process_batch` |
| Weekly compose: refs at the base's seq (`refs_at_seq`, 2026-08-23) | checkpoint `refs.pb` at that seq: 1 GET; else newest checkpoint ≤ seq (2 GETs) + the log entries through the seq (already cached by the refs sync in practice) — replayed in memory, never a local write | 1, or 2 + tail | `log_reader.rs::refs_at_seq`, `bundles.rs::compose_full_from_base` |
| Maintainer bundle pass, refs level (retention + settle closed slots) | 1 list GET → at most 1 CAS (retention) → at most **1 CAS for every verdict of the pass** (`record_skipped_many`; was one CAS per settled slot until 2026-08-22: a rig repo with 9,654 closed slots paid 9,654 CAS per pass and, past the 4,096-verdict cap, forever) | ≤ 3 | `walgit-bundle/src/lib.rs::settle_closed_slots`, `ops.rs::record_skipped_many` |
| Repository listing (`/api/v1/owners*`, `/services/api/owners*`, maintainer/bridge passes) | 0 within `LIST_TTL` (30 s, per instance); else delimited `repos/` → (delimited `repos/<o>/` ∥ owners) → (HEAD `manifest.pb` ∥ repos): 3 rounds | 1 + owners + repos | `registry.rs::list` |
| Orphan log slot (failure path only) | +1 fresh manifest GET, +HEAD per probe, +Create at next seq | — | `publish.rs::claim_log_slot` |
| Collab aggregation read (report/thread/board; D45 fold) | refs sync (1 cond GET) → snapshot object-header size probe → fault + one `cat-file --batch` over the snapshot blob + unfolded inbox tail (bounded: tail ≤ 20k refs, snapshot ≤ 64 MiB — the fold turns "one object per historical entry" into 1 ref + 1 blob) | +1 metadata round on the remote path; local path uses one `git cat-file -s` before body read | `web/api.rs::collab_load` |
| CI artifact/log read | CLI fresh clone: exact-actor `HEAD ?actor=` size preflight → one `git fetch` only on 200; server GET: refs sync (1 cond GET) → object-header size probe per same-address candidate → fault/read → SHA-256 verify before serving | CLI +1 HEAD only when the object is safe to fetch; server +1 metadata round on the remote path, local path uses `git cat-file -s` before body read | `ci_cmd.rs::remote_artifact_fits_cap`, `web/api.rs::collab_ci_artifact{,_size}` |

`healthy_request_round_trip_budgets` in `crates/walgit-server/tests/sim.rs` pins the healthy MemoryStore
counts at push **5**, warm refs **1**, cold refs with one tail segment **2**, and checkpoint **4**. Cold open used to spend an
extra unconditional manifest GET (3 requests, 3 sequential rounds); it now applies the manifest it already
fetched directly (2 requests, 2 rounds). `claim_log_slot`, `cas_landed`, and `put_immutable_create` add probes
only after Create/CAS failure, so the measured happy-path counts remain unchanged.

Keep this table current; when you change a protocol, update the row and put the before/after depth in the
commit message. The sim harness can enforce it: `FaultStore::stats().ops` counts exact store requests per
link, so a scenario can assert "a push on a healthy link is ≤ N requests" as a regression test.

## 3. Rules of thumb
- **Depth before count.** Two PUTs in parallel cost one round trip; the same two in sequence cost two.
  `tokio::join!` independent writes; never `await` uploads one after the other.
- **Let the conditional write be the read.** `PutMode::Create` → 412 *is* "it exists"; `Update(v)` → 412 *is*
  "someone moved it" and GCS tells you the current generation. Don't GET to decide what a conditional write will
  tell you for free.
- **Verification goes on the failure path.** e.g. `put_immutable_create` HEADs only after a 412; `cas_landed`
  re-reads the manifest only after a non-412 error. The happy path must not pay for rare cases.
- **Carry state in the object you already fetch.** The manifest is the one GET every request makes: anything a
  reader needs at refs level (pack set + side-file inventory, checkpoint pointer, log segments, revision, writer)
  belongs in it, so no second request is needed to know *what* to fetch.
- **Re-use version tokens you hold.** After a CAS success you have the new generation: don't re-GET to learn it.
  After a 412 you may have `current`: skip the GET when the store provides it.
- **Batch at the CAS.** The single-flight publisher (group commit, `wal.batch_window`) turns N concurrent pushes
  into one log PUT + one CAS. The push broker makes one writer per hot repo so 412s stop being the common case.
- **Never pay per ref or per pack on a hot path.** O(1) manifest, O(k) resolve; pack data by range, never by
  "download and see".
- **Immutable means cache forever**, on every layer (process LRU, bucket `cache/api/v1`, HTTP `immutable`).
  One instance computing something all instances need = a shared-cache write, not N recomputations.
- **Jitter every retry** that targets a shared CAS'd object; a synchronized retry storm is a self-inflicted
  serialization on the 1 write/s object.
- **Measure, don't guess**: `walgit-traces` shows the store span tree per request (count + critical path);
  sim `Stats::ops` gives exact counts; the measurement table holds the numbers.

## 4. Anti-patterns seen (don't reintroduce)
- HEAD/GET "to be safe" before a PUT that is conditional anyway.
- Sequential `.await` on independent uploads.
- Re-sync (GET) after a CAS success to "refresh".
- LIST to find the latest checkpoint/segment (the manifest knows).
- An undelimited LIST over `repos/` to enumerate repositories: it walks every object in the bucket (122 k keys, 8–9 s,
  GCS deadline retries on the broker, 2026-08-22). Enumerate *prefixes* (`list_prefixes`), probe manifests, cache.
- Fixing a liveness bug by adding a wait or a probe to the *happy* path.
- A retry loop without backoff+jitter on `manifest.pb`.

## 5. Checklist for a protocol change (paste into the PR/commit)
- Depth/requests before → after for each affected row in §2.
- What moved to the failure path, and how often that path runs (measured or reasoned).
- Which CAS'd object's write rate changes.
- Sim scenario(s) covering the new failure mode; `Stats::ops` budget assertion if the hot path changed.
