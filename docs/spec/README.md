# Bounded contract checks

These models explore finite interleavings of publication, snapshots, replacement, delivery and retirement.
They do not prove a refinement of Rust, validate Git pack bytes, or certify deployment performance. The
executable inventory is [tlc/cases.tsv](tlc/cases.tsv); a checked-in row is not an execution receipt.

This is a port of the upstream `tobi/walgit` specification suite (2026-09-12,
`fc4ce3d`/`40bdb45`/`72daddd`) onto this repository's WAL. The models are byte-for-byte upstream; the
model-to-code map below names this repository's implementation and tests, and the gaps are this port's
honest audit of where the models outrun the code. Read the models as **bounded interleaving checks, not as a
refinement proof and not as certification of any deployment**.

## Running the checks

Run `just spec` for the matrix plus the bounded reference and its old-value negative control. Use
`just spec-fragments` to omit those two reference runs, or `just spec-full` to add larger reference arms.
Requirements: Java 11 or newer (CI uses 21), Bash, ripgrep, curl and a SHA-256 utility (`sha256sum` or
`shasum`). GNU `timeout` is optional: `scripts/run-spec.sh` uses it when present (`timeout`/`gtimeout`) and
otherwise runs its own bounded watchdog, so a stock macOS host needs no coreutils install. Defaults are two
JVM jobs, two workers and 2 GiB heap per job. Full reference runs are serial with eight workers and 8 GiB.
Override `WALGIT_TLC_JOBS`, `WALGIT_TLC_WORKERS`, `WALGIT_TLC_HEAP` and `WALGIT_TLC_TIMEOUT` explicitly;
the timeout defaults to 600 seconds per invocation. More jobs multiply the heap budget.

`WALGIT_SPEC_MODELS` (space-separated TLA module names) narrows a run to rows of the one matrix in
`tlc/cases.tsv`; an unknown name fails rather than silently running nothing. CI's bounded job uses it for the
first batch, with a 1800 s per-invocation cap and a 45 min job cap (both recorded in `.github/workflows/ci.yml`).

The **first batch** is four models, each with at least one mutation or negated reachability witness:

| Model | Fixed arm | Mutation / negated witness arms |
|---|---|---|
| PublicationView | `publication-view-fixed` | `publication-view-bug` (`NoLostUpdate`) |
| WALCheckpoint | `checkpoint-fixed` | `checkpoint-bug` (`SegAtMin`) |
| LogSlotClaim | `log-slot-fixed` | burned retry / early sweep / ambiguous delete / deterministic bytes, plus `NoOrphanSwept`, `NoRetryCommit`, `NoAmbiguousCommit`, `NoCrashLateCommit` |
| FrontReplay | `front-replay-fixed` | ambiguous replay / connect taxonomy / no precondition / no-op publishing, plus `NoFallbackApply`, `NoAmbiguousRecovery`, `NoDuplicateRejected`, `NoHonestConfusion`, `NoQuietNoOp` |

Each invocation retains its own effective configuration, log and verdict under `target/test-logs/spec-*`.
A fixed arm requires exit 0, the completion banner and an exhausted enumeration queue. A mutation or
negated reachability witness requires exit 12 and exactly its named invariant. A temporal boundary requires
exit 13 and its named property. Counterexamples require a trace and completion footer. Unexpected errors,
deadlocks, timeouts, duplicate verdicts and truncated logs fail; `scripts/test-spec-runner.sh` checks these
classifier rules without a JVM. A witness counterexample demonstrates reachability, not a failed guarantee.

Finite fragments explicitly include the stuttering already allowed by their bracketed next-state relation.
This does not change their temporal behavior or weaken their invariants. It lets TLC check with its normal
deadlock detection enabled; it does not establish application deadlock freedom. Progress is a separate
property with explicit fairness assumptions. Liveness configurations must not use symmetry reduction.
Maintenance safety models do not prove eventual scheduling, repair deadlines or sustained convergence.

## Public contract identifiers

