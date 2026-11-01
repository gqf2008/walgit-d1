//! Folding the collab inbox into `refs/collab/meta/snapshot` — as the
//! **maintainer's** own housekeeping (D66), not an operator's chore.
//!
//! Why it belongs here: every collab lane budgets by **ref count**
//! (`COLLAB_REF_BUDGET`, D45's 20k aggregation budget), and a fold is a pure
//! function of the inbox — the same inbox folds to the same snapshot bytes. So
//! a repository whose inbox only ever grows stops aggregating, quietly, until
//! somebody remembers `walgit collab gc`. That is a trap, and a bucket that
//! self-heals is the house style (D22: every maintainer unit is a pure function
//! of state, bounded, and repeatable).
//!
//! Why it needs a key: the snapshot document is **signed** by the folder
//! (`walgit_wal::collab::sign_snapshot`), and a snapshot nobody can verify
//! strands every reader. D45 therefore handed the fold to an operator principal;
//! the maintainer stands in for one only when `maintenance.collab_fold_principal`
//! and `…_key` name a **registered** principal and its seed — otherwise the unit
//! is never planned (folding stays manual, and the CLI still works).
//!
//! What it does, in one atomic ref transaction: set `refs/collab/meta/snapshot`
//! to the new (or existing) snapshot blob, and delete exactly the inbox refs
//! whose entries that snapshot carries. A reader of snapshot ∪ inbox sees the
//! same ledger before and after — that equality is the acceptance test, and it
//! is what makes the prune safe with no retention window: nothing is dropped
//! that the snapshot does not already hold.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use ed25519_dalek::SigningKey;
use walgit_git::RepoId;
use walgit_wal::RepoHandle;
use walgit_wal::collab::{
    COLLAB_SNAPSHOT_MAX_BYTES, Entry, SnapshotRecord, build_snapshot, build_snapshot_truncated,
    parse_snapshot,
};

use crate::AppState;
use crate::ops::Log;
use walgit_wal::Reporter;

use crate::web::api::{self, Need};

/// The ref a fold moved the ledger into.
pub const SNAPSHOT_REF: &str = "refs/collab/meta/snapshot";
/// The append-only namespace a fold drains.
pub const INBOX_PREFIX: &str = "refs/collab/inbox/";
/// The registered-key namespace (`docs/D1_PROTOCOL.md` §4.3).
const PRINCIPALS_PREFIX: &str = "refs/collab/meta/principals/";

/// The identity a fold signs as: a registered principal and its seed.
pub struct Signer {
    pub principal: String,
    pub key: SigningKey,
}

impl Signer {
    /// Load the configured fold identity, or `None` when the host has none (the
    /// unit is then never planned — folding stays a manual `walgit collab gc`).
    pub fn from_config(cfg: &walgit_config::Config) -> anyhow::Result<Option<Signer>> {
        let Some(principal) = cfg.maintenance.collab_fold_principal.clone() else {
            return Ok(None);
        };
        let Some(path) = cfg.maintenance.collab_fold_key.clone() else {
            return Ok(None);
        };
        let key = read_seed(&path)
            .with_context(|| format!("maintenance.collab_fold_key {}", path.display()))?;
        Ok(Some(Signer { principal, key }))
    }

    /// The base64 public key the registry must carry for this seed.
    fn public_key_b64(&self) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(self.key.verifying_key().to_bytes())
    }
}

