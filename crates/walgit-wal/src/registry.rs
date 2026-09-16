//! Registry: process-wide map of `RepoId` -> Arc<RepoHandle>.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use futures::StreamExt;
use prost::Message;
use walgit_git::{LocalRepo, ObjectFormat, RepoId};
use walgit_proto::WAL_FORMAT_VERSION;
use walgit_proto::keys;
use walgit_proto::v1::Manifest;
use walgit_store::{DynStore, ObjectStore, Prefixed, PutBody, PutMode, StoreError};

use crate::error::WalError;
use crate::handle::RepoHandle;
use crate::state::{RepoState, load_state, save_state};
use crate::store_proto::get_message;

pub struct Registry {
    store: DynStore,
    cfg: Arc<walgit_config::Config>,
    cache_root: std::path::PathBuf,
    repos: DashMap<RepoId, Arc<RepoHandle>>,
    /// Per-repo single-flight guard for open/create so two concurrent first
    /// requests never both `git init` / materialize the same repo.
    opening: DashMap<RepoId, Arc<tokio::sync::Mutex<()>>>,
    /// Background task log + (repo, kind) locks for this instance.
    tasks: Arc<crate::tasks::Tasks>,
    /// Pack-data block cache shared by every remote reader.
    blocks: Arc<crate::remote::BlockCache>,
    /// `list()` answer + when it was computed (`LIST_TTL`); single-flight refresh.
    listing: tokio::sync::Mutex<Option<(Instant, Arc<Vec<RepoId>>)>>,
}

/// How long a repository listing is served from memory before the bucket is asked again.
/// Owner/repo pages and the maintainer's pass both call `list()`; a new repository
/// created on another host appears within this on every instance (on this one immediately).
const LIST_TTL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Default, Debug)]
pub struct EvictReport {
    pub evicted: usize,
    pub remaining_bytes: u64,
}

impl Registry {
    pub fn new(store: DynStore, cfg: Arc<walgit_config::Config>) -> Arc<Self> {
        let cache_root = cfg.cache.dir.clone();
        let blocks = crate::remote::BlockCache::new(cfg.cache.remote_block_bytes.as_u64());
        Arc::new(Registry {
            store,
            cfg,
            cache_root,
            repos: DashMap::new(),
            opening: DashMap::new(),
            tasks: crate::tasks::Tasks::new(),
            blocks,
            listing: tokio::sync::Mutex::new(None),
        })
    }

    pub fn store(&self) -> &DynStore {
        &self.store
    }

    pub fn tasks(&self) -> &Arc<crate::tasks::Tasks> {
        &self.tasks
    }

    pub fn blocks(&self) -> &Arc<crate::remote::BlockCache> {
        &self.blocks
    }

    pub fn config(&self) -> &Arc<walgit_config::Config> {
        &self.cfg
    }

