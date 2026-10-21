//! `sync()` implementation: freshness check, catch-up, materialization.

use std::sync::Arc;

use crate::error::WalError;
use crate::store_proto::{get_message, get_message_if_changed};
use tracing::Instrument;
use walgit_git::LocalRepo;
use walgit_proto::keys;
use walgit_proto::v1::{EntryKind, LogEntry, Manifest, PackRef, RefSnapshot};
use walgit_store::{GetOptions, GetResult, ObjectStore, Prefixed, Version};

/// A read guard held for the lifetime of a request. While any guard is alive
/// no pack is removed locally (the inner `RwLock` read guard prevents it).
pub struct ReadGuard<'a> {
    pub(crate) _guard: tokio::sync::RwLockReadGuard<'a, ()>,
    pub(crate) handle: &'a super::handle::RepoHandle,
}

impl ReadGuard<'_> {
    pub fn manifest(&self) -> Arc<Manifest> {
        self.handle.manifest.read().clone()
    }
    pub fn local(&self) -> &LocalRepo {
        &self.handle.local
    }
}

/// How much of the WAL a sync must bring to the local copy.
///
/// `Refs` applies the checkpoint ref snapshot and every log entry's ref
/// transaction but downloads no packs: enough for `info/refs`, `ls-refs`,
/// `bundle-uri` and the web `refs` endpoint, i.e. everything a cold instance
/// must answer instantly. `Full` additionally reconciles the local pack set
/// with `Manifest.packs` (download missing, drop superseded) and is required
/// before serving or verifying objects (upload-pack, receive-pack, compaction).
///
/// `Serve` is `Full` for everything that fits an instance, except tier-2
/// base packs when `cache.store_mount` is configured: those keep their
/// side-files (idx/rev/bitmap/commit-graph) local and point `pack-<sha>.pack`
/// at the mounted bucket object, so a 32 GB base never lands on tmpfs while
/// git still resolves any object. Without a mount `Serve` == `Full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyncLevel {
    Refs,
    Serve,
    Full,
}

impl SyncLevel {
    /// Whether this level reconciles the local pack set.
    pub fn wants_packs(self) -> bool {
        self >= SyncLevel::Serve
    }
}

/// How one live pack is served on this instance (see `RepoHandle::serve_plan`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackPlan {
    /// Real local copy of `.pack` + side-files.
    Local,
    /// Side-files local, `.pack` a symlink to this path in the store mount.
    Link(std::path::PathBuf),
    /// Only the commit-graph layer local; data via the remote reader.
    Remote,
}

impl PackPlan {
    /// Bytes this plan puts on tmpfs for `p`.
    pub fn tmpfs_bytes(&self, p: &PackRef) -> u64 {
        match self {
            PackPlan::Local => p.pack_size + p.idx_size,
            PackPlan::Link(_) => p.idx_size,
            PackPlan::Remote => 0,
        }
    }
}

/// Result of a sync operation, holding either a read guard (common case) or
/// indicating the repo was not found.
pub(crate) enum SyncOutcome {
    Unchanged,
    Changed {
        meta_version: Version,
        // Boxed: the fresh manifest (pack set, checkpoint pointer, settings)
        // is the large variant; Unchanged is unit-sized and is the common
        // case on every per-request freshness check.
        manifest: Box<Manifest>,
    },
}

/// Perform a conditional GET on manifest.pb and return the outcome.
pub(crate) async fn freshness_check(
    store: &Prefixed,
    known: Option<&Version>,
) -> Result<SyncOutcome, WalError> {
    match known {
        Some(v) => match get_message_if_changed::<Manifest>(store, keys::MANIFEST, v).await? {
            None => Ok(SyncOutcome::Unchanged),
            Some((meta, manifest)) => Ok(SyncOutcome::Changed {
                meta_version: meta.version,
                manifest: Box::new(manifest),
            }),
        },
        None => match get_message::<Manifest>(store, keys::MANIFEST).await? {
            None => Err(WalError::NotFound),
            Some((meta, manifest)) => Ok(SyncOutcome::Changed {
                meta_version: meta.version,
                manifest: Box::new(manifest),
            }),
        },
    }
}

/// Download a pack+idx from the store and install it into the local repo.
/// Uses a temp file, then `install_pack` (atomic rename).
pub(crate) async fn download_and_install_pack(
    store: &Prefixed,
    local: &LocalRepo,
    pack: &PackRef,
    tmp_dir: &std::path::Path,
    progress: Option<ProgressFn<'_>>,
) -> Result<(), WalError> {
    let span = tracing::info_span!(
        "wal.download_pack",
        checksum = %pack.checksum,
        bytes = pack.pack_size,
        objects = pack.object_count,
    );
    // Instrument awaited futures instead of carrying a thread-local span guard
    // across await points.

    let checksum = &pack.checksum;
    let pack_key = keys::pack_key(checksum);
    let idx_key = keys::idx_key(checksum);

    // Skip if already installed locally.
    let oid = gix_hash::ObjectId::from_hex(checksum.as_bytes())
        .map_err(|e| WalError::Corrupt(format!("invalid pack checksum {checksum}: {e}")))?;
    if local.pack_path(&oid).exists() {
        // Already installed; only fetch side files the manifest now advertises
        // (a base "promotion" publishes the same pack with a bitmap).
        for (flag, ext, key) in side_files(pack) {
            if !flag {
                continue;
            }
            let dest = local.pack_path(&oid).with_extension(ext);
            if dest.exists() {
                continue;
            }
            let tmp = tmp_dir.join(format!("{checksum}.{ext}"));
            if let Err(e) = download_object(store, &key, &tmp, None, None)
                .instrument(span.clone())
                .await
            {
                tracing::warn!(checksum = %checksum, ext, error = %e, "side file download failed");
                continue;
            }
            if std::fs::rename(&tmp, &dest).is_err() {
                // Cross-volume fallback into the committed side-file name:
                // transient sibling + rename, never a truncated
                // `pack-<checksum>.<ext>` a reader could adopt (issue #144).
                let _ = walgit_git::copy_into_place(&tmp, &dest);
                let _ = std::fs::remove_file(&tmp);
            }
        }
        return Ok(());
    }

    let pack_path = tmp_dir.join(format!("pack-{checksum}.pack"));
    let idx_path = tmp_dir.join(format!("pack-{checksum}.idx"));

    // Pack + idx + advertised side-files in one round: they are independent
    // immutable objects. Sequential was pack then idx then each side-file
    // (measurements: a 2.1 GB idx at ~10 MB/s *then* the 0.4 GB bitmap at
    // 40 MB/s). Each download is already striped.
    let mut extra = Vec::new();
    let mut side_futs = Vec::new();
    for (flag, ext, key) in side_files(pack) {
        if !flag {
            continue;
        }
        let path = tmp_dir.join(format!("pack-{checksum}.{ext}"));
        extra.push(path.clone());
        side_futs.push(
            async move { download_object(store, &key, &path, None, None).await }
                .instrument(span.clone()),
        );
    }
    let (pack_r, idx_r, side_rs) = tokio::join!(
        download_object(
            store,
            &pack_key,
            &pack_path,
            nonzero(pack.pack_size),
            progress
        )
        .instrument(span.clone()),
        download_object(store, &idx_key, &idx_path, nonzero(pack.idx_size), progress)
            .instrument(span.clone()),
        futures::future::join_all(side_futs),
    );
    pack_r?;
    idx_r?;
    for r in side_rs {
        r?;
    }

    local
        .install_pack(&pack_path, &idx_path, &extra)
        .instrument(span.clone())
        .await?;
    if pack.kind == walgit_proto::v1::PackKind::History as i32 {
        local.mark_history_pack(&oid, &pack.derived_from).await?;
        tracing::info!(checksum = %checksum, base = %pack.derived_from, bytes = pack.pack_size, "history pack installed (commits + trees local)");
    }
    Ok(())
}