/// A seed file is 32 bytes of hex, the shape `walgit collab join` writes.
fn read_seed(path: &Path) -> anyhow::Result<SigningKey> {
    let text = std::fs::read_to_string(path)?;
    let bytes = hex::decode(text.trim()).map_err(|e| anyhow::anyhow!("not hex: {e:?}"))?;
    let seed: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("a seed is exactly 32 bytes ({} found)", bytes.len()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// What one fold did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FoldOutcome {
    /// Inbox refs folded into the snapshot (and pruned).
    pub folded: usize,
    /// Blobs left in place because they are not collab entries.
    pub left: usize,
    /// The snapshot the repository now points at.
    pub snapshot: Option<String>,
    /// Whether the snapshot was rebuilt (false ⇒ prune-only fold).
    pub rebuilt: bool,
    /// Records dropped to stay under the size cap (0 ⇒ complete).
    pub dropped: u64,
    /// The WAL seq the fold was published at (0 ⇒ nothing published).
    pub seq: u64,
}

/// Count the repository's inbox refs without listing anything: the refs
/// snapshot is already in memory after `sync_refs` (`next_unit_at` does it).
pub fn inbox_refs(handle: &RepoHandle) -> usize {
    handle.local().refs().map_or(0, |snapshot| {
        snapshot
            .refs
            .iter()
            .filter(|r| r.name.starts_with(INBOX_PREFIX))
            .count()
    })
}

/// Fold `id`'s collab inbox, as `signer`, and publish it.
pub async fn fold(
    st: &Arc<AppState>,
    id: &RepoId,
    signer: &Signer,
    log: Log<'_>,
) -> anyhow::Result<FoldOutcome> {
    let handle = st.registry.open(id).await?;
    handle.sync_refs().await?;
    let r = api::view(st, handle.clone(), Need::Objects, Reporter::none())
        .await
        .map_err(|e| anyhow::anyhow!("open objects for {id}: {e:?}"))?;

    // The refs as this fold read them: every `old` below is a CAS expectation,
    // and the snapshot's is the baseline a concurrent fold would move.
    let refs = r.ref_list();
    let mut inbox: Vec<(String, String)> = Vec::new();
    let mut baseline: Option<String> = None;
    for (name, oid) in &refs {
        if name == SNAPSHOT_REF {
            baseline = Some(oid.clone());
        } else if name.starts_with(INBOX_PREFIX) {
            inbox.push((name.clone(), oid.clone()));
        }
    }
    if inbox.is_empty() {
        return Ok(FoldOutcome {
            snapshot: baseline,
            ..FoldOutcome::default()
        });
    }

    // One batch read for the snapshot and every inbox blob: 60–80 ms each, so a
    // per-entry GET would make a fold of 20k refs an hour of round trips.
    let mut wanted: Vec<String> = inbox.iter().map(|(_, oid)| oid.clone()).collect();
    if let Some(oid) = &baseline {
        wanted.push(oid.clone());
    }
    let blobs = api::git_cat_file_batch(r.local(), &wanted, Some(COLLAB_SNAPSHOT_MAX_BYTES))
        .await
        .map_err(|e| anyhow::anyhow!("read collab blobs: {e:?}"))?;

    // Records the existing snapshot already carries, keyed by oid: an oid
    // addresses the original bytes, so a record is never re-serialized.
    let mut records: Vec<SnapshotRecord> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    if let Some(oid) = &baseline {
        let Some(bytes) = blobs.get(oid) else {
            anyhow::bail!("{SNAPSHOT_REF} {oid} is not readable; refusing to fold over a baseline I cannot see")
        };
        let snap = parse_snapshot(bytes).map_err(|e| anyhow::anyhow!("{SNAPSHOT_REF}: {e:?}"))?;
        for record in snap.entries {
            seen.entry(record.oid.clone()).or_insert(records.len());
            records.push(record);
        }
        log(format!(
            "collab-fold: baseline {SNAPSHOT_REF} carries {} record(s)",
            records.len()
        ));
    }

    let mut left = 0usize;
    let mut changed = false;
    let mut folded: Vec<(String, String)> = Vec::new();
    for (name, oid) in &inbox {
        let Some(bytes) = blobs.get(oid) else {
            left += 1;
            continue;
        };
        let Ok(json) = String::from_utf8(bytes.clone()) else {
            left += 1;
            continue;
        };
        let Ok(entry) = serde_json::from_str::<Entry>(&json) else {
            left += 1;
            continue;
        };
        let Some(principal) = name
            .strip_prefix(INBOX_PREFIX)
            .and_then(|p| p.rsplit_once('/'))
            .map(|(p, _)| p.to_string())
        else {
            left += 1;
            continue;
        };
        // The principal recorded for an entry is the one whose inbox it sits
        // in — unless that tuple is already held by the *actor's own* inbox ref,
        // which is the authoritative placement (`walgit collab gc`'s
        // `keep_fold_record`, same rule).
        if let Some(index) = seen.get(oid).copied()
            && let Some(existing) = records.get_mut(index)
        {
            let actor_owned = principal == entry.actor;
            let held_by_actor = existing.principal == entry.actor;
            if actor_owned && !held_by_actor {
                existing.principal = principal;
                changed = true;
            }
        } else {
            seen.insert(oid.clone(), records.len());
            records.push(SnapshotRecord {
                oid: oid.clone(),
                principal,
                json,
            });
            changed = true;
        }
        folded.push((name.clone(), oid.clone()));
    }

    let over_cap = blobs
        .get(baseline.as_deref().unwrap_or_default())
        .is_some_and(|b| b.len() > COLLAB_SNAPSHOT_MAX_BYTES);
    if !changed && !over_cap {
        log(format!(
            "collab-fold: nothing to fold ({left} blob(s) left in place)"
        ));
        return Ok(FoldOutcome {
            folded: 0,
            left,
            snapshot: baseline,
            ..FoldOutcome::default()
        });
    }

    // Rebuild only when the ledger changed: a prune-only fold reuses the
    // baseline snapshot (a rebuild would differ only in `ts` — pure ref churn).
    let rebuilt = changed || over_cap;
    let (blob, dropped): (Option<Vec<u8>>, Vec<String>) = if rebuilt {
        let (snapshot, dropped) = build_capped(
            &signer.principal,
            now(),
            records,
            signer,
            COLLAB_SNAPSHOT_MAX_BYTES,
        )?;
        if !dropped.is_empty() {
            log(format!(
                "collab-fold: WARNING snapshot truncated — {} oldest record(s) dropped from the \
                 snapshot and kept in the inbox; the ledger is marked complete=false",
                dropped.len()
            ));
        }
        (Some(snapshot), dropped)
    } else {
        (None, Vec::new())
    };
    let snapshot_new = match &blob {
        Some(bytes) => Some(
            api::git_hash_object(r.local(), bytes)
                .await
                .map_err(|e| anyhow::anyhow!("hash snapshot: {e:?}"))?,
        ),
        None => None,
    };

    // Refuse a fold whose signature the registry would not verify: an
    // unverifiable snapshot strands every reader, and an append-only log cannot
    // be repaired in place.
    check_registered(st, &r, signer).await?;

    let mut updates: Vec<(String, Option<String>, String)> = Vec::with_capacity(folded.len() + 1);
    if let Some(oid) = &snapshot_new {
        updates.push((SNAPSHOT_REF.to_string(), baseline.clone(), oid.clone()));
    }
    // Prune exactly what the new snapshot carries. A truncated document keeps
    // only the newest records, so the ones it dropped stay in the inbox: an
    // entry deleted here and missing from the snapshot is deleted from the only
    // copy the repository has (`docs/D1_PROTOCOL.md` §9.3's `complete: false`
    // is a statement about the snapshot, never a licence to drop records).
    let dropped_set: std::collections::HashSet<&str> =
        dropped.iter().map(String::as_str).collect();
    let mut pruned = 0usize;
    for (name, oid) in &folded {
        if dropped_set.contains(oid.as_str()) {
            continue;
        }
        pruned += 1;
        updates.push((name.clone(), Some(oid.clone()), String::new()));
    }
    let seq = api::publish_refs(st, &handle, &r, &signer.principal, &updates, blob.as_deref())
        .await
        .map_err(|e| anyhow::anyhow!("publish fold: {e:?}"))?;
    let shown = snapshot_new
        .as_deref()
        .or(baseline.as_deref())
        .unwrap_or("new");
    log(format!(
        "collab-fold: folded {pruned} inbox ref(s) into {SNAPSHOT_REF} ({shown}), seq {seq}; {left} \
         left in place, {} kept out of the snapshot",
        dropped.len(),
    ));

    Ok(FoldOutcome {
        folded: pruned,
        left,
        snapshot: snapshot_new.or(baseline),
        rebuilt,
        dropped: dropped.len() as u64,
        seq,
    })
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed())
}