    /// Open an existing repo. Err(NotFound) if manifest.pb absent.
    pub async fn open(&self, id: &RepoId) -> Result<Arc<RepoHandle>, WalError> {
        // Every request that touches a repo goes through here: tag the
        // enclosing `http.request` span (no-op outside one).
        tracing::Span::current().record("repo", tracing::field::display(id));
        if let Some(h) = self.repos.get(id) {
            return Ok(h.clone());
        }
        let gate = self.opening.entry(id.clone()).or_default().clone();
        let _g = gate.lock().await;
        if let Some(h) = self.repos.get(id) {
            return Ok(h.clone());
        }

        let prefix = id.store_prefix();
        let prefixed = Prefixed::new(self.store.clone(), prefix);

        // Read manifest (NotFound if absent)
        let Some((meta, manifest)) = get_message::<Manifest>(&prefixed, keys::MANIFEST).await?
        else {
            return Err(WalError::NotFound);
        };

        // Open or init local repo (LocalRepo joins owner/name.git onto the root).
        let local = if let Some(l) = LocalRepo::open(&self.cache_root, id)? { l } else {
            let format = parse_object_format(&manifest.object_format);
            LocalRepo::init(&self.cache_root, id, format)?
        };

        // Load state
        let state = load_state(local.path());

        // `!=`, not `<`: the WAL is append-only, so `applied_seq > head_seq`
        // means the bucket was wiped/rebuilt and the local refs must be
        // rebuilt from the fold, not kept (#36). An empty manifest always
        // reapplies: `applied_seq == head_seq == 0` can still sit on a
        // leftover local dir whose packed-refs survived the wipe (the state
        // file may even be lost), and resetting them is a trivial write.
        let needs_apply = state.applied_seq != manifest.head_seq || manifest.head_seq == 0;
        let manifest_version = meta.version.clone();

        let handle = RepoHandle::new(
            id.clone(),
            local,
            prefixed,
            self.cfg.clone(),
            manifest.clone(),
            Some(manifest_version.clone()),
            state,
            self.tasks.clone(),
            self.blocks.clone(),
        );

        let handle = Arc::new(handle);
        handle.set_self_arc(handle.clone());

        // The manifest GET above is already fresh. Apply that exact value
        // directly instead of issuing a second manifest GET merely to learn
        // what we already hold. Cold open remains one manifest round, followed
        // by checkpoint/log objects in parallel/sequence as required.
        if needs_apply {
            crate::sync::apply_delta(&handle, &manifest, &manifest_version).await?;
        }

        self.repos.insert(id.clone(), handle.clone());
        Ok(handle)
    }

    /// Delete a repository: every object under its store prefix, the cached
    /// handle and the local copy. Err(NotFound) if the manifest does not exist.
    /// Other instances notice on their next freshness check (manifest GET -> 404).
    pub async fn delete(&self, id: &RepoId) -> Result<(), WalError> {
        let prefixed = Prefixed::new(self.store.clone(), id.store_prefix());
        if get_message::<Manifest>(&prefixed, keys::MANIFEST)
            .await?
            .is_none()
        {
            return Err(WalError::NotFound);
        }
        // Drop the handle first so no request on this instance publishes into a
        // prefix that is being removed; in-flight requests hold their own Arc.
        self.repos.remove(id);
        self.invalidate_listing();
        // Manifest first: it is the linearization point, so the repo disappears
        // atomically for readers; remaining objects are unreferenced garbage.
        prefixed.delete(keys::MANIFEST, None).await?;
        let mut after: Option<String> = None;
        loop {
            let mut stream = prefixed.list_keys("", after.as_deref());
            let mut last = None;
            while let Some(res) = stream.next().await {
                let key = res?;
                prefixed.delete(&key, None).await?;
                last = Some(key);
            }
            match last {
                Some(k) => after = Some(k),
                None => break,
            }
        }
        let local_dir = id.local_dir(&self.cache_root);
        if local_dir.exists() {
            #[cfg(windows)]
            crate::platform::clear_readonly_recursive(&local_dir);
            remove_dir_all_with_retry(&local_dir).await?;
        }
        Ok(())
    }