/// `<mount>/<store prefix>/<repo prefix>/wal/<sha>.pack` (see `RepoHandle::mount_dir`).
pub(crate) fn mount_pack_path(
    mount_repo_dir: &std::path::Path,
    checksum: &str,
) -> std::path::PathBuf {
    mount_repo_dir.join(keys::pack_key(checksum))
}

/// Serve-level install of a base pack: side-files downloaded onto tmpfs, the
/// `.pack` itself a symlink into the mounted bucket (`target`). If the pack
/// was already installed as a real copy nothing changes.
pub(crate) async fn link_and_install_pack(
    store: &Prefixed,
    local: &LocalRepo,
    pack: &PackRef,
    tmp_dir: &std::path::Path,
    target: &std::path::Path,
) -> Result<(), WalError> {
    let checksum = &pack.checksum;
    let oid = gix_hash::ObjectId::from_hex(checksum.as_bytes())
        .map_err(|e| WalError::Corrupt(format!("invalid pack checksum {checksum}: {e}")))?;
    if local.pack_path(&oid).exists() {
        return Ok(());
    }
    let span =
        tracing::info_span!("wal.link_pack", checksum = %checksum, target = %target.display());
    let idx_path = tmp_dir.join(format!("pack-{checksum}.idx"));
    // The remote reader may already hold this index (web API on the same
    // instance): same bytes, hard-link instead of a second 2 GB download.
    // The copy fallback's destination is this pass's private scratch
    // (`tmp_dir`; `install_pack` below renames it into objects/pack), which
    // no concurrent reader adopts as serving state — so this site needs no
    // tmp+rename of its own (issue #144's audit).
    let remote_idx = crate::remote::idx_dir(local.path()).join(format!("{checksum}.idx"));
    let mut extra = Vec::new();
    let mut side_futs = Vec::new();
    // Idx + rev + bitmap + commit-graph in one round (each already striped).
    // Sequential idx-then-sides left the 2.1 GB idx on the critical path
    // alone (measurements: 10 MB/s idx, then 40 MB/s bitmap).
    for (flag, ext, key) in side_files(pack) {
        if !flag {
            continue;
        }
        let path = tmp_dir.join(format!("pack-{checksum}.{ext}"));
        extra.push(path.clone());
        side_futs.push(
            async move { download_object(store, &key, &path, None, None).await }
                .instrument(span.clone()),
        );
    }
    let idx_r = if remote_idx.is_file()
        && (std::fs::hard_link(&remote_idx, &idx_path).is_ok()
            || std::fs::copy(&remote_idx, &idx_path).is_ok())
    {
        tracing::info!(checksum = %checksum, "pack index reused from the remote reader");
        let side_rs = futures::future::join_all(side_futs).await;
        for r in side_rs {
            r?;
        }
        Ok(())
    } else {
        let idx_key = keys::idx_key(checksum);
        let (idx_r, side_rs) = tokio::join!(
            download_object(store, &idx_key, &idx_path, nonzero(pack.idx_size), None)
                .instrument(span.clone()),
            futures::future::join_all(side_futs),
        );
        for r in side_rs {
            r?;
        }
        idx_r
    };
    idx_r?;
    let link = tmp_dir.join(format!("pack-{checksum}.pack"));
    let _ = std::fs::remove_file(&link);
    crate::platform::symlink(target, &link)?;
    local
        .install_pack(&link, &idx_path, &extra)
        .instrument(span.clone())
        .await?;
    tracing::info!(checksum = %checksum, target = %target.display(), "base pack linked from store mount");
    // A history pack already installed for this base: the midx must now
    // cover both (history preferred) — see LocalRepo::write_history_midx.
    if local
        .packs()?
        .iter()
        .any(|p| p.history_of.as_deref() == Some(checksum.as_str()))
    {
        local.write_history_midx().await?;
    }
    Ok(())
}