| ID | Obligation |
|---|---|
| C1 | An acknowledged publication is durable from the bucket alone. |
| C2/C3 | Reads revalidate and use one committed snapshot; concurrent writers compose without regressing fields. |
| C4 | Ref old values hold at the successful CAS, including retries. |
| C5 | Complete submissions commit all-or-none; independent batch members remain independent. |
| C6a/C6d | Current tips and candidate external dependencies are covered by the exact resulting committed inventory. |
| C6b | Fetch omissions require honest negotiation: common closure, declared shallow boundaries, granted filters, negotiated thin bases, or **negotiated packfile URIs under exact coverage**. The URI case is the fifth lawful class. Never ACK synthetic roots to the client or subtract them on an engine that emits no URLs. |
| C6c | Granted filters preserve on-demand access to reachable omitted objects. |
| C7/C8 | Uncommitted candidates confer no serving authority; receipts distinguish committed, rejected and unknown outcomes. |
| C9 | Ambiguous forwarding never causes local replay; duplicate no-op requests publish no new entry. |
| C10a/C10b | Maintenance preserves objects and active historical reads. Issued URI records and bytes have no arbitrary expiry or cap. |
| C15/C16 | Conditional termination and eventual folding require the stated fairness and finite-workload assumptions. |
| B2 | Each CAS retry derives evidence from its newly captured basis. |
| B5/B6 | Immutable identity is exact; multi-read operations never combine generations. |

The bounded [WALContract](WALContract.tla) is a reference abstraction, not executable validation to install
mechanically. Its fast bound of two pushes cannot reach a successful multi-ref transaction, and its original
transaction pool cannot collect previously published objects. BatchPublication and MCTrim provide dedicated
multi-ref and historical-trim witnesses. `MC_incomplete` intentionally violates `PushTerminates`: incomplete
inputs have no rejection action in that frozen abstraction. This implementation must instead reject missing
inputs all-or-none within a bounded deadline; this control does not demonstrate a new defect.

WALCheckpoint assumes contiguous segments. Its `SegAtMin` equality is stronger than the concrete rules for
burned sequence gaps and genesis and must not be copied into runtime validation. Segmented packs also invalidate
an at-most-one-base invariant. ClosureProvenance and PackReplacement include hypothetical pruning mutations;
public maintenance is conserving and exposes no pruning mode.

## Model-to-code map

The exact fixed arms, faults and negated witnesses are named in the matrix. A listed model can expose a
required implementation obligation even while the corresponding audit below remains incomplete. Every gap
is listed separately; none is closed by a green model rerun.