    /// CAS-create manifest.pb (`PutMode::Create`). Err(AlreadyExists) on 412.
    pub async fn create(
        &self,
        id: &RepoId,
        format: ObjectFormat,
    ) -> Result<Arc<RepoHandle>, WalError> {
        if let Some(h) = self.repos.get(id) {
            return Ok(h.clone());
        }
        let gate = self.opening.entry(id.clone()).or_default().clone();
        let _g = gate.lock().await;
        if let Some(h) = self.repos.get(id) {
            return Ok(h.clone());
        }

        let prefix = id.store_prefix();
        let prefixed = Prefixed::new(self.store.clone(), prefix);

        // Create manifest with PutMode::Create
        let manifest = Manifest {
            format_version: WAL_FORMAT_VERSION,
            repo: id.to_string(),
            object_format: format.as_str().to_string(),
            head_seq: 0,
            min_seq: 0,
            checkpoint: None,
            log_segments: vec![],
            packs: vec![],
            updated_at: Some(walgit_proto::time::now()),
            writer: crate::handle::instance_id(),
            revision: 1,
            settings: None,
            reclaiming: vec![],
        };

        let buf = manifest.encode_to_vec();
        match prefixed
            .put(
                keys::MANIFEST,
                PutBody::Bytes(bytes::Bytes::from(buf)),
                PutMode::Create.into(),
            )
            .await
        {
            Ok(meta) => {
                // Init local repo
                let local = LocalRepo::init(&self.cache_root, id, format)?;
                // `git init --bare` is a no-op on a surviving directory: a
                // leftover cache dir (bucket wiped, repo recreated under the
                // same name) still holds the old packed-refs, and receive-pack
                // would advertise refs the brand-new empty manifest does not
                // know (#36). A fresh manifest means an empty repository —
                // local refs are a pure function of the manifest, so reset
                // them to empty.
                local.load_ref_snapshot(&walgit_proto::v1::RefSnapshot::default())?;

                let state = RepoState::default();
                save_state(local.path(), &state)?;

                let handle = RepoHandle::new(
                    id.clone(),
                    local,
                    prefixed,
                    self.cfg.clone(),
                    manifest,
                    Some(meta.version),
                    state,
                    self.tasks.clone(),
                    self.blocks.clone(),
                );
                let handle = Arc::new(handle);
                handle.set_self_arc(handle.clone());

                self.repos.insert(id.clone(), handle.clone());
                self.invalidate_listing();
                Ok(handle)
            }
            Err(StoreError::PreconditionFailed { .. }) => Err(WalError::AlreadyExists),
            Err(e) => Err(WalError::Store(e)),
        }
    }

    /// Open or create.
    pub async fn open_or_create(
        &self,
        id: &RepoId,
        format: ObjectFormat,
    ) -> Result<Arc<RepoHandle>, WalError> {
        match self.open(id).await {
            Ok(h) => Ok(h),
            Err(WalError::NotFound) => self.create(id, format).await,
            Err(e) => Err(e),
        }
    }

    /// Every repository (sorted owner, name), from memory for `LIST_TTL`, else recomputed with
    /// **delimited** listings — `repos/` → owners, `repos/<o>/` → names (one round, all owners
    /// in parallel) — and one HEAD of `manifest.pb` per candidate (parallel, one round) so a
    /// prefix whose manifest is gone (deleted repository) is not a repository. Never a walk over
    /// the objects under `repos/`: that was 122 k keys and 8–9 s per owners page (2026-08-22).
    pub async fn list(&self) -> Result<Vec<RepoId>, WalError> {
        let mut slot = self.listing.lock().await;
        if let Some((at, repos)) = slot.as_ref()
            && at.elapsed() < LIST_TTL
        {
            return Ok(repos.as_ref().clone());
        }
        let repos = Arc::new(self.list_uncached().await?);
        *slot = Some((Instant::now(), repos.clone()));
        Ok(repos.as_ref().clone())
    }

    /// Forget the cached listing (after this host created or deleted a repository).
    fn invalidate_listing(&self) {
        if let Ok(mut slot) = self.listing.try_lock() {
            *slot = None;
        }
    }

    async fn list_uncached(&self) -> Result<Vec<RepoId>, WalError> {
        let owners = self.store.list_prefixes("repos/").await?;
        let per_owner = futures::stream::iter(owners)
            .map(|owner_prefix| {
                let store = self.store.clone();
                async move { store.list_prefixes(&owner_prefix).await }
            })
            .buffer_unordered(16)
            .collect::<Vec<_>>()
            .await;
        let mut candidates = Vec::new();
        for r in per_owner {
            for repo_prefix in r? {
                // repos/<owner>/<repo>/
                if let Some(id) = repo_prefix
                    .strip_prefix("repos/")
                    .and_then(|s| s.strip_suffix('/'))
                    .and_then(|s| RepoId::from_str(s).ok())
                {
                    candidates.push(id);
                }
            }
        }
        let present = futures::stream::iter(candidates)
            .map(|id| {
                let store = self.store.clone();
                async move {
                    let key = format!("{}{}", id.store_prefix(), keys::MANIFEST);
                    store.head(&key).await.map(|m| m.map(|_| id))
                }
            })
            .buffer_unordered(32)
            .collect::<Vec<_>>()
            .await;
        let mut repos = Vec::new();
        for r in present {
            if let Some(id) = r? {
                repos.push(id);
            }
        }
        repos.sort_by(|a, b| {
            a.owner()
                .cmp(b.owner())
                .then_with(|| a.name().cmp(b.name()))
        });
        Ok(repos)
    }