/// `(advertised, extension, store key)` for every side-file a pack may carry.
fn side_files(pack: &PackRef) -> [(bool, &'static str, String); 3] {
    let c = &pack.checksum;
    [
        (pack.has_rev, "rev", keys::rev_key(c)),
        (pack.has_bitmap, "bitmap", keys::bitmap_key(c)),
        (
            pack.has_commit_graph,
            "commit-graph",
            keys::commit_graph_key(c),
        ),
    ]
}

/// Download an object to `dest`. Small objects stream straight to the file;
/// large ones (packs) are fetched as concurrent range reads written at their
/// offsets (object stores deliver ~100 MB/s per connection; striping gets the
/// NIC's worth), with bounded memory (PAR * CHUNK).
/// `progress(delta_bytes, total_bytes)` is called as chunks land (callers
/// throttle). `known_size` skips the happy-path HEAD (ROUNDTRIPS: HEAD ≈ GET;
/// `PackRef` already carries pack/idx sizes).
pub(crate) type ProgressFn<'a> = &'a (dyn Fn(u64, u64) + Send + Sync);

fn nonzero(n: u64) -> Option<u64> {
    (n > 0).then_some(n)
}

pub(crate) async fn download_object(
    store: &Prefixed,
    key: &str,
    dest: &std::path::Path,
    known_size: Option<u64>,
    progress: Option<ProgressFn<'_>>,
) -> Result<(), WalError> {
    use futures::{StreamExt, TryStreamExt};
    const CHUNK: u64 = 32 * 1024 * 1024;
    // 16 stripes in flight: one gRPC stream tops out around 10–20 MB/s from
    // a serverless host, the NIC well beyond 100 MB/s (a large repository's 2.1 GB idx took 217 s
    // on the broker with 8).
    const PAR: usize = 16;

    let size = match known_size {
        Some(n) => n,
        None => match store.head(key).await? {
            Some(m) => m.size,
            None => {
                return Err(WalError::Store(walgit_store::StoreError::NotFound {
                    key: key.to_string(),
                }));
            }
        },
    };
    let report = |n: u64| {
        if let Some(p) = progress {
            p(n, size);
        }
    };
    if size <= CHUNK {
        let res = store.get(key, GetOptions::default()).await?;
        return match res {
            GetResult::Object { body, .. } => {
                let mut file = tokio::fs::File::create(dest).await?;
                let mut body = body;
                while let Some(chunk) = body.next().await {
                    let chunk = chunk?;
                    tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
                    report(chunk.len() as u64);
                }
                tokio::io::AsyncWriteExt::flush(&mut file).await?;
                Ok(())
            }
            GetResult::NotModified { .. } => {
                Err(WalError::Corrupt(format!("unexpected 304 for {key}")))
            }
        };
    }
    let file = std::fs::File::create(dest)?;
    file.set_len(size)?;
    let file = std::sync::Arc::new(file);
    let starts: Vec<u64> = (0..size.div_ceil(CHUNK)).map(|i| i * CHUNK).collect();
    let report = &report;
    futures::stream::iter(starts)
        .map(|start| {
            let file = file.clone();
            let end = (start + CHUNK).min(size);
            async move {
                let res = store
                    .get(
                        key,
                        GetOptions {
                            range: Some(start..end),
                            ..Default::default()
                        },
                    )
                    .await?;
                let body = match res {
                    GetResult::Object { body, .. } => body,
                    GetResult::NotModified { .. } => {
                        return Err(WalError::Corrupt(format!("unexpected 304 for {key}")));
                    }
                };
                let len = usize::try_from(end - start).map_err(|_| {
                    WalError::Corrupt(format!("range {start}..{end} of {key} exceeds usize"))
                })?;
                let bytes = walgit_store::util::collect(body, len).await?;
                if bytes.len() as u64 != end - start {
                    return Err(WalError::Corrupt(format!(
                        "short range read for {key}: {}..{} got {}",
                        start,
                        end,
                        bytes.len()
                    )));
                }
                let n = bytes.len() as u64;
                tokio::task::spawn_blocking(move || {
                    crate::platform::write_all_at(&file, start, &bytes)
                })
                .await
                .map_err(|e| WalError::Corrupt(format!("write task failed: {e}")))??;
                report(n);
                Ok::<(), WalError>(())
            }
        })
        .buffer_unordered(PAR)
        .try_collect::<Vec<()>>()
        .await?;
    Ok(())
}

fn refs_not_replayable(manifest: &Manifest, from_seq: u64, to_seq: u64, detail: &str) -> WalError {
    WalError::Corrupt(format!(
        "refs not replayable: {detail}; from_seq={from_seq} to_seq={to_seq} checkpoint.seq={} min_seq={} head_seq={}",
        manifest.checkpoint.as_ref().map_or(0, |cp| cp.seq),
        manifest.min_seq,
        manifest.head_seq,
    ))
}

/// Verify that the manifest's checkpoint plus named log segments cover the
/// requested tail `(from_seq, to_seq]`. A checkpoint may only replace the log
/// through its own sequence; anything older than that is folded away. Sequence
/// numbers may contain holes: a crashed writer burns an orphaned head slot, so
/// only the last named segment must reach `to_seq`.
///
/// This deliberately checks the *fold boundary* and tail presence, not dense
/// seq coverage: without burned-range metadata an orphan slot and a missing
/// committed segment are indistinguishable. The fold boundary is the bug
/// #148 exercised; denser validation needs a manifest representation for
/// burned ranges (tracked as follow-up).
pub(crate) fn ensure_replayable(
    manifest: &Manifest,
    from_seq: u64,
    to_seq: u64,
) -> Result<(), WalError> {
    let requested_start = from_seq.saturating_add(1);
    if requested_start > to_seq {
        return Ok(());
    }
    let coverage_start = manifest
        .checkpoint
        .as_ref()
        .map_or(1, |cp| cp.seq.saturating_add(1))
        .max(manifest.min_seq.max(1));
    if requested_start < coverage_start {
        return Err(refs_not_replayable(
            manifest,
            from_seq,
            to_seq,
            "checkpoint folded the requested range",
        ));
    }

    let segments: Vec<_> = manifest
        .log_segments
        .iter()
        .filter(|segment| segment.last_seq >= requested_start && segment.first_seq <= to_seq)
        .collect();
    if segments.iter().any(|segment| segment.last_seq >= to_seq) {
        return Ok(());
    }
    Err(refs_not_replayable(
        manifest,
        from_seq,
        to_seq,
        "log segments do not cover the tail",
    ))
}

/// Apply the delta between the current local state and the new manifest.
/// Downloads missing packs, replays log entries, applies ref transactions.
///
/// Local refs are a pure function of the manifest: fold(checkpoint, tail).
/// The WAL is append-only within a repo's life, so `applied_seq > head_seq`
/// (or the manifest's revision moving backwards) means the bucket was wiped
/// and rebuilt — the local refs can no longer be reached incrementally and
/// are rebuilt from the fold. An empty manifest means an empty repository: a
/// leftover local dir (same `cache.dir`, wiped bucket — `git init --bare` is
/// a no-op on a surviving directory) must not keep advertising refs the WAL
/// no longer knows (#36).
pub(crate) async fn apply_delta(
    handle: &super::handle::RepoHandle,
    new_manifest: &Manifest,
    new_version: &Version,
) -> Result<(), WalError> {
    apply_delta_with_rebuild(handle, new_manifest, new_version, true).await?;
    handle.mark_refs_verified();
    Ok(())
}

pub(crate) async fn apply_delta_with_rebuild(
    handle: &super::handle::RepoHandle,
    new_manifest: &Manifest,
    new_version: &Version,
    force_refs_rebuild: bool,
) -> Result<(), WalError> {
    apply_delta_inner(handle, new_manifest, new_version, force_refs_rebuild, true).await
}

/// Apply the delta and optionally refresh gix. Cold materialization defers the
/// refresh until pack reconciliation finishes, so refs and packs become
/// visible in one ODB reload instead of one before and one after the packs.
async fn apply_delta_inner(
    handle: &super::handle::RepoHandle,
    new_manifest: &Manifest,
    new_version: &Version,
    force_refs_rebuild: bool,
    refresh: bool,
) -> Result<(), WalError> {
    let store = &handle.store;
    let local = &handle.local;
    let current_state = handle.state.lock().clone();
    let head_seq = new_manifest.head_seq;
    let rebuilt = force_refs_rebuild
        || current_state.applied_seq > head_seq
        || new_manifest.revision < current_state.revision;
    // Any path through apply_delta may rewrite refs (checkpoint load, full
    // replay, incremental replay, or the empty-repo reset). Invalidate the
    // process-local proof first; only a complete success marks it verified.
    handle.mark_refs_unverified();

    if head_seq == 0 {
        // Empty manifest ⇒ empty refs, unconditionally: cheap (one small
        // packed-refs write) and the only way to also cover a leftover dir
        // whose state file was lost (`applied_seq == 0` yet refs present).
        let packs_empty = local.packs()?.is_empty();
        local.load_ref_snapshot(&RefSnapshot::default())?;
        {
            let mut state = handle.state.lock();
            state.applied_seq = 0;
            // An empty manifest with no local packs is fully reconciled at
            // this revision: otherwise the first refs sync marks packs dirty
            // and prefetches, spending an extra manifest round trip on a
            // healthy empty-repo push (CI budget test).
            state.packs_dirty = !packs_empty;
            if new_manifest.packs.is_empty() && packs_empty {
                state.packs_revision = new_manifest.revision;
            }
        }
    } else {
        // If we have a checkpoint and haven't loaded it yet — or the bucket
        // was rebuilt and we must restart from it — load its refs. Its packs
        // are a subset of `Manifest.packs` and are reconciled below.
        let checkpoint_seq = new_manifest.checkpoint.as_ref().map_or(0, |c| c.seq);
        if checkpoint_seq > head_seq {
            return Err(refs_not_replayable(
                new_manifest,
                checkpoint_seq,
                head_seq,
                "checkpoint is ahead of head",
            ));
        }
        let need_checkpoint_load =
            checkpoint_seq > 0 && (rebuilt || current_state.applied_seq < checkpoint_seq);
        let replay_from = if need_checkpoint_load {
            checkpoint_seq
        } else if rebuilt {
            0
        } else {
            current_state.applied_seq
        };
        ensure_replayable(new_manifest, replay_from, head_seq)?;

        // The checkpoint's times feed `first_state_time` / `refs_as_of`; old refs
        // carry none, the object always does.
        handle.learn_checkpoint_times().await?;
        if need_checkpoint_load {
            let refs_key = keys::checkpoint_refs_key(checkpoint_seq);
            if let Some((_, snap)) = get_message::<RefSnapshot>(store, &refs_key).await? {
                local.load_ref_snapshot(&snap)?;
                handle.state.lock().applied_seq = checkpoint_seq;
            } else {
                return Err(refs_not_replayable(
                    new_manifest,
                    replay_from,
                    head_seq,
                    "checkpoint refs object is missing",
                ));
            }
        } else if rebuilt {
            // No checkpoint to rebuild from: start empty and replay the whole
            // log (no checkpoint ⇒ min_seq ≤ 1 ⇒ the full log is present).
            local.load_ref_snapshot(&RefSnapshot::default())?;
            handle.state.lock().applied_seq = 0;
        }

        // Replay log entries (refs, and superseded-pack bookkeeping) from
        // applied_seq+1 to head_seq. Packs are never touched here.
        let applied_seq = handle.state.lock().applied_seq;
        if applied_seq < head_seq {
            replay_log(handle, new_manifest, applied_seq, head_seq).await?;
        }
    }

    {
        let mut state = handle.state.lock();
        state.manifest_version = Some(new_version.as_str().to_string());
        state.applied_seq = head_seq;
        state.revision = new_manifest.revision;
    }
    crate::state::save_state(local.path(), &handle.state.lock().clone())?;
    if refresh {
        local.refresh_async().await?;
    }
    Ok(())
}

/// Make the local pack set match `manifest.packs`: download what is missing
/// (bounded concurrency, striped range reads) and remove packs superseded by
/// COMPACT entries applied since the last full sync. Idempotent; records
/// `packs_revision` on success so the next full sync is a no-op check.
pub(crate) async fn reconcile_packs(
    handle: &super::handle::RepoHandle,
    manifest: &Manifest,
    level: SyncLevel,
) -> Result<bool, WalError> {
    reconcile_packs_inner(handle, manifest, level, false).await
}

/// `background_history = true`: this call *is* the background history-pack
/// install and must download them instead of deferring.
pub(crate) async fn reconcile_packs_inner(
    handle: &super::handle::RepoHandle,
    manifest: &Manifest,
    level: SyncLevel,
    background_history: bool,
) -> Result<bool, WalError> {
    let store = &handle.store;
    let local = &handle.local;
    // Pack reconciliation is the proof itself: invalidate the previous
    // process-local result before touching the cache. Only a complete pass
    // (including the state write below) may mark it verified again.
    handle.mark_packs_unverified();
    // Test hook: simulate an unknown blocking call inside the install path
    // (what prod had: 2.6–43 s runtime stalls during materialization). With
    // the bulk runtime this only delays bulk work.
    if let Some(ms) = std::env::var("WALGIT_TEST_BLOCK_INSTALL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    let plan: std::collections::HashMap<String, PackPlan> = handle
        .serve_plan(manifest, level)
        .into_iter()
        .map(|(p, how)| (p.checksum, how))
        .collect();
    let span = tracing::info_span!(
        "wal.reconcile_packs",
        repo = %handle.id,
        live = manifest.packs.len(),
        downloaded = 0usize,
        removed = 0usize,
    );

    let remote_served_before: std::collections::HashSet<String> =
        handle.state.lock().remote_served.iter().cloned().collect();
    let installed_packs: std::collections::HashSet<String> = local
        .packs()?
        .into_iter()
        // A Full sync replaces mount symlinks with real copies.
        .filter(|p| level != SyncLevel::Full || !local.pack_path(&p.checksum).is_symlink())
        .map(|p| p.checksum.to_string())
        .collect();
    let tmp_dir = local.path().join(".walgit-tmp");
    tokio::fs::create_dir_all(&tmp_dir).await.ok();

    // Remote-served packs: no data here, only the commit-graph layer (the
    // `remote_served` list is rebuilt from the plan every pass).
    let mut remote_served: Vec<String> = Vec::new();
    for p in &manifest.packs {
        if plan.get(&p.checksum) != Some(&PackPlan::Remote) {
            continue;
        }
        remote_served.push(p.checksum.clone());
        if p.has_commit_graph {
            let oid = gix_hash::ObjectId::from_hex(p.checksum.as_bytes()).map_err(|e| {
                WalError::Corrupt(format!("invalid pack checksum {}: {e}", p.checksum))
            })?;
            let dest = local.pack_path(&oid).with_extension("commit-graph");
            if !dest.exists() {
                let tmp = tmp_dir.join(format!("pack-{}.commit-graph", p.checksum));
                download_object(
                    store,
                    &keys::commit_graph_key(&p.checksum),
                    &tmp,
                    None,
                    None,
                )
                .instrument(span.clone())
                .await?;
                if std::fs::rename(&tmp, &dest).is_err() {
                    // Cross-volume fallback into the committed
                    // `pack-<checksum>.commit-graph` name: transient sibling +
                    // rename so no reader adopts a truncated layer (#144).
                    walgit_git::copy_into_place(&tmp, &dest)?;
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        }
        if !remote_served_before.contains(&p.checksum) {
            tracing::info!(repo = %handle.id, pack = %p.checksum, bytes = p.pack_size, "base pack served remotely (no mount, does not fit)");
        }
    }
    {
        let mut st = handle.state.lock();
        st.remote_served.clone_from(&remote_served);
    }
    let remote_set: std::collections::HashSet<&str> = remote_served
        .iter()
        .map(std::string::String::as_str)
        .collect();

    // History packs (D18) are an accelerator, not a requirement: a fetch can
    // be served from the linked/remote base right away. They are installed by
    // a background task (`RepoHandle::spawn_history_pack_install`) so the
    // first request on an instance never waits for a 7.5 GB download.
    let is_history = |p: &PackRef| p.kind == walgit_proto::v1::PackKind::History as i32;
    let deferred_history: Vec<PackRef> = manifest
        .packs
        .iter()
        .filter(|p| is_history(p) && !installed_packs.contains(&p.checksum) && !background_history)
        .cloned()
        .collect();
    if !deferred_history.is_empty() {
        handle.spawn_history_pack_install(deferred_history);
    }
    // Installed packs whose manifest entry now advertises a side-file this
    // host lacks (`annotate-pack` / the rev-index unit retrofitting a `.rev`
    // or a bitmap onto a published pack): fetch just the side-file. Without
    // this the fleet never converged — a large repository's base got its `.rev` in the
    // bucket and every host kept rebuilding the reverse index per fetch.
    for p in manifest
        .packs
        .iter()
        .filter(|p| installed_packs.contains(&p.checksum))
    {
        let Ok(oid) = gix_hash::ObjectId::from_hex(p.checksum.as_bytes()) else {
            continue;
        };
        for (flag, ext, key) in side_files(p) {
            let dest = local.pack_path(&oid).with_extension(ext);
            if !flag || dest.exists() {
                continue;
            }
            let tmp = tmp_dir.join(format!("pack-{}.{ext}", p.checksum));
            match download_object(store, &key, &tmp, None, None)
                .instrument(span.clone())
                .await
            {
                Ok(()) => {
                    if std::fs::rename(&tmp, &dest).is_err() {
                        // Cross-volume fallback into the committed side-file
                        // name: transient sibling + rename, never a truncated
                        // `pack-<checksum>.<ext>` a reader could adopt (#144).
                        let _ = walgit_git::copy_into_place(&tmp, &dest);
                        let _ = std::fs::remove_file(&tmp);
                    }
                    tracing::info!(repo = %handle.id, pack = %p.checksum, ext, "side-file installed for an installed pack");
                }
                Err(e) => {
                    tracing::warn!(repo = %handle.id, pack = %p.checksum, ext, error = %e, "side-file download failed");
                }
            }
        }
    }
    let missing_packs: Vec<&PackRef> = manifest
        .packs
        .iter()
        .filter(|p| {
            !installed_packs.contains(&p.checksum) && !remote_set.contains(p.checksum.as_str())
        })
        .filter(|p| background_history || !is_history(p))
        .collect();
    span.record("downloaded", missing_packs.len());
    let downloaded: Vec<PackRef> = missing_packs
        .iter()
        .map(|p| (*p).clone())
        .chain(
            // Newly remote-served packs count as "installed" for commit-graph
            // maintenance (their layer becomes the chain base).
            manifest
                .packs
                .iter()
                .filter(|p| {
                    remote_set.contains(p.checksum.as_str())
                        && !remote_served_before.contains(&p.checksum)
                })
                .cloned(),
        )
        .collect();

    let reporter = handle.reporter();
    let link_target = |p: &PackRef| match plan.get(&p.checksum) {
        Some(PackPlan::Link(t)) => Some(t.clone()),
        _ => None,
    };
    let total_bytes: u64 = missing_packs
        .iter()
        .map(|p| {
            if link_target(p).is_some() {
                p.idx_size
            } else {
                p.pack_size + p.idx_size
            }
        })
        .sum();
    if !missing_packs.is_empty() {
        reporter.notice(format!(
            "Materializing {} pack(s) ({}) from the WAL onto this instance",
            missing_packs.len(),
            crate::remote::human_bytes(total_bytes)
        ));
    }
    let done_all = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let throttle = Arc::new(crate::progress::Throttle::new(
        std::time::Duration::from_millis(250),
    ));
    let sem = Arc::new(tokio::sync::Semaphore::new(8));
    let mut tasks = Vec::new();
    for p in missing_packs {
        let sem = sem.clone();
        let store = store.clone();
        let local = local.clone();
        let p = p.clone();
        let tmp_dir = tmp_dir.clone();
        let reporter = reporter.clone();
        let done_all = done_all.clone();
        let throttle = throttle.clone();
        let link_to = link_target(&p);
        tasks.push(tokio::spawn(
            async move {
                // The semaphore is dropped only after all tasks are joined, so
                // it is never closed while a task is acquiring.
                let _permit = sem.acquire().await.map_err(|e| {
                    WalError::Corrupt(format!("pack download semaphore closed: {e}"))
                })?;
                // Per-object progress arrives as absolute (done,total); turn it
                // into deltas for the shared counter.
                let cb = |delta: u64, _t: u64| {
                    // download_object reports a per-chunk delta so pack+idx+
                    // side-files can run in one round without a shared cursor.
                    let all =
                        done_all.fetch_add(delta, std::sync::atomic::Ordering::Relaxed) + delta;
                    if throttle.tick(false) {
                        reporter.bar("Downloading packs", all, Some(total_bytes), "bytes");
                    }
                };
                match link_to {
                    Some(target) => {
                        link_and_install_pack(&store, &local, &p, &tmp_dir, &target).await
                    }
                    None => {
                        download_and_install_pack(&store, &local, &p, &tmp_dir, Some(&cb)).await
                    }
                }
            }
            .instrument(span.clone()),
        ));
    }
    let had_downloads = !tasks.is_empty();
    for t in tasks {
        t.await.map_err(|e| WalError::Corrupt(e.to_string()))??;
    }
    if had_downloads {
        reporter.bar("Downloading packs", total_bytes, Some(total_bytes), "bytes");
        reporter.notice("Packs installed; local copy is complete");
    }
    maintain_commit_graph(handle, manifest, &downloaded).await;

    // Remove every local pack that is not live in this manifest: superseded
    // packs from COMPACT entries, plus residue from interrupted installs or an
    // older process whose state file looked current. Removal needs no active
    // readers (they hold `rw.read()` through their ReadGuard), so it **tries**
    // the write lock: if any reader is active (a clone streaming for minutes),
    // the removal simply waits for a later pass — never queue as a writer,
    // which would block every new reader on the instance.
    let live: std::collections::HashSet<&str> =
        manifest.packs.iter().map(|p| p.checksum.as_str()).collect();
    // Serialize the candidate scan + removal + state commit with writers that
    // make a pack visible before its manifest CAS. If a writer holds the lock
    // we cannot know whether its pack is visible yet, so keep the state dirty
    // and retry on the next sync.
    let prune_lock = handle.prune_lock();
    let Ok(_prune_guard) = prune_lock.try_lock() else {
        handle.state.lock().packs_dirty = true;
        return Ok(false);
    };
    let pending = std::mem::take(&mut handle.state.lock().pending_pack_removals);
    let mut removed = 0usize;
    let mut still_pending: Vec<String> = Vec::new();
    let mut deferred_staged: Vec<String> = Vec::new();
    let mut needs_midx_rebuild = false;
    let mut candidates: std::collections::BTreeSet<String> = pending
        .iter()
        .filter(|s| !live.contains(s.as_str()))
        .filter(|s| {
            if handle.pack_staged(s) {
                deferred_staged.push((*s).clone());
                false
            } else {
                true
            }
        })
        .cloned()
        .collect();
    for pack in local.packs()? {
        let checksum = pack.checksum.to_string();
        if !live.contains(checksum.as_str()) {
            if handle.pack_staged(&checksum) {
                deferred_staged.push(checksum);
            } else {
                candidates.insert(checksum);
            }
        }
    }
    let to_remove: Vec<(String, gix_hash::ObjectId)> = candidates
        .into_iter()
        .map(|s| {
            gix_hash::ObjectId::from_hex(s.as_bytes())
                .map(|o| (s.clone(), o))
                .map_err(|e| WalError::Corrupt(format!("invalid supersedes checksum: {e}")))
        })
        .collect::<Result<_, _>>()?;
    if !to_remove.is_empty() {
        if let Ok(_w) = handle.rw.try_write() {
            // The candidate set was built before the write lock; a publish
            // may have staged (or a concurrent sync may have re-listed) one of
            // these packs in the meantime. Re-check both facts under the lock.
            let live_now: std::collections::HashSet<String> = handle
                .manifest()
                .packs
                .iter()
                .map(|p| p.checksum.clone())
                .collect();
            let mut recheck: Vec<(String, gix_hash::ObjectId)> = Vec::new();
            for (checksum, oid) in to_remove {
                if live_now.contains(&checksum) || handle.pack_staged(&checksum) {
                    deferred_staged.push(checksum);
                } else {
                    recheck.push((checksum, oid));
                }
            }
            let to_remove = recheck;
            // One call for the whole superseded set: on Windows the cold
            // handle swap inside remove_packs releases this process's own
            // pack-index mmaps once, not once per pack (K swaps would drop
            // and re-parse every surviving index K times).
            let existing: Vec<gix_hash::ObjectId> = to_remove
                .iter()
                .filter(|(_, oid)| local.pack_path(oid).exists())
                .map(|(_, oid)| *oid)
                .collect();
            if !existing.is_empty() {
                needs_midx_rebuild = local.remove_packs(&existing)?;
                tracing::info!(repo = %handle.id, packs = existing.len(), "local packs outside the manifest removed");
            }
            removed = existing.len();
        } else {
            tracing::info!(repo = %handle.id, packs = to_remove.len(), "packs outside the manifest kept for now: readers active; retried on the next sync");
            still_pending.extend(to_remove.iter().map(|(s, _)| s.clone()));
        }
    }
    span.record("removed", removed);
    still_pending.extend(deferred_staged);
    if !still_pending.is_empty() {
        handle.state.lock().pending_pack_removals = still_pending;
    }

    // The rw.write guard above is dropped before this point. Build the new
    // MIDX here rather than under the reader lock; the caller performs the
    // single final gix reload after this function returns.
    if needs_midx_rebuild {
        local.rebuild_history_midx().await?;
    }

    {
        let mut state = handle.state.lock();
        state.packs_revision = manifest.revision;
        state.packs_dirty = false;
    }
    crate::state::save_state(local.path(), &handle.state.lock().clone())?;
    if handle.state.lock().packs_ready() {
        handle.mark_packs_verified();
    }
    Ok(true)
}

/// Keep the local commit-graph chain current after packs were installed:
/// a tier-2 pack that ships a commit-graph layer becomes the chain base
/// (replacing whatever was there), and every other newly installed pack's
/// commits are folded in as an incremental layer (`--split --stdin-packs`,
/// cheap: generation numbers come from the existing layers). Best effort:
/// the graph is an accelerator, a failure only costs speed.
pub(crate) async fn maintain_commit_graph(
    handle: &super::handle::RepoHandle,
    manifest: &Manifest,
    installed: &[PackRef],
) {
    if !handle.cfg.git.commit_graph {
        return;
    }
    let local = &handle.local;
    let mut base_changed = false;
    for p in installed.iter().filter(|p| p.has_commit_graph) {
        if let Ok(oid) = gix_hash::ObjectId::from_hex(p.checksum.as_bytes()) {
            // Filesystem + gix reopen: off the runtime.
            let l = local.clone();
            let res = tokio::task::spawn_blocking(move || l.install_commit_graph_base(&oid)).await;
            match res {
                Ok(Ok(true)) => base_changed = true,
                Ok(Ok(false)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(pack = %p.checksum, error = %e, "commit-graph base install failed");
                }
                Err(e) => {
                    tracing::warn!(pack = %p.checksum, error = %e, "commit-graph base install task failed");
                }
            }
        }
    }
    // After a base change every non-base pack must be re-added (the old chain
    // layers were dropped); otherwise only what was just installed.
    // History packs hold the base's commits, already covered by its layer.
    let is_history = |p: &&PackRef| p.kind == walgit_proto::v1::PackKind::History as i32;
    let candidates: Vec<&PackRef> = if base_changed {
        manifest
            .packs
            .iter()
            .filter(|p| !p.has_commit_graph && !is_history(p))
            .collect()
    } else {
        installed
            .iter()
            .filter(|p| !p.has_commit_graph && !is_history(p))
            .collect()
    };
    let packs: Vec<gix_hash::ObjectId> = candidates
        .iter()
        .filter_map(|p| gix_hash::ObjectId::from_hex(p.checksum.as_bytes()).ok())
        .filter(|o| local.pack_path(o).exists())
        .collect();
    if packs.is_empty() {
        return;
    }
    let started = std::time::Instant::now();
    if let Err(e) = local
        .update_commit_graph(&packs, handle.cfg.git.commit_graph_changed_paths)
        .await
    {
        tracing::warn!(repo = %handle.id, error = %e, "commit-graph update failed");
    } else {
        tracing::info!(repo = %handle.id, packs = packs.len(), ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX), "commit-graph updated");
    }
}

/// Replay log entries in (`from_seq`, `to_seq`] from the manifest's log segments.
pub(crate) async fn replay_log(
    handle: &super::handle::RepoHandle,
    manifest: &Manifest,
    from_seq: u64,
    to_seq: u64,
) -> Result<(), WalError> {
    ensure_replayable(manifest, from_seq, to_seq)?;
    let store = &handle.store;

    // Find segments that overlap (from_seq, to_seq]
    let segments: Vec<&walgit_proto::v1::LogSegmentRef> = manifest
        .log_segments
        .iter()
        .filter(|s| s.last_seq > from_seq && s.first_seq <= to_seq)
        .collect();

    // Fetch every segment in parallel (order kept), then apply ALL entries in
    // one pass: `apply_ref_txns_offline` rewrites packed-refs, which is O(refs)
    // — per segment that was k × 500 ms on a 500 k-ref repo (2026-08-21,
    // test/refs500k: 2 tail entries = 1 s of every cold refs sync). One
    // rewrite per sync, and the tail's GETs overlap instead of serializing.
    let keys: Vec<String> = segments.iter().map(|s| s.key.clone()).collect();
    let mut fetched: Vec<Option<bytes::Bytes>> = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(16) {
        let futs: Vec<_> = chunk
            .iter()
            .map(|key| {
                let store = store.clone();
                let key = key.clone();
                async move {
                    let res = store.get(&key, GetOptions::default()).await?;
                    Ok::<Option<bytes::Bytes>, WalError>(match res {
                        GetResult::Object { meta, body } => {
                            let size = usize::try_from(meta.size).map_err(|_| {
                                WalError::Corrupt(format!(
                                    "log segment {} size {} exceeds usize",
                                    key, meta.size
                                ))
                            })?;
                            Some(walgit_store::util::collect(body, size).await?)
                        }
                        GetResult::NotModified { .. } => None,
                    })
                }
            })
            .collect();
        for r in futures::future::join_all(futs).await {
            fetched.push(r?);
        }
    }

    let mut all: Vec<LogEntry> = Vec::new();
    for bytes in fetched {
        let Some(bytes) = bytes else { continue };
        // Decode frames (tolerate partial trailing frame)
        let (entries, _) = walgit_proto::frame::decode_entries(&bytes)
            .map_err(|e| WalError::Corrupt(format!("log segment decode: {e}")))?;
        // Only the oldest live segment witnesses the repository's first state.
        if entries
            .first()
            .is_some_and(|e| e.seq <= manifest.min_seq.max(1))
            && let Some(first) = entries
                .first()
                .and_then(|e| e.created_at.as_ref())
                .map(walgit_proto::time::to_system)
        {
            let mut slot = handle.first_entry_time.lock();
            if slot.map(|t| first < t).unwrap_or(true) {
                *slot = Some(first);
            }
        }
        all.extend(
            entries
                .into_iter()
                .filter(|e| e.seq > from_seq && e.seq <= to_seq),
        );
    }
    all.sort_by_key(|e| e.seq);
    if from_seq < to_seq && all.last().is_none_or(|entry| entry.seq < to_seq) {
        return Err(refs_not_replayable(
            manifest,
            from_seq,
            to_seq,
            "decoded log entries do not reach the tail",
        ));
    }
    if let Some(last) = all
        .last()
        .and_then(|e| e.created_at.as_ref())
        .map(walgit_proto::time::to_system)
    {
        let mut slot = handle.last_entry_time.lock();
        if slot.map(|t| last > t).unwrap_or(true) {
            *slot = Some(last);
        }
    }
    let wanted: Vec<&LogEntry> = all.iter().collect();
    apply_entries(handle, &wanted)?;

    Ok(())
}

/// Apply a batch of log entries to the local repo (refs level).
///
/// Ref transactions are merged and written once (`apply_ref_txns_offline`:
/// no `git update-ref`, so it works before the packs exist locally).
/// COMPACT entries only record their superseded packs; `reconcile_packs`
/// installs/removes packs against `Manifest.packs` on the next full sync.
pub(crate) fn apply_entries(
    handle: &super::handle::RepoHandle,
    entries: &[&LogEntry],
) -> Result<(), WalError> {
    let local = &handle.local;
    let mut txns: Vec<&walgit_proto::v1::RefTransaction> = Vec::new();
    let mut supersedes: Vec<String> = Vec::new();
    for entry in entries {
        match entry.kind() {
            EntryKind::Push | EntryKind::RefUpdate => {
                if let Some(txn) = &entry.txn {
                    txns.push(txn);
                }
            }
            EntryKind::Compact => {
                supersedes.extend(entry.supersedes.iter().cloned());
            }
            // Checkpoints and settings live on the manifest; the entries are history only.
            EntryKind::Checkpoint | EntryKind::Settings => {}
            EntryKind::Unspecified => {
                tracing::warn!(seq = entry.seq, "unspecified log entry kind, skipping");
            }
        }
    }
    if !txns.is_empty() {
        local.apply_ref_txns_offline(&txns)?;
    }
    if !supersedes.is_empty() {
        let mut state = handle.state.lock();
        for s in supersedes {
            if !state.pending_pack_removals.contains(&s) {
                state.pending_pack_removals.push(s);
            }
        }
    }
    Ok(())
}

pub(crate) async fn materialize_from_scratch(
    handle: &super::handle::RepoHandle,
    manifest: &Manifest,
    version: &Version,
) -> Result<(), WalError> {
    let span = tracing::info_span!(
        "wal.materialize",
        repo = %handle.id,
        head_seq = manifest.head_seq,
    );
    // Instrument the awaited materialization future; do not hold an enter guard.

    // Reset state
    handle.mark_packs_unverified();
    handle.state.lock().applied_seq = 0;
    // "From scratch" must be honest about refs too: with no checkpoint to
    // load, apply_delta replays the whole log on top of the *existing* local
    // refs — start from empty so the result is the fold, not the fold merged
    // into whatever a leftover dir happened to hold (#36).
    if manifest.checkpoint.is_none() {
        handle.local.load_ref_snapshot(&RefSnapshot::default())?;
    }

    // Apply delta from scratch (checkpoint + all log entries), then packs.
    // Defer the gix reload until both phases are complete: reconciliation
    // installs the packs, and one final refresh makes the whole cold copy
    // visible atomically.
    apply_delta_inner(handle, manifest, version, true, false)
        .instrument(span.clone())
        .await?;
    reconcile_packs(handle, manifest, SyncLevel::Serve)
        .instrument(span.clone())
        .await?;
    handle.local.refresh_async().await?;
    handle.mark_refs_verified();
    Ok(())
}

/// The **bulk runtime**: a small dedicated tokio runtime (own worker threads)
/// that runs pack materialization (striped downloads, 32 MiB chunk copies,
/// tmpfs writes, install renames, gix reopen, commit-graph/midx subprocess
/// waits). Whatever inside that path is CPU-heavy or secretly blocking can
/// only delay other bulk work — request workers on the main runtime keep
/// serving refs in milliseconds (prod 2026-08-20: the main runtime stalled
/// 2.6–43 s repeatedly for the whole duration of one repo's 7.5 GB + another's
/// 12 GB materializations; the watchdog caught it, the cause hid among a dozen
/// candidates; isolation makes the question moot).
static BULK_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

fn bulk_runtime() -> &'static tokio::runtime::Runtime {
    // The builder fails only on OS resource exhaustion; this runtime is
    // required for every pack-materialization path, so there is no fallback.
    #[allow(
        clippy::expect_used,
        reason = "required runtime; build fails only under OS resource exhaustion with no recoverable fallback"
    )]
    BULK_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("walgit-bulk")
            .enable_all()
            .build()
            .expect("bulk runtime")
    })
}