| Contract | This repository's code | Existing tests | Gaps |
|---|---|---|---|
| C1 | `crates/walgit-wal/src/publish.rs` (`process_batch`, `cas_landed`, `put_immutable_create`); `crates/walgit-wal/src/handle.rs` (`sync_refs`) | `walgit-wal/tests/wal.rs`: `test_create_and_three_pushes`, `a_landed_cas_is_ok_even_when_the_local_apply_fails_and_the_next_sync_repairs_it`; `walgit-server/tests/e2e.rs`: `reads_after_an_acknowledged_push_never_show_the_previous_tip` | Durability is inherited from the store contract, not modelled; a broker-forwarded ACK is not re-read from a second instance in a fault test. |
| C2/C3 | `crates/walgit-wal/src/sync.rs` (`freshness_check`, `replay_log`, `apply_entries`, `reconcile_packs`); `crates/walgit-wal/src/checkpoint.rs`; the CAS retry in `publish.rs::process_batch` | `test_two_registries_cross_sync`, `cold_rematerialize_refreshes_once`, `test_concurrent_different_refs`; `sim`: `sim_safety_then_liveness`, `liveness_checkpoint_racing_compaction` | No warm-race twin where a refs-level reader keeps a pre-CAS snapshot while a compaction or seal commits; publisher/checkpointer/sealer interleaving is not combined. |
| C4 | `publish.rs::verify_txn` and the 412 refetch/revalidate path in `process_batch`; `crates/walgit-git/src/receive.rs` | `test_concurrent_same_ref_conflict`, `test_zero_oid_means_absent_for_create_and_delete`; `e2e`: `atomic_push_rejects_all_refs_when_one_is_non_ff` | No test that a retry's basis moved between the refetch and the next CAS (ABA), and no multi-retry old-value audit. |
| C5 | The per-repository group commit in `publish.rs::process_batch` (`wal.batch_window`) | `test_batching_50_concurrent`, `test_concurrent_different_refs`; `e2e`: `push_clone_roundtrip_v2` | A mixed batch (one member fails policy or connectivity) has no independence test; independence is argued per request, not observed per batch. |
| C6a/C6d | `publish.rs` pack publication and inline manifest live set; `sync.rs::reconcile_packs`; `publish.rs::write_superseded_markers` | `reconcile_prunes_local_pack_not_in_manifest`, `restart_forces_pack_reconcile_even_with_clean_state`, `reconcile_keeps_staged_pack`, `cancelled_publish_keeps_pack_protected_until_publisher_finishes` | Candidate external (thin-pack base) dependencies are not audited; no test that a pushed tip's closure equals the committed pack inventory after a thin pack is fixed. |
| C6b | `crates/walgit-git/src/upload_gix.rs` (filter/shallow walk, no thin packs); `crates/walgit-server/src/smart.rs` (v2 negotiation); `crates/walgit-server/src/bundles.rs` (clone/catch-up lists) | `e2e`: `partial_clone_blob_none_and_lazy_fetch`, `shallow_clone_then_unshallow`, `partial_clone_tree_zero_and_depth_with_filter`, `bundle_uri_clone_fetches_server_bundle`, `bundles_require_refuses_zero_have_fetches_with_the_fix` | The packfile-URI class is not implemented: bundles are the clone byte path, and no test proves a bundle slot still covers every lawful omission class after chain pruning. |
| C6c | `upload_gix.rs` faulter; `crates/walgit-wal/src/remote.rs`; `sync.rs::download_object` | `test_history_pack_keeps_tree_walks_local`, `test_serve_level_remote_serves_base_without_mount`; `e2e`: `large_repo_clone_timing` | No per-filter on-demand object test (`blob:none`, `tree:0`, sparse) against a remote-served base; the gix path is not exercised when the base is local. |
| C7/C8 | `publish.rs::cas_landed` (committed vs not), `claim_log_slot` (burn/contend), `crates/walgit-wal/src/error.rs` (`WalError`, `Reclaiming`) | `sim`: `liveness_after_a_lost_cas_response`, `liveness_orphaned_log_segment_does_not_block_writers`; `a_landed_cas_is_ok_even_when_the_local_apply_fails_and_the_next_sync_repairs_it` | No typed "unknown" receipt reaches the Git client: a fresh manifest read that fails surfaces as an error, and no test pins that failure mode. |
| C9 | `crates/walgit-server/src/forward.rs`; the buffered fallback in `crates/walgit-server/src/smart.rs` | `forward.rs`: `broker_401_falls_back_instead_of_relaying`, `relayed_4xx_carries_www_authenticate` | The local fallback is taken on any `request.send()` error, including one after request bytes were delivered; no test covers an ambiguous delivery (FrontReplay's connect-lies control). |
| C10a/C10b | `publish.rs::publish_compact_impl` + `update_reclaiming`; `checkpoint.rs::write_witness_checkpoint`; `log_reader.rs::refs_at_seq`; `crates/walgit-server/src/ops.rs` (`gc_superseded_packs`, `gc_wal_objects`); `crates/walgit-wal/src/registry.rs` leases | `publish_compact_tier2_writes_an_exact_witness`, `refs_at_seq_finds_a_retained_witness_after_later_folds`, `refs_at_seq_rejects_a_witness_older_than_a_folded_cut`, `a_supersession_without_its_marker_does_not_commit`, `a_compaction_skips_the_marker_for_a_checksum_gc_has_claimed`, `a_claim_release_is_fenced_by_owner_and_token`; `sim`: `geometric_fold_never_touches_the_base_or_the_history_pack` | No deterministic test holds a pinned historical local reader across a real GC pass; the reclaiming fence is tested at the claim level, not under a concurrent adopt/delete race. |
| C15/C16 | `publish.rs::publisher_task` (retry/backoff); `checkpoint.rs::checkpoint_due`; `crates/walgit-server/src/maintain.rs::run_pass` | `sim`: `liveness_stale_instance_cannot_starve_the_core`, `liveness_publisher_survives_a_crash_mid_publish`, `liveness_compaction_after_lease_holder_dies` | Fairness is model-side only; there is no deadline assertion for publisher termination under sustained contention and no bounded-work test for eventual folding. |
| B2 | The 412 path in `publish.rs::process_batch` (refetch, revalidate, re-sequence); `claim_log_slot` | `test_concurrent_same_ref_conflict`; `sim`: `liveness_after_a_lost_cas_response` | No negative control proves a retry uses the newly captured basis rather than the pre-412 one. |
| B5/B6 | `publish.rs::put_immutable_create`; `sync.rs`; `checkpoint.rs`; `error.rs` | `test_compact_replays_on_other_registry`, `test_recreate_after_bucket_wipe_on_leftover_cache_has_empty_refs`, `test_rebuilt_bucket_replaces_leftover_refs`, `test_open_empty_manifest_on_leftover_cache_has_empty_refs` | Multi-read operations (web overview, bundle compose) have no single-generation consistency test; a concurrent rewrite/wipe twin remains open. |

## Remaining scope and limits

The following are gaps, each stated separately. None is closed by rerunning a green model.

- The models are bounded and do not prove a refinement of this repository's Rust.
- CacheDiscipline, TempReclaim and ReadinessCarry were not part of this first batch; their public-cache
  applicability still needs review, corrections and concrete twins.
- PolicyPinLocality models policy blobs pinned to committed Git refs; this repository loads `policy.json`,
  so that model cannot be presented as a proof of its policy protocol.
- Packfile-URI negotiation under exact coverage (the fifth lawful omission class) is not implemented; the
  clone byte path is bundle-uri.
- There is no bucket-collector proof: PackCoverage assumes retention, MCTrim models a reader-safe collector,
  and neither proves the implemented GC.
- Cold truncated-read readiness remains open.
- Seal-evidence cooldown remains open.
- Producer quarantine/import and namespace recreation are separate gates.
- Resource limits, a distributable protected-URI client, live backend/edge tests and representative
  performance measurements are separate gates.
- No model proves authentication, pack encoding, index hashes or Git wire correctness; real regression
  tests must cover those boundaries.

## Tool provenance

The checksum-pinned jar is upstream TLA+ `v1.8.0`. The upstream port pinned revision
`867aefb69ffc2452031292587b389d1fc3eb43ff` (built 2026-09-12, SHA-256
`db131ddb48e7004d823bef4493df7b35694babe37505b9d9fa5685e7a331f1f1`). By 2026-10-19 that rolling asset
was no longer served: the release had been rebuilt from revision
`932d35ec2b9fdb3d3e4d0a3321689c634a1f6b43` (built 2026-10-15) and the old bytes could not be obtained.
The pin was re-derived from the release asset and its manifest; the current SHA-256 is
`15d103b7e5617780bcc904c0af12f1ad242e2b345c0301136c5be4588f17edac`, matching the
[release asset digest](https://github.com/tlaplus/tlaplus/releases/tag/v1.8.0) and the jar manifest
(`X-Git-Revision: 932d35e`). The first-batch matrix arms were rerun after the re-pin; the remaining arms
are exercised by `just spec` / `just spec-full` and are not claimed green by this document. This tag is rolling. A changed
asset must fail checksum verification until provenance is reviewed and checks rerun. Historical state
counts are not reused as new evidence. The runner does not substitute another checker, especially one
without equivalent result, liveness and input semantics.