    /// Disk cache maintenance: evict idle repos beyond `cache.max_bytes` / `evict_idle_after`.
    // pub API: the server's maintenance loop and the sim await it; its work is
    // sync (fs scans + registry removal) so `async` is only the crate boundary.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl, reason = "pub async fn awaited by callers in walgit-server; the body is sync by nature")]
    pub async fn evict_idle(&self) -> Result<EvictReport, WalError> {
        let evict_after = self.cfg.cache.evict_idle_after;
        // D25: budget mode evicts past `cache.max_bytes`; disk mode only under
        // disk pressure (filesystem of `cache.dir` above `disk_high_watermark`)
        // — then down to the low mark (watermark − 10 %), oldest idle first.
        let max_bytes = if self.cfg.cache_is_disk() {
            match disk_usage(&self.cfg.cache.dir) {
                Some((used, total)) if total > 0 && self.cfg.cache.disk_high_watermark > 0.0 => {
                    // Watermark config is a fraction; byte counts past f64's
                    // 2^53 (8 PiB) are outside any filesystem this targets.
                    #[allow(clippy::cast_precision_loss, reason = "disk-watermark arithmetic is f64 by design (config fraction vs. byte counts)")]
                    let frac = used as f64 / total as f64;
                    metrics::gauge!("walgit_cache_disk_used_fraction").set(frac);
                    if frac <= self.cfg.cache.disk_high_watermark {
                        return Ok(EvictReport::default());
                    }
                    // `low` is a fraction of `total`, so it is always in
                    // [0, total]: no truncation or sign loss is possible.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss, reason = "low = (watermark − 10%) × total, in [0, total] by construction")]
                    let low = ((self.cfg.cache.disk_high_watermark - 0.10).max(0.0) * total as f64)
                        as u64;
                    // Other data on the filesystem counts against us: target =
                    // cache bytes − (used − low).
                    let over = used.saturating_sub(low);
                    let cache_bytes: u64 = self
                        .repos
                        .iter()
                        .map(|e| dir_size(e.value().local.path()))
                        .sum();
                    tracing::warn!(
                        used,
                        total,
                        over,
                        "cache disk above high watermark: evicting idle repositories"
                    );
                    cache_bytes.saturating_sub(over)
                }
                _ => return Ok(EvictReport::default()),
            }
        } else {
            self.cfg.cache.max_bytes.as_u64()
        };
        let now = std::time::Instant::now();

        let mut evicted = 0;
        let mut candidates: Vec<(RepoId, Instant, Arc<RepoHandle>)> = Vec::new();

        // Collect idle repos. In-use checks happen again while evicting: a
        // request may acquire a ReadGuard after this snapshot.
        for entry in &self.repos {
            let handle = entry.value();
            let last_access = handle.last_access();
            if now.duration_since(last_access) > evict_after {
                candidates.push((entry.key().clone(), last_access, handle.clone()));
            }
        }

        // Sort by oldest access first.
        candidates.sort_by_key(|(_, t, _)| *t);

        // Calculate total cache size and evict as needed.
        let mut total_bytes: u64 = candidates
            .iter()
            .map(|(_, _, h)| dir_size(h.local.path()))
            .sum();

        for (id, _, handle) in &candidates {
            if total_bytes <= max_bytes {
                break;
            }
            // Hold both gates through removal. `sync_mutex` excludes a sync
            // beginning between the idle snapshot and removal; `rw.write`
            // excludes leaked/long-lived request ReadGuards. The old code only
            // probed sync_mutex and immediately dropped it, so it could delete
            // packs underneath an active reader.
            let Ok(_sync) = handle.sync_mutex.try_lock() else {
                continue;
            };
            let Ok(_write) = handle.rw.try_write() else {
                continue;
            };
            let path = handle.local.path().to_path_buf();
            let bytes = dir_size(&path);
            self.repos.remove(id);
            let _ = std::fs::remove_dir_all(&path);
            total_bytes = total_bytes.saturating_sub(bytes);
            evicted += 1;
        }

        Ok(EvictReport {
            evicted,
            remaining_bytes: total_bytes,
        })
    }
}