/// The snapshot document, truncated when the full ledger would exceed
/// `max_bytes`: `docs/D1_PROTOCOL.md` §9.3 drops the **oldest** entries first and
/// marks the ledger `complete: false` with `dropped_entries`. An automatic fold
/// cannot "refuse to publish" its way back to serviceability, so this is the
/// only truncation an unattended unit performs — and it says so in the log.
fn build_capped(
    actor: &str,
    ts: i64,
    records: Vec<SnapshotRecord>,
    signer: &Signer,
    max_bytes: usize,
) -> anyhow::Result<(Vec<u8>, Vec<String>)> {
    let sample = build_snapshot(actor, ts, records.clone(), &signer.key);
    let bytes = serde_json::to_vec(&sample)?;
    if bytes.len() <= max_bytes {
        return Ok((bytes, Vec::new()));
    }
    // Drop oldest-first until it fits. A record's own serialized size bounds its
    // contribution (plus one comma), and `ts` comes from its bytes because the
    // records are oid-sorted, not time-sorted.
    let total = records.len();
    let mut order: Vec<(i64, usize)> = records
        .iter()
        .enumerate()
        .map(|(i, record)| {
            let ts = serde_json::from_str::<serde_json::Value>(&record.json)
                .ok()
                .and_then(|v| v.get("ts").and_then(serde_json::Value::as_i64))
                .unwrap_or(i64::MAX);
            (ts, i)
        })
        .collect();
    order.sort_unstable();
    let mut size = bytes.len();
    let mut dropped: Vec<usize> = Vec::new();
    for (_, index) in &order {
        if size <= max_bytes {
            break;
        }
        let Some(record) = records.get(*index) else {
            continue;
        };
        size = size.saturating_sub(serde_json::to_vec(record)?.len() + 1);
        dropped.push(*index);
    }
    dropped.sort_unstable();
    // The oids the truncated document does NOT carry: their inbox refs must
    // survive, or the fold deletes the only copy of an entry it did not keep.
    let dropped_oids: Vec<String> = dropped
        .iter()
        .filter_map(|i| records.get(*i).map(|r| r.oid.clone()))
        .collect();
    let dropped_count = dropped.len() as u64;
    let kept: Vec<SnapshotRecord> = records
        .into_iter()
        .enumerate()
        .filter(|(i, _)| dropped.binary_search(i).is_err())
        .map(|(_, record)| record)
        .collect();
    let _ = total;
    let snap = build_snapshot_truncated(actor, ts, kept, dropped_count, &signer.key);
    Ok((serde_json::to_vec(&snap)?, dropped_oids))
}

