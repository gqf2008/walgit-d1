//! Repository maintenance operations ("make the repo great"): fsck, compaction,
//! bundle builds, checkpoints, re-materialize. Shared by the background loops
//! (`walgit serve` roles), the CLI, and the web UI's `POST …/ops/{op}` route,
//! which streams the op's log as SSE and records the outcome per instance.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;
use tracing::Instrument;

use prost::Message;
use serde::Serialize;
use walgit_config::Config;
use walgit_git::{RepackMode, RepackOptions, RepoId};
use walgit_store::ObjectStoreExt;
use walgit_wal::RepoHandle;

use crate::AppState;

/// Callback that receives human-readable progress lines.
pub type Log<'a> = &'a (dyn Fn(String) + Send + Sync);

pub fn noop_log(_: String) {}

// ---------------------------------------------------------------------------
// Catalogue
// ---------------------------------------------------------------------------

/// Ops the UI can trigger. `id` is the URL segment.
#[derive(Serialize, Clone)]
pub struct OpSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Query parameters the op accepts (documentation for the UI).
    pub params: &'static [&'static str],
    /// Whether this op changes the WAL (everything but fsck/sync is a write).
    pub mutating: bool,
}

/// How many missing oids one `fsck.pb` carries (the repair unit works through them;
/// the next fsck finds whatever is left).
pub const FSCK_MISSING_LIST_MAX: usize = 100_000;

/// Whether a task kind is a maintenance op (the units the D31 maintenance drain
/// waits for; request-driven tasks — sync, prewarm, history-pack install — are not).
pub fn is_op(kind: &str) -> bool {
    OPS.iter().any(|o| o.id == kind)
}

pub const OPS: &[OpSpec] = &[
    OpSpec {
        id: "fsck",
        label: "fsck",
        description: "git fsck --full --strict on this instance's copy (deep object/connectivity check). \
                      connectivity=1 skips object content checks. Records the verdict at fsck.pb \
                      (missing objects feed the repair unit).",
        params: &["connectivity"],
        mutating: false,
    },
    OpSpec {
        id: "gc",
        label: "Bucket GC",
        description: "Reclaim superseded packs: delete the packs a COMPACT entry dropped once their \
                      marker is older than compaction.retention_superseded (the provenance window). At most `max` packs per call (default 32). Ref-level: \
                      reads the manifest and marker objects, never pack data.",
        params: &["max"],
        mutating: true,
    },
    OpSpec {
        id: "repair",
        label: "Repair",
        description: "Fetch the objects the last fsck found missing from upstream.git and publish them \
                      as a pack (COMPACT entry, no ref change).",
        params: &[],
        mutating: true,
    },
    OpSpec {
        id: "follow",
        label: "Follow upstream",
        description: "Bring the refs in upstream.follow up to upstream.git's now: fetch the delta over this copy's \
                      objects, ingest it like a push, fast-forward only, one PUSH entry (principal=upstream). The \
                      maintaining host runs this every maintenance.follow_interval when a ref moved.",
        params: &[],
        mutating: true,
    },
    OpSpec {
        id: "rev-index",
        label: "Reverse index",
        description: "Build pack-<sha>.rev for a published pack that has none (git < 2.41 wrote none), upload it \
                      as the side-file and advertise it in the manifest (has_rev). Without it git rebuilds the \
                      reverse index in memory on every pack-objects (a large repository's base: 2.85 s per fetch).",
        params: &["pack"],
        mutating: true,
    },
    OpSpec {
        id: "compact",
        label: "Compact",
        description: "Geometric repack (or full base rebuild with bitmaps) under the per-repo compaction lease, \
                      published as a COMPACT WAL entry. force=1 ignores the trigger thresholds; base=1 forces a bitmap'd base rebuild.",
        params: &["force", "base"],
        mutating: true,
    },
    OpSpec {
        id: "bundle",
        label: "Bundle",
        description: "Build and publish a bundle-uri bundle now (strategy=<name>, default: the first full strategy; \
                      strategy=due builds whatever the schedule says is due).",
        params: &["strategy"],
        mutating: true,
    },
    OpSpec {
        id: "checkpoint",
        label: "Checkpoint",
        description: "Write a checkpoint (pack set + ref snapshot) at the current head so cold materialize and bundles start from here.",
        params: &[],
        mutating: true,
    },
    OpSpec {
        id: "sync",
        label: "Sync",
        description: "Revalidate the manifest and catch this instance's local copy up to the WAL head.",
        params: &[],
        mutating: false,
    },
    OpSpec {
        id: "rematerialize",
        label: "Re-materialize",
        description: "Throw away this instance's local copy and rebuild it from the store (repair).",
        params: &[],
        mutating: false,
    },
];

pub fn spec(id: &str) -> Option<&'static OpSpec> {
    OPS.iter().find(|o| o.id == id)
}

// ---------------------------------------------------------------------------
// Running an op = a walgit_wal task (unique id, (repo, kind) lock, log,
// attachable stream at GET …/tasks/{id})
// ---------------------------------------------------------------------------

pub enum StartError {
    UnknownOp,
    /// The same op is already running here; attach to this task instead.
    AlreadyRunning(Arc<walgit_wal::tasks::TaskState>),
}

/// Start `op` for `id` on this instance as a background task and return its
/// state (stream it with [`crate::sse::task_stream`]). The op keeps running if
/// every client goes away.
pub async fn start(
    state: Arc<AppState>,
    id: RepoId,
    op: &str,
    params: HashMap<String, String, impl std::hash::BuildHasher>,
) -> Result<Arc<walgit_wal::tasks::TaskState>, StartError> {
    // `walgit_wal::tasks::begin_task` takes the concrete default-hasher map;
    // fold any caller hasher into it up front.
    let params: HashMap<String, String> = params.into_iter().collect();
    let spec = spec(op).ok_or(StartError::UnknownOp)?;
    let handle = state
        .registry
        .open(&id)
        .await
        .map_err(|_| StartError::UnknownOp)?;
    let task = match handle.begin_task(spec.id, params.clone()) {
        walgit_wal::Begin::Started(t) => t,
        walgit_wal::Begin::AlreadyRunning(s) => return Err(StartError::AlreadyRunning(s)),
    };
    let task_state = task.state.clone();
    let op_id = spec.id;
    let span = task.span();
    let join = tokio::spawn(
        async move {
            let reporter = task.reporter();
            let repo = id.to_string();
            let log = move |line: String| {
                tracing::info!(repo = %repo, op = op_id, "{line}");
                reporter.notice(line);
            };
            let res = run(&state, &id, op_id, &params, &log).await;
            match res {
                Ok((summary, value)) => {
                    task.finish_ok(summary, Some(value));
                }
                Err(e) => {
                    task.finish_err(500, e);
                }
            }
        }
        .instrument(span),
    );
    task_state.set_abort_handle(join.abort_handle());
    Ok(task_state)
}