fn parse_object_format(s: &str) -> ObjectFormat {
    match s {
        "sha256" => ObjectFormat::Sha256,
        _ => ObjectFormat::Sha1,
    }
}

/// Bytes a repo directory occupies: symlinks (mount-linked base packs) count
/// as their link size, hard links (the pack index shared between the Serve
/// level and the remote reader) once — on Unix, where `(device, inode)` makes
/// them identifiable. The Windows twin overcounts such shares slightly.
#[cfg(unix)]
fn dir_size(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fn walk(p: &std::path::Path, seen: &mut std::collections::HashSet<(u64, u64)>) -> u64 {
        let mut total = 0;
        if let Ok(entries) = std::fs::read_dir(p) {
            for entry in entries.flatten() {
                let path = entry.path();
                // `DirEntry::metadata` does not follow symlinks: a linked base
                // pack is a few bytes here, as it should be.
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    total += walk(&path, seen);
                } else if meta.nlink() <= 1 || seen.insert((meta.dev(), meta.ino())) {
                    total += meta.len();
                }
            }
        }
        total
    }
    walk(path, &mut std::collections::HashSet::new())
}

/// The Windows `dir_size`: the same walk, minus hard-link dedup. NTFS file identity
/// (`file_index`) sits behind the unstable `windows_by_handle`; rather than poke
/// handles per entry, shared side-files (a pack index reused by the remote reader)
/// are counted once per link. Eviction budgets only ever read high — conservative,
/// never an overrun.
#[cfg(windows)]
fn dir_size(path: &std::path::Path) -> u64 {
    fn walk(p: &std::path::Path) -> u64 {
        let mut total = 0;
        if let Ok(entries) = std::fs::read_dir(p) {
            for entry in entries.flatten() {
                let path = entry.path();
                // As on Unix: `DirEntry::metadata` does not follow symlinks, so a
                // linked base pack contributes its few link bytes here.
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    total += walk(&path);
                } else {
                    total += meta.len();
                }
            }
        }
        total
    }
    walk(path)
}

/// `(used, total)` bytes of the filesystem holding `path`; see [`crate::platform`] for
/// what "used" means per OS and why it is the caller's free bytes either way.
fn disk_usage(path: &std::path::Path) -> Option<(u64, u64)> {
    crate::platform::capacity(path).map(|(free, total)| (total.saturating_sub(free), total))
}

/// Is this error the "a writer recreated something / a handle is still open"
/// shape that a bounded teardown retry is for?
///
/// `ErrorKind` carries the portable semantics; the raw codes cover platforms
/// whose `ErrorKind` mapping predates `DirectoryNotEmpty` and the Windows
/// sharing/access violations that have no portable kind. Windows bodies:
/// git's `READ_ONLY` pack attribute was cleared by the caller, but a still-open
/// handle (this process's pack-index mmap, a real-time scanner) reports
/// `SHARING_VIOLATION` (32) / `ACCESS_DENIED` (5) for a moment, and a
/// concurrent writer (pack prefetch, commit-graph update, a git child just
/// flushing) can recreate a file inside the tree mid-teardown, which the final
/// `RemoveDirectory` reports as `DIR_NOT_EMPTY` (145). `33`/`1224` are the
/// lock-violation / user-mapped-file shapes the same handle races produce.
fn is_retryable_teardown_error(e: &std::io::Error) -> bool {
    if matches!(
        e.kind(),
        std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::AlreadyExists
    ) {
        return true;
    }
    if cfg!(windows) {
        e.raw_os_error()
            .is_some_and(|c| matches!(c, 5 | 32 | 33 | 145 | 1224))
    } else {
        // EEXIST = 17; ENOTEMPTY = 39 on Linux, 66 on macOS.
        e.raw_os_error().is_some_and(|c| matches!(c, 17 | 39 | 66))
    }
}

