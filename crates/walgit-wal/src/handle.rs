//! `RepoHandle`: per-repository state, sync, publish, checkpoint.

use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Instant;

use parking_lot::{Mutex as PLMutex, RwLock as PLRwLock};
use tokio::sync::{Mutex as TokioMutex, RwLock as TokioRwLock, mpsc};
use tracing::Instrument;
use walgit_git::{LocalRepo, RepoId};
use walgit_proto::v1::Manifest;
use walgit_proto::{keys, v1::PackRef};
use walgit_store::{ObjectStore, Prefixed, Version};

use crate::error::WalError;
use crate::progress::{ProgressRx, ProgressTx, Reporter};
use crate::publish::{PublishRequest, PublishResult};
use crate::remote::{BlockCache, RemotePacks};
use crate::state::RepoState;
use crate::sync::{PackPlan, SyncLevel};
use crate::tasks::{Begin, Tasks};

pub(crate) fn instance_id() -> String {
    walgit_store::coord::instance_id().to_string()
}

/// When a checkpoint's state is from (see `RepoHandle::checkpoint_times`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointTimes {
    /// When the checkpoint was written: state exists as of this instant.
    pub created_at: Option<std::time::SystemTime>,
    /// Earliest state the repository ever had (D22, carried forward).
    pub first_state_at: Option<std::time::SystemTime>,
    /// `created_at` of the newest entry folded: the state it holds is as of then.
    pub as_of: Option<std::time::SystemTime>,
}

pub struct RepoHandle {
    pub(crate) id: RepoId,
    pub(crate) local: LocalRepo,
    pub(crate) store: Prefixed,
    pub(crate) cfg: Arc<walgit_config::Config>,

    // Prevents pack removal during reads. Uses tokio::sync::RwLock because
    // guards are held across .await points (freshness check, apply_delta).
    pub(crate) rw: TokioRwLock<()>,
    // Single in-flight sync.
    pub(crate) sync_mutex: TokioMutex<()>,
    /// Serializes pack reconciliation (downloads/links/removals). Held
    /// *without* `sync_mutex`/`rw.write`, so a refs-level request is never
    /// stuck behind a multi-GB materialization (only removals take the write
    /// lock, briefly).
    pub(crate) pack_mutex: TokioMutex<()>,

    // Current manifest (last known). Short critical sections, no await.
    pub(crate) manifest: PLRwLock<Arc<Manifest>>,
    pub(crate) manifest_version: PLMutex<Option<Version>>,
    /// Guards the *pair* above. Writers install `(manifest, version)` under it
    /// and readers snapshot under it, so the cache can never hold an old
    /// manifest next to a newer version — which would let a CAS succeed on a
    /// manifest body that predates the claim it is supposed to see (#175).
    pub(crate) manifest_pair: PLMutex<()>,

    // Persistent local state.
    pub(crate) state: PLMutex<RepoState>,
    // Process-local proof that refs were rebuilt from a verified checkpoint +
    // contiguous log tail. Never persisted: a restart must re-verify even when
    // the on-disk state claims applied_seq == head_seq.
    pub(crate) refs_verified: AtomicBool,
    // Process-local proof that the local pack set was reconciled against the
    // manifest by this process. Never persisted: a restart must scan the cache
    // even when the on-disk state claims packs_revision == revision, because a
    // crash can leave an orphan pack that the persisted state never recorded.
    pub(crate) packs_verified: AtomicBool,
    // Packs ingested by this process and waiting for their manifest CAS, with
    // one reference per live `StagedPackGuard` (receive holds an outer guard
    // while `enqueue_publish_at` holds its detached waiter guard). They are not
    // live in the manifest yet, so reconciliation must not mistake them for
    // restart residue and delete them before upload.
    pub(crate) staged_packs: PLMutex<HashMap<String, usize>>,
    // Coarse mutual exclusion between "a pack is becoming visible / is being
    // published" and prune. Ingest/repack/publish hold it for the whole
    // final-name-visible → CAS window; reconcile uses try_lock and defers
    // pruning when a writer is active. This closes the install→stage race
    // that a set membership check alone cannot (issue #148 review).
    pub(crate) prune_lock: Arc<tokio::sync::Mutex<()>>,

    // Freshness TTL.
    pub(crate) last_freshness: PLMutex<Option<Instant>>,

    // Eviction tracking.
    pub(crate) last_access: PLMutex<Instant>,

    // Self-referential Arc for spawning the publisher task.
    pub(crate) self_arc: std::sync::OnceLock<Arc<RepoHandle>>,
    // Single-flight publisher channel.
    pub(crate) publish_tx: PLMutex<Option<mpsc::UnboundedSender<PublishRequest>>>,
    // Number of callers currently waiting for a publish response. Used by
    // the publisher to distinguish a lone push from a concurrent batch.
    pub(crate) publish_waiters: AtomicUsize,
    // Single-flight background pack prefetch after a refs-level sync.
    pub(crate) prefetch_inflight: std::sync::atomic::AtomicBool,
    /// Time of the oldest *live* log entry (the segment holding `min_seq`) this
    /// handle replayed, for `first_state_time`. Never a later segment: a process
    /// that warmed from disk and only fetched the tail must not take the tail's
    /// first entry for the repository's first state (prod 2026-08-21: the SSD host
    /// took seq 8 / 03:22Z and planned every earlier slot `unavailable`).
    pub(crate) first_entry_time: parking_lot::Mutex<Option<std::time::SystemTime>>,
    /// Times of the current checkpoint read from the checkpoint *object* when
    /// the manifest's `CheckpointRef` carries none (checkpoints written before
    /// the ref had `created_at`, e.g. a large repository's import): `(seq, created_at,
    /// first_state_at, as_of)`. One small GET per checkpoint per process.
    pub(crate) checkpoint_times: parking_lot::Mutex<Option<(u64, CheckpointTimes)>>,
    /// Time of the newest log entry seen (replayed or published): the floor
    /// for explicit `created_at` on publish.
    pub(crate) last_entry_time: parking_lot::Mutex<Option<std::time::SystemTime>>,
    /// `created_at` of seq 1 when this process published it: the checkpoint's `first_state_at`
    /// without a log read. Deliberately *not* `first_entry_time` (the slot planner's witness): a
    /// writer's own push today must not turn the current slots `unavailable` — the planner keeps
    /// learning first state from replayed segments and checkpoints only.
    pub(crate) first_seq_published_at: parking_lot::Mutex<Option<std::time::SystemTime>>,
    pub(crate) history_install_inflight: std::sync::atomic::AtomicBool,
    /// Fires when a history-pack install finishes (for `wait_history_pack_installed`).
    pub(crate) history_install_done: tokio::sync::Notify,
    /// D24: effective config cache — (settings revision, host config ⊕ settings).
    pub(crate) effective: parking_lot::Mutex<Option<(u64, Arc<walgit_config::Config>)>>,

    // Progress packets of every task touching this repo (SSE envelope).
    pub(crate) progress: ProgressTx,
    pub(crate) tasks: Arc<Tasks>,
    // Reporter of the task currently running inside sync (None = repo channel only).
    pub(crate) active_reporter: PLMutex<Option<Reporter>>,
    // Remote object access (pack set too large to materialize).
    pub(crate) blocks: Arc<BlockCache>,
    pub(crate) remote: PLMutex<Option<Arc<RemotePacks>>>,
}

/// How a request may read objects after [`RepoHandle::sync_objects`].
#[derive(Clone)]
pub enum ObjectAccess {
    /// Packs are installed in the local repository; run git/gix locally.
    Local,
    /// Pack set too large for this instance: indexes local, data by range read.
    Remote(Arc<RemotePacks>),
}

impl ObjectAccess {
    pub fn is_remote(&self) -> bool {
        matches!(self, ObjectAccess::Remote(_))
    }
}

/// RAII protection for an ingested pack that is not in the manifest yet.
/// The reconciler must not prune it while a publish/CAS is in flight.
pub struct StagedPackGuard {
    handle: Arc<RepoHandle>,
    checksum: String,
}

impl Drop for StagedPackGuard {
    fn drop(&mut self) {
        if !self.handle.unstage_pack(&self.checksum) {
            // Another guard still owns the pack (receive + publisher waiter).
            return;
        }
        // A guard that drops before its pack became live means the ingest or
        // publish failed/was cancelled. Record the orphan so a later reconcile
        // removes it even when the manifest revision did not change.
        let live = self
            .handle
            .manifest()
            .packs
            .iter()
            .any(|p| p.checksum == self.checksum);
        if !live {
            let mut state = self.handle.state.lock();
            if !state.pending_pack_removals.contains(&self.checksum) {
                state.pending_pack_removals.push(self.checksum.clone());
            }
            state.packs_dirty = true;
        }
    }
}

