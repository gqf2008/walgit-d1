//! Resumable base rebuild (`docs/BUNDLE_URI_DESIGN.md` §5a): `git repack -adb` of a big
//! repository is 16–30 min of one core plus a 10-min history pack; a deploy mid-way (D31
//! interrupts units at once) used to throw that away and — worse — the repack rewrote the
//! *serving copy's* `objects/pack` in place.
//!
//! Now the rebuild works in a **scratch copy** under `<cache.dir>/_rebuild/<owner>/<repo>.git`
//! (on the SSD host `/data` is a bind mount that outlives the container; the copy is
//! `copy_file_range`, which XFS turns into a reflink — seconds, no bytes duplicated until
//! written), records a **phase marker** next to it after each completed phase, and the next
//! unit **resumes iff the manifest's `head_seq` is unchanged** since the rebuild started
//! (otherwise the scratch is discarded: a push landed, the pack would miss its objects).
//! Finished packs are hard-linked into the serving copy only at publish time — the serving
//! copy is never rewritten — and publish is idempotent (an already-live checksum is not
//! re-published; the supersede set is the packs that existed when the rebuild started).
//! D31 is unchanged: SIGTERM kills `git repack`; git writes packs by temp name + rename, so a
//! half-written pack never looks final; the marker's phase is whatever completed.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, bail};
use walgit_config::Config;
use walgit_git::{LocalRepo, RepackMode, RepackOptions};
use walgit_wal::RepoHandle;

use crate::ops::Log;

/// Phases in order; the marker names the last one that completed.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Copied,
    Repacked,
    HistoryPack,
    CommitGraph,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Marker {
    /// `manifest.head_seq` when the scratch copy was taken: resume only while it is unchanged.
    pub started_head_seq: u64,
    pub phase: Phase,
    /// The base pack(s) the repack produced (hex; one unless git split the pack).
    #[serde(default)]
    pub new_packs: Vec<String>,
    /// The D18 history pack derived from the first base (hex).
    #[serde(default)]
    pub history: Option<String>,
    /// The pack carrying the commit-graph layer (hex).
    #[serde(default)]
    pub commit_graph: Option<String>,
}

/// Test hook (sim): abort the rebuild of `repo` right after `phase`'s marker was written — a
/// SIGTERM between phases. Per repository so parallel tests do not see each other's hook.
pub static TEST_ABORT_AFTER: parking_lot::Mutex<Option<(String, Phase)>> =
    parking_lot::Mutex::new(None);

fn abort_after(repo: &walgit_git::RepoId, phase: Phase) -> anyhow::Result<()> {
    let hook = TEST_ABORT_AFTER.lock().clone();
    if let Some((r, p)) = hook
        && r == repo.to_string()
        && p == phase
    {
        bail!("rebuild aborted by test hook after phase {phase:?}");
    }
    Ok(())
}

pub struct RebuildOutcome {
    /// Checksums published (base(s), then the history pack).
    pub packs: Vec<String>,
    pub superseded: usize,
    /// True when this unit continued an earlier, interrupted rebuild.
    pub resumed: bool,
}

fn scratch_root(cfg: &Config) -> PathBuf {
    cfg.cache.dir.join("_rebuild")
}

fn marker_path(cfg: &Config, id: &walgit_git::RepoId) -> PathBuf {
    scratch_root(cfg)
        .join(id.owner())
        .join(format!("{}.json", id.name()))
}