/// How many extra attempts a local-cache teardown gets after the first one.
const TEARDOWN_RETRIES: u64 = 3;

/// Run `remove`, retrying the transient teardown shapes with a bounded
/// 100/200/300 ms backoff. Extracted so the policy is testable without racing a
/// real writer (issue #220: the retry set was Windows-only, so a loaded
/// macOS/Linux instance answered `DELETE /<owner>/<repo>` with
/// `Directory not empty (os error 66)`).
async fn retry_teardown<F, Fut>(mut remove: F) -> std::io::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<()>>,
{
    let mut attempt: u64 = 0;
    loop {
        match remove().await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if is_retryable_teardown_error(&e) && attempt < TEARDOWN_RETRIES => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(100 * attempt)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// [`retry_teardown`] around the real directory removal: clears git's `READ_ONLY`
/// attribute again on Windows in case the previous pass raced a just-written
/// file.
async fn remove_dir_all_with_retry(local_dir: &std::path::Path) -> Result<(), WalError> {
    let dir = local_dir.to_path_buf();
    retry_teardown(move || {
        let dir = dir.clone();
        async move {
            #[cfg(windows)]
            crate::platform::clear_readonly_recursive(&dir);
            tokio::fs::remove_dir_all(dir).await
        }
    })
    .await
    .map_err(WalError::Io)
}

#[cfg(test)]
mod teardown_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn not_empty() -> std::io::Error {
        #[cfg(windows)]
        let raw = 145;
        #[cfg(target_os = "linux")]
        let raw = 39;
        // macOS and the BSDs use 66 for ENOTEMPTY.
        #[cfg(not(any(windows, target_os = "linux")))]
        let raw = 66;
        std::io::Error::from_raw_os_error(raw)
    }

    /// The unix half of #49/#220: ENOTEMPTY (macOS 66 / Linux 39) must get the
    /// same bounded retry Windows' 145 gets, and give up after it.
    #[test]
    fn retry_predicate_covers_both_platform_error_sets() {
        assert!(is_retryable_teardown_error(&not_empty()));
        assert!(is_retryable_teardown_error(&std::io::Error::from(
            std::io::ErrorKind::AlreadyExists
        )));
        assert!(!is_retryable_teardown_error(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }

    #[tokio::test]
    async fn retry_teardown_succeeds_after_a_transient_writer_race() {
        let calls = AtomicU64::new(0);
        let result = retry_teardown(|| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move { if n < 2 { Err(not_empty()) } else { Ok(()) } }
        })
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "first failure + 2 retries");
    }

    #[tokio::test]
    async fn retry_teardown_gives_up_after_the_bound() {
        let calls = AtomicU64::new(0);
        let result = retry_teardown(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move { Err(not_empty()) }
        })
        .await;
        assert!(result.is_err(), "a permanently busy tree must surface");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            TEARDOWN_RETRIES + 1,
            "one first attempt plus the bounded retries"
        );
    }

    #[tokio::test]
    async fn retry_teardown_treats_a_vanished_tree_as_success() {
        let result =
            retry_teardown(|| async { Err(std::io::Error::from(std::io::ErrorKind::NotFound)) })
                .await;
        assert!(result.is_ok(), "{result:?}");
    }
}