impl RepoHandle {
    // The one constructor of RepoHandle: its parameters are exactly the
    // externally supplied fields of the struct (the rest are internal
    // plumbing initialized inline); registry has two mechanical call sites.
    // A params struct would duplicate the struct's own field list.
    #[allow(
        clippy::too_many_arguments,
        reason = "constructor mirroring RepoHandle's fields; registry call sites are mechanical"
    )]
    pub(crate) fn new(
        id: RepoId,
        local: LocalRepo,
        store: Prefixed,
        cfg: Arc<walgit_config::Config>,
        manifest: Manifest,
        version: Option<Version>,
        state: RepoState,
        tasks: Arc<Tasks>,
        blocks: Arc<BlockCache>,
    ) -> Self {
        let (progress, _) = tokio::sync::broadcast::channel(1024);
        RepoHandle {
            id,
            local,
            store,
            cfg,
            rw: TokioRwLock::new(()),
            sync_mutex: TokioMutex::new(()),
            pack_mutex: TokioMutex::new(()),
            manifest: PLRwLock::new(Arc::new(manifest)),
            manifest_version: PLMutex::new(version),
            manifest_pair: PLMutex::new(()),
            state: PLMutex::new(state),
            refs_verified: AtomicBool::new(false),
            packs_verified: AtomicBool::new(false),
            staged_packs: PLMutex::new(HashMap::new()),
            prune_lock: Arc::new(tokio::sync::Mutex::new(())),
            last_freshness: PLMutex::new(None),
            last_access: PLMutex::new(Instant::now()),
            self_arc: std::sync::OnceLock::new(),
            publish_tx: PLMutex::new(None),
            publish_waiters: AtomicUsize::new(0),
            prefetch_inflight: std::sync::atomic::AtomicBool::new(false),
            first_entry_time: parking_lot::Mutex::new(None),
            checkpoint_times: parking_lot::Mutex::new(None),
            last_entry_time: parking_lot::Mutex::new(None),
            first_seq_published_at: parking_lot::Mutex::new(None),
            history_install_inflight: std::sync::atomic::AtomicBool::new(false),
            history_install_done: tokio::sync::Notify::new(),
            effective: parking_lot::Mutex::new(None),
            progress,
            tasks,
            active_reporter: PLMutex::new(None),
            blocks,
            remote: PLMutex::new(None),
        }
    }

    /// Live progress of everything happening to this repo on this instance.
    /// Subscribe *before* starting the work you want to watch.
    pub fn subscribe_progress(&self) -> ProgressRx {
        self.progress.subscribe()
    }

    /// Reporter for work done on behalf of this repo: the running sync task's
    /// (so packets land in its record) or the bare repo channel.
    pub fn reporter(&self) -> Reporter {
        self.active_reporter
            .lock()
            .clone()
            .unwrap_or_else(|| Reporter::for_repo(self.progress.clone()))
    }

    pub fn tasks(&self) -> &Arc<Tasks> {
        &self.tasks
    }

    pub fn progress_tx(&self) -> ProgressTx {
        self.progress.clone()
    }

    /// Begin a task of `kind` on this repo (lock per (repo, kind); packets
    /// mirrored into the repo channel).
    pub fn begin_task(&self, kind: &str, params: HashMap<String, String>) -> Begin {
        self.tasks.begin(
            &self.id.to_string(),
            kind,
            params,
            Some(self.progress.clone()),
        )
    }

    /// Whether the live pack set fits this instance's cache as a full local
    /// copy (what the web API's object path needs to go local instead of the
    /// remote reader).
    pub fn packs_fit(&self) -> bool {
        self.check_fits(&self.manifest(), SyncLevel::Full).is_ok()
    }

    /// Whether a serving copy fits: like [`packs_fit`] but base packs that can
    /// be linked from the store mount count only their side-files.
    pub fn serve_fits(&self) -> bool {
        self.check_fits(&self.manifest(), SyncLevel::Serve).is_ok()
    }

    /// Repository root inside the mounted bucket (`cache.store_mount`), if
    /// configured and present: `<mount>/<store.prefix>repos/<o>/<r>/`.
    pub fn mount_dir(&self) -> Option<std::path::PathBuf> {
        let mount = self.cfg.cache.store_mount.as_ref()?;
        let dir = mount
            .join(self.cfg.store_prefix())
            .join(self.id.store_prefix());
        dir.is_dir().then_some(dir)
    }

    /// Whether `checksum`'s pack is readable from the store mount.
    pub fn mounted_pack(&self, checksum: &str) -> Option<std::path::PathBuf> {
        let dir = self.mount_dir()?;
        let p = crate::sync::mount_pack_path(&dir, checksum);
        p.is_file().then_some(p)
    }

    /// Current remote reader (if this repo is served remotely).
    pub fn remote(&self) -> Option<Arc<RemotePacks>> {
        self.remote.lock().clone()
    }

    /// Make objects readable: full sync (packs local) when they fit, otherwise
    /// a refs sync plus the remote reader (indexes local, data by range read).
    /// Long work registers as a task ("materialize" / "remote-index") whose
    /// progress streams on the repo channel.
    pub async fn sync_objects(
        &self,
    ) -> Result<(crate::sync::ReadGuard<'_>, ObjectAccess), WalError> {
        // Common case: packs fit (as far as the last known manifest says) →
        // one full sync, one manifest round trip. TooLarge (manifest grew) falls
        // through to the remote path.
        if !self.cfg.wal.remote_objects || self.packs_fit() {
            match self.sync_level(SyncLevel::Serve).await {
                Ok(guard) => return Ok((guard, ObjectAccess::Local)),
                Err(WalError::TooLarge { .. }) if self.cfg.wal.remote_objects => {}
                Err(e) => return Err(e),
            }
        }
        let guard = self.sync_level(SyncLevel::Refs).await?;
        let manifest = guard.manifest();
        if self.check_fits(&manifest, SyncLevel::Full).is_ok() {
            drop(guard);
            let guard = self.sync_level(SyncLevel::Serve).await?;
            return Ok((guard, ObjectAccess::Local));
        }
        // Remote: reuse the reader for this manifest revision, else (re)open.
        if let Some(r) = self.remote.lock().clone()
            && r.revision == manifest.revision
        {
            return Ok((guard, ObjectAccess::Remote(r)));
        }
        let remote = self.open_remote(&manifest).await?;
        Ok((guard, ObjectAccess::Remote(remote)))
    }

    async fn open_remote(&self, manifest: &Manifest) -> Result<Arc<RemotePacks>, WalError> {
        match self.begin_task("remote-index", HashMap::new()) {
            Begin::Started(task) => {
                let reporter = task.reporter();
                let store = self.store.clone();
                let manifest = manifest.clone();
                let path = self.local.path().to_path_buf();
                let hash = self.local.object_format().kind();
                let blocks = self.blocks.clone();
                let object_cache_bytes = self.cfg.cache.remote_object_bytes.as_u64();
                let reporter_bulk = reporter.clone();
                let res = crate::sync::on_bulk_runtime(async move {
                    RemotePacks::open(
                        store,
                        &manifest,
                        &path,
                        hash,
                        blocks,
                        object_cache_bytes,
                        &reporter_bulk,
                    )
                    .await
                })
                .instrument(task.span())
                .await;
                match res {
                    Ok(r) => {
                        let r = Arc::new(r);
                        *self.remote.lock() = Some(r.clone());
                        task.finish_ok(
                            format!(
                                "{} pack index(es) ready, {} objects addressable",
                                r.pack_count(),
                                r.total_objects()
                            ),
                            None,
                        );
                        Ok(r)
                    }
                    Err(e) => {
                        task.finish_err(503, e.to_string());
                        Err(e)
                    }
                }
            }
            Begin::AlreadyRunning(state) => {
                // Another request is opening it: wait for that task, then reuse.
                let _ = state.wait_done(std::time::Duration::from_secs(600)).await;
                match state.outcome() {
                    Some(Ok(_)) => {}
                    Some(Err((_, m))) => {
                        return Err(WalError::Corrupt(format!("remote index task failed: {m}")));
                    }
                    None => return Err(WalError::Corrupt("remote index task vanished".into())),
                }
                self.remote
                    .lock()
                    .clone()
                    .ok_or_else(|| WalError::Corrupt("remote reader missing after task".into()))
            }
        }
    }

    pub(crate) fn set_self_arc(&self, arc: Arc<RepoHandle>) {
        let _ = self.self_arc.set(arc);
    }

    // ---- public API ----

    pub fn id(&self) -> &RepoId {
        &self.id
    }

    pub fn local(&self) -> &LocalRepo {
        &self.local
    }

    pub fn store(&self) -> &Prefixed {
        &self.store
    }

    pub fn config(&self) -> &Arc<walgit_config::Config> {
        &self.cfg
    }

    pub fn manifest(&self) -> Arc<Manifest> {
        self.manifest.read().clone()
    }

    pub fn manifest_version(&self) -> Option<Version> {
        self.manifest_version.lock().clone()
    }

    /// The `(manifest, version)` pair a CAS must be based on, read in the only
    /// order that cannot pair an old manifest with a new version.
    ///
    /// Writers publish a new manifest and then its version (separate locks), so
    /// reading version-first makes `(old manifest, new version)` unobservable:
    /// if we see the new version, the manifest write that preceded it is already
    /// visible. The opposite pairing — new manifest, old version — is harmless:
    /// the CAS simply loses and retries.
    ///
    /// Every `PutMode::Update(version)` decision must use this (#175).
    pub fn manifest_snapshot(&self) -> (Arc<Manifest>, Option<Version>) {
        let _pair = self.manifest_pair.lock();
        let version = self.manifest_version.lock().clone();
        let manifest = self.manifest.read().clone();
        (manifest, version)
    }

    /// Install a new `(manifest, version)` pair atomically. Every writer must
    /// use this instead of assigning the two fields separately.
    pub(crate) fn install_manifest(&self, manifest: Arc<Manifest>, version: Option<Version>) {
        let _pair = self.manifest_pair.lock();
        *self.manifest.write() = manifest;
        *self.manifest_version.lock() = version;
    }

    /// Last applied log entry sequence (local replay progress).
    pub fn applied_seq(&self) -> u64 {
        self.state.lock().applied_seq
    }

    /// Whether this process rebuilt refs from a verified checkpoint + log tail.
    pub(crate) fn refs_verified(&self) -> bool {
        self.refs_verified.load(Ordering::Acquire)
    }

    pub(crate) fn mark_refs_verified(&self) {
        self.refs_verified.store(true, Ordering::Release);
    }

    /// Invalidate the process-local refs proof before any path that resets or
    /// rewrites refs (force rebuild / rematerialize). A failed rebuild must not
    /// leave a previously-true flag behind (issue #148 review).
    pub(crate) fn mark_refs_unverified(&self) {
        self.refs_verified.store(false, Ordering::Release);
    }

    /// Whether this process reconciled the local pack set against the manifest.
    pub(crate) fn packs_verified(&self) -> bool {
        self.packs_verified.load(Ordering::Acquire)
    }

    pub(crate) fn mark_packs_verified(&self) {
        self.packs_verified.store(true, Ordering::Release);
    }

    pub(crate) fn mark_packs_unverified(&self) {
        self.packs_verified.store(false, Ordering::Release);
    }

    pub(crate) fn stage_pack(&self, checksum: &str) {
        *self
            .staged_packs
            .lock()
            .entry(checksum.to_string())
            .or_default() += 1;
    }

    pub(crate) fn unstage_pack(&self, checksum: &str) -> bool {
        let mut staged = self.staged_packs.lock();
        if let Some(count) = staged.get_mut(checksum) {
            if *count > 1 {
                *count -= 1;
                false
            } else {
                staged.remove(checksum);
                true
            }
        } else {
            false
        }
    }

    pub(crate) fn pack_staged(&self, checksum: &str) -> bool {
        self.staged_packs.lock().contains_key(checksum)
    }

    /// Stage a pack for as long as the returned guard lives. Unlike the
    /// manual stage/unstage pair this is cancellation-safe: a dropped future
    /// releases the protection. Returns `None` when the handle has no
    /// self-`Arc` yet (unit construction), in which case callers keep the
    /// existing manual pairing.
    /// Shared lock used by ingest/repack/publish while a pack is visible but
    /// not yet in the manifest. `reconcile_packs_inner` takes it with
    /// `try_lock` and defers pruning if a writer holds it.
    pub fn prune_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(&self.prune_lock)
    }

    /// Acquire the writer side of pack reconciliation. Holding this guard means
    /// the process-local pack proof is invalid until a later reconcile succeeds,
    /// including when the mutation fails before any `StagedPackGuard` exists.
    pub async fn prune_guard(&self) -> tokio::sync::OwnedMutexGuard<()> {
        let guard = self.prune_lock.clone().lock_owned().await;
        self.mark_packs_unverified();
        guard
    }

    pub fn stage_pack_guard(&self, checksum: &str) -> Option<StagedPackGuard> {
        let handle = self.self_arc.get().cloned()?;
        self.stage_pack(checksum);
        Some(StagedPackGuard {
            handle,
            checksum: checksum.to_string(),
        })
    }

    /// Issue #4 reverse-direction diagnostics (`WALGIT_TEST_REFS_DIAG`): the
    /// read-side truth in one dump — the manifest's named log segments (each
    /// segment's folded-up-to seq) **probed against the bucket**, the slot a
    /// next writer would claim at `head_seq + 1` (an unlisted/late segment
    /// there would be foldable into every sync), the applied state, and the
    /// refs cache/disk snapshot. Called by the advertise paths when the tip
    /// they served disagrees with the local `ref_view`; env-gated (the store
    /// GETs are the price of a red that carries the mechanism, and prod never
    /// reaches them because the gate is checked first).
    pub async fn refs_diag_read_dump(&self, tag: &str, detail: &str) {
        if std::env::var("WALGIT_TEST_REFS_DIAG").is_err() {
            return;
        }
        let m = self.manifest();
        let state = self.state.lock().clone();
        eprintln!("REFS_DIAG {tag} repo={} {detail}", self.id);
        eprintln!(
            "REFS_DIAG   state: head_seq={} applied_seq={} revision={} manifest_revision={} manifest_version={:?}",
            m.head_seq,
            state.applied_seq,
            state.revision,
            m.revision,
            self.manifest_version
                .lock()
                .as_ref()
                .map(walgit_store::Version::as_str)
        );
        // Manifest-named log segments vs the bucket: a reader folds exactly
        // these segments (replay_log filters `manifest.log_segments`), so a
        // segment missing from the bucket = a silently skipped tail entry and
        // a segment present but unlisted would be an orphan no reader folds.
        // Meta-only probes: `head` costs a stat per segment; `get` would pull
        // every named segment's full body over the wire just to print a size.
        for s in &m.log_segments {
            let bucket = match self.store.head(&s.key).await {
                Ok(Some(meta)) => format!("present {}B", meta.size),
                Ok(None) => "ABSENT".to_string(),
                Err(e) => format!("err {e}"),
            };
            eprintln!(
                "REFS_DIAG   seg key={} folded={}..{} bucket={bucket}",
                s.key, s.first_seq, s.last_seq
            );
        }
        // The slot the next writer would claim, and whether an unlisted
        // segment already sits there (a CAS loser that left its segment).
        let next_seq = m.head_seq + 1;
        let next_key = keys::log_segment_key(next_seq);
        let next_bucket = match self.store.head(&next_key).await {
            Ok(Some(meta)) => format!("present {}B", meta.size),
            Ok(None) => "absent".to_string(),
            Err(e) => format!("err {e}"),
        };
        let listed = m
            .log_segments
            .iter()
            .any(|s| s.key == next_key || (s.first_seq..=s.last_seq).contains(&next_seq));
        eprintln!(
            "REFS_DIAG   slot head+1={next_seq} key={next_key} bucket={next_bucket} listed={listed}"
        );
        // The refs cache + disk snapshot for refs/heads/main (the test ref;
        // the refs_diag fields answer which layer the view came from).
        let d = self.local.refs_diag("refs/heads/main");
        let (cache_key_gen, cache_current, cache_data, cache_pending) = match &d.cache {
            Some(c) => (
                c.key_generation,
                c.current,
                c.data_oid.clone(),
                c.pending_oids.clone(),
            ),
            None => (0, false, String::new(), Vec::new()),
        };
        eprintln!(
            "REFS_DIAG   refs/heads/main: gen={} parses={} loose={} packed={} head={}",
            d.generation, d.parses, d.loose_oid, d.packed_oid, d.head
        );
        eprintln!(
            "REFS_DIAG   cache: key_gen={cache_key_gen} current={cache_current} data={cache_data} pending={cache_pending:?}"
        );
    }

    /// Persisted manifest version string from the local state file.
    pub fn local_version(&self) -> Option<String> {
        self.state.lock().manifest_version.clone()
    }

    pub fn last_access(&self) -> Instant {
        *self.last_access.lock()
    }

    pub fn touch(&self) {
        *self.last_access.lock() = Instant::now();
    }

    /// Freshness check + serving catch-up (refs **and** packs; base packs
    /// linked from the store mount when configured, see [`SyncLevel::Serve`]).
    /// Returns a read guard; while any guard is alive no pack is removed
    /// locally. Required before anything that reads or verifies objects
    /// (upload-pack, receive-pack).
    pub async fn sync(&self) -> Result<crate::sync::ReadGuard<'_>, WalError> {
        self.sync_level(SyncLevel::Serve).await
    }

    /// Like [`sync`] but every pack is a real local copy (base rebuilds,
    /// bundle builds and anything else that streams whole packs). Refused with
    /// `TooLarge` when the set does not fit `cache.max_bytes`.
    pub async fn sync_full(&self) -> Result<crate::sync::ReadGuard<'_>, WalError> {
        self.sync_level(SyncLevel::Full).await
    }

    /// Freshness check + refs-only catch-up: applies the WAL's ref state but
    /// downloads no packs. This is the cheap cold-start path for
    /// `info/refs`, `ls-refs`, `bundle-uri` and the web `refs` endpoint.
    /// When packs are not yet reconciled and `wal.prefetch_packs` is on, a
    /// background Serve sync is kicked off so the first fetch finds them — for
    /// pack sets up to `wal.prefetch_max_bytes` only ([`prefetch_wanted`]).
    pub async fn sync_refs(&self) -> Result<crate::sync::ReadGuard<'_>, WalError> {
        let guard = self.sync_level(SyncLevel::Refs).await?;
        if self.prefetch_wanted() {
            self.spawn_pack_prefetch();
        }
        Ok(guard)
    }

    /// Whether a refs-level sync should pull the serving copy in the background:
    /// configured, not yet reconciled, this host serves the repository's objects
    /// (placement — a host that does not never pulls its packs, not even in the
    /// background), the copy fits the cache, and what lands on disk is at most
    /// `wal.prefetch_max_bytes` (0 = unbounded). Bigger sets are materialized by
    /// the request that needs them, narrated.
    pub fn prefetch_wanted(&self) -> bool {
        if !self.cfg.wal.prefetch_packs || self.packs_ready() {
            return false;
        }
        if !self.cfg.placement.serves(self.id.owner(), self.id.name()) || !self.serve_fits() {
            return false;
        }
        let cap = self.cfg.wal.prefetch_max_bytes.as_u64();
        if cap == 0 {
            return true;
        }
        let manifest = self.manifest();
        let bytes: u64 = self
            .serve_plan(&manifest, SyncLevel::Serve)
            .iter()
            .map(|(p, how)| how.tmpfs_bytes(p))
            .sum();
        if bytes > cap {
            tracing::debug!(repo = %self.id, bytes, cap, "pack prefetch skipped: serving copy above wal.prefetch_max_bytes; materialized on demand");
            return false;
        }
        true
    }

    /// True when the persisted state says the local pack set matches the last
    /// applied manifest. Object-serving paths also require `packs_verified`:
    /// on a fresh process the state may look clean while the cache contains an
    /// orphan pack from a crash, so `level_satisfied` forces one scan.
    pub fn packs_ready(&self) -> bool {
        self.state.lock().packs_ready()
    }

    /// Install history packs (D18) in the background: a `history-pack` task
    /// under `pack_mutex`, narrated like any materialization; serving goes on
    /// from the base meanwhile and switches to the local history pack once
    /// the midx is in place.
    pub(crate) fn spawn_history_pack_install(&self, packs: Vec<PackRef>) {
        if self.history_install_inflight.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(arc) = self.self_arc.get().cloned() else {
            self.history_install_inflight
                .store(false, Ordering::Release);
            return;
        };
        tokio::spawn(async move {
            let span = tracing::info_span!("wal.history_pack_install", repo = %arc.id, packs = packs.len());
            let res = async {
                let _pack_guard = crate::lockwait::timed(
                    "pack_mutex",
                    &arc.id,
                    arc.cfg.telemetry.lock_wait_warn,
                    || arc.pack_mutex.try_lock().ok(),
                    arc.pack_mutex.lock(),
                )
                .await;
                let manifest = arc.manifest();
                let task = match arc.begin_task("history-pack", HashMap::new()) {
                    Begin::Started(t) => Some(t),
                    Begin::AlreadyRunning(_) => None,
                };
                if let Some(t) = &task {
                    *arc.active_reporter.lock() = Some(t.reporter());
                }
                let arc2 = arc.clone();
                let r = crate::sync::on_bulk_runtime(async move {
                    crate::sync::reconcile_packs_inner(&arc2, &manifest, SyncLevel::Serve, true)
                        .await
                })
                .await;
                *arc.active_reporter.lock() = None;
                if let Some(t) = task {
                    match &r {
                        Ok(()) => {
                            t.finish_ok(
                                "history pack installed: commits + trees are local".to_string(),
                                None,
                            );
                        }
                        Err(e) => {
                            t.finish_err(500, e.to_string());
                        }
                    }
                }
                r
            }
            .instrument(span)
            .await;
            if let Err(e) = res {
                tracing::warn!(repo = %arc.id, error = %e, "history pack install failed");
            }
            arc.history_install_inflight.store(false, Ordering::Release);
            arc.history_install_done.notify_waiters();
        });
    }

    /// Whether a background history-pack install (D18) is running now.
    pub fn history_pack_install_inflight(&self) -> bool {
        self.history_install_inflight.load(Ordering::Acquire)
    }

    /// Wait until no history-pack install is in flight (returns immediately
    /// when none is). Prewarm uses it so `/readyz` only flips once the serving
    /// copy is complete — history pack + midx installed — not merely planned.
    pub async fn wait_history_pack_installed(&self) {
        loop {
            let notified = self.history_install_done.notified();
            if !self.history_pack_install_inflight() {
                return;
            }
            notified.await;
        }
    }

    fn spawn_pack_prefetch(&self) {
        if self.prefetch_inflight.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(arc) = self.self_arc.get().cloned() else {
            self.prefetch_inflight.store(false, Ordering::Release);
            return;
        };
        tokio::spawn(async move {
            let span = tracing::info_span!("wal.prefetch_packs", repo = %arc.id);
            if let Err(e) = arc.sync_impl_level(SyncLevel::Serve).instrument(span).await {
                tracing::warn!(repo = %arc.id, error = ?e, "background pack prefetch failed");
            }
            arc.prefetch_inflight.store(false, Ordering::Release);
        });
    }

    async fn sync_level(&self, level: SyncLevel) -> Result<crate::sync::ReadGuard<'_>, WalError> {
        let span = tracing::info_span!(
            "wal.sync",
            repo = %self.id,
            level = ?level,
            changed = false,
            entries_applied = 0u64,
        );
        self.touch();
        // Phase 1 — refs (manifest freshness + ref state), under sync_mutex
        // only; sub-second. Phase 2 — packs, under pack_mutex only. Neither
        // takes `rw.write()`: that lock exists solely so a superseded pack is
        // not unlinked under an active reader, and it is only ever
        // `try_write()`n (see sync.rs) — a queued writer on a tokio RwLock
        // blocks every *new* reader until all current readers are gone, and a
        // clone's ReadGuard lives for the whole stream (prod 2026-08-20: a
        // 24-minute clone + one queued writer = every info/refs on the
        // instance waited 60–680 s on `rw.read()`).
        self.sync_refs_phase(&span).instrument(span.clone()).await?;
        if level.wants_packs() {
            self.sync_packs_phase(level, &span)
                .instrument(span.clone())
                .await?;
        }
        let read_guard = crate::lockwait::timed(
            "rw.read",
            &self.id,
            self.cfg.telemetry.lock_wait_warn,
            || self.rw.try_read().ok(),
            self.rw.read().instrument(span.clone()),
        )
        .await;
        Ok(crate::sync::ReadGuard {
            _guard: read_guard,
            handle: self,
        })
    }

    /// Manifest freshness check + ref state apply (never packs, never
    /// `rw.write()`: ref files and the gix handle are replaced atomically).
    async fn sync_refs_phase(&self, span: &tracing::Span) -> Result<(), WalError> {
        if self.refs_verified() && self.freshness_ttl_active() {
            return Ok(());
        }
        let _sync_guard = crate::lockwait::timed(
            "sync_mutex",
            &self.id,
            self.cfg.telemetry.lock_wait_warn,
            || self.sync_mutex.try_lock().ok(),
            self.sync_mutex.lock(),
        )
        .await;
        if self.refs_verified() && self.freshness_ttl_active() {
            return Ok(());
        }
        let force_refs_rebuild = !self.refs_verified();
        self.sync_locked_inner(span, force_refs_rebuild).await
    }

    /// Make the local pack set match the plan for `level` (download / link /
    /// remote-serve / drop superseded). Registers the `materialize` task.
    /// Serialized by `pack_mutex`; concurrent readers keep reading (packs are
    /// only added; removals take the write lock for the rename alone).
    async fn sync_packs_phase(
        &self,
        level: SyncLevel,
        _span: &tracing::Span,
    ) -> Result<(), WalError> {
        if self.level_satisfied(level) {
            return Ok(());
        }
        // A second caller of a running materialize waits here: the task join, measured.
        let _pack_guard = crate::lockwait::timed(
            "pack_mutex",
            &self.id,
            self.cfg.telemetry.lock_wait_warn,
            || self.pack_mutex.try_lock().ok(),
            self.pack_mutex.lock(),
        )
        .await;
        if self.level_satisfied(level) {
            return Ok(());
        }
        let manifest = self.manifest();
        self.check_fits(&manifest, level)?;
        let task = match self.begin_task("materialize", HashMap::new()) {
            Begin::Started(t) => {
                *self.active_reporter.lock() = Some(t.reporter());
                Some(t)
            }
            Begin::AlreadyRunning(_) => None, // cannot happen: pack_mutex is held
        };
        // The whole materialization runs on the bulk runtime (own threads):
        // nothing in it can stall this runtime's request workers.
        let arc = self.self_arc.get().cloned();
        let res = if let Some(arc) = arc {
            let m = manifest.clone();
            let task_span = task.as_ref().map(super::tasks::TaskHandle::span);
            crate::sync::on_bulk_runtime(async move {
                let work = async {
                    crate::sync::reconcile_packs(&arc, &m, level).await?;
                    arc.local.refresh_async().await?;
                    Ok::<(), WalError>(())
                };
                match task_span {
                    Some(sp) => work.instrument(sp).await,
                    None => work.await,
                }
            })
            .await
        } else {
            let res = async {
                crate::sync::reconcile_packs(self, &manifest, level).await?;
                self.local.refresh_async().await?;
                Ok::<(), WalError>(())
            };
            match &task {
                Some(t) => res.instrument(t.span()).await,
                None => res.await,
            }
        };
        *self.active_reporter.lock() = None;
        if let Some(t) = task {
            match &res {
                Ok(()) => {
                    t.finish_ok(
                        format!("local copy complete at seq {}", self.applied_seq()),
                        None,
                    );
                }
                Err(e) => {
                    let status = if matches!(e, WalError::TooLarge { .. }) {
                        503
                    } else {
                        500
                    };
                    t.finish_err(status, e.to_string());
                }
            }
        }
        res
    }

    /// Freshness check + apply, with `sync_mutex` and the write lock held by
    /// the caller.
    /// Never pull a pack set that cannot fit the local cache (tmpfs):
    /// a 32 GB monorepo must not OOM a 20 GiB instance. Refs-level syncs are fine.
    /// Counts only what would land on tmpfs (see [`serve_plan`]).
    fn check_fits(&self, manifest: &Manifest, level: SyncLevel) -> Result<(), WalError> {
        let max = self.cfg.cache_budget_bytes();
        let plan = self.serve_plan(manifest, level);
        let bytes: u64 = plan.iter().map(|(p, how)| how.tmpfs_bytes(p)).sum();
        if max > 0 && bytes > max {
            metrics::counter!("walgit_sync_too_large_total").increment(1);
            return Err(WalError::TooLarge { bytes, max });
        }
        Ok(())
    }

    /// How each live pack is served on this instance at `level`:
    /// * every pack a local copy when the whole set fits `cache.max_bytes`
    ///   (or the level is `Full`);
    /// * otherwise (`Serve`) tier-2 bases are **linked** from the store mount
    ///   when one has them, else **remote-served** (commit-graph layer local,
    ///   data through the remote reader; `wal.remote_objects`), tiers < 2 stay
    ///   local copies — that is the invariant "everything newer than the base
    ///   fits every instance".
    pub fn serve_plan(&self, manifest: &Manifest, level: SyncLevel) -> Vec<(PackRef, PackPlan)> {
        let max = self.cfg.cache_budget_bytes();
        let full: u64 = manifest
            .packs
            .iter()
            .map(|p| p.pack_size + p.idx_size)
            .sum();
        if level != SyncLevel::Serve || max == 0 || full <= max {
            return manifest
                .packs
                .iter()
                .map(|p| (p.clone(), PackPlan::Local))
                .collect();
        }
        let mount = self.mount_dir();
        if mount.is_none()
            && let Some(m) = self.cfg.cache.store_mount.as_ref()
        {
            tracing::warn!(repo = %self.id, mount = %m.display(), "store mount configured but the repository directory is not visible in it (gcsfuse not up yet?): base packs served remotely until it is");
        }
        manifest
            .packs
            .iter()
            .map(|p| {
                // History packs (commits + trees of a base) are always local:
                // that is the point of them.
                let how = if p.tier != 2 || p.kind == walgit_proto::v1::PackKind::History as i32 {
                    PackPlan::Local
                } else if let Some(m) = mount.as_ref() {
                    let target = crate::sync::mount_pack_path(m, &p.checksum);
                    if target.is_file() {
                        PackPlan::Link(target)
                    } else {
                        tracing::warn!(repo = %self.id, pack = %p.checksum, target = %target.display(), "base pack not readable through the store mount (gcsfuse not up yet?): serving it remotely until it is");
                        if self.cfg.wal.remote_objects { PackPlan::Remote } else { PackPlan::Local }
                    }
                } else if self.cfg.wal.remote_objects {
                    PackPlan::Remote
                } else {
                    PackPlan::Local
                };
                (p.clone(), how)
            })
            .collect()
    }

    /// Tier-2 packs this instance serves remotely (see [`PackPlan::Remote`]).
    /// Non-empty means upload-pack must use the gix engine with an
    /// [`ObjectFaulter`](walgit_git::ObjectFaulter) over [`remote_reader`].
    pub fn remote_served(&self) -> Vec<String> {
        self.state.lock().remote_served.clone()
    }

    /// The remote reader for the current manifest (opened once per manifest
    /// revision; pack indexes local, data by range read). Registers the
    /// `remote-index` task while opening.
    pub async fn remote_reader(&self) -> Result<Arc<RemotePacks>, WalError> {
        let manifest = self.manifest();
        if let Some(r) = self.remote.lock().clone()
            && r.revision == manifest.revision
        {
            return Ok(r);
        }
        self.open_remote(&manifest).await
    }

    /// Freshness check + refs apply, with `sync_mutex` and the write lock held
    /// by the caller. Packs are never touched here (see `sync_packs_phase`).
    async fn sync_locked_inner(
        &self,
        span: &tracing::Span,
        force_refs_rebuild: bool,
    ) -> Result<(), WalError> {
        let known = self.manifest_version.lock().clone();
        let outcome = crate::sync::freshness_check(&self.store, known.as_ref()).await?;
        let force_refs_rebuild = force_refs_rebuild || !self.refs_verified();
        match outcome {
            crate::sync::SyncOutcome::Unchanged => {
                if force_refs_rebuild {
                    let manifest = self.manifest();
                    let version = known.ok_or_else(|| {
                        WalError::Corrupt(
                            "manifest version missing while rebuilding refs".to_string(),
                        )
                    })?;
                    let before = self.state.lock().applied_seq;
                    crate::sync::apply_delta_with_rebuild(self, &manifest, &version, true).await?;
                    span.record("entries_applied", manifest.head_seq.saturating_sub(before));
                    self.mark_refs_verified();
                }
                self.update_freshness();
            }
            crate::sync::SyncOutcome::Changed {
                meta_version,
                manifest,
            } => {
                // Monotonic: never apply a manifest older than the one this instance already holds. A
                // publish on this instance commits locally (manifest, version, refs) outside this lock, so
                // a sync that read the manifest just before that CAS can arrive here with the previous
                // revision — applying it rewrote packed-refs to the pre-push state and rolled the known
                // version back, so one `ls-remote` right after an acknowledged push answered the OLD tip
                // (a concurrency regression test; the next request's conditional GET then
                // repaired it). The revision increments on every manifest write.
                let cur = self.manifest();
                let initialised = self.manifest_version.lock().is_some();
                if manifest.revision < cur.revision {
                    tracing::debug!(repo = %self.id, read_rev = manifest.revision, held_rev = cur.revision, "stale manifest read ignored (a local publish is ahead)");
                    if force_refs_rebuild {
                        let version = self.manifest_version.lock().clone().ok_or_else(|| {
                            WalError::Corrupt(
                                "manifest version missing while rebuilding refs".to_string(),
                            )
                        })?;
                        let before = self.state.lock().applied_seq;
                        crate::sync::apply_delta_with_rebuild(self, &cur, &version, true).await?;
                        span.record("entries_applied", cur.head_seq.saturating_sub(before));
                        self.mark_refs_verified();
                    }
                    self.update_freshness();
                    return Ok(());
                }
                if initialised && manifest.revision == cur.revision {
                    if force_refs_rebuild {
                        let before = self.state.lock().applied_seq;
                        crate::sync::apply_delta_with_rebuild(self, &manifest, &meta_version, true)
                            .await?;
                        span.record("entries_applied", manifest.head_seq.saturating_sub(before));
                        self.mark_refs_verified();
                    }
                    // Same content under a version we did not record (a publish that learned the version
                    // by HEAD): adopt the version so the next check is a 304.
                    self.install_manifest(Arc::new(*manifest), Some(meta_version));
                    self.update_freshness();
                    return Ok(());
                }
                span.record("changed", true);
                let before = self.state.lock().applied_seq;
                crate::sync::apply_delta_with_rebuild(
                    self,
                    &manifest,
                    &meta_version,
                    force_refs_rebuild,
                )
                .await?;
                span.record("entries_applied", manifest.head_seq.saturating_sub(before));
                self.install_manifest(Arc::new(*manifest), Some(meta_version));
                self.mark_refs_verified();
                self.update_freshness();
            }
        }
        Ok(())
    }

    fn level_satisfied(&self, level: SyncLevel) -> bool {
        match level {
            SyncLevel::Refs => true,
            // A base remote-served only because the mount was not yet
            // readable (gcsfuse comes up after the container) is re-planned
            // on the next sync once the file is visible.
            SyncLevel::Serve => {
                self.packs_verified() && self.packs_ready() && !self.remote_served_but_mountable()
            }
            SyncLevel::Full => {
                self.packs_verified() && self.packs_ready() && !self.has_linked_packs()
            }
        }
    }

    /// Remote-served packs whose file is now readable through the mount.
    fn remote_served_but_mountable(&self) -> bool {
        let served = self.remote_served();
        if served.is_empty() {
            return false;
        }
        let Some(m) = self.mount_dir() else {
            return false;
        };
        served
            .iter()
            .any(|c| crate::sync::mount_pack_path(&m, c).is_file())
    }

    /// Any local pack that is a symlink into the store mount.
    fn has_linked_packs(&self) -> bool {
        self.local.packs().is_ok_and(|ps| {
            ps.iter()
                .any(|p| self.local.pack_path(&p.checksum).is_symlink())
        })
    }

    /// Internal serving sync (no read guard). Used by `publish/checkpoint/read_log`.
    pub(crate) async fn sync_impl(&self) -> Result<(), WalError> {
        self.sync_impl_level(SyncLevel::Serve).await
    }

    pub(crate) async fn sync_impl_level(&self, level: SyncLevel) -> Result<(), WalError> {
        let span = tracing::info_span!("wal.sync", repo = %self.id, level = ?level, changed = false, entries_applied = 0u64);
        self.sync_refs_phase(&span).instrument(span.clone()).await?;
        if level.wants_packs() {
            self.sync_packs_phase(level, &span)
                .instrument(span.clone())
                .await?;
        }
        Ok(())
    }

    /// Force a verified full refs rebuild, bypassing the freshness TTL and the
    /// persisted `applied_seq` shortcut. Checkpointing uses this before it may
    /// fold the log into a new snapshot.
    pub(crate) async fn rebuild_refs(&self) -> Result<(), WalError> {
        let span = tracing::info_span!("wal.sync", repo = %self.id, level = ?SyncLevel::Refs, changed = false, entries_applied = 0u64);
        let _sync_guard = crate::lockwait::timed(
            "sync_mutex",
            &self.id,
            self.cfg.telemetry.lock_wait_warn,
            || self.sync_mutex.try_lock().ok(),
            self.sync_mutex.lock(),
        )
        .await;
        self.sync_locked_inner(&span, true).await
    }

    /// Force full re-materialize from store (repair).
    pub async fn rematerialize(&self) -> Result<(), WalError> {
        let _sync_guard = crate::lockwait::timed(
            "sync_mutex",
            &self.id,
            self.cfg.telemetry.lock_wait_warn,
            || self.sync_mutex.try_lock().ok(),
            self.sync_mutex.lock(),
        )
        .await;
        let _pack_guard = crate::lockwait::timed(
            "pack_mutex",
            &self.id,
            self.cfg.telemetry.lock_wait_warn,
            || self.pack_mutex.try_lock().ok(),
            self.pack_mutex.lock(),
        )
        .await;

        // Read manifest fresh
        let Some((meta, manifest)) =
            crate::store_proto::get_message::<Manifest>(&self.store, walgit_proto::keys::MANIFEST)
                .await?
        else {
            return Err(WalError::NotFound);
        };

        // Reset state and re-materialize. A previous process-local proof is
        // invalid from here: only a complete rebuild may set it again.
        self.mark_refs_unverified();
        crate::sync::materialize_from_scratch(self, &manifest, &meta.version).await?;

        self.install_manifest(Arc::new(manifest), Some(meta.version));
        self.last_freshness.lock().take();

        Ok(())
    }

    /// Publish a push.
    pub async fn publish_push(
        &self,
        pack: Option<walgit_git::IngestedPack>,
        txn: walgit_proto::v1::RefTransaction,
        meta: HashMap<String, String>,
    ) -> Result<PublishResult, WalError> {
        self.enqueue_publish(pack, txn, meta, false).await
    }

    /// Publish a push when the caller has already completed `sync()`.
    ///
    /// Receive-pack holds a read guard while parsing and ingesting the pack.
    /// Reusing that freshness check avoids a second conditional manifest GET
    /// before the publisher's first CAS attempt. The publisher still syncs
    /// after every CAS conflict.
    pub async fn publish_push_synced(
        &self,
        pack: Option<walgit_git::IngestedPack>,
        txn: walgit_proto::v1::RefTransaction,
        meta: HashMap<String, String>,
    ) -> Result<PublishResult, WalError> {
        self.enqueue_publish(pack, txn, meta, true).await
    }

    /// Publish with an explicit entry time (history replay into the WAL): the
    /// log entry's `created_at` is `at` instead of now, validated monotonic
    /// (≥ the head entry's time; ≥ earlier explicit times in the same batch),
    /// else every ref of the transaction is rejected with the reason. The WAL
    /// itself never enforces fast-forward — that is receive-pack/policy — so a
    /// replay may move `main` non-ancestrally between slots; callers pass
    /// `old_oid` = the current value (or "" to skip the old check).
    pub async fn publish_push_at(
        &self,
        pack: Option<walgit_git::IngestedPack>,
        txn: walgit_proto::v1::RefTransaction,
        meta: HashMap<String, String>,
        at: std::time::SystemTime,
    ) -> Result<PublishResult, WalError> {
        self.enqueue_publish_at(
            pack,
            txn,
            meta,
            false,
            Some(walgit_proto::time::from_system(at)),
        )
        .await
    }

    async fn enqueue_publish(
        &self,
        pack: Option<walgit_git::IngestedPack>,
        txn: walgit_proto::v1::RefTransaction,
        meta: HashMap<String, String>,
        synced: bool,
    ) -> Result<PublishResult, WalError> {
        self.enqueue_publish_at(pack, txn, meta, synced, None).await
    }

    async fn enqueue_publish_at(
        &self,
        pack: Option<walgit_git::IngestedPack>,
        txn: walgit_proto::v1::RefTransaction,
        meta: HashMap<String, String>,
        synced: bool,
        created_at: Option<prost_types::Timestamp>,
    ) -> Result<PublishResult, WalError> {
        let staged_checksum = pack.as_ref().map(|p| p.checksum.to_string());
        // Keep the pack protected until the publisher has produced its final
        // result. The guard is owned by a detached task below, not by the
        // caller's future: cancelling the caller must not drop the protection
        // while the publisher is still about to CAS the manifest.
        let waiter = self
            .self_arc
            .get()
            .cloned()
            .ok_or_else(|| WalError::Corrupt("publish requires a self Arc".into()))?;
        let staged_guard = staged_checksum.as_ref().map(|checksum| {
            self.stage_pack(checksum);
            StagedPackGuard {
                handle: waiter.clone(),
                checksum: checksum.clone(),
            }
        });
        self.publish_waiters.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let request = PublishRequest {
            pack,
            txn,
            meta,
            synced,
            created_at,
            response: tx,
        };

        let sender = self.get_or_init_publisher();
        if sender.send(request).is_err() {
            self.publish_waiters.fetch_sub(1, Ordering::Relaxed);
            drop(staged_guard);
            return Err(WalError::Corrupt("publisher channel closed".into()));
        }

        let (caller_tx, caller_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = match rx.await {
                Ok(result) => result,
                Err(_) => Err(WalError::Corrupt("publisher dropped response".into())),
            };
            waiter.publish_waiters.fetch_sub(1, Ordering::Relaxed);
            drop(staged_guard);
            let _ = caller_tx.send(result);
        });
        caller_rx.await.unwrap_or_else(|_| {
            Err(WalError::Corrupt(
                "publish waiter task dropped response".into(),
            ))
        })
    }
    /// Publish a ref-only update (no pack).
    pub async fn publish_ref_update(
        &self,
        txn: walgit_proto::v1::RefTransaction,
        meta: HashMap<String, String>,
    ) -> Result<PublishResult, WalError> {
        self.publish_push(None, txn, meta).await
    }

    /// Publish a compact entry.
    pub async fn publish_compact(
        &self,
        new_pack: walgit_git::PackInfo,
        supersedes: Vec<gix_hash::ObjectId>,
        tier: u32,
    ) -> Result<u64, WalError> {
        let checksum = new_pack.checksum.to_string();
        let _guard = self
            .stage_pack_guard(&checksum)
            .ok_or_else(|| WalError::Corrupt("publish_compact requires a self Arc".into()))?;
        crate::publish::publish_compact_impl(self, new_pack, supersedes, tier).await
    }

    /// D24: the repository's settings as last applied (manifest-inline).
    pub fn settings(&self) -> Option<walgit_proto::v1::RepoSettings> {
        self.manifest().settings.clone()
    }

    /// D24: effective configuration = host config ⊕ this repository's
    /// settings, cached per settings revision. Settings that no longer parse
    /// against this build fall back to the host config with a warning
    /// (never a failure on a read path).
    /// Effective config for a *specific* manifest generation (not "whatever is
    /// current"): D24 settings are inline, so a caller that linearizes on a
    /// manifest must derive its config from that same manifest (#175).
    pub fn effective_config_for(&self, manifest: &Manifest) -> Arc<walgit_config::Config> {
        let Some(settings) = manifest.settings.as_ref() else {
            return self.cfg.clone();
        };
        if settings.revision == 0 {
            return self.cfg.clone();
        }
        match self.cfg.with_settings(settings.toml.as_str()) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                tracing::warn!(repo = %self.id, revision = settings.revision, error = %e, "repo settings do not apply to this build; using the host config");
                self.cfg.clone()
            }
        }
    }

    pub fn effective_config(&self) -> Arc<walgit_config::Config> {
        let settings = self.settings();
        let rev = settings.as_ref().map_or(0, |s| s.revision);
        if rev == 0 {
            return self.cfg.clone();
        }
        if let Some((r, c)) = self.effective.lock().as_ref()
            && *r == rev
        {
            return c.clone();
        }
        let toml = settings.as_ref().map_or("", |s| s.toml.as_str());
        let cfg = match self.cfg.with_settings(toml) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                tracing::warn!(repo = %self.id, revision = rev, error = %e, "repo settings do not apply to this build; using the host config");
                self.cfg.clone()
            }
        };
        *self.effective.lock() = Some((rev, cfg.clone()));
        cfg
    }

    /// D24: validate and publish new settings (whole-document replace): a
    /// SETTINGS log entry + the manifest's inline copy, one CAS. Returns the
    /// new revision. `toml` empty = clear.
    pub async fn publish_settings(
        &self,
        toml: &str,
        author: &str,
        message: &str,
    ) -> Result<u64, WalError> {
        // Validate against THIS host's config (the effective config must load).
        self.cfg
            .with_settings(toml)
            .map_err(|e| WalError::Invalid(format!("{e:#}")))?;
        crate::publish::publish_settings_impl(self, toml, author, message).await
    }

    /// Write checkpoint at current head.
    pub async fn write_checkpoint(&self) -> Result<walgit_proto::v1::CheckpointRef, WalError> {
        crate::checkpoint::write_checkpoint_impl(self).await
    }

    /// Attach side-files (rev/bitmap/commit-graph) to a published pack and
    /// advertise them in the manifest (CAS). See `publish::annotate_pack_impl`.
    pub async fn annotate_pack(
        &self,
        checksum: &str,
        rev: Option<std::path::PathBuf>,
        bitmap: Option<std::path::PathBuf>,
        commit_graph: Option<std::path::PathBuf>,
    ) -> Result<walgit_proto::v1::PackRef, WalError> {
        crate::publish::annotate_pack_impl(self, checksum, rev, bitmap, commit_graph).await
    }

    /// List/clear packs bucket GC is reclaiming (#175). GC lists a pack before
    /// deleting any of its objects; a publisher must not re-adopt a listed
    /// checksum. Both sides go through the manifest CAS.
    /// Returns the manifest the claim set was read/committed against, so the
    /// caller can derive per-repo config (D24 settings are inline) for exactly
    /// that generation (#175).
    pub async fn update_reclaiming(
        &self,
        add: &[String],
        remove_own: &[String],
        recover: &[(String, String, String)],
        token: &str,
    ) -> Result<Arc<Manifest>, WalError> {
        crate::publish::update_reclaiming(self, add, remove_own, recover, token).await
    }

    /// Publish an already built pack (`pack-<checksum>.pack` + `.idx`) as a
    /// tier-`tier` COMPACT entry superseding nothing; `history_of = Some(base)`
    /// marks it a history pack of that base (D18).
    pub async fn add_pack(
        &self,
        pack: &std::path::Path,
        idx: &std::path::Path,
        tier: u32,
        history_of: Option<String>,
    ) -> Result<u64, WalError> {
        let checksum = pack
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("pack-"))
            .and_then(|name| name.strip_suffix(".pack"))
            .map(str::to_string);
        let _guard = checksum
            .as_ref()
            .map(|checksum| {
                self.stage_pack_guard(checksum)
                    .ok_or_else(|| WalError::Corrupt("add_pack requires a self Arc".into()))
            })
            .transpose()?;
        crate::publish::add_pack_impl(self, pack, idx, tier, history_of).await
    }

    /// Whether the last known manifest wants a checkpoint, and why.
    pub fn checkpoint_due(&self) -> Option<crate::checkpoint::CheckpointTrigger> {
        crate::checkpoint::checkpoint_due(&self.manifest(), &self.cfg.wal)
    }

    /// Download `wal/<checksum>.pack` + `.idx` (+ advertised side-files) from the
    /// store into `dir` as `pack-<checksum>.*` (striped), for tooling that
    /// rebuilds a historical copy elsewhere (`walgit wal materialize`). The
    /// live local copy is never touched.
    pub async fn fetch_pack_into(
        &self,
        pack: &walgit_proto::v1::PackRef,
        dir: &std::path::Path,
    ) -> Result<(), WalError> {
        tokio::fs::create_dir_all(dir).await?;
        let c = &pack.checksum;
        let pack_path = dir.join(format!("pack-{c}.pack"));
        let idx_path = dir.join(format!("pack-{c}.idx"));
        let pack_key = walgit_proto::keys::pack_key(c);
        let idx_key = walgit_proto::keys::idx_key(c);
        let pack_size = pack.pack_size;
        let idx_size = pack.idx_size;
        let pack_fut = async {
            crate::sync::download_object(
                &self.store,
                &pack_key,
                &pack_path,
                (pack_size > 0).then_some(pack_size),
                None,
            )
            .await
        };
        let idx_fut = async {
            crate::sync::download_object(
                &self.store,
                &idx_key,
                &idx_path,
                (idx_size > 0).then_some(idx_size),
                None,
            )
            .await
        };
        let mut side_futs = Vec::new();
        for (flag, ext, key) in [
            (pack.has_rev, "rev", walgit_proto::keys::rev_key(c)),
            (pack.has_bitmap, "bitmap", walgit_proto::keys::bitmap_key(c)),
            (
                pack.has_commit_graph,
                "commit-graph",
                walgit_proto::keys::commit_graph_key(c),
            ),
        ] {
            if flag {
                let store = self.store.clone();
                let path = dir.join(format!("pack-{c}.{ext}"));
                side_futs.push(async move {
                    crate::sync::download_object(&store, &key, &path, None, None).await
                });
            }
        }
        let (pack_r, idx_r, sides) =
            tokio::join!(pack_fut, idx_fut, futures::future::join_all(side_futs));
        pack_r?;
        idx_r?;
        let _ = sides;
        Ok(())
    }

    /// When this repository's WAL history starts (oldest replayable state):
    /// the first live log segment's entry time, else the checkpoint's. Slots
    /// before it are "unavailable" for bundles. None when unknown.
    pub fn first_state_time(&self) -> Option<std::time::SystemTime> {
        // Three witnesses that state existed at a time; the earliest wins:
        // the checkpoint's `first_state_at` (D22: the earliest entry ever, carried
        // forward), the checkpoint's own write time — a checkpoint *is* state as
        // of when it was written, whatever its entries say — and the oldest live
        // log entry this process replayed. Prod 2026-08-21: a large repository's import
        // checkpoint (seq 1, 08-19 21:33Z) was ignored because its manifest ref
        // carries no times; the bundler then cut "08-19 23:00" slots from today's
        // main. The times now come from the checkpoint object when the ref has none.
        let cp = self.checkpoint_times();
        [
            cp.as_ref().and_then(|c| c.first_state_at),
            cp.as_ref().and_then(|c| c.created_at),
        ]
        .into_iter()
        .flatten()
        .chain(*self.first_entry_time.lock())
        .min()
    }

    /// Times of the current checkpoint: from the manifest's `CheckpointRef`, else
    /// from the checkpoint object read during sync (`checkpoint_times` cache).
    /// None = no checkpoint, or its times are not known to this process yet.
    pub fn checkpoint_times(&self) -> Option<CheckpointTimes> {
        let m = self.manifest();
        let cp = m.checkpoint.as_ref()?;
        let from_ref = CheckpointTimes {
            created_at: cp.created_at.as_ref().map(walgit_proto::time::to_system),
            first_state_at: cp
                .first_state_at
                .as_ref()
                .map(walgit_proto::time::to_system),
            as_of: cp.as_of.as_ref().map(walgit_proto::time::to_system),
        };
        if from_ref.created_at.is_some() {
            return Some(from_ref);
        }
        match self.checkpoint_times.lock().as_ref() {
            Some((seq, t)) if *seq == cp.seq => Some(*t),
            _ => Some(from_ref),
        }
    }

    /// Read the checkpoint object's times when the manifest ref has none
    /// (one 240-byte GET per checkpoint per process; no-op otherwise).
    pub(crate) async fn learn_checkpoint_times(&self) -> Result<(), WalError> {
        use prost::Message;
        use walgit_store::ObjectStoreExt;
        let m = self.manifest();
        let Some(cp) = m.checkpoint.as_ref() else {
            return Ok(());
        };
        if cp.created_at.is_some()
            || matches!(self.checkpoint_times.lock().as_ref(), Some((seq, _)) if *seq == cp.seq)
        {
            return Ok(());
        }
        if let Some((_, bytes)) = self.store.get_bytes(&cp.key).await? {
            let cpo = walgit_proto::v1::Checkpoint::decode(bytes.as_ref())
                .map_err(|e| WalError::Corrupt(format!("checkpoint decode: {e}")))?;
            let t = CheckpointTimes {
                created_at: cpo.created_at.as_ref().map(walgit_proto::time::to_system),
                first_state_at: None,
                as_of: None,
            };
            *self.checkpoint_times.lock() = Some((cp.seq, t));
        }
        Ok(())
    }

    /// Ref state as of a point in time (checkpoint + log replay in memory,
    /// refs-level, pure). See `log_reader::refs_as_of`.
    pub async fn refs_as_of(
        &self,
        at: std::time::SystemTime,
    ) -> Result<(walgit_proto::v1::RefSnapshot, u64), WalError> {
        crate::log_reader::refs_as_of(self, at).await
    }

    /// Ref state at WAL `seq` exactly (refs-level, pure). See `log_reader::refs_at_seq`.
    pub async fn refs_at_seq(&self, seq: u64) -> Result<walgit_proto::v1::RefSnapshot, WalError> {
        crate::log_reader::refs_at_seq(self, seq).await
    }

    /// Read log entries [`from_seq`, `to_seq`].
    pub async fn read_log(
        &self,
        from_seq: u64,
        to_seq: Option<u64>,
    ) -> Result<Vec<walgit_proto::v1::LogEntry>, WalError> {
        crate::log_reader::read_log_impl(self, from_seq, to_seq).await
    }

    // ---- internal helpers ----

    fn freshness_ttl_active(&self) -> bool {
        let ttl = self.cfg.wal.freshness_ttl;
        if ttl == std::time::Duration::ZERO {
            return false;
        }
        let last = self.last_freshness.lock();
        match *last {
            Some(t) => t.elapsed() < ttl,
            None => false,
        }
    }

    fn update_freshness(&self) {
        *self.last_freshness.lock() = Some(Instant::now());
    }

    fn get_or_init_publisher(&self) -> mpsc::UnboundedSender<PublishRequest> {
        let mut guard = self.publish_tx.lock();
        if let Some(tx) = &*guard {
            // A publisher task that died (panic mid-batch) leaves a sender to
            // a dropped receiver; respawn instead of failing every push on
            // this instance forever.
            if !tx.is_closed() {
                return tx.clone();
            }
            tracing::warn!(repo = %self.id, "publisher task is gone; respawning");
        }
        let (tx, rx) = mpsc::unbounded_channel();
        // `self_arc` is set by the registry right after construction, before the
        // handle is ever handed out; every publish path runs through such a
        // handle, so the OnceLock is always filled here.
        #[allow(
            clippy::expect_used,
            reason = "self_arc is set at construction (registry::RepoRegistry::open) before any handle is published"
        )]
        let arc = self
            .self_arc
            .get()
            .expect("self_arc must be set before publish")
            .clone();
        tokio::spawn(crate::publish::publisher_task(arc, rx));
        *guard = Some(tx.clone());
        tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use walgit_proto::prost::Message;
    use walgit_store::memory::MemoryStore;
    use walgit_store::{PutBody, PutMode, PutOptions};

    /// #175: the `(manifest, version)` pair a CAS is built on must be installed
    /// and observed *atomically*. Two writers that install their two fields
    /// separately can interleave so the cache ends up holding an older manifest
    /// next to a newer version; a CAS then succeeds against a body that predates
    /// the manifest it is updating and silently erases a `reclaiming` claim
    /// bucket GC just listed — the manifest ends up pointing at deleted bytes.
    ///
    /// Here every writer installs a matched pair, so a concurrent snapshot must
    /// never observe them mismatched. Reverting `install_manifest` to two
    /// separate field writes (no `manifest_pair`) makes this fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn manifest_snapshot_never_pairs_a_stale_manifest_with_a_new_version() {
        use std::sync::atomic::AtomicUsize;

        let store = MemoryStore::shared();
        let mut cfg = walgit_config::Config::default();
        cfg.store.backend = walgit_config::StoreBackend::Memory;
        cfg.store.memory_backend_intentional = true;
        cfg.cache.dir = tempfile::tempdir().unwrap().keep();
        let registry = crate::registry::Registry::new(store, Arc::new(cfg));

        let id = RepoId::new("o", "pair").unwrap();
        let seed = Manifest {
            repo: id.to_string(),
            revision: 0,
            ..Default::default()
        };
        let prefixed = Prefixed::new(registry.store().clone(), id.store_prefix());
        prefixed
            .put(
                keys::MANIFEST,
                PutBody::Bytes(seed.encode_to_vec().into()),
                PutOptions::from(PutMode::Create),
            )
            .await
            .unwrap();
        let handle = registry.open(&id).await.unwrap();
        // Normalise the opening pair (a store-generated version) to the same
        // `v<revision>` scheme the writers use, so every snapshot can be checked
        // without special-casing the seed.
        handle.install_manifest(handle.manifest(), Some(Version::new("v0")));

        let writers: u64 = 3;
        let iters = 50_000u64;
        let bad = Arc::new(AtomicBool::new(false));
        let writers_done = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(usize::try_from(writers).unwrap() + 3));

        std::thread::scope(|scope| {
            for w in 0..writers {
                let h = handle.clone();
                let start = start.clone();
                let writers_done = writers_done.clone();
                scope.spawn(move || {
                    start.wait();
                    for i in 0..iters {
                        let rev = (w + 1) * 1_000_000 + i;
                        let mut m = (*h.manifest()).clone();
                        m.revision = rev;
                        h.install_manifest(Arc::new(m), Some(Version::new(format!("v{rev}"))));
                    }
                    writers_done.fetch_add(1, Ordering::Release);
                });
            }
            for _ in 0..3 {
                let h = handle.clone();
                let bad = bad.clone();
                let start = start.clone();
                let writers_done = writers_done.clone();
                scope.spawn(move || {
                    start.wait();
                    // Sample until every writer has stopped, then once more so a
                    // tear written last is still caught.
                    loop {
                        let (m, v) = h.manifest_snapshot();
                        if let Some(v) = v
                            && v.as_str() != format!("v{}", m.revision)
                        {
                            bad.store(true, Ordering::Relaxed);
                        }
                        if writers_done.load(Ordering::Acquire) == usize::try_from(writers).unwrap() {
                            let (m, v) = h.manifest_snapshot();
                            if let Some(v) = v
                                && v.as_str() != format!("v{}", m.revision)
                            {
                                bad.store(true, Ordering::Relaxed);
                            }
                            break;
                        }
                    }
                });
            }
        });

        assert!(
            !bad.load(Ordering::Relaxed),
            "manifest_snapshot observed a torn (manifest, version) pair"
        );
    }
}