fn read_marker(path: &Path) -> Option<Marker> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_marker(path: &Path, m: &Marker) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("marker path {} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Recursive copy. `std::fs::copy` uses `copy_file_range` on Linux, which XFS/btrfs satisfy
/// with a reflink when source and destination share a filesystem (seconds for 40 GB, no
/// bytes duplicated until written) and which degrades to a plain copy elsewhere. Each
/// directory is snapshotted first and the snapshot copied after, so the concurrent-writer
/// rules live in [`copy_snapshot`] with [`snapshot_dir`] as their deterministic test seam.
/// `gone` collects the entries the tolerances dropped, so the caller can say so out loud
/// (issue #139: a tolerance that leaves no trace is how #138 stayed unattributed for a week).
fn copy_tree(src: &Path, dst: &Path, gone: &mut Vec<PathBuf>) -> std::io::Result<u64> {
    std::fs::create_dir_all(dst).map_err(|e| at(&e, dst))?;
    copy_snapshot(dst, &snapshot_dir(src)?, gone)
}

/// One directory as it looked at a point in time: `(name, path, kind)`, all captured
/// **before** any bytes move. The gap between taking this and running [`copy_snapshot`] is
/// where the serving copy's concurrent writers operate — `maintain_commit_graph`, spawned
/// after every publish (`walgit-wal/src/publish.rs:1092-1099`), installs a commit-graph base
/// by renaming the layer and chain in and then **deleting the superseded layers**
/// (`walgit-git/src/lib.rs`, `install_commit_graph_base`). Splitting enumeration from copying
/// exists so the regression tests can delete entries inside that gap deterministically
/// instead of gambling on a real race (issue #139).
type DirSnapshot = Vec<(std::ffi::OsString, PathBuf, std::fs::FileType)>;

fn snapshot_dir(dir: &Path) -> std::io::Result<DirSnapshot> {
    let mut out = DirSnapshot::new();
    for ent in std::fs::read_dir(dir).map_err(|e| at(&e, dir))? {
        let ent = ent.map_err(|e| at(&e, dir))?;
        let path = ent.path();
        let kind = ent.file_type().map_err(|e| at(&e, &path))?;
        out.push((ent.file_name(), path, kind));
    }
    Ok(out)
}

/// Names git and walgit write a file under while it is still in flight, and never after
/// publishing it. Two conventions exist and both are covered:
///
/// - **suffix** — `*.lock` (git's own staging: `commit-graph-chain.lock`) and `*.tmp`
///   (walgit's: `install_commit_graph_base` stages `graph-<hash>.graph.tmp` and
///   `commit-graph-chain.tmp` before renaming);
/// - **prefix** — `tmp_*` from git's `mkstemps`: `tmp_graph_*` for a split commit-graph
///   layer (measured on git 2.50.1 — it does *not* use a `.tmp` suffix there), and
///   `tmp_pack_*` / `tmp_idx_*` / `tmp_obj_*` in `objects/pack/`.
///
/// Committed state always arrives by rename from one of these, so a transient name is
/// never part of the serving truth — skip it, gone by copy time or not.
fn is_transient_git_name(name: &std::ffi::OsStr) -> bool {
    let lossy = name.to_string_lossy();
    lossy.ends_with(".lock")
        || lossy.ends_with(".tmp")
        || lossy.starts_with("tmp_graph_")
        || lossy.starts_with("tmp_pack_")
        || lossy.starts_with("tmp_idx_")
        || lossy.starts_with("tmp_obj_")
}

/// Copy one [`snapshot_dir`] listing into `dst` (which must exist). The rules for entries the
/// concurrent writers can take away mid-flight:
///
/// - **Transient names are skipped** ([`is_transient_git_name`]: `.lock`/`.tmp` suffixes and
///   git's `tmp_graph_*`/`tmp_pack_*`/`tmp_idx_*`/`tmp_obj_*` prefixes): committed state always
///   arrives by rename, so a file still under a transient name is not part of the serving truth.
///   The `.lock` half of this rule came from CI 2026-09-01
///   (`fetch_from_front_that_serves_the_base_remotely`: the scratch inherited a mid-flight
///   `commit-graph-chain.lock` and the rebuild's own `commit-graph write` died on "Unable to
///   create ...lock: File exists"); that fix stopped at the lock and left the source-side
///   rename/delete race itself open — issue #138's verdict, closed by issue #139, which
///   extends the skip to `.tmp` and git's `tmp_*` staging names and adds the tolerances below.
/// - **`NotFound` inside the commit-graph state is tolerated** ([`is_graph_state`], i.e. the
///   `objects/info/commit-graphs/` directory and the monolithic `objects/info/commit-graph`):
///   those deletions happen *after* the new chain's rename (`install_commit_graph_base`'s
///   order), so dropping an entry the serving copy has just abandoned cannot misdirect the
///   copy. Worst case the scratch carries a chain that dangles (names a layer the copy
///   skipped, or one it never enumerated): the only readers before `Phase::CommitGraph`
///   rewrite it are this file's `LocalRepo::repack` and `write_history_pack` git subprocesses
///   — measured on git 2.50.1 they merely warn "unable to find all commit-graph files" and
///   exit 0, and `git commit-graph write --reachable --split=replace` (the `CommitGraph` phase,
///   `walgit-git/src/lib.rs` `write_pack_commit_graph`) replaces the whole chain and
///   regenerates the layer, also exit 0. So a stale chain cannot become a second kind of red;
///   `install_commit_graph_base` itself never runs against the scratch (its only non-test
///   caller is `maintain_commit_graph` on the serving handle, `walgit-wal/src/sync.rs:831`).
/// - **Everything else is fatal**: a vanishing entry elsewhere — above all `objects/pack/` —
///   is real corruption or a real bug and must fail loudly, naming the source path
///   ([`at`]) so the anyhow chain prints the dead file at the leaf.
fn copy_snapshot(
    dst: &Path,
    entries: &DirSnapshot,
    gone: &mut Vec<PathBuf>,
) -> std::io::Result<u64> {
    let mut bytes = 0u64;
    for (name, from, kind) in entries {
        if is_transient_git_name(name) {
            continue;
        }
        let to = dst.join(name);
        if kind.is_dir() {
            bytes += copy_tree(from, &to, gone)?;
        } else if kind.is_file() {
            match std::fs::copy(from, &to) {
                Ok(n) => bytes += n,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && is_graph_state(from) => {
                    gone.push(from.clone());
                }
                Err(e) => return Err(at(&e, from)),
            }
        } else if kind.is_symlink() {
            // A mount-linked base (`pack-<sha>.pack` → store mount) is never rebuilt here:
            // the rebuild needs real files (compact_repo syncs Full first).
            let target = std::fs::read_link(from).map_err(|e| at(&e, from))?;
            // The failure here is creating `to` (privilege, existing entry), not reading
            // `from` — name the path that actually broke.
            walgit_wal::platform::symlink(&target, &to).map_err(|e| at(&e, &to))?;
        }
    }
    Ok(bytes)
}

/// The graph-state entries whose concurrent deletion [`copy_snapshot`] tolerates: everything
/// in git's split commit-graph directory (layers and the chain file) plus the monolithic
/// `objects/info/commit-graph` that `install_commit_graph_base` removes right after naming
/// the chain. Nothing under `objects/pack/` qualifies — there a `NotFound` stays fatal.
fn is_graph_state(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    parent.ends_with("objects/info/commit-graphs")
        || (parent.ends_with("objects/info")
            && path.file_name().is_some_and(|n| n == "commit-graph"))
}

/// A plain `io::Error`'s Display hides the path it came from (only `get_ref()` carries it) —
/// which is why the #138 CI red ended at a bare "The system cannot find the file specified.
/// (os error 2)". Keep the kind and put the source path in the message.
fn at(e: &std::io::Error, path: &Path) -> std::io::Error {
    std::io::Error::new(e.kind(), format!("{e}: {}", path.display()))
}

/// Bytes available to this (unprivileged) process on `path`'s filesystem; see
/// [`walgit_wal::platform`] for what that means on each OS.
fn disk_avail(path: &Path) -> Option<u64> {
    walgit_wal::platform::capacity(path).map(|(free, _total)| free)
}

/// Hard-link (or copy) every side-file of `pack` from `from` into `into`'s pack dir; existing
/// files are left alone (a resumed install).
fn install_pack(
    from: &LocalRepo,
    into: &LocalRepo,
    pack: &gix_hash::ObjectId,
) -> anyhow::Result<()> {
    let src = from.pack_path(pack);
    let dst = into.pack_path(pack);
    let pack_dir = dst
        .parent()
        .ok_or_else(|| anyhow::anyhow!("pack path {} has no parent directory", dst.display()))?;
    std::fs::create_dir_all(pack_dir)?;
    for ext in ["pack", "idx", "rev", "bitmap", "commit-graph", "history"] {
        let s = src.with_extension(ext);
        if !s.exists() {
            continue;
        }
        let d = dst.with_extension(ext);
        if d.exists() {
            continue;
        }
        if std::fs::hard_link(&s, &d).is_err() {
            // Never copy straight into the committed name — a concurrent
            // reader could adopt the truncated file (issue #144; on ExFAT
            // hard links do not exist, so this fallback is the *only* path).
            walgit_git::copy_into_place(&s, &d)
                .with_context(|| format!("installing {}", d.display()))?;
        }
    }
    Ok(())
}

/// Rebuild the tier-2 base of `handle`'s repository in a scratch copy, resuming an interrupted
/// rebuild when the WAL head has not moved, then install + publish: the new base (superseding
/// every live pack that existed when the rebuild started), the D18 history pack, the
/// commit-graph layer. The caller holds the compaction lease and has synced Full.
pub async fn rebuild_base(
    handle: &RepoHandle,
    cfg: &Config,
    log: Log<'_>,
) -> anyhow::Result<RebuildOutcome> {
    let id = handle.id().clone();
    let root = scratch_root(cfg);
    let scratch_dir = id.local_dir(&root);
    let marker_path = marker_path(cfg, &id);
    let manifest = handle.manifest();
    let head = manifest.head_seq;

    // 1. Resume or start over.
    let mut marker = match read_marker(&marker_path) {
        Some(m) if m.started_head_seq == head && scratch_dir.join("objects").is_dir() => {
            log(format!(
                "resuming base rebuild started at head_seq {head}: phase {:?} done",
                m.phase
            ));
            m
        }
        Some(m) => {
            log(format!(
                "discarding interrupted base rebuild (started at head_seq {}, head is now {head}, phase {:?}): starting over",
                m.started_head_seq, m.phase
            ));
            let _ = std::fs::remove_dir_all(&scratch_dir);
            let _ = std::fs::remove_file(&marker_path);
            start_scratch(handle, cfg, &manifest, &scratch_dir, &marker_path, log)?
        }
        None => {
            let _ = std::fs::remove_dir_all(&scratch_dir);
            start_scratch(handle, cfg, &manifest, &scratch_dir, &marker_path, log)?
        }
    };
    let resumed = marker.phase > Phase::Copied;
    let scratch =
        LocalRepo::open(&root, &id)?.context("scratch copy did not open as a repository")?;

    // 2. Repack (full, bitmap) in the scratch copy.
    if marker.phase < Phase::Repacked {
        let t = Instant::now();
        let result = scratch
            .repack(RepackOptions {
                mode: RepackMode::Full,
                write_bitmap: true,
                write_midx: false,
                keep: Vec::new(),
            })
            .await?;
        let mut new: Vec<String> = result
            .new_packs
            .iter()
            .map(|p| p.checksum.to_hex().to_string())
            .collect();
        if new.is_empty() {
            // Already a single pack: `repack -adb` only added the bitmap. The base is that pack.
            let mut packs = scratch.packs()?;
            packs.sort_by_key(|p| std::cmp::Reverse(p.pack_size));
            if let Some(p) = packs
                .iter()
                .find(|p| p.has_bitmap && p.history_of.is_none())
            {
                new.push(p.checksum.to_hex().to_string());
            }
        }
        log(format!(
            "repack done in {:.1}s: {} new pack(s), {} removed in the scratch copy",
            t.elapsed().as_secs_f64(),
            result.new_packs.len(),
            result.removed.len()
        ));
        if new.is_empty() {
            bail!("full repack produced no base pack");
        }
        marker.new_packs = new;
        marker.phase = Phase::Repacked;
        write_marker(&marker_path, &marker)?;
        abort_after(&id, Phase::Repacked)?;
    }
    let bases: Vec<gix_hash::ObjectId> = marker
        .new_packs
        .iter()
        .filter_map(|h| gix_hash::ObjectId::from_hex(h.as_bytes()).ok())
        .collect();

    // 3. History pack (D18) of the first base.
    if marker.phase < Phase::HistoryPack {
        if cfg.git.history_pack
            && let Some(base) = bases.first()
        {
            let t = Instant::now();
            match scratch.write_history_pack(base).await {
                Ok(hp) => {
                    log(format!(
                        "history pack {} for base {base}: {} bytes, {} objects in {:.1}s",
                        hp.checksum,
                        hp.pack_size,
                        hp.object_count,
                        t.elapsed().as_secs_f64()
                    ));
                    marker.history = Some(hp.checksum.to_hex().to_string());
                }
                Err(e) => log(format!("history pack failed (continuing without): {e}")),
            }
        }
        marker.phase = Phase::HistoryPack;
        write_marker(&marker_path, &marker)?;
        abort_after(&id, Phase::HistoryPack)?;
    }

    // 4. One commit-graph layer on the biggest base.
    if marker.phase < Phase::CommitGraph {
        if cfg.git.commit_graph {
            let packs = scratch.packs()?;
            let biggest = bases
                .iter()
                .filter_map(|b| packs.iter().find(|p| &p.checksum == b))
                .max_by_key(|p| p.pack_size)
                .map(|p| p.checksum);
            if let Some(b) = biggest {
                let t = Instant::now();
                match scratch
                    .write_pack_commit_graph(&b, cfg.git.commit_graph_changed_paths)
                    .await
                {
                    Ok(bytes) => {
                        log(format!(
                            "commit-graph layer on pack {b}: {bytes} bytes in {:.1}s",
                            t.elapsed().as_secs_f64()
                        ));
                        marker.commit_graph = Some(b.to_hex().to_string());
                    }
                    Err(e) => log(format!(
                        "commit-graph write failed (continuing without): {e}"
                    )),
                }
            }
        }
        marker.phase = Phase::CommitGraph;
        write_marker(&marker_path, &marker)?;
        abort_after(&id, Phase::CommitGraph)?;
    }

    // 5. Install into the serving copy (links, never a rewrite) and publish. The supersede set is
    //    every pack that was live when the rebuild started (seq ≤ started_head_seq) and is not one
    //    of the new ones; pushes that landed since keep their packs.
    let mut to_install = bases.clone();
    if let Some(h) = marker
        .history
        .as_deref()
        .and_then(|h| gix_hash::ObjectId::from_hex(h.as_bytes()).ok())
    {
        to_install.push(h);
    }
    // Installed packs are visible under final names before publish_compact
    // CASes them; hold the prune lock across install → publish.
    let _prune_guard = handle.prune_guard().await;
    for p in &to_install {
        install_pack(&scratch, handle.local(), p)?;
    }
    handle.local().refresh_async().await?;
    let local_packs = handle.local().packs()?;
    let manifest = handle.manifest();
    let new_set: std::collections::HashSet<String> =
        to_install.iter().map(|c| c.to_hex().to_string()).collect();
    let supersedes: Vec<gix_hash::ObjectId> = manifest
        .packs
        .iter()
        .filter(|p| p.seq <= marker.started_head_seq && !new_set.contains(&p.checksum))
        .filter_map(|p| gix_hash::ObjectId::from_hex(p.checksum.as_bytes()).ok())
        .collect();
    let superseded = supersedes.len();
    let mut supersedes_left = Some(supersedes);
    let mut published = Vec::new();
    // Installed packs are visible before publish_compact CASes them into the
    // manifest; keep them protected from a concurrent prune.
    let _staged_packs: Vec<_> = to_install
        .iter()
        .filter_map(|c| handle.stage_pack_guard(&c.to_hex().to_string()))
        .collect();
    for c in &to_install {
        let hex = c.to_hex().to_string();
        let Some(info) = local_packs.iter().find(|p| &p.checksum == c).cloned() else {
            bail!("pack {hex} not visible in the serving copy after install");
        };
        let already = manifest.packs.iter().find(|p| p.checksum == hex);
        // Already live at the right tier and not the carrier of the supersede set: nothing to publish.
        if let Some(p) = already
            && p.tier == 2
            && (p.has_bitmap || info.history_of.is_some())
            && supersedes_left.as_ref().is_none_or(std::vec::Vec::is_empty)
        {
            log(format!(
                "pack {hex} is already live as tier 2: not re-published"
            ));
            published.push(hex);
            continue;
        }
        let sup = supersedes_left.take().unwrap_or_default();
        let seq = handle.publish_compact(info, sup, 2).await?;
        // #195: this base's refs must stay replayable *after* later folds push the
        // live checkpoint past `seq`. Write the exact witness checkpoint here —
        // `refs_at_seq(seq)` then finds it (the reader lists retained witnesses
        // when the live checkpoint is newer than the cut) and bundle compose works
        // however far the WAL folds afterwards.
        let packs = handle.manifest().packs.clone();
        match walgit_wal::write_witness_checkpoint(&handle, seq, packs).await {
            Ok(cp) => log(format!(
                "witness checkpoint written at seq {} for base {hex}",
                cp.seq
            )),
            Err(e) => log(format!(
                "witness checkpoint at seq {seq} failed ({e}); compose may need a rebuild after the next fold"
            )),
        }
        log(format!(
            "published pack {hex} as seq {seq}{}",
            if already.is_some() {
                " (promotion)"
            } else {
                ""
            }
        ));
        published.push(hex);
    }

    // 6. Done: the scratch and its marker go.
    let _ = std::fs::remove_dir_all(&scratch_dir);
    let _ = std::fs::remove_file(&marker_path);
    Ok(RebuildOutcome {
        packs: published,
        superseded,
        resumed,
    })
}

fn start_scratch(
    handle: &RepoHandle,
    cfg: &Config,
    manifest: &walgit_proto::v1::Manifest,
    scratch_dir: &Path,
    marker_path: &Path,
    log: Log<'_>,
) -> anyhow::Result<Marker> {
    // Headroom: the repack writes a new pack about the size of the live set (a reflink copy
    // costs nothing until then). Fail loudly rather than fill the disk the serving copy lives on.
    let need: u64 = manifest
        .packs
        .iter()
        .map(|p| p.pack_size + p.idx_size)
        .sum();
    if let Some(avail) = disk_avail(&cfg.cache.dir)
        && avail < need
    {
        bail!(
            "not enough disk for a base rebuild under {}: {} available, the pack set is {} (the repack writes a pack that size)",
            cfg.cache.dir.display(),
            walgit_wal::remote::human_bytes(avail),
            walgit_wal::remote::human_bytes(need)
        );
    }
    let t = Instant::now();
    let mut gone: Vec<PathBuf> = Vec::new();
    let bytes = copy_tree(handle.local().path(), scratch_dir, &mut gone)
        .context("copying the serving copy to the scratch dir")?;
    log(format!(
        "scratch copy of the serving copy at {} ({} in {:.1}s; reflinked where the filesystem allows)",
        scratch_dir.display(),
        walgit_wal::remote::human_bytes(bytes),
        t.elapsed().as_secs_f64()
    ));
    if !gone.is_empty() {
        // Benign by construction (the writer deletes these only after renaming the new chain
        // in), but say it: the alternative is a future reader staring at a scratch copy that
        // quietly lost entries, with nothing to correlate. Capped — the set is bounded by the
        // commit-graph layer count in practice, this is belt space.
        let shown: Vec<String> = gone
            .iter()
            .take(5)
            .map(|p| {
                p.file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            })
            .collect();
        log(format!(
            "tolerated {} commit-graph entr{} deleted during the copy: {}",
            gone.len(),
            if gone.len() == 1 { "y" } else { "ies" },
            shown.join(", ")
        ));
    }
    let m = Marker {
        started_head_seq: manifest.head_seq,
        phase: Phase::Copied,
        new_packs: Vec::new(),
        history: None,
        commit_graph: None,
    };
    write_marker(marker_path, &m)?;
    abort_after(handle.id(), Phase::Copied)?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::{copy_snapshot, copy_tree, is_transient_git_name, snapshot_dir};

    /// The scratch copy must never inherit git's transient `.lock` files: a
    /// concurrent `git commit-graph write` on the serving copy leaves
    /// `commit-graph-chain.lock` mid-flight, and a copy would break the
    /// rebuild's own write ("Unable to create ...lock: File exists", CI
    /// 2026-09-01). Ordinary files must still be copied.
    #[test]
    fn copy_tree_skips_lock_files() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("objects/info/commit-graphs")).unwrap();
        std::fs::write(
            src.path()
                .join("objects/info/commit-graphs/commit-graph-chain.lock"),
            "stale",
        )
        .unwrap();
        std::fs::write(src.path().join("objects/info/commit-graph-chain"), "hash\n").unwrap();
        std::fs::write(src.path().join("packed-refs"), "ref: x\n").unwrap();

        copy_tree(src.path(), dst.path(), &mut Vec::new()).unwrap();

        assert!(
            !dst.path()
                .join("objects/info/commit-graphs/commit-graph-chain.lock")
                .exists(),
            "the stale lock must not be copied into the scratch"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("objects/info/commit-graph-chain")).unwrap(),
            "hash\n"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("packed-refs")).unwrap(),
            "ref: x\n"
        );
    }

    /// Snapshot a `objects/info/commit-graphs` directory, delete the layer the way a
    /// concurrent `install_commit_graph_base` does after renaming the new chain, then run the
    /// copy step. Issue #138's CI red (`base_rebuild_resumes_after_a_kill_between_any_two_phases`,
    /// os error 2 in `copying the serving copy to the scratch dir`) is exactly this window;
    /// the scratch must survive it, and the deleted layer must not be invented.
    #[test]
    fn copy_snapshot_survives_a_commit_graph_layer_deleted_after_the_snapshot()
    -> std::io::Result<()> {
        let src = tempfile::tempdir()?;
        let dst = tempfile::tempdir()?;
        let graphs = src.path().join("objects/info/commit-graphs");
        std::fs::create_dir_all(&graphs)?;
        let layer = graphs.join("graph-1111111111222222222233333333334444444444.graph");
        std::fs::write(&layer, "layer bytes")?;
        let chain = "1111111111222222222233333333334444444444";
        std::fs::write(graphs.join("commit-graph-chain"), format!("{chain}\n"))?;

        let entries = snapshot_dir(&graphs)?;
        assert_eq!(entries.len(), 2, "layer + chain enumerated");
        std::fs::remove_file(&layer)?;

        let into = dst.path().join("objects/info/commit-graphs");
        std::fs::create_dir_all(&into)?;
        let mut gone: Vec<std::path::PathBuf> = Vec::new();
        copy_snapshot(&into, &entries, &mut gone)?;

        // Tolerating is not the same as being silent: the caller narrates what it dropped, so
        // a future scratch that "lost a layer" has a line to correlate with (issue #138's
        // week of no-attribution was bought with exactly this missing trace).
        assert_eq!(
            gone.iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["graph-1111111111222222222233333333334444444444.graph".to_string()],
            "the tolerated deletion must be reported to the caller"
        );
        assert_eq!(
            std::fs::read_to_string(into.join("commit-graph-chain"))?,
            format!("{chain}\n"),
            "the chain that survived must arrive"
        );
        assert!(
            !into
                .join("graph-1111111111222222222233333333334444444444.graph")
                .exists(),
            "the deleted layer must not be half-created"
        );
        Ok(())
    }

    /// The tolerance is scoped to the commit-graph state: a `objects/pack/` entry that
    /// vanished between snapshot and copy is real corruption or a real bug and must still
    /// fail — and the error must name the dead path (issue #139 (c): the #138 red stopped at
    /// a bare "The system cannot find the file specified. (os error 2)" because an
    /// `io::Error`'s Display hides its path).
    #[test]
    fn copy_snapshot_fails_and_names_a_pack_file_deleted_after_the_snapshot() -> std::io::Result<()>
    {
        let src = tempfile::tempdir()?;
        let dst = tempfile::tempdir()?;
        let pack_dir = src.path().join("objects/pack");
        std::fs::create_dir_all(&pack_dir)?;
        let pack = pack_dir.join("pack-2222222222333333333344444444445555555555.pack");
        std::fs::write(&pack, "pack bytes")?;
        std::fs::write(
            pack_dir.join("pack-2222222222333333333344444444445555555555.idx"),
            "idx bytes",
        )?;

        let entries = snapshot_dir(&pack_dir)?;
        std::fs::remove_file(&pack)?;

        let into = dst.path().join("objects/pack");
        std::fs::create_dir_all(&into)?;
        let err = copy_snapshot(&into, &entries, &mut Vec::new())
            .expect_err("a vanishing pack must not be tolerated");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            err.to_string()
                .contains("pack-2222222222333333333344444444445555555555.pack"),
            "the error must name the dead file, got: {err}"
        );
        Ok(())
    }

    /// Both transient name families must stay out of the scratch copy: the `.lock` of the
    /// 2026-09-01 incident and the `.tmp` rename-targets `install_commit_graph_base` writes
    /// (layer `.tmp`, chain `.tmp`).
    #[test]
    fn copy_snapshot_skips_transient_names() -> std::io::Result<()> {
        let src = tempfile::tempdir()?;
        let dst = tempfile::tempdir()?;
        let graphs = src.path().join("objects/info/commit-graphs");
        std::fs::create_dir_all(&graphs)?;
        std::fs::write(
            graphs.join("graph-3333333333444444444455555555556666666666.graph.tmp"),
            "mid-write",
        )?;
        std::fs::write(graphs.join("commit-graph-chain.lock"), "stale lock")?;
        // git 2.50.1 stages a split layer under this mkstemps *prefix*, not a `.tmp`
        // suffix — the name the suffix rule alone would miss (issue #143's review).
        std::fs::write(graphs.join("tmp_graph_9k2mQx"), "git's own mid-write layer")?;
        std::fs::write(
            graphs.join("commit-graph-chain"),
            "3333333333444444444455555555556666666666\n",
        )?;

        let entries = snapshot_dir(&graphs)?;
        assert!(
            entries.iter().any(|(n, _, _)| is_transient_git_name(n)),
            "the snapshot must contain the transient names for this test to mean anything"
        );
        let into = dst.path().join("objects/info/commit-graphs");
        std::fs::create_dir_all(&into)?;
        copy_snapshot(&into, &entries, &mut Vec::new())?;

        let names: Vec<String> = std::fs::read_dir(&into)?
            .map(|ent| Ok(ent?.file_name().to_string_lossy().into_owned()))
            .collect::<std::io::Result<Vec<_>>>()?;
        assert!(
            names.iter().any(|n| n == "commit-graph-chain"),
            "the committed chain must arrive: {names:?}"
        );
        assert!(
            names
                .iter()
                .all(|n| !is_transient_git_name(std::ffi::OsStr::new(n))),
            "transient names leaked into the scratch copy: {names:?}"
        );
        // Literal, not predicate-based: asserting with `is_transient_git_name` again would
        // pass on a predicate that simply stopped recognizing git's staging names.
        assert!(
            !names.iter().any(|n| n.starts_with("tmp_")),
            "git's `tmp_*` staging names must never reach the scratch copy: {names:?}"
        );
        Ok(())
    }

    /// The skip rule has to know both staging conventions (suffix and `mkstemps` prefix) and
    /// must not swallow committed names — including the side-files a base publishes with its
    /// pack (`pack-<chk>.commit-graph` lives in `objects/pack/` and is committed state).
    #[test]
    fn transient_names_cover_both_conventions_and_no_committed_names() {
        for n in [
            "graph-1.graph.tmp",
            "commit-graph-chain.lock",
            "tmp_graph_9k2mQx",
            "tmp_pack_Ab12Cd",
            "tmp_idx_Ef34Gh",
            "tmp_obj_Ij56Kl",
        ] {
            assert!(
                is_transient_git_name(std::ffi::OsStr::new(n)),
                "{n} is in-flight state and must be skipped"
            );
        }
        for n in [
            "graph-1.graph",
            "commit-graph-chain",
            "pack-a553ef17.pack",
            "pack-a553ef17.idx",
            "pack-a553ef17.rev",
            "pack-a553ef17.bitmap",
            "pack-a553ef17.commit-graph",
            "http-backend",
        ] {
            assert!(
                !is_transient_git_name(std::ffi::OsStr::new(n)),
                "{n} is committed state and must be copied"
            );
        }
    }

    /// Scope of the tolerance, inside `objects/info/` itself: the monolithic `commit-graph`
    /// is graph state — `install_commit_graph_base` removes it right after the chain rename —
    /// so its disappearance is tolerated; a sibling file's is not, and must name its path.
    #[test]
    fn copy_snapshot_tolerates_a_monolithic_commit_graph_but_not_its_siblings()
    -> std::io::Result<()> {
        let src = tempfile::tempdir()?;
        let dst = tempfile::tempdir()?;
        let info = src.path().join("objects/info");
        std::fs::create_dir_all(&info)?;
        std::fs::write(info.join("commit-graph"), "monolithic layer")?;
        std::fs::write(info.join("http-backend"), "ref: x\n")?;

        // The monolithic graph gone after the snapshot: tolerated, the rest arrives.
        let entries = snapshot_dir(&info)?;
        std::fs::remove_file(info.join("commit-graph"))?;
        let into = dst.path().join("objects/info");
        std::fs::create_dir_all(&into)?;
        copy_snapshot(&into, &entries, &mut Vec::new())?;
        assert!(!into.join("commit-graph").exists());
        assert_eq!(
            std::fs::read_to_string(into.join("http-backend"))?,
            "ref: x\n"
        );

        // A sibling gone after the snapshot: fatal, with its path named.
        let entries = snapshot_dir(&info)?;
        std::fs::remove_file(info.join("http-backend"))?;
        let err = copy_snapshot(&into, &entries, &mut Vec::new())
            .expect_err("objects/info siblings are not graph state");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            err.to_string().contains("http-backend"),
            "the error must name the dead file, got: {err}"
        );
        Ok(())
    }
}