/// The fold principal must be registered with exactly this key.
async fn check_registered(
    st: &Arc<AppState>,
    r: &api::Repo,
    signer: &Signer,
) -> anyhow::Result<()> {
    let want = signer.public_key_b64();
    let names: Vec<String> = r
        .ref_list()
        .into_iter()
        .filter_map(|(name, _)| name.strip_prefix(PRINCIPALS_PREFIX).map(str::to_string))
        .collect();
    let seen = format!(
        "registered principal(s): {}",
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        }
    );
    let key = r
        .principal_key(st, &signer.principal)
        .await
        .map_err(|e| anyhow::anyhow!("read the principal registry: {e:?}"))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "maintenance.collab_fold_principal {:?} is not registered in this repository ({seen}); \
                 register it with `walgit collab principal-register --principal {} --key <keyfile> \
                 --push origin`, or leave the pair unset and fold manually",
                signer.principal,
                signer.principal
            )
        })?;
    anyhow::ensure!(
        key.trim() == want,
        "maintenance.collab_fold_key is not the key registered for {:?} (registry has a different \
         public key); the snapshot it signs would never verify",
        signer.principal
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(oid: &str, ts: i64) -> SnapshotRecord {
        SnapshotRecord {
            oid: oid.to_string(),
            principal: "alice".to_string(),
            json: format!(r#"{{"version":1,"kind":"issue","id":"c","actor":"alice","ts":{ts},"sig":"x"}}"#),
        }
    }

    fn signer() -> Signer {
        Signer {
            principal: "alice".to_string(),
            key: SigningKey::from_bytes(&[7u8; 32]),
        }
    }

    /// A truncated snapshot must say **which** records it left out: the caller
    /// prunes inbox refs by that set, and a record that is neither in the
    /// snapshot nor in its inbox is gone from an append-only ledger.
    #[test]
    fn a_truncated_snapshot_names_the_records_it_dropped() {
        let records = vec![record("old", 10), record("middle", 20), record("new", 30)];
        // A cap that fits exactly one record: the newest one.
        let signer = signer();
        let one = build_snapshot("alice", 40, vec![record("new", 30)], &signer.key);
        let cap = serde_json::to_vec(&one).unwrap().len();
        let (bytes, dropped) = build_capped("alice", 40, records.clone(), &signer, cap).unwrap();
        assert_eq!(dropped, vec!["old".to_string(), "middle".to_string()], "{dropped:?}");
        let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(doc["complete"], false, "{doc}");
        assert_eq!(doc["dropped_entries"], 2, "{doc}");

        // Everything fits: nothing is dropped, nothing is marked incomplete.
        let (bytes, dropped) = build_capped("alice", 40, records, &signer, 1 << 20).unwrap();
        assert!(dropped.is_empty(), "{dropped:?}");
        // A complete snapshot carries no `complete`/`dropped_entries` at all:
        // the fields exist to explain a truncation, not to restate the norm.
        let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(doc.get("complete").is_none(), "{doc}");
    }
}