/// Run `fut` on the bulk runtime and await its result from the caller's
/// runtime. The future must be `'static + Send` (use `Arc<RepoHandle>`).
pub(crate) async fn on_bulk_runtime<T: Send + 'static>(
    fut: impl std::future::Future<Output = Result<T, WalError>> + Send + 'static,
) -> Result<T, WalError> {
    let span = tracing::Span::current();
    let (tx, rx) = tokio::sync::oneshot::channel();
    bulk_runtime().spawn(async move {
        let r = fut.instrument(span).await;
        let _ = tx.send(r);
    });
    rx.await
        .map_err(|_| WalError::Corrupt("bulk runtime task dropped".into()))?
}

#[cfg(test)]
mod download_tests {
    use super::download_object;
    use walgit_store::{ObjectStoreExt, Prefixed, PutMode, memory::MemoryStore};

    #[tokio::test]
    async fn striped_download_matches_source() {
        // > CHUNK (32 MiB) so the ranged/striped path runs, with a ragged tail.
        let size = 70 * 1024 * 1024 + 12345;
        let mut data = vec![0u8; size];
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for b in &mut data {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x.to_le_bytes()[0];
        }
        let store = MemoryStore::shared();
        store
            .put_bytes("p/big.pack", data.clone(), PutMode::Create)
            .await
            .unwrap();
        store
            .put_bytes("p/small.pack", b"tiny".to_vec(), PutMode::Create)
            .await
            .unwrap();
        let prefixed = Prefixed::new(store, "p/");
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.pack");
        download_object(&prefixed, "big.pack", &big, Some(size as u64), None)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&big).unwrap(), data);
        let small = dir.path().join("small.pack");
        download_object(&prefixed, "small.pack", &small, None, None)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&small).unwrap(), b"tiny");
    }

}