/// The last connectivity audit of `handle`'s repository, if any.
pub async fn read_fsck(
    handle: &RepoHandle,
) -> Result<Option<walgit_proto::v1::FsckReport>, String> {
    use walgit_store::ObjectStoreExt;
    match handle.store().get_bytes(walgit_proto::keys::FSCK).await {
        Ok(Some((_, bytes))) => walgit_proto::v1::FsckReport::decode(bytes.as_ref())
            .map(Some)
            .map_err(|e| e.to_string()),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Folded WAL objects one GC pass may delete: log segments the live checkpoint
/// already folded, and checkpoints no longer referenced (#175). They are tiny
/// (a few KB each); the bound exists to keep one unit bounded, not for bytes.
const GC_MAX_WAL_OBJECTS_PER_UNIT: usize = 512;

/// Reclaim folded WAL objects (#175): log segments below `manifest.min_seq`,
/// and checkpoint directories that are neither the live checkpoint nor the one
/// a live base pack's refs are replayed from.
///
/// Age anchor: the **live checkpoint's `created_at`**. Everything this pass is
/// allowed to delete was folded at or before that instant, so
/// `now - created_at > retention_wal` is a conservative (late) bound for all of
/// them — and it needs no per-object timestamp, which the store contract does
/// not expose.
///
/// Returns `(objects deleted, bytes freed, complete)`.
async fn gc_wal_objects(
    handle: &RepoHandle,
    manifest: Arc<walgit_proto::v1::Manifest>,
    max: usize,
    retention: std::time::Duration,
    lost: &std::sync::atomic::AtomicBool,
    log: Log<'_>,
) -> Result<(u64, u64, bool), String> {
    use futures::StreamExt;
    use walgit_proto::keys;
    use walgit_store::ObjectStore;

    if retention.is_zero() {
        return Ok((0, 0, true));
    }
    let Some(checkpoint) = manifest.checkpoint.as_ref() else {
        // Nothing is folded yet: every log segment is live.
        return Ok((0, 0, true));
    };
    let Some(created_at) = checkpoint
        .created_at
        .as_ref()
        .map(walgit_proto::time::to_system)
    else {
        // No anchor ⇒ no proof of age ⇒ keep everything this pass.
        return Ok((0, 0, true));
    };
    let age = std::time::SystemTime::now()
        .duration_since(created_at)
        .unwrap_or_default();
    if age <= retention {
        log(format!(
            "gc: folded WAL objects are younger than the {}h window — nothing to do",
            retention.as_secs() / 3600
        ));
        return Ok((0, 0, true));
    }

    // List both prefixes once: the deletion set is derived from what exists.
    let mut log_segments: Vec<(String, u64)> = Vec::new(); // (key, first_seq)
    let checkpoint_dirs: Vec<(u64, Vec<(String, u64)>)>; // (seq, objects)
    {
        let mut stream = handle.store().list(keys::LOG_DIR, None);
        while let Some(m) = stream.next().await {
            let m = m.map_err(|e| e.to_string())?;
            let Some(name) = m.key.strip_prefix(keys::LOG_DIR) else {
                continue;
            };
            let Some(seq_hex) = name.strip_suffix(".pb") else {
                continue;
            };
            let Ok(first_seq) = u64::from_str_radix(seq_hex, 16) else {
                continue;
            };
            log_segments.push((m.key, first_seq));
        }
        let mut dirs: std::collections::BTreeMap<u64, Vec<(String, u64)>> =
            std::collections::BTreeMap::new();
        let mut stream = handle.store().list(keys::CHECKPOINTS_DIR, None);
        while let Some(m) = stream.next().await {
            let m = m.map_err(|e| e.to_string())?;
            let Some(rest) = m.key.strip_prefix(keys::CHECKPOINTS_DIR) else {
                continue;
            };
            let Some((seq_hex, _)) = rest.split_once('/') else {
                continue;
            };
            let Ok(seq) = u64::from_str_radix(seq_hex, 16) else {
                continue;
            };
            dirs.entry(seq).or_default().push((m.key, m.size));
        }
        checkpoint_dirs = dirs.into_iter().collect();
    }

    // What a live base pack's refs are replayed from: either an exact checkpoint
    // at its seq, or the newest checkpoint at or before it **plus the log
    // segments covering (that checkpoint, base_seq]** (#195). Keep the whole
    // witness — the checkpoint alone cannot reconstruct the refs.
    let mut keep: std::collections::HashSet<u64> = std::collections::HashSet::new();
    keep.insert(checkpoint.seq);
    let mut keep_logs: std::collections::HashSet<u64> = std::collections::HashSet::new();
    // Fail closed until every live base has an *exact, complete* checkpoint at
    // its seq: a base without one has no witness we can prove — the segments its
    // refs would be replayed from sit below `min_seq` and are invisible in
    // `manifest.log_segments`, and a partial directory (one PUT of the writer's
    // pair landed) is not a witness either. In that case this whole pass deletes
    // nothing for the repo: neither the folded segments nor any older checkpoint
    // (that directory is the only remaining fallback candidate).
    let mut keep_all_folded_logs = false;
    for base in walgit_wal::base_packs(&manifest) {
        // A witness is a directory that actually holds both objects the replay
        // needs — not merely a seq that appears in the listing.
        let exact = checkpoint_dirs
            .iter()
            .filter(|(seq, _)| *seq == base.seq)
            .any(|(_, objects)| {
                let has = |want: &str| objects.iter().any(|(key, _)| key == want);
                has(&keys::checkpoint_key(base.seq)) && has(&keys::checkpoint_refs_key(base.seq))
            });
        if !exact {
            keep_all_folded_logs = true;
        }
        let witness_seq = checkpoint_dirs
            .iter()
            .rev()
            .find(|(seq, _)| *seq <= base.seq)
            .map_or(0, |(seq, _)| *seq);
        if witness_seq > 0 {
            keep.insert(witness_seq);
        }
        // The tail the replay reads: segments that start at or before the base
        // and reach past the checkpoint it replays from.
        for segment in &manifest.log_segments {
            if segment.first_seq <= base.seq && segment.last_seq > witness_seq {
                keep_logs.insert(segment.first_seq);
            }
        }
    }

    let live_logs: std::collections::HashSet<u64> =
        manifest.log_segments.iter().map(|s| s.first_seq).collect();
    let mut deleted = 0u64;
    let mut deletion_count = 0usize;
    let mut freed = 0u64;
    let mut complete = true;
    let mut pending: Vec<Vec<(String, u64)>> = Vec::new();

    for (key, first_seq) in log_segments {
        if live_logs.contains(&first_seq) || first_seq >= manifest.min_seq {
            continue; // still needed for replay
        }
        if keep_all_folded_logs || keep_logs.contains(&first_seq) {
            continue; // a live base's replay witness (or nothing provable)
        }
        pending.push(vec![(key, 0)]);
    }
    for (seq, objects) in checkpoint_dirs {
        if keep.contains(&seq) {
            continue;
        }
        if keep_all_folded_logs {
            // No provable witness for some live base: leave every older
            // checkpoint directory as a fallback candidate.
            continue;
        }
        // Only directories strictly *older* than the live checkpoint are
        // candidates: a checkpoint directory is PUT before its manifest CAS
        // (checkpoint.rs: two immutable PUTs, then the CAS), so a concurrent
        // writer's directory can exist while the snapshot still points at the
        // previous seq — deleting it would leave that CAS pointing at nothing.
        // A genuinely abandoned newer directory is reclaimed by a later pass,
        // once a still-newer checkpoint has become live.
        if seq >= checkpoint.seq {
            continue;
        }
        pending.push(objects);
    }

    for objects in pending {
        if deletion_count >= max {
            complete = false;
            break;
        }
        if lost.load(std::sync::atomic::Ordering::SeqCst) {
            // The lease is gone: another pass may own this work now.
            complete = false;
            break;
        }
        for (key, size) in objects {
            if deletion_count >= max {
                complete = false;
                break;
            }
            if lost.load(std::sync::atomic::Ordering::SeqCst) {
                complete = false;
                break;
            }
            match handle.store().delete(&key, None).await {
                Ok(()) | Err(walgit_store::StoreError::NotFound { .. }) => {
                    deleted += 1;
                    deletion_count += 1;
                    freed += size;
                }
                Err(e) => return Err(format!("gc: delete {key}: {e}")),
            }
            if lost.load(std::sync::atomic::Ordering::SeqCst) {
                complete = false;
                break;
            }
        }
    }
    if deleted > 0 || !complete {
        log(format!(
            "gc: reclaimed {deleted} folded WAL object(s), {freed} bytes{}",
            if complete { "" } else { " (more next pass)" }
        ));
    }
    Ok((deleted, freed, complete))
}

/// One bounded bucket-GC unit: delete superseded packs that have aged past
/// `compaction.retention_superseded` (#175).
///
/// `Manifest.packs` is the live set. A pack a COMPACT entry dropped is found by
/// its `wal/<checksum>.superseded` marker, written when that entry committed
/// (`publish_compact_impl`) — the manifest alone cannot express *when* a pack
/// left the live set, and the superseding log entry is eventually folded into a
/// checkpoint. Only marked packs are candidates, which is also what protects a
/// pack a concurrent publisher uploaded but has not CAS'd yet: no marker, no
/// candidate.
///
/// Bounded by `max` packs and [`GC_MAX_MARKERS_PER_UNIT`] newly written markers
/// (D22: one unit of the most important missing work).
///
/// Returns `(reclaimed packs, freed bytes, markers written, complete)`.
async fn gc_superseded_packs(
    handle: &RepoHandle,
    max: usize,
    lease_ttl: std::time::Duration,
    owner: &str,
    token: &str,
    lost: &std::sync::atomic::AtomicBool,
    log: Log<'_>,
) -> Result<(u64, u64, u64, bool), String> {
    use futures::StreamExt;
    use prost::Message;
    use std::sync::atomic::Ordering;
    use walgit_proto::keys;
    use walgit_proto::v1::SupersededPack;
    use walgit_store::ObjectStore;

    // Fresh refs *first*: the retention window (D24: `[compaction]` is a
    // per-repo settings section), the live set and the marker ages must all
    // come from one generation. Reading a stale cached handle would let another
    // instance's settings change land after we captured the window (#175).
    let (retention, live) = {
        let _guard = handle.sync_refs().await.map_err(|e| e.to_string())?;
        let retention = handle.effective_config().compaction.retention_superseded;
        let live: std::collections::HashSet<String> = handle
            .manifest()
            .packs
            .iter()
            .map(|p| p.checksum.clone())
            .collect();
        (retention, live)
    };
    let now = std::time::SystemTime::now();

    // Crash/lease-loss recovery. A claim whose holder died must never stay
    // forever: with a marker still present the checksum can never be reclaimed
    // (and every push/compact regenerating those bytes is refused); with the
    // marker already retired it can never even become a candidate again. Either
    // way this pass — which holds the lease — takes it over, but only once the
    // claim is older than a few lease TTLs: a younger claim may still belong to
    // a live pass whose lease we simply cannot see. "Not ours" is judged on
    // `(owner, token)`: a different token of the *same* instance is a different
    // holder.
    let grace = (lease_ttl.saturating_mul(3)).max(std::time::Duration::from_secs(300));
    // Marker gone ⇒ nothing left to reclaim: release the stale claim by an
    // exact compare-and-remove.
    let mut orphan_claims: Vec<(String, String, String)> = Vec::new();
    // Marker still there ⇒ adopt it: the same CAS compare-removes the stale
    // tuple and adds ours, so the pass can then finish the reclamation.
    let mut takeovers: Vec<(String, String, String)> = Vec::new();
    // Checksums this pass adopted above; if the scan then finds nothing to do
    // (the marker vanished between our GET and the take-over CAS) they carry no
    // work and must be released, or the publisher is refused until the next
    // `gc_interval`.
    let mut adopted: Vec<String> = Vec::new();
    for claim in handle.manifest().reclaiming.clone() {
        let ours = claim.owner == owner && claim.token == token;
        if ours || live.contains(&claim.checksum) {
            continue;
        }
        // A missing `since` is not evidence of age: keep the claim.
        let old_enough = claim
            .since
            .as_ref()
            .map(walgit_proto::time::to_system)
            .is_some_and(|t| now.duration_since(t).unwrap_or_default() >= grace);
        if !old_enough {
            continue;
        }
        match handle
            .store()
            .get_bytes(&keys::superseded_key(&claim.checksum))
            .await
        {
            Ok(None) => orphan_claims.push((claim.checksum, claim.owner, claim.token)),
            Ok(Some(_)) => takeovers.push((claim.checksum, claim.owner, claim.token)),
            Err(e) => log(format!(
                "gc: reading marker for {} failed ({e})",
                claim.checksum
            )),
        }
    }
    if !orphan_claims.is_empty() || !takeovers.is_empty() {
        // One CAS: release the retired ones, adopt the rest. `recover` is the
        // exact compare-and-remove list — it must carry BOTH kinds, or the
        // adoption below is a no-op (the stale tuple is still there and `add`
        // never overwrites an existing claim).
        let adopt: Vec<String> = takeovers.iter().map(|(c, _, _)| c.clone()).collect();
        let mut recover: Vec<(String, String, String)> = orphan_claims.clone();
        recover.extend(takeovers.iter().cloned());
        handle
            .update_reclaiming(&adopt, &[], &recover, token)
            .await
            .map_err(|e| e.to_string())?;
        adopted.extend(adopt);
        if !orphan_claims.is_empty() {
            log(format!(
                "gc: released {} claim(s) whose marker was already retired",
                orphan_claims.len()
            ));
        }
        if !takeovers.is_empty() {
            log(format!(
                "gc: took over {} stale claim(s) from dead holder(s)",
                takeovers.len()
            ));
        }
    }

    // ---- Reconcile: stamp orphans the live set no longer references --------
    //
    // A pack normally leaves the live set through a COMPACT entry, which writes
    // its marker. Some never get one: packs superseded before markers existed
    // (#175/#177), a pack whose upload was abandoned before the manifest CAS,
    // and a pack orphaned by a crash between the object PUT and the log entry.
    // The marker scan below cannot see any of them, so LIST the pack directory
    // and stamp what the *fresh* manifest does not reference.
    //
    // `superseded_at = now` is the conservative stamp: an orphan's real
    // supersession time is unknowable, and stamping "now" only ever delays
    // reclamation. Deletion still requires the full retention window, the claim
    // CAS and a liveness re-check against a fresh manifest, so an in-flight pack
    // that gets adopted meanwhile becomes live and is skipped.
    let marker_prefix = keys::superseded_key("");
    let mut markers: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let mut stream = handle.store().list(keys::SUPERSEDED_DIR, None);
        while let Some(m) = stream.next().await {
            let m = m.map_err(|e| e.to_string())?;
            if let Some(checksum) = m.key.strip_prefix(&marker_prefix) {
                markers.insert(checksum.to_string());
            }
        }
    }
    let mut marked = 0u64;
    let mut reconcile_truncated = false;
    {
        let mut stream = handle.store().list(keys::WAL_DIR, None);
        while let Some(m) = stream.next().await {
            let m = m.map_err(|e| e.to_string())?;
            let Some(rest) = m.key.strip_prefix(keys::WAL_DIR) else {
                continue;
            };
            // Markers live one segment down (`wal/_superseded/<checksum>`) and
            // are not pack objects.
            if rest.contains('/') {
                continue;
            }
            // Any side-file spelling counts: a checksum whose `.pack` is
            // already gone but whose `.idx`/`.rev`/`.bitmap`/`.commit-graph`
            // survived is exactly the garbage this reconcile is for (the
            // reclamation loop deletes whatever exists and tolerates the rest).
            let Some((checksum, ext)) = rest.split_once('.') else {
                continue;
            };
            if !WAL_OBJECT_EXTS.contains(&ext) {
                continue;
            }
            if live.contains(checksum) || markers.contains(checksum) {
                continue;
            }
            // Bound counts *successful* writes: a lost `Create` race (above)
            // costs a round trip but no marker, and the concurrent writers are
            // the publish path's COMPACT (bounded by packs it supersedes), not
            // an unbounded stream.
            if marked >= GC_MAX_MARKERS_PER_UNIT as u64 {
                reconcile_truncated = true;
                break;
            }
            let marker = SupersededPack {
                checksum: checksum.to_string(),
                superseded_at: Some(walgit_proto::time::now()),
                // No superseding COMPACT entry: 0 marks a reconciled orphan
                // (`seq` is provenance only — nothing reads it for decisions).
                seq: 0,
            };
            match handle
                .store()
                .put_bytes(
                    &keys::superseded_key(checksum),
                    marker.encode_to_vec(),
                    walgit_store::PutMode::Create,
                )
                .await
            {
                Ok(_) => {
                    markers.insert(checksum.to_string());
                    marked += 1;
                }
                // Marked concurrently — the publish path (COMPACT) or another
                // pass — after our LIST: `Create` is what keeps *its* stamp, and
                // a stamp is the age record, so overwriting would restart the
                // retention window. (The fault-injected test for this race is
                // tracked with the other suspended/taken-over-pass cases in #179;
                // no in-process test can produce it without a store hook.)
                Err(walgit_store::StoreError::PreconditionFailed { .. }) => {}
                Err(e) => return Err(format!("gc: marking orphan {checksum}: {e}")),
            }
        }
    }
    if marked > 0 || reconcile_truncated {
        log(format!(
            "gc: marked {marked} orphan object(s) for reclamation{}",
            if reconcile_truncated {
                " (more next pass)"
            } else {
                ""
            }
        ));
    }

    // Collect markers that have aged past the retention window *and* whose pack
    // is not live in that same generation. Reading each marker is one GET; the
    // listing is bounded by the number of packs a repo ever superseded within
    // the window, not by its object count. Dropping live checksums before the
    // bound below is what keeps an old marker on a re-adopted pack from
    // spending quota and starving real candidates.
    let mut candidates: Vec<(String, std::time::SystemTime)> = Vec::new();
    // The marker keys came from the reconcile's LIST above — including the ones
    // this pass just wrote (stamped `now`, so the retention check below drops
    // them). Re-listing would be one more round trip for the same set.
    //
    // A marker a *concurrent* COMPACT writes between that LIST and this scan is
    // not a candidate this pass. That can only delay it: its stamp is `now`, so
    // it was never reclaimable now, and every deletion re-reads the marker and
    // the retention from the claim-CAS manifest (plus a fresh live check), so a
    // missed candidate can never be reclaimed *earlier*. (Re-listing would not
    // close that window either — the marker can land after any LIST.)
    let scanned = markers.len() as u64;
    for checksum in &markers {
        let key = keys::superseded_key(checksum);
        let Some((_, raw)) = handle
            .store()
            .get_bytes(&key)
            .await
            .map_err(|e| e.to_string())?
        else {
            // Marker retired between the LIST and this GET.
            continue;
        };
        let marker = SupersededPack::decode(raw.as_ref()).map_err(|e| e.to_string())?;
        if live.contains(&marker.checksum) {
            continue;
        }
        let Some(at) = marker
            .superseded_at
            .as_ref()
            .map(walgit_proto::time::to_system)
        else {
            continue;
        };
        if now.duration_since(at).unwrap_or_default() <= retention {
            continue;
        }
        candidates.push((marker.checksum, at));
    }
    // Adopted checksums that did not become candidates carry no work: their
    // marker vanished between our GET and the take-over CAS, or it is not aged
    // yet. Release them now rather than refusing publishers until the next
    // `gc_interval`.
    if !adopted.is_empty() {
        let pending: std::collections::HashSet<&str> =
            candidates.iter().map(|(c, _)| c.as_str()).collect();
        let idle: Vec<String> = adopted
            .iter()
            .filter(|c| !pending.contains(c.as_str()))
            .cloned()
            .collect();
        if !idle.is_empty() {
            handle
                .update_reclaiming(&[], &idle, &[], token)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    if candidates.is_empty() {
        // A truncated reconcile is unfinished work: stay due (no gc.pb) so the
        // next pass writes the remaining markers.
        return Ok((0, 0, marked, !reconcile_truncated));
    }
    // Oldest first: a partial unit should reclaim the longest-dead packs.
    // Oldest first, checksum as the tie-break: a COMPACT entry stamps every pack
    // it drops with one `entry_time`, so ties are the norm — and a HashSet
    // iteration order would make *which* 32 a bounded pass reclaim random.
    candidates.sort_by_key(|(checksum, at)| (*at, checksum.clone()));
    // Anything past the bound is real work this pass did not do: the unit must
    // stay due, not wait a whole `gc_interval`.
    let truncated = candidates.len() > max;
    candidates.truncate(max);
    let checksums: Vec<String> = candidates.iter().map(|(c, _)| c.clone()).collect();

    // List the candidates in `Manifest.reclaiming` *before* touching any
    // object: that CAS is what orders reclamation against adoption. A publisher
    // that would put one of these checksums back into `packs` refuses while it
    // is listed (both sides go through the manifest CAS, so exactly one wins).
    // Claims another live holder already has are left to it.
    let before_claim = handle.manifest();
    let claimable: Vec<String> = checksums
        .iter()
        .filter(|c| {
            // Only claims this pass already owns are skipped: a claim held by
            // anyone else (another owner, or another token) was either taken
            // over above or is fresh and must be left alone.
            !before_claim
                .reclaiming
                .iter()
                .any(|r| &r.checksum == *c && !(r.owner == owner && r.token == token))
        })
        .cloned()
        .collect();
    drop(before_claim);
    let claim_manifest = handle
        .update_reclaiming(&claimable, &[], &[], token)
        .await
        .map_err(|e| e.to_string())?;
    // Only claims carrying *this* pass's fence are ours to delete; the helper
    // skips live checksums and anything another owner listed.
    let owned: std::collections::HashSet<String> = claim_manifest
        .reclaiming
        .iter()
        .filter(|r| r.owner == owner && r.token == token)
        .map(|r| r.checksum.clone())
        .collect();
    let live: std::collections::HashSet<String> = claim_manifest
        .packs
        .iter()
        .map(|p| p.checksum.clone())
        .collect();
    // The claim CAS is the generation every deletion below linearizes in, so
    // the retention window must be the one from *that* manifest — another
    // instance may have published new settings since our first sync.
    let retention = handle
        .effective_config_for(&claim_manifest)
        .compaction
        .retention_superseded;
    // Our own fence has a *deadline*: the recovery path may take this claim over
    // once it is older than `grace`, so a pass that has been running that long
    // must stop deleting. This is the only check a *suspended* process can rely
    // on — `lost` cannot be updated while it is stopped, and the object store
    // (S3/R2) cannot always prove that a re-created object differs from the one
    // we read. Stopping at `grace` is what keeps a stale pass from ever acting
    // on a claim a taker could already own.
    let fence_from = claim_manifest
        .reclaiming
        .iter()
        .find(|r| r.owner == owner && r.token == token)
        .and_then(|r| r.since.as_ref())
        .map(walgit_proto::time::to_system);
    let within_fence = || {
        fence_from.is_some_and(|t| {
            std::time::SystemTime::now()
                .duration_since(t)
                .unwrap_or_default()
                < grace
        })
    };

    let mut deleted = 0u64;
    let mut freed = 0u64;
    let mut all_complete = !truncated && !reconcile_truncated;
    // (checksum, marker version) for packs whose objects are all gone.
    let mut reclaimed: Vec<(String, walgit_store::Version)> = Vec::new();
    // Claims we hold but must release without deleting (the marker was
    // refreshed, or the pack turned out live).
    let mut release: Vec<String> = Vec::new();
    for (checksum, at) in candidates {
        if !owned.contains(&checksum) || live.contains(&checksum) {
            // A publisher re-adopted it before our claim, or another GC pass
            // owns it. Leave the pack and its marker alone.
            log(format!(
                "gc: {checksum} is live again or owned elsewhere — skipped"
            ));
            continue;
        }
        // Fail closed on a lease we no longer hold, and re-verify the fence in
        // the store before every destructive step: if a later pass recovered
        // this claim (or the lease moved on), our authorization is gone.
        if lost.load(Ordering::SeqCst) || !within_fence() {
            log("gc: lease lost or claim past its deadline — stopping".to_string());
            all_complete = false;
            break;
        }
        if !claim_still_ours(handle, &checksum, owner, token).await? {
            log(format!(
                "gc: claim for {checksum} is no longer ours — skipped"
            ));
            all_complete = false;
            continue;
        }
        // Re-read the marker now that we own the claim: the timestamp scanned
        // above may predate a concurrent supersession that refreshed it after
        // our read.
        let marker_key = keys::superseded_key(&checksum);
        let Some((meta, raw)) = handle
            .store()
            .get_bytes(&marker_key)
            .await
            .map_err(|e| e.to_string())?
        else {
            // Marker gone: someone else retired it; drop the claim.
            release.push(checksum.clone());
            continue;
        };
        let marker = SupersededPack::decode(raw.as_ref()).map_err(|e| e.to_string())?;
        let fresh_at = marker
            .superseded_at
            .as_ref()
            .map_or(at, walgit_proto::time::to_system);
        if now.duration_since(fresh_at).unwrap_or_default() <= retention {
            log(format!(
                "gc: {checksum} was superseded again inside the window — skipped"
            ));
            release.push(checksum.clone());
            all_complete = false;
            continue;
        }
        // Same spellings as WAL_OBJECT_EXTS (the reconcile only marks those).
        let side_files = [
            keys::pack_key(&checksum),
            keys::idx_key(&checksum),
            keys::rev_key(&checksum),
            keys::bitmap_key(&checksum),
            keys::commit_graph_key(&checksum),
        ];
        let mut complete = true;
        for key in &side_files {
            if lost.load(Ordering::SeqCst) || !within_fence() {
                log("gc: lease lost or claim past its deadline — stopping".to_string());
                complete = false;
                all_complete = false;
                break;
            }
            match handle.store().head(key).await {
                Ok(Some(meta)) => {
                    // Re-verify the fence per object: a pass that was suspended
                    // long enough for its lease to lapse cannot see `lost`
                    // update while it is stopped, and by the time it resumes
                    // another pass may have adopted the claim, finished the
                    // reclamation and let a publisher re-adopt the checksum.
                    // Re-checking here (plus the version taken above) closes
                    // both orders: an earlier re-upload fails this check, a
                    // later one fails the version-conditioned delete.
                    if !within_fence() {
                        log(format!(
                            "gc: claim for {checksum} is past its deadline — leaving {key}"
                        ));
                        complete = false;
                        all_complete = false;
                        break;
                    }
                    if !claim_still_ours(handle, &checksum, owner, token).await? {
                        log(format!("gc: claim for {checksum} moved on — leaving {key}"));
                        complete = false;
                        all_complete = false;
                        break;
                    }
                    // …and once more *after* that round trip: the check above is
                    // itself an await, and a pass suspended across it (or across
                    // the deadline) must not go on to delete. What remains is the
                    // store's own HEAD→DELETE, which is the accepted
                    // lease-guarded check-then-act window on S3.
                    if lost.load(Ordering::SeqCst) || !within_fence() {
                        log(format!(
                            "gc: claim for {checksum} passed its deadline — leaving {key}"
                        ));
                        complete = false;
                        all_complete = false;
                        break;
                    }
                    freed += meta.size;
                    match handle.store().delete(key, Some(meta.version)).await {
                        Ok(()) | Err(walgit_store::StoreError::NotFound { .. }) => {}
                        Err(walgit_store::StoreError::PreconditionFailed { .. }) => {
                            log(format!("gc: {key} changed under us — leaving it"));
                            complete = false;
                        }
                        Err(e) => return Err(format!("gc: delete {key}: {e}")),
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    // A transient HEAD error must not silently strand the pack:
                    // leave the marker so the next pass retries.
                    log(format!("gc: head {key} failed ({e}) — leaving the marker"));
                    complete = false;
                }
            }
        }
        if !complete {
            // The marker is the only record that this pack is garbage: keep it
            // until every object is gone. The claim stays too, so the pack is
            // still listed against re-adoption.
            all_complete = false;
            continue;
        }
        log(format!(
            "gc: reclaimed {checksum} (superseded {:.1}h ago)",
            now.duration_since(fresh_at)
                .unwrap_or_default()
                .as_secs_f64()
                / 3600.0
        ));
        reclaimed.push((checksum, meta.version));
        deleted += 1;
    }
    if !reclaimed.is_empty() || !release.is_empty() {
        // Retire the markers *while the claim is still held*. The claim stops
        // the checksum re-entering `packs`, and `publish_compact_impl` skips
        // marker writes for claimed checksums — so no fresh marker can appear
        // under us. That is what makes the retire safe even on a backend whose
        // conditional delete is HEAD+compare+DELETE (S3), where a release-then-
        // delete order could drop a marker written in the gap.
        for (checksum, version) in &reclaimed {
            if lost.load(Ordering::SeqCst) || !within_fence() {
                all_complete = false;
                break;
            }
            // The retire is destructive too: re-verify the fence immediately
            // before it, so a claim a newer pass took over cannot be retired by
            // this stale one.
            if !claim_still_ours(handle, checksum, owner, token).await? {
                log(format!(
                    "gc: claim for {checksum} moved on — leaving its marker"
                ));
                all_complete = false;
                continue;
            }
            // Re-check after that await, for the same reason as the object delete.
            if lost.load(Ordering::SeqCst) || !within_fence() {
                log(format!(
                    "gc: claim for {checksum} passed its deadline — leaving its marker"
                ));
                all_complete = false;
                break;
            }
            match handle
                .store()
                .delete(&keys::superseded_key(checksum), Some(version.clone()))
                .await
            {
                Ok(()) | Err(walgit_store::StoreError::NotFound { .. }) => {}
                Err(walgit_store::StoreError::PreconditionFailed { .. }) => {
                    // A marker that outlived our claim generation (e.g. written
                    // by an older pass): leave it for the next unit.
                    log(format!(
                        "gc: marker {checksum} changed — leaving it for the next pass"
                    ));
                    all_complete = false;
                }
                Err(e) => return Err(format!("gc: delete marker {checksum}: {e}")),
            }
        }
        // Release last: a released claim with the marker gone would let a
        // publisher re-adopt the checksum with no record of what happened.
        if lost.load(Ordering::SeqCst) || !within_fence() {
            // A pass that lost its lease (or ran past its own claim deadline)
            // releases nothing: the claim is left for the age-fenced recovery.
            all_complete = false;
        } else {
            let mut release_claims: Vec<String> = release.clone();
            release_claims.extend(reclaimed.iter().map(|(c, _)| c.clone()));
            handle
                .update_reclaiming(&[], &release_claims, &[], token)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    if scanned > 0 {
        log(format!(
            "gc: scanned {scanned} marker(s), reclaimed {deleted}, freed {freed} bytes"
        ));
    }
    Ok((deleted, freed, marked, all_complete))
}

/// Is `checksum` still listed under *this* pass's fence in the store? A later
/// pass that recovered the claim (age fence + dead holder) or a lost lease
/// makes this false, and the caller must not delete under it anymore (#175).
async fn claim_still_ours(
    handle: &RepoHandle,
    checksum: &str,
    owner: &str,
    token: &str,
) -> Result<bool, String> {
    use prost::Message;
    let Some((_, raw)) = handle
        .store()
        .get_bytes(walgit_proto::keys::MANIFEST)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(false);
    };
    let manifest = walgit_proto::v1::Manifest::decode(raw.as_ref()).map_err(|e| e.to_string())?;
    Ok(manifest
        .reclaiming
        .iter()
        .any(|r| r.checksum == checksum && r.owner == owner && r.token == token))
}

/// Upper bound on packs reclaimed per GC unit: a unit must stay bounded so one
/// pass cannot monopolise a maintainer (D22).
const GC_MAX_PACKS_PER_UNIT: usize = 32;

/// Orphan *markers* one GC pass may write. Markers are tiny and writing one is
/// a single conditional PUT, but a repo that predates the marker scheme (#177)
/// has hundreds of orphans: bounding the pass keeps the unit inside its lease
/// and makes it repeat until the reconcile is complete, instead of blocking the
/// loop on one repo.
pub const GC_MAX_MARKERS_PER_UNIT: usize = 512;

/// `wal/<checksum>.<ext>` spellings the reclamation loop deletes. The reconcile
/// only marks these: a key it cannot delete would be marked, skipped by the
/// delete loop and marked again on the next pass — forever. Keep both sides in
/// sync (the delete loop below spells them out as `keys::*_key`).
const WAL_OBJECT_EXTS: [&str; 5] = ["pack", "idx", "rev", "bitmap", "commit-graph"];

/// The last bucket-GC pass of `handle`'s repository, if any (#175).
pub async fn read_gc(handle: &RepoHandle) -> Result<Option<walgit_proto::v1::GcReport>, String> {
    use walgit_store::ObjectStoreExt;
    match handle.store().get_bytes(walgit_proto::keys::GC).await {
        Ok(Some((_, bytes))) => walgit_proto::v1::GcReport::decode(bytes.as_ref())
            .map(Some)
            .map_err(|e| e.to_string()),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn flag(params: &HashMap<String, String>, key: &str) -> bool {
    params
        .get(key)
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

async fn run(
    state: &Arc<AppState>,
    id: &RepoId,
    op: &str,
    params: &HashMap<String, String>,
    log: Log<'_>,
) -> Result<(String, serde_json::Value), String> {
    let handle = state.registry.open(id).await.map_err(|e| e.to_string())?;
    match op {
        "fsck" => {
            let connectivity = flag(params, "connectivity");
            let guard = handle.sync().await.map_err(|e| e.to_string())?;
            let seq = handle.applied_seq();
            log(format!(
                "local copy at seq {} (manifest head {}), running git fsck{}",
                seq,
                handle.manifest().head_seq,
                if connectivity {
                    " --connectivity-only"
                } else {
                    " --full --strict"
                }
            ));
            let t0 = Instant::now();
            let mut lines = 0u64;
            let mut missing: Vec<String> = Vec::new();
            let report = handle
                .local()
                .fsck_streaming(connectivity, |l| {
                    lines += 1;
                    // `missing blob <oid>` / `missing tree <oid>` …
                    if let Some(rest) = l.strip_prefix("missing ")
                        && let Some(oid) = rest.split_whitespace().nth(1)
                        && oid.len() >= 40
                    {
                        missing.push(oid.to_string());
                    }
                    log(l);
                })
                .await
                .map_err(|e| e.to_string())?;
            drop(guard);
            missing.sort_unstable();
            missing.dedup();
            // The audit result lives in the bucket (not the WAL): the repair unit and
            // the gauge read it; every host sees the same verdict.
            let fsck = walgit_proto::v1::FsckReport {
                seq,
                at: Some(walgit_proto::time::now()),
                host: crate::maintain::host_name(state),
                missing_total: missing.len() as u64,
                missing: missing
                    .iter()
                    .take(FSCK_MISSING_LIST_MAX)
                    .cloned()
                    .collect(),
                problems: report.problems,
                elapsed_secs: t0.elapsed().as_secs_f64(),
                repaired_seq: 0,
            };
            handle
                .store()
                .put_bytes(
                    walgit_proto::keys::FSCK,
                    fsck.encode_to_vec(),
                    walgit_store::PutMode::Overwrite,
                )
                .await
                .map_err(|e| format!("writing fsck.pb: {e}"))?;
            // f64 is the metrics-gauge contract; missing-object counts are ≪ 2^53.
            #[allow(
                clippy::cast_precision_loss,
                reason = "f64 is the metrics-gauge contract; missing-object counts ≪ 2^53"
            )]
            metrics::gauge!("walgit_repo_missing_objects", "repo" => id.to_string())
                .set(missing.len() as f64);
            tracing::info!(repo = %id, seq, missing = missing.len(), problems = report.problems, elapsed_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX), "fsck recorded");
            let summary = if report.ok {
                format!(
                    "fsck clean ({lines} lines, {:.0}s)",
                    t0.elapsed().as_secs_f64()
                )
            } else {
                format!(
                    "fsck found {} problem(s) ({} missing object(s)), exit {:?}",
                    report.problems,
                    missing.len(),
                    report.exit_code
                )
            };
            let value = serde_json::json!({"ok": report.ok, "problems": report.problems, "missing": missing.len(), "seq": seq});
            // Missing objects are a *finding*, not a failure of the unit: the repair
            // unit is the response (plan shows it). Corrupt objects stay a failure.
            if report.ok || !missing.is_empty() {
                Ok((summary, value))
            } else {
                Err(summary)
            }
        }
        "repair" => {
            // Desired state: every object reachable from refs is in a live pack.
            // Input: fsck.pb's missing list (the audit); source: upstream.git
            // (GitHub serves blob/tree wants by SHA); output: one pack published as
            // a COMPACT entry superseding nothing (exactly what `wal add-pack --tier 0`
            // did by hand for a large repository's 1,952 blobs, the original large-repository measurements).
            let cfg = handle.effective_config();
            let upstream = cfg
                .upstream
                .git
                .clone()
                .ok_or("repair: no upstream.git for this repository")?;
            let fsck = read_fsck(&handle)
                .await?
                .ok_or("repair: no fsck.pb (run fsck first)")?;
            if fsck.missing.is_empty() {
                return Ok((
                    "nothing to repair".into(),
                    serde_json::json!({"missing": 0}),
                ));
            }
            if usize::try_from(fsck.missing_total).unwrap_or(usize::MAX) > fsck.missing.len() {
                log(format!(
                    "fsck listed {} of {} missing objects; repairing those, the next fsck finds the rest",
                    fsck.missing.len(),
                    fsck.missing_total
                ));
            }
            let token = match cfg.upstream.token_env.as_deref() {
                Some(name) => Some(
                    state
                        .lfs_upstream
                        .secret(name)
                        .map_err(|e| format!("upstream token: {e}"))?,
                ),
                None => None,
            };
            let t0 = Instant::now();
            log(format!(
                "fetching {} object(s) from {upstream}",
                fsck.missing.len()
            ));
            let pack = walgit_git::repair::fetch_objects_as_pack(
                &upstream,
                token.as_deref(),
                &fsck.missing,
                &state.cfg.cache.dir.join("repair"),
            )
            .await
            .map_err(|e| format!("repair fetch: {e}"))?;
            log(format!(
                "packed {} object(s), {} bytes in {:.1}s; publishing",
                pack.objects,
                pack.bytes,
                t0.elapsed().as_secs_f64()
            ));
            let seq = handle
                .add_pack(&pack.pack, &pack.idx, 0, None)
                .await
                .map_err(|e| format!("publish: {e}"))?;
            let _ = tokio::fs::remove_dir_all(&pack.dir).await;
            // Record the repair on the audit so the unit is not due again until the
            // next fsck re-verifies (it will: the plan compares seqs).
            let done = walgit_proto::v1::FsckReport {
                repaired_seq: seq,
                ..fsck
            };
            handle
                .store()
                .put_bytes(
                    walgit_proto::keys::FSCK,
                    done.encode_to_vec(),
                    walgit_store::PutMode::Overwrite,
                )
                .await
                .map_err(|e| format!("writing fsck.pb: {e}"))?;
            metrics::counter!("walgit_repair_objects_total", "repo" => id.to_string())
                .increment(pack.objects);
            tracing::info!(repo = %id, seq, objects = pack.objects, bytes = pack.bytes, %upstream, elapsed_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX), "repair published");
            Ok((
                format!(
                    "repaired {} object(s) ({} bytes) from upstream at seq {seq}",
                    pack.objects, pack.bytes
                ),
                serde_json::json!({"seq": seq, "objects": pack.objects, "bytes": pack.bytes}),
            ))
        }
        "follow" => crate::follow::op(state, &handle, id, params, log).await,
        "rev-index" => {
            // Desired state: every pack in the manifest advertises a `.rev`.
            let checksum = params
                .get("pack")
                .cloned()
                .ok_or("rev-index: missing `pack` (checksum)")?;
            let oid = gix_hash::ObjectId::from_hex(checksum.as_bytes())
                .map_err(|e| format!("rev-index: bad checksum {checksum}: {e}"))?;
            let t0 = Instant::now();
            let rev = handle
                .local()
                .write_rev_index(&oid)
                .await
                .map_err(|e| format!("rev-index: {e}"))?;
            let bytes = std::fs::metadata(&rev).map_or(0, |m| m.len());
            log(format!(
                "pack-{checksum}.rev: {bytes} bytes in {:.1}s; publishing",
                t0.elapsed().as_secs_f64()
            ));
            handle
                .annotate_pack(&checksum, Some(rev), None, None)
                .await
                .map_err(|e| format!("rev-index publish: {e}"))?;
            tracing::info!(repo = %id, pack = %checksum, bytes, elapsed_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX), "rev index published");
            Ok((
                format!("pack-{checksum}.rev ({bytes} bytes) published"),
                serde_json::json!({"pack": checksum, "bytes": bytes}),
            ))
        }
        "gc" => {
            // Fresh refs before anything: the lease TTL and the retention window
            // are per-repo settings (D24), and a direct `POST /ops/gc` bypasses
            // the maintainer's own sync — a cached handle must not hand out a
            // lease (or a window) sized by a superseded settings revision.
            let manifest = handle
                .sync_refs_fresh()
                .await
                .map_err(|e| e.to_string())?;
            let effective = handle.effective_config_for(manifest.as_ref());
            // Per-repo lease: two GC passes must not interleave. Claims carry
            // an owner+token fence, so a later pass cannot touch ours while we
            // still hold it — but the lease is still what keeps two passes from
            // doing the same work, so it is *renewed* for the whole run.
            let lease_ttl = effective.compaction.lease_ttl;
            // Guard against a mis-set (e.g. zero) TTL: a lease nobody can renew
            // in time is worse than no GC at all.
            let lease_ttl = lease_ttl.max(std::time::Duration::from_secs(30));
            let lease_store: walgit_store::DynStore = Arc::new(handle.store().clone());
            let lease = walgit_store::coord::try_acquire(
                lease_store,
                &walgit_proto::keys::lease_key("gc"),
                walgit_store::coord::instance_id(),
                "gc",
                lease_ttl,
            )
            .await
            .map_err(|e| e.to_string())?;
            let Some(lease) = lease else {
                return Ok((
                    "gc: lease held by another instance".to_string(),
                    serde_json::json!({"skipped": "lease-held"}),
                ));
            };
            let lease = Arc::new(tokio::sync::Mutex::new(lease));
            // A per-pass fence: every claim this pass lists carries `token` and
            // is re-checked before each delete, so a later pass that recovers a
            // claim (or takes the lease) cannot have its work conflated with
            // ours. `lost` is set the moment a heartbeat finds the lease gone.
            let token = uuid::Uuid::new_v4().to_string();
            let owner = walgit_store::coord::instance_id().to_string();
            let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
            let heartbeat = walgit_store::coord::LeaseGuard::spawn_heartbeat_watched(
                lease.clone(),
                lease_ttl / 3,
                lease_ttl,
                stop_rx,
                lost.clone(),
            );
            // Clamped: the caller may not widen a unit past the bound the
            // lease/reporting semantics were sized for.
            let max = params
                .get("max")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(GC_MAX_PACKS_PER_UNIT)
                .min(GC_MAX_PACKS_PER_UNIT);
            // Folded WAL objects first: their pass is independent of the pack
            // claims, and running it before the pack phase keeps it from being
            // starved whenever the pack side reports "incomplete".
            let retention_wal = effective.maintenance.retention_wal;
            let wal_outcome = gc_wal_objects(
                &handle,
                manifest,
                GC_MAX_WAL_OBJECTS_PER_UNIT,
                retention_wal,
                &lost,
                log,
            )
            .await;
            let outcome =
                gc_superseded_packs(&handle, max, lease_ttl, &owner, &token, &lost, log).await;
            // Stop renewing gracefully (never abort: a cancelled heartbeat whose
            // remote CAS already landed leaves our version stale, and the
            // release would then silently miss), then release through the guard.
            let _ = stop_tx.send(true);
            let _ = heartbeat.await;
            if let Ok(mutex) = Arc::try_unwrap(lease)
                && let Err(e) = mutex.into_inner().release().await
            {
                log(format!("gc: lease release failed: {e}"));
            }
            let (wal_objects, wal_bytes, wal_complete) = wal_outcome?;
            let (packs, bytes, marked, complete) = outcome?;
            let complete = complete && wal_complete;
            if !complete {
                // Something could not be deleted: the claims stay (correct — the
                // pack is still there), so do NOT write gc.pb. The unit stays
                // due and the next pass retries instead of leaving the checksum
                // refused for a whole gc_interval.
                return Ok((
                    format!(
                        "gc: incomplete ({packs} packs reclaimed, {marked} marked, {wal_objects} folded WAL objects); retrying next pass"
                    ),
                    serde_json::json!({
                        "packs": packs, "bytes": bytes, "marked": marked,
                        "wal_objects": wal_objects, "wal_bytes": wal_bytes, "complete": false
                    }),
                ));
            }
            let report = walgit_proto::v1::GcReport {
                at: Some(walgit_proto::time::now()),
                packs,
                bytes,
                host: crate::maintain::host_name(state),
            };
            handle
                .store()
                .put_bytes(
                    walgit_proto::keys::GC,
                    report.encode_to_vec(),
                    walgit_store::PutMode::Overwrite,
                )
                .await
                .map_err(|e| format!("writing gc.pb: {e}"))?;
            let mut summary = match (packs, marked) {
                (0, 0) => "gc: nothing superseded past retention".to_string(),
                (0, m) => format!("gc: marked {m} orphan object(s); none past retention yet"),
                (p, 0) => format!("gc: {p} superseded pack(s), {bytes} bytes freed"),
                (p, m) => {
                    format!("gc: {p} superseded pack(s), {bytes} bytes freed; marked {m} orphan(s)")
                }
            };
            if wal_objects > 0 {
                let _ = write!(
                    summary,
                    "; {wal_objects} folded WAL object(s), {wal_bytes} bytes freed"
                );
            }
            tracing::info!(repo = %id, packs, bytes, marked, wal_objects, wal_bytes, "bucket gc");
            Ok((
                summary,
                serde_json::json!({
                    "packs": packs, "bytes": bytes, "marked": marked,
                    "wal_objects": wal_objects, "wal_bytes": wal_bytes
                }),
            ))
        }

        "compact" => {
            let force = flag(params, "force");
            let base = flag(params, "base");
            let out = compact_repo(
                &handle,
                &state.cfg,
                CompactRequest {
                    force,
                    rebuild_base: base,
                },
                log,
            )
            .await
            .map_err(|e| e.to_string())?;
            let summary = out.summary();
            Ok((summary, serde_json::to_value(&out).unwrap_or_default()))
        }
        "bundle" => {
            if !state.cfg.bundles.enabled {
                return Err("bundles are disabled in config".into());
            }
            let strategy = params.get("strategy").cloned().unwrap_or_default();
            if let Some(slot) = params.get("slot").and_then(|v| v.parse::<u64>().ok()) {
                // One calendar slot (the maintenance loop's unit): content as of the slot.
                log(format!(
                    "building {strategy} slot {slot} ({})",
                    walgit_bundle::slots::from_epoch(slot)
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs())
                ));
                // A FULL slot of a repository that has a tier-2 base is a compose
                // of that base (header = refs at the base's seq) — never a
                // `pack-objects` of the whole history into a file (a large repository: 32 GB
                // through this host). The maintainer rebuilds the base first on
                // an ssd host when pushes landed since (`Unit::BaseRebuild`).
                let cfg_eff = handle.effective_config();
                let is_full = cfg_eff
                    .bundles
                    .strategy
                    .iter()
                    .any(|s| s.name == strategy && s.kind == walgit_config::BundleKind::Full);
                if is_full && walgit_wal::base_pack(&handle.manifest()).is_some() {
                    log(format!(
                        "{strategy} slot {slot}: composing header ∘ tier-2 base (no bytes through this host)"
                    ));
                    let e = crate::bundles::compose_full_from_base(
                        &state.registry,
                        id,
                        &strategy,
                        &cfg_eff,
                        slot,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    state.caches.bundle_list.invalidate(&id.to_string());
                    return Ok((
                        format!(
                            "slot {} {} composed: {} bytes at seq {}, token {}",
                            e.strategy, slot, e.size, e.seq, e.creation_token
                        ),
                        serde_json::json!({ "id": e.id, "strategy": e.strategy, "slot": slot, "size": e.size, "seq": e.seq, "key": e.key, "built": true, "composed": true }),
                    ));
                }
                let entry = state
                    .bundles
                    .build_slot_unit(id, &strategy, slot)
                    .await
                    .map_err(|e| e.to_string())?;
                // The list changed (or a skip was recorded): this host's next
                // `GET bundles/list` must show it, not a cached render.
                state.caches.bundle_list.invalidate(&id.to_string());
                return Ok(match entry {
                    Some(e) => (
                        format!(
                            "slot {} {} built: {} bytes at seq {}, token {}",
                            e.strategy, slot, e.size, e.seq, e.creation_token
                        ),
                        serde_json::json!({ "id": e.id, "strategy": e.strategy, "slot": slot, "size": e.size, "seq": e.seq, "key": e.key, "built": true }),
                    ),
                    None => (
                        format!(
                            "slot {strategy} {slot}: nothing to build (built elsewhere, no new objects, or no refs at that time)"
                        ),
                        serde_json::json!({ "slot": slot, "built": false }),
                    ),
                });
            }
            if strategy == "due" {
                log("building all due bundle strategies".into());
                let entries = state
                    .bundles
                    .run_due(id, std::time::SystemTime::now())
                    .await
                    .map_err(|e| e.to_string())?;
                for e in &entries {
                    log(format!(
                        "built {} ({} bytes, token {})",
                        e.strategy, e.size, e.creation_token
                    ));
                }
                state.caches.bundle_list.invalidate(&id.to_string());
                let names: Vec<String> = entries.iter().map(|e| e.strategy.clone()).collect();
                return Ok((
                    format!("built {} due bundle(s)", entries.len()),
                    serde_json::json!({ "built": names }),
                ));
            }
            let strategy = if strategy.is_empty() {
                state
                    .cfg
                    .bundles
                    .strategy
                    .iter()
                    .find(|s| s.kind == walgit_config::BundleKind::Full)
                    .or(state.cfg.bundles.strategy.first())
                    .map(|s| s.name.clone())
                    .ok_or_else(|| "no bundle strategies configured".to_string())?
            } else {
                strategy
            };
            log(format!(
                "building bundle strategy {strategy} (git bundle create on the local copy, upload, CAS list)"
            ));
            let entry = state
                .bundles
                .build(id, &strategy)
                .await
                .map_err(|e| e.to_string())?;
            state.caches.bundle_list.invalidate(&id.to_string());
            let summary = format!(
                "bundle {} built: {} bytes at seq {}, creationToken {}",
                entry.strategy, entry.size, entry.seq, entry.creation_token
            );
            Ok((
                summary,
                serde_json::json!({
                    "id": entry.id, "strategy": entry.strategy, "kind": entry.kind,
                    "size": entry.size, "seq": entry.seq, "creation_token": entry.creation_token,
                    "key": entry.key,
                }),
            ))
        }
        "checkpoint" => {
            // Refs-level: a checkpoint is manifest + ref snapshot, it never
            // needs the packs on this instance (works for a large repository on a front).
            let guard = handle.sync_refs().await.map_err(|e| e.to_string())?;
            drop(guard);
            log(format!(
                "writing checkpoint at seq {}",
                handle.manifest().head_seq
            ));
            let cp = handle.write_checkpoint().await.map_err(|e| e.to_string())?;
            Ok((
                format!("checkpoint written at seq {}", cp.seq),
                serde_json::json!({ "at_seq": cp.seq }),
            ))
        }
        "sync" => {
            let before = handle.applied_seq();
            let guard = handle.sync().await.map_err(|e| e.to_string())?;
            drop(guard);
            let after = handle.applied_seq();
            let summary = format!(
                "synced: local seq {before} → {after}, manifest {}",
                handle
                    .manifest_version()
                    .map(|v| v.to_string())
                    .unwrap_or_default()
            );
            Ok((
                summary,
                serde_json::json!({ "before": before, "after": after }),
            ))
        }
        "rematerialize" => {
            log("discarding local copy and rebuilding from the store".into());
            handle.rematerialize().await.map_err(|e| e.to_string())?;
            Ok((
                format!("re-materialized at seq {}", handle.applied_seq()),
                serde_json::json!({ "seq": handle.applied_seq() }),
            ))
        }
        _ => Err("unknown op".into()),
    }
}

// ---------------------------------------------------------------------------
// Compaction (shared with the serve loop and the CLI)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct CompactRequest {
    /// Ignore the trigger thresholds.
    pub force: bool,
    /// Force a full base rebuild (one pack + bitmap).
    pub rebuild_base: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CompactOutcome {
    NotTriggered {
        tier0_packs: usize,
        tier0_bytes: u64,
    },
    LeaseHeld,
    Published {
        rebuild_base: bool,
        tier: u32,
        packs: Vec<String>,
        superseded: usize,
    },
}

impl CompactOutcome {
    pub fn summary(&self) -> String {
        match self {
            CompactOutcome::NotTriggered {
                tier0_packs,
                tier0_bytes,
            } => format!(
                "compaction not triggered ({tier0_packs} fresh packs, {tier0_bytes} bytes); use force=1"
            ),
            CompactOutcome::LeaseHeld => "compaction lease held by another instance".into(),
            CompactOutcome::Published {
                rebuild_base,
                tier,
                packs,
                superseded,
            } => format!(
                "{} published: {} pack(s) at tier {tier}, superseding {superseded}",
                if *rebuild_base {
                    "base rebuild"
                } else {
                    "geometric compaction"
                },
                packs.len()
            ),
        }
    }
}

/// Decide whether `handle` needs compaction, take the per-repo lease, repack
/// Whether the compaction trigger fires for `handle` (same rule as
/// [`compact_repo`] without `force`; base rebuilds are the VM job's on tmpfs hosts).
pub fn compaction_triggered(handle: &RepoHandle, cfg: &Config) -> bool {
    let manifest = handle.manifest();
    let tier0: Vec<_> = manifest.packs.iter().filter(|p| p.tier == 0).collect();
    let tier0_bytes: u64 = tier0.iter().map(|p| p.pack_size).sum();
    fold_due(tier0.len(), tier0_bytes, cfg)
}

/// Geometric folding is due when the fresh tier is over its count or byte trigger **and there is
/// something to fold**: one pack folds into itself (`git repack --geometric` writes nothing), so a
/// single big tier-0 pack — an import that never became a base — must not make every maintainer
/// pass run a 5 s no-op compaction (acme/large, 11.9 GB, 2026-08-22).
pub fn fold_due(tier0_count: usize, tier0_bytes: u64, cfg: &Config) -> bool {
    tier0_count >= 2
        && (tier0_count >= cfg.compaction.trigger_packs
            || tier0_bytes >= cfg.compaction.trigger_bytes.as_u64())
}

/// the local copy and publish the result as a COMPACT entry.
pub async fn compact_repo(
    handle: &RepoHandle,
    cfg: &Config,
    req: CompactRequest,
    log: Log<'_>,
) -> anyhow::Result<CompactOutcome> {
    // Sync to get the latest manifest, then release the read guard: the
    // publisher needs the repo lock and repack runs on the local copy anyway.
    // A base rebuild rewrites every byte, so it needs real local copies (never
    // a mount-linked base); geometric folding only touches tiers < 2.
    if req.rebuild_base {
        drop(handle.sync_full().await?);
    } else {
        drop(handle.sync().await?);
    }

    let manifest = handle.manifest();
    let tier0_packs: Vec<_> = manifest.packs.iter().filter(|p| p.tier == 0).collect();
    let tier0_count = tier0_packs.len();
    let tier0_bytes: u64 = tier0_packs.iter().map(|p| p.pack_size).sum();
    let base_bytes: u64 = manifest
        .packs
        .iter()
        .filter(|p| p.tier == 2)
        .map(|p| p.pack_size)
        .sum();
    let non_base_bytes: u64 = manifest
        .packs
        .iter()
        .filter(|p| p.tier < 2)
        .map(|p| p.pack_size)
        .sum();
    // The base (tier 2, one pack + bitmap) is rebuilt only when asked — the weekly slot's
    // `BaseRebuild` unit on the ssd host or `walgit compact --base` (AGENTS §2.5) — never by a
    // ratio inside the fold unit: on 2026-08-22 a redundant second full pack in a large repository's manifest
    // made "non-base ≥ 0.5 × base" true forever and every Compact unit ran a 30-min `repack -adb`
    // (7 × 32 GB packs in the bucket). Otherwise fold fresh packs geometrically into the medium tier.
    let rebuild_base = req.rebuild_base;
    let should_compact = req.force || fold_due(tier0_count, tier0_bytes, cfg) || rebuild_base;
    log(format!(
        "{} live packs: {tier0_count} fresh ({tier0_bytes} bytes), base {base_bytes} bytes, non-base {non_base_bytes} bytes; rebuild_base={rebuild_base}",
        manifest.packs.len()
    ));
    if !should_compact {
        return Ok(CompactOutcome::NotTriggered {
            tier0_packs: tier0_count,
            tier0_bytes,
        });
    }

    // Per-repo lease (the store handle is prefixed with the repo key).
    let lease_key = walgit_proto::keys::lease_key("compact");
    let holder = walgit_store::coord::instance_id();
    let lease_store: walgit_store::DynStore = Arc::new(handle.store().clone());
    let lease = walgit_store::coord::try_acquire(
        lease_store,
        &lease_key,
        holder,
        "compact",
        cfg.compaction.lease_ttl,
    )
    .await?;
    let Some(lease) = lease else {
        return Ok(CompactOutcome::LeaseHeld);
    };

    // A geometric fold never touches the base or a history pack (D18): both are `--keep-pack`'d.
    // On 2026-08-22 a fold on the SSD host (every pack a real local file) rolled a large repository's 32 GB base
    // and its 6 GB history pack into a tier-1 pack (seq 101) — no history pack for a day, and
    // the chain of consequences above.
    let protected: Vec<gix_hash::ObjectId> = manifest
        .packs
        .iter()
        .filter(|p| p.tier == 2 || p.kind == walgit_proto::v1::PackKind::History as i32)
        .filter_map(|p| gix_hash::ObjectId::from_hex(p.checksum.as_bytes()).ok())
        .collect();
    // Base rebuild: resumable, in a scratch copy, serving copy never rewritten (`rebuild.rs`,
    // BUNDLE_URI_DESIGN §5a). It installs + publishes itself; the lease is ours until it returns.
    if rebuild_base {
        log("lease acquired; base rebuild in a scratch copy (resumable)".to_string());
        let out = crate::rebuild::rebuild_base(handle, cfg, log).await;
        if let Err(e) = lease.release().await {
            log(format!("lease release failed: {e}"));
        }
        let out = out?;
        if out.resumed {
            log("rebuild resumed an earlier interrupted run".to_string());
        }
        return Ok(CompactOutcome::Published {
            rebuild_base: true,
            tier: 2,
            packs: out.packs,
            superseded: out.superseded,
        });
    }

    let repack_opts = RepackOptions {
        mode: RepackMode::Geometric {
            factor: cfg.compaction.factor,
        },
        write_bitmap: false,
        write_midx: true,
        keep: protected,
    };
    let tier = 1u32;
    log("lease acquired; running git repack -d --geometric --write-midx".to_string());
    let t = Instant::now();
    // repack installs packs under final names before publish_compact CASes
    // them; keep the prune lock for the whole install → publish window.
    let _prune_guard = handle.prune_guard().await;
    let result = match handle.local().repack(repack_opts).await {
        Ok(r) => r,
        Err(e) => {
            let _ = lease.release().await;
            return Err(e.into());
        }
    };
    log(format!(
        "repack done in {:.1}s: {} new pack(s), {} removed",
        t.elapsed().as_secs_f64(),
        result.new_packs.len(),
        result.removed.len()
    ));

    // Geometric: the new pack(s) supersede exactly what git removed — of the packs the manifest
    // lists (a stale local file nobody advertises is not a supersede).
    let live: std::collections::HashSet<String> =
        manifest.packs.iter().map(|p| p.checksum.clone()).collect();
    let supersedes: Vec<gix_hash::ObjectId> = result
        .removed
        .iter()
        .copied()
        .filter(|c| live.contains(&c.to_hex().to_string()))
        .collect();
    let superseded = supersedes.len();
    let mut supersedes_left = Some(supersedes);
    let mut packs = Vec::new();
    let mut first_err = None;
    // Repack installed these packs under final names before publish_compact
    // CASes them into the manifest; protect them from a concurrent prune.
    let _staged_packs: Vec<_> = result
        .new_packs
        .iter()
        .filter_map(|p| handle.stage_pack_guard(&p.checksum.to_hex().to_string()))
        .collect();
    for p in &result.new_packs {
        let hex = p.checksum.to_hex().to_string();
        let size = p.pack_size;
        match handle
            .publish_compact(p.clone(), supersedes_left.take().unwrap_or_default(), tier)
            .await
        {
            Ok(seq) => {
                log(format!("published pack {hex} ({size} bytes) as seq {seq}"));
                packs.push(hex);
            }
            Err(e) => {
                log(format!("publish_compact failed for {hex}: {e}"));
                first_err.get_or_insert(e);
            }
        }
    }
    if let Err(e) = lease.release().await {
        log(format!("lease release failed: {e}"));
    }
    if let Some(e) = first_err {
        return Err(e.into());
    }
    Ok(CompactOutcome::Published {
        rebuild_base: false,
        tier,
        packs,
        superseded,
    })
}

#[cfg(test)]
mod fold_tests {
    use super::fold_due;

    #[test]
    fn one_fresh_pack_never_triggers_folding_however_large() {
        let cfg = walgit_config::Config::default(); // trigger_packs 16, trigger_bytes 1 GiB
        assert!(
            !fold_due(1, 11_891_739_367, &cfg),
            "a single 11.9 GB import pack folds into itself"
        );
        assert!(!fold_due(0, 0, &cfg));
        assert!(
            fold_due(2, 2 << 30, &cfg),
            "two packs over the byte trigger"
        );
        assert!(fold_due(16, 1024, &cfg), "count trigger");
        assert!(!fold_due(15, 1024, &cfg));
    }
}
