//! D1 collaboration protocol (`docs/D1_PROTOCOL.md` §5/§6/§7): the
//! deterministic aggregation over `refs/collab/*` — entry schema, canonical
//! form, Ed25519 verification, `thread` / `pr` / `merge_rule_eval` / `report`.
//! Pure functions: every client that reads the same refs and verifies the same
//! signatures computes the same answer. Shared by the `walgit collab` CLI and
//! the server's collab API.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use walgit_git::ObjectFormat;

// ---- docs/D1_PROTOCOL.md §5 entry schema -------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub version: u32,
    pub kind: String,
    pub id: String,
    pub actor: String,
    pub ts: i64,
    /// Previous entry's object id in the thread, or "" for the root.
    pub parent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<EntryRefs>,
    pub body: serde_json::Value,
    /// `ed25519:<base64>` over the canonical form of the entry without `sig`.
    #[serde(default)]
    pub sig: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct EntryRefs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

/// A parsed entry together with its object id and the principal whose inbox
/// holds it.
#[derive(Clone, Debug)]
pub struct EntryRef {
    pub oid: String,
    pub principal: String,
    pub entry: Entry,
}

impl EntryRef {
    /// Whether the entry counts as verified: the signature checks against the
    /// actor's registered key **and** the inbox it was found in names the
    /// entry's own principal. The inbox model (`docs/D1_PROTOCOL.md` §4.5) shards write access by
    /// principal; a policy that lets anyone write any inbox must not smuggle
    /// an entry across principals — the signature alone only proves the actor
    /// signed it, not that it belongs in this inbox.
    pub fn is_verified(&self, principals: &HashMap<String, String, impl std::hash::BuildHasher>) -> bool {
        self.principal == self.entry.actor
            && principals
                .get(&self.entry.actor)
                .is_some_and(|k| verify_entry(&self.entry, k).is_ok())
    }
}

// ---- docs/D1_PROTOCOL.md §5.3 canonical form (the signed bytes) ----------------------------------

/// Recursive key-sorted, whitespace-free JSON: objects sorted by key bytes,
/// arrays in order, strings JSON-escaped, numbers as JSON numbers. This must
/// match the SDK's `canonicalize` (web/sdk/repos.ts) so cross-language
/// verifiers reproduce the exact signed input.
pub fn canonicalize(value: &serde_json::Value) -> String {
    let mut out = String::new();
    canonicalize_into(value, &mut out);
    out
}

fn canonicalize_into(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => out.push_str(&n.to_string()),
        serde_json::Value::String(s) => {
            out.push_str(&serde_json::to_string(s).unwrap_or_default());
        }
        serde_json::Value::Array(a) => {
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonicalize_into(v, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(o) => {
            out.push('{');
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort_unstable();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                canonicalize_into(&o[*k], out);
            }
            out.push('}');
        }
    }
}

/// The canonical bytes an entry's signature covers: the entry without `sig`.
pub fn entry_canonical(entry: &Entry) -> String {
    let mut unsigned = entry.clone();
    unsigned.sig.clear();
    let value = serde_json::to_value(unsigned).unwrap_or_default();
    canonicalize(&value)
}

// ---- verification ------------------------------------------------------------

/// Verify an entry against its principal's public key (base64) from
/// `refs/collab/meta/principals/<principal>`.
pub fn verify_entry(entry: &Entry, public_key_b64: &str) -> Result<(), String> {
    let sig_b64 = entry
        .sig
        .strip_prefix("ed25519:")
        .ok_or_else(|| "sig must be `ed25519:<base64>`".to_string())?;
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(sig_b64)
        .map_err(|e| format!("bad sig base64: {e}"))?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|e| format!("bad signature: {e}"))?;
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(public_key_b64.trim())
        .map_err(|e| format!("bad public key base64: {e}"))?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "public key must be 32 raw Ed25519 bytes".to_string())?;
    let vk = VerifyingKey::from_bytes(&key_bytes).map_err(|e| format!("bad public key: {e}"))?;
    vk.verify_strict(entry_canonical(entry).as_bytes(), &sig)
        .map_err(|e| format!("signature verification failed: {e}"))
}

/// Sign an entry's canonical form with a `SigningKey`; returns `ed25519:<base64>`.
/// Used by the write path (`walgit collab entry` / the SDK); the aggregation
/// core only verifies, so this is exercised by tests.
#[allow(dead_code)]
pub fn sign_entry(entry: &mut Entry, key: &SigningKey) -> String {
    let canonical = entry_canonical(entry);
    let sig = key.sign(canonical.as_bytes());
    format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())
    )
}

// ---- docs/D1_PROTOCOL.md §9 the fold: a signed snapshot of the inbox (D45) ---------------------
//
// The append-only inbox namespace hits two walls as it grows (the per-request
// aggregation budget and the clone/fetch advertisement size), so the inbox
// folds the way the WAL log folds: one CAS-moved ref,
// `refs/collab/meta/snapshot`, carries every folded entry verbatim, and every
// aggregation reads snapshot ∪ unfolded tail, deduped by oid. The fold is a
// pure function of the entry set, so aggregation before and after is
// byte-identical. Normative text: `docs/D1_PROTOCOL.md` §9.

/// Where the folded collab state lives: one ref per repository, moved forward
/// by `walgit collab gc` (snapshot first, then the pruned inbox refs are
/// deleted — a mid-fold reader sees duplicates, never a loss).
pub const SNAPSHOT_REF: &str = "refs/collab/meta/snapshot";

/// One folded entry: its blob oid (recomputable from `json`), the inbox it was
/// found in, and the raw entry bytes, verbatim. The record is the digest
/// manifest that keeps the per-entry signature chain verifiable after the
/// inbox ref is pruned — entry trust never derives from the snapshot's own
/// signature; every folded entry still verifies per `docs/D1_PROTOCOL.md` §5.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SnapshotRecord {
    pub oid: String,
    pub principal: String,
    pub json: String,
}

impl SnapshotRecord {
    /// The record as an aggregation input, or `None` when the bytes do not
    /// hash to the declared oid or do not parse as an entry — skipped exactly
    /// like a corrupt inbox blob: visible degradation (the totals move), never
    /// silent trust.
    pub fn entry_ref(&self, format: ObjectFormat) -> Option<EntryRef> {
        if git_blob_oid(self.json.as_bytes(), format) != self.oid {
            return None;
        }
        let entry: Entry = serde_json::from_str(&self.json).ok()?;
        Some(EntryRef {
            oid: self.oid.clone(),
            principal: self.principal.clone(),
            entry,
        })
    }
}

/// The snapshot document at `SNAPSHOT_REF`: the folded entries plus fold
/// provenance (`actor`, `ts`, `sig`). The signature covers the canonical form
/// of the document without `sig` (the `docs/D1_PROTOCOL.md` §5.3 canonical contract) and attests who
/// folded, when; readers verify contained entries independently.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    pub version: u32,
    pub kind: String,
    pub actor: String,
    pub ts: i64,
    pub entries: Vec<SnapshotRecord>,
    /// Whether this fold carries the whole ledger it was built from. Absent =
    /// true (pre-marker snapshots); `false` marks a deliberately truncated
    /// snapshot, with `dropped_entries` saying how many records were left out.
    #[serde(default = "snapshot_complete_default", skip_serializing_if = "is_true")]
    pub complete: bool,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub dropped_entries: u64,
    #[serde(default)]
    pub sig: String,
}

/// Upper bound for one repository's folded snapshot, shared by the fold (write
/// side: refuses to publish over it) and the server (read side: refuses to
/// materialize it).
pub const COLLAB_SNAPSHOT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Upper bound for one collab entry blob read through aggregation. Larger
/// blobs are skipped (visible degradation) instead of materialized — the
/// per-request budget counts refs, not bytes (docs/D1_PROTOCOL.md §13).
pub const COLLAB_ENTRY_MAX_BYTES: usize = 256 * 1024;

fn snapshot_complete_default() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// The git blob id of raw bytes: `<hash>("blob <len>\0" + bytes)` in the
/// repository's object format — how a record's declared oid is pinned to its
/// bytes. Both SHA-1 and SHA-256 repositories are first-class.
pub fn git_blob_oid(bytes: &[u8], format: ObjectFormat) -> String {
    let mut h = gix_hash::hasher(format.kind());
    h.update(format!("blob {}\0", bytes.len()).as_bytes());
    h.update(bytes);
    #[allow(
        clippy::expect_used,
        reason = "the header update above makes try_finalize infallible"
    )]
    let oid = h.try_finalize().expect("git object hash finalization");
    oid.to_string()
}

/// Parse and validate a snapshot document. Fails closed on a wrong
/// version/kind — a corrupt snapshot document errors the read (silently
/// dropping every folded entry would rewrite history); per-record integrity is
/// enforced in `SnapshotRecord::entry_ref`.
pub fn parse_snapshot(bytes: &[u8]) -> Result<Snapshot, String> {
    let snap: Snapshot =
        serde_json::from_slice(bytes).map_err(|e| format!("collab snapshot: {e}"))?;
    if snap.version != 1 {
        return Err(format!(
            "collab snapshot: unsupported version {} (only 1 exists)",
            snap.version
        ));
    }
    if snap.kind != "collab_snapshot" {
        return Err(format!("collab snapshot: kind {:?} is not collab_snapshot", snap.kind));
    }
    Ok(snap)
}

/// The canonical bytes the snapshot's signature covers: the document without
/// `sig`, under the `docs/D1_PROTOCOL.md` §5.3 canonical contract.
pub fn snapshot_canonical(snap: &Snapshot) -> String {
    let mut unsigned = snap.clone();
    unsigned.sig.clear();
    let value = serde_json::to_value(unsigned).unwrap_or_default();
    canonicalize(&value)
}

/// Sign a snapshot document; returns `ed25519:<base64>`. Symmetric to
/// `sign_entry`; used by the fold's write path (`walgit collab gc`).
pub fn sign_snapshot(snap: &mut Snapshot, key: &SigningKey) -> String {
    let canonical = snapshot_canonical(snap);
    let sig = key.sign(canonical.as_bytes());
    format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())
    )
}

/// Verify a snapshot's own signature against a public key (base64) — fold
/// provenance for audit and the watch notification; never a trust input for
/// the contained entries.
pub fn verify_snapshot(snap: &Snapshot, public_key_b64: &str) -> Result<(), String> {
    let sig_b64 = snap
        .sig
        .strip_prefix("ed25519:")
        .ok_or_else(|| "sig must be `ed25519:<base64>`".to_string())?;
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(sig_b64)
        .map_err(|e| format!("bad sig base64: {e}"))?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|e| format!("bad signature: {e}"))?;
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(public_key_b64.trim())
        .map_err(|e| format!("bad public key base64: {e}"))?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "public key must be 32 raw Ed25519 bytes".to_string())?;
    let vk = VerifyingKey::from_bytes(&key_bytes).map_err(|e| format!("bad public key: {e}"))?;
    vk.verify_strict(snapshot_canonical(snap).as_bytes(), &sig)
        .map_err(|e| format!("signature verification failed: {e}"))
}

/// Build the fold: records deduped by oid and sorted oid-ascending (a pure
/// function of the set — the same inbox folds to the same snapshot bytes),
/// signed by `actor`. Carries existing snapshot records verbatim: an oid
/// addresses the original bytes, so records are never re-serialized.
pub fn build_snapshot(
    actor: &str,
    ts: i64,
    mut entries: Vec<SnapshotRecord>,
    key: &SigningKey,
) -> Snapshot {
    build_snapshot_with(actor, ts, entries, true, 0, key)
}

/// A fold that deliberately dropped `dropped_entries` records to stay under
/// `COLLAB_SNAPSHOT_MAX_BYTES`. The marker makes a truncated ledger
/// distinguishable from a complete one (`complete: false`).
pub fn build_snapshot_truncated(
    actor: &str,
    ts: i64,
    entries: Vec<SnapshotRecord>,
    dropped_entries: u64,
    key: &SigningKey,
) -> Snapshot {
    build_snapshot_with(actor, ts, entries, false, dropped_entries, key)
}

fn build_snapshot_with(
    actor: &str,
    ts: i64,
    mut entries: Vec<SnapshotRecord>,
    complete: bool,
    dropped_entries: u64,
    key: &SigningKey,
) -> Snapshot {
    entries.sort_by(|a, b| a.oid.cmp(&b.oid));
    entries.dedup_by(|a, b| a.oid == b.oid);
    let mut snap = Snapshot {
        version: 1,
        kind: "collab_snapshot".to_string(),
        actor: actor.to_string(),
        ts,
        entries,
        complete,
        dropped_entries,
        sig: String::new(),
    };
    snap.sig = sign_snapshot(&mut snap, key);
    snap
}

/// The aggregation input as a *set* keyed by oid (D45 read semantics): the
/// same blob under two refs — a retried push, or a snapshot record overlapping
/// a not-yet-deleted inbox ref mid-fold — is one entry, so the answer never
/// depends on how the refs were read or whether a fold is in flight. Shared by
/// the CLI's `CollabReader` and the server's `collab_load` so both aggregate
/// the identical set.
#[derive(Default)]
pub struct EntrySet {
    entries: Vec<EntryRef>,
    by_oid: HashMap<String, usize>,
}

impl EntrySet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert one occurrence of an entry. A duplicate oid does not double
    /// count; when the copies disagree on the inbox (a signed entry planted in
    /// someone else's inbox next to the legitimate copy), the occurrence whose
    /// principal is the entry's own actor wins — a planted copy never
    /// displaces the legitimate one.
    pub fn insert(&mut self, er: EntryRef) {
        match self.by_oid.get(&er.oid) {
            None => {
                self.by_oid.insert(er.oid.clone(), self.entries.len());
                self.entries.push(er);
            }
            Some(&i) => {
                let replace = self.entries.get(i).is_some_and(|cur| {
                    cur.principal != cur.entry.actor && er.principal == er.entry.actor
                });
                if replace && let Some(slot) = self.entries.get_mut(i) {
                    *slot = er;
                }
            }
        }
    }

    pub fn into_entries(self) -> Vec<EntryRef> {
        self.entries
    }
}

// ---- docs/D1_PROTOCOL.md §6/§7 deterministic aggregation ------------------------------------------

/// One issue/thread: entries referencing the same `id`, topologically ordered
/// by the `parent` chain (deterministic: ts as the tie-break).
/// Structured cross-thread references (issue #75 ③): the entry-body fields
/// `related` / `depends_on` carry arrays of entry oids. Extracted here so the
/// thread view and the write path agree on the exact convention.
/// Transition 门禁(issue #75 ①, 方案 A:硬编码通用状态机)。
///
/// `status` 条目写 `done` 时校验前置条件:线程当前处于 `needs-review`
/// 且存在 verified approve review。其余流转自由;`closed` 不限前置。
pub fn validate_status_transition(
    thread_entries: &[&EntryRef],
    principals: &HashMap<String, String, impl std::hash::BuildHasher>,
) -> Result<(), String> {
    let current = card_status(thread_entries);
    if current != "needs-review" {
        return Err(format!(
            "transition → done requires current status needs-review (got {current})"
        ));
    }
    let authors: Vec<&str> = thread_entries
        .iter()
        .filter(|r| r.entry.kind == "patch" && r.is_verified(principals))
        .map(|r| r.entry.actor.as_str())
        .collect();
    let has_approve = thread_entries.iter().any(|r| {
        r.entry.kind == "review"
            && r.entry.body.get("decision").and_then(|v| v.as_str()) == Some("approve")
            && r.is_verified(principals)
            && !authors.contains(&r.entry.actor.as_str())
    });
    if !has_approve {
        return Err("transition → done requires a verified approve review in the thread".into());
    }
    Ok(())
}

pub fn referenced_oids(body: &serde_json::Value) -> Vec<String> {
    ["related", "depends_on"]
        .iter()
        .filter_map(|k| body.get(k))
        .filter_map(|v| v.as_array())
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// The referenced oids missing from `known` (the set of all collab entry oids
/// in the repository). Empty = every reference resolves.
pub fn broken_refs<S: std::hash::BuildHasher>(
    referenced: &[String],
    known: &std::collections::HashSet<String, S>,
) -> Vec<String> {
    referenced
        .iter()
        .filter(|o| !known.contains(*o))
        .cloned()
        .collect()
}

pub fn thread<'a>(entries: &[&'a EntryRef]) -> Vec<&'a EntryRef> {
    let by_oid: HashMap<&str, &EntryRef> = entries
        .iter()
        .map(|e| (e.oid.as_str(), *e))
        .collect();
    let mut emitted: HashMap<&str, bool> = HashMap::new();
    let mut out: Vec<&EntryRef> = Vec::new();
    // Deterministic first pass order: (ts, actor, oid).
    let mut pending: Vec<&EntryRef> = entries.to_vec();
    pending.sort_by(|a, b| {
        (a.entry.ts, a.entry.actor.as_str(), a.oid.as_str())
            .cmp(&(b.entry.ts, b.entry.actor.as_str(), b.oid.as_str()))
    });
    // Ready entries always emit; a round with no emission means every remaining
    // entry waits on another pending one. Content addressing makes a real parent
    // cycle unconstructible, so that is malformed input — exit defensively and
    // emit the remainder in sorted order (deterministic). The round-bounded
    // guard this replaces was evaluated against the shrinking pending list, so
    // long chains whose ts descends along the chain truncated and reordered
    // their tail (cc-ai-d1-protocol-followups P0).
    while !pending.is_empty() {
        let before = pending.len();
        let mut next: Vec<&EntryRef> = Vec::new();
        for e in pending {
            let ready = e.entry.parent.is_empty()
                || !by_oid.contains_key(e.entry.parent.as_str())
                || emitted.get(e.entry.parent.as_str()).copied().unwrap_or(false);
            if ready {
                emitted.insert(&e.oid, true);
                out.push(e);
            } else {
                next.push(e);
            }
        }
        pending = next;
        if pending.len() == before {
            break;
        }
    }
    out.extend(pending); // no-progress remainder: emit the rest deterministically
    out
}

/// The aggregated PR view for an `id` that has a `patch` entry.
#[derive(Serialize, Clone, Debug)]
pub struct PrView {
    pub id: String,
    pub base: Option<String>,
    pub head: Option<String>,
    pub status: String,
    pub reviews: Vec<Review>,
    /// Verified `approve` reviews by non-agent actors.
    pub human_approvals: Vec<Review>,
    /// Verified `patch` authors (distinct, in thread order). Self-approvals by
    /// these actors never count toward the merge rule or the `done` gate.
    pub authors: Vec<String>,
    pub unverified: Vec<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Review {
    pub actor: String,
    pub decision: String,
    pub ts: i64,
    pub oid: String,
}

fn is_agent(actor: &str) -> bool {
    actor.starts_with("svc-")
}

pub fn pr_view(
    entries: &[&EntryRef],
    principals: &HashMap<String, String, impl std::hash::BuildHasher>,
) -> PrView {
    let ordered = thread(entries);
    let mut base = None;
    let mut head = None;
    let mut status = "open".to_string();
    let mut reviews: Vec<Review> = Vec::new();
    let mut human_approvals: Vec<Review> = Vec::new();
    let mut unverified: Vec<String> = Vec::new();
    let mut authors: Vec<String> = Vec::new();
    for r in &ordered {
        let e = &r.entry;
        let verified = r.is_verified(principals);
        if e.kind == "patch" {
            if let Some(rs) = &e.refs {
                base = rs.base.clone().or(base);
                head = rs.head.clone().or(head);
            }
            if verified && !authors.contains(&e.actor) {
                authors.push(e.actor.clone());
            }
        }
        if !verified {
            unverified.push(format!("{}@{}", e.actor, r.oid));
        }
        match e.kind.as_str() {
            "status" => {
                if let Some(st) = e.body.get("status").and_then(|v| v.as_str())
                    && (st == "merged" || st == "closed")
                {
                    status = st.to_string();
                }
            }
            "merge_result" => {
                if e.body
                    .get("merged")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    status = "merged".to_string();
                }
            }
            "review" => {
                let decision = e
                    .body
                    .get("decision")
                    .and_then(|v| v.as_str())
                    .unwrap_or("comment")
                    .to_string();
                let review = Review {
                    actor: e.actor.clone(),
                    decision: decision.clone(),
                    ts: e.ts,
                    oid: r.oid.clone(),
                };
                if decision == "approve" && verified {
                    human_approvals.push(review.clone());
                }
                reviews.push(review);
            }
            _ => {}
        }
    }
    PrView {
        id: ordered.first().map(|r| r.entry.id.clone()).unwrap_or_default(),
        base,
        head,
        status,
        reviews,
        human_approvals,
        authors,
        unverified,
    }
}

// ---- merge rule evaluation ---------------------------------------------------

/// A minimal merge rule document (stored at `refs/collab/meta/rules` or given
/// on the CLI). `docs/D1_PROTOCOL.md` §7.3: merge rules are deterministic functions of the log.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MergeRules {
    /// Ref patterns whose merges require human approvals (e.g.
    /// `["refs/heads/main"]`). Empty = nothing protected.
    #[serde(default)]
    pub protect: Vec<String>,
    #[serde(default = "default_approvals")]
    pub require_human_approvals: usize,
}

fn default_approvals() -> usize {
    1
}

#[derive(Serialize, Clone, Debug)]
pub struct MergeEval {
    pub allowed: bool,
    pub reason: String,
    pub satisfied_by: Vec<String>,
}

/// Evaluate whether the PR may be merged: protected bases need at least
/// `require_human_approvals` verified approvals from non-agent actors.
pub fn merge_rule_eval(rules: &MergeRules, pr: &PrView) -> MergeEval {
    let protected = pr
        .base
        .as_deref()
        .is_some_and(|b| rules.protect.iter().any(|p| p == b || b.starts_with(p)));
    if !protected {
        return MergeEval {
            allowed: true,
            reason: "base is not protected".to_string(),
            satisfied_by: Vec::new(),
        };
    }
    // Distinct approvers, and never the patch's own author: one principal
    // approving twice — or approving their own change — is not two reviews.
    let mut satisfied_by: Vec<String> = Vec::new();
    for r in &pr.human_approvals {
        if r.decision != "approve" || is_agent(&r.actor) || pr.authors.contains(&r.actor) {
            continue;
        }
        if !satisfied_by.contains(&r.actor) {
            satisfied_by.push(r.actor.clone());
        }
    }
    if satisfied_by.len() >= rules.require_human_approvals {
        MergeEval {
            allowed: true,
            reason: format!(
                "{} distinct non-author human approval(s) on protected base",
                satisfied_by.len()
            ),
            satisfied_by,
        }
    } else {
        MergeEval {
            allowed: false,
            reason: format!(
                "protected base needs {} distinct non-author human approval(s), got {}",
                rules.require_human_approvals,
                satisfied_by.len()
            ),
            satisfied_by,
        }
    }
}

// ---- report: read-only observability dashboard (docs/D1_PROTOCOL.md §7.4/§8) -------------------------

#[derive(Serialize, Clone, Debug)]
pub struct ReportThread {
    pub id: String,
    /// The root entry's `body.title`, "" when it has none (issue #131 — the
    /// same rule as `BoardCard.title`, so the report list and the board cards
    /// never disagree about what a thread is called).
    pub title: String,
    pub entries: usize,
    pub verified: usize,
    pub last_ts: i64,
    pub kinds: Vec<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ReportPr {
    pub id: String,
    /// The root entry's `body.title`, "" when it has none (issue #131).
    pub title: String,
    pub base: Option<String>,
    pub head: Option<String>,
    pub status: String,
    pub approvals: usize,
    pub merge_allowed: bool,
    pub merge_reason: String,
}

/// One CI run, projected by the D1-CI §7 aggregation (`docs/D1_CI_PROTOCOL.md` §8.3 —
/// the report's CI section; pure-CI threads are not board cards, but the
/// report and the SPA need to find them: the guide's claim-race diagram and
/// `walgit collab report`'s CI section both read this).
#[derive(Serialize, Clone, Debug)]
pub struct ReportRun {
    pub id: String,
    pub task: String,
    pub repo_ref: String,
    pub commit: String,
    /// pending | claimed | stale | done (`RunState::as_str`).
    pub state: String,
    pub conclusion: Option<String>,
    pub runner: Option<String>,
    pub claims: usize,
    pub last_ts: i64,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Report {
    /// Ordered by the projection, not by the caller: newest activity first
    /// (`last_ts` descending, id ascending breaks ties). Every client
    /// (CLI text/markdown/html, the API JSON, the SPA table) renders this
    /// order, which keeps the two clients byte-identical.
    pub threads: Vec<ReportThread>,
    /// `open` first, then `merged`, then `closed`; within a status newest
    /// activity first, id ascending breaks ties.
    pub prs: Vec<ReportPr>,
    /// CI runs (D1-CI §8.3) — pure-CI threads are skipped above and projected here
    /// by `ci::collect_runs` instead.
    pub runs: Vec<ReportRun>,
    pub total_entries: usize,
    pub verified_entries: usize,
    pub unverified_entries: usize,
    pub missing_principals: usize,
    pub by_actor: Vec<(String, usize)>,
    pub by_kind: Vec<(String, usize)>,
}

/// The thread's canonical root: the first verified root entry (parent empty)
/// in thread order, falling back to the first entry when no verified root
/// exists (unsigned legacy threads keep their old identity rule). One
/// extraction rule for `BoardCard`, `ReportThread` and `ReportPr` — and the
/// verified preference means an unregistered pass-by cannot rewrite a card's
/// identity by appending a backdated root.
fn canonical_root<'a>(
    ordered: &[&'a EntryRef],
    principals: &HashMap<String, String, impl std::hash::BuildHasher>,
) -> Option<&'a EntryRef> {
    ordered
        .iter()
        .copied()
        .find(|r| r.entry.parent.is_empty() && r.is_verified(principals))
        .or_else(|| ordered.first().copied())
}

fn root_title(
    ordered: &[&EntryRef],
    principals: &HashMap<String, String, impl std::hash::BuildHasher>,
) -> String {
    canonical_root(ordered, principals)
        .and_then(|r| r.entry.body.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

pub fn build_report(
    entries: &[&EntryRef],
    principals: &HashMap<String, String, impl std::hash::BuildHasher>,
    rules: &MergeRules,
    now: i64,
) -> Report {
    let mut by_thread: BTreeMap<&str, Vec<&EntryRef>> = BTreeMap::new();
    for e in entries {
        by_thread.entry(e.entry.id.as_str()).or_default().push(e);
    }
    let mut report = Report::default();
    for (id, group) in &by_thread {
        // CI run threads are not work-unit cards (docs/D1_CI_PROTOCOL.md
        // D1-CI §8.3): their surfaces are `walgit ci status`, the report's CI
        // section and the SPA thread badge — on a board they would only ever
        // be untitled "open" cards.
        if group
            .iter()
            .all(|r| r.entry.kind == crate::ci::CI_CLAIM_KIND || r.entry.kind == crate::ci::CI_RESULT_KIND)
        {
            continue;
        }
        let ordered = thread(group);
        let verified = ordered.iter().filter(|r| r.is_verified(principals)).count();
        let mut kinds: Vec<String> = group.iter().map(|r| r.entry.kind.clone()).collect();
        kinds.sort();
        kinds.dedup();
        report.threads.push(ReportThread {
            id: (*id).to_string(),
            title: root_title(&ordered, principals),
            entries: group.len(),
            verified,
            last_ts: group.iter().map(|r| r.entry.ts).max().unwrap_or(0),
            kinds,
        });
    }
    let mut prs: Vec<(i64, ReportPr)> = Vec::new();
    for (id, group) in &by_thread {
        if group.iter().any(|r| r.entry.kind == "patch") {
            let pr = pr_view(group, principals);
            let eval = merge_rule_eval(rules, &pr);
            let last_ts = group.iter().map(|r| r.entry.ts).max().unwrap_or(0);
            prs.push((
                last_ts,
                ReportPr {
                    id: (*id).to_string(),
                    title: root_title(&thread(group), principals),
                    base: pr.base.clone(),
                    head: pr.head.clone(),
                    status: pr.status.clone(),
                    approvals: pr.human_approvals.len(),
                    merge_allowed: eval.allowed,
                    merge_reason: eval.reason.clone(),
                },
            ));
        }
    }
    // The list order is part of the projection (issue #131 follow-up: a
    // meaningful order the CLI text, the API JSON and the SPA all agree on —
    // clients render arrival order and never re-sort). Threads: newest
    // activity first. PRs: `open` before `merged` before `closed`, then newest
    // activity first. Both tie-break on id ascending, so the sort is a total
    // order over the input: the same refs give the same bytes on every client.
    report.threads.sort_by(|a, b| {
        b.last_ts
            .cmp(&a.last_ts)
            .then_with(|| a.id.cmp(&b.id))
    });
    prs.sort_by(|(ts_a, a), (ts_b, b)| {
        let rank = |s: &str| match s {
            "open" => 0u8,
            "merged" => 1,
            _ => 2, // "closed" and anything else a status entry can name
        };
        rank(&a.status)
            .cmp(&rank(&b.status))
            .then_with(|| ts_b.cmp(ts_a))
            .then_with(|| a.id.cmp(&b.id))
    });
    report.prs = prs.into_iter().map(|(_, p)| p).collect();
    report.total_entries = entries.len();
    for r in entries {
        let verified = r.is_verified(principals);
        if verified {
            report.verified_entries += 1;
        } else {
            report.unverified_entries += 1;
            if !principals.contains_key(&r.entry.actor) {
                report.missing_principals += 1;
            }
        }
    }
    let mut by_actor: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for r in entries {
        *by_actor.entry(&r.entry.actor).or_default() += 1;
        *by_kind.entry(&r.entry.kind).or_default() += 1;
    }
    report.by_actor = by_actor.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    report.by_kind = by_kind.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    // D1-CI §8.3: the skipped pure-CI threads are the report's CI section — one
    // summary per run, straight from the D1-CI §7 aggregation (only verified,
    // well-formed entries drive it; unverified stay visible in by_kind).
    report.runs = crate::ci::collect_runs(entries, principals, now)
        .into_iter()
        .map(|(id, run)| ReportRun {
            id,
            task: run.task,
            repo_ref: run.repo_ref,
            commit: run.commit,
            state: run.state.as_str().to_string(),
            conclusion: run.conclusion.map(|c| c.as_str().to_string()),
            runner: run.runner,
            claims: run.attempts.iter().map(|a| a.claims.len()).sum(),
            last_ts: run.last_ts,
        })
        .collect();
    report
}

// ---- board: a deterministic projection of the threads (docs/D1_PROTOCOL.md §7.4/§8) -----------------
//
// The board is not state and not a view with its own write path: it is the
// thread set folded under a declarative column definition versioned with the
// repository (`.walgit/board.toml`, plain git — not a collab ref, so editing it
// is an ordinary commit reviewed like any other). Moving a card is an ordinary
// signed `status` entry; the projection re-derives the columns from it.

/// Where the board definition is versioned (repo-relative, in the tree).
pub const BOARD_PATH: &str = ".walgit/board.toml";

/// One lane's predicate. A card enters the **first** declared column whose
/// predicate it satisfies; empty predicate fields match anything, so a column
/// with none of them set is the catch-all. Cards matching no column are not on
/// the board — say what you want to see, don't get what you didn't ask for.
#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BoardColumnDef {
    pub name: String,
    /// The thread must contain an entry of this kind.
    #[serde(default)]
    pub kind: String,
    /// The card's effective status (`card_status`) must equal this.
    #[serde(default)]
    pub status: String,
    /// `allowed` | `blocked`: the merge-rule verdict on the thread's patch.
    /// Cards without a `patch` match neither value (there is no verdict).
    #[serde(default)]
    pub merge: String,
    /// Only cards carrying at least one unverified entry.
    #[serde(default)]
    pub unverified: bool,
}

#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BoardSortBy {
    /// Last activity (`last_ts`) — the default.
    #[default]
    Ts,
    /// Thread id.
    Id,
}

#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BoardSortDirection {
    /// Newest first — the default.
    #[default]
    Desc,
    Asc,
}

/// Stable order inside a column. The sort field carries the configured
/// direction; the card id breaks ties ascending either way, so the output is a
/// total order over the input (same refs ⇒ byte-identical board, any order the
/// refs were read in).
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BoardSort {
    #[serde(default)]
    pub by: BoardSortBy,
    #[serde(default)]
    pub direction: BoardSortDirection,
}

/// The board definition: `version` + column predicates + a sort. Kept
/// deliberately minimal — a board nobody can re-derive by hand is a second
/// state source, not a projection.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BoardDef {
    pub version: u32,
    #[serde(default)]
    pub sort: BoardSort,
    /// `[[column]]` in the document, in declaration order (first match wins).
    #[serde(default, rename = "column")]
    pub columns: Vec<BoardColumnDef>,
}

impl BoardDef {
    /// Fails closed: an unsupported version, no columns, an empty/duplicate
    /// column name or an unknown `merge` verdict is a broken board — a client
    /// must show the error, not silently fold cards into the wrong lane.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!(
                "unsupported board version {} (only 1 exists)",
                self.version
            ));
        }
        if self.columns.is_empty() {
            return Err("a board needs at least one column".to_string());
        }
        for c in &self.columns {
            if c.name.is_empty() {
                return Err("column name must not be empty".to_string());
            }
            if !c.merge.is_empty() && c.merge != "allowed" && c.merge != "blocked" {
                return Err(format!(
                    "column {:?}: merge must be \"allowed\" or \"blocked\"",
                    c.name
                ));
            }
        }
        let mut names: Vec<&str> = self.columns.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        if names.len() != self.columns.len() {
            return Err("duplicate column name".to_string());
        }
        Ok(())
    }
}

/// Parse + validate a board definition document.
pub fn parse_board_def(doc: &str) -> Result<BoardDef, String> {
    let def: BoardDef = toml::from_str(doc).map_err(|e| e.to_string())?;
    def.validate()?;
    Ok(def)
}

/// The board when the repository defines none: one lane per well-known status
/// plus a catch-all. Deterministic like any other projection input.
pub fn default_board() -> BoardDef {
    BoardDef {
        version: 1,
        sort: BoardSort::default(),
        columns: vec![
            BoardColumnDef {
                name: "open".to_string(),
                status: "open".to_string(),
                ..BoardColumnDef::default()
            },
            BoardColumnDef {
                name: "merged".to_string(),
                status: "merged".to_string(),
                ..BoardColumnDef::default()
            },
            BoardColumnDef {
                name: "closed".to_string(),
                status: "closed".to_string(),
                ..BoardColumnDef::default()
            },
            BoardColumnDef {
                name: "other".to_string(),
                ..BoardColumnDef::default()
            },
        ],
    }
}

/// One card: what the thread aggregation says about a work unit, flattened for
/// the three renderers (CLI, endpoint, SPA). Field order here is the JSON wire
/// order — every client serializes this exact struct, which is what makes the
/// byte-equality acceptance test meaningful.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BoardCard {
    pub id: String,
    /// The root entry's `body.title`, "" when it has none.
    pub title: String,
    /// The root entry's human-written prose (issue #112 — the same first
    /// non-empty field walk the thread page renders; "" for machine entries).
    pub prose: String,
    pub actor: String,
    /// Effective work-unit status (see `card_status`).
    pub status: String,
    /// Current owner (agent/human) from the latest `status` entry that names
    /// one; empty when unassigned. This is separate from `actor` (the signing
    /// principal) so a proxy principal can report the real worker.
    pub owner: String,
    /// Worktree name/path from the latest `status` entry that names one.
    pub worktree: String,
    /// Branch from the latest `status` entry that names one.
    pub branch: String,
    /// Current task summary from the latest `status` entry (`work`, falling
    /// back to `note`); inherited until a later status changes or clears it.
    pub work: String,
    pub created_ts: i64,
    pub last_ts: i64,
    pub entries: usize,
    pub verified: usize,
    pub unverified: usize,
    /// Distinct entry kinds in the thread, sorted.
    pub kinds: Vec<String>,
    /// Tip of the parent chain — the `parent` a follow-up entry chains to (the
    /// board page's move posts its `status` entry on it).
    pub last_oid: String,
    /// Merge-rule verdict when the thread has a patch, else `null`.
    pub merge: Option<BoardMergeCard>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BoardMergeCard {
    pub allowed: bool,
    pub reason: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BoardColumn {
    pub name: String,
    pub cards: Vec<BoardCard>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Board {
    pub columns: Vec<BoardColumn>,
}

/// The card's effective status: entries are applied in thread order and the
/// last match wins — every `status` entry sets its `body.status`, a
/// `merge_result` with `merged = true` sets "merged" (a later `status` entry
/// still overrides it); default "open". Deliberately wider than `pr_view`'s
/// open/merged/closed state machine: the board tracks the work unit (`docs/D1_PROTOCOL.md` §5.2
/// names in-progress / needs-review / blocked / needs-human), the PR view
/// tracks merge state.
fn card_status(ordered: &[&EntryRef]) -> String {
    let mut status = "open".to_string();
    for r in ordered {
        match r.entry.kind.as_str() {
            "status" => {
                if let Some(st) = r.entry.body.get("status").and_then(|v| v.as_str()) {
                    status = st.to_string();
                }
            }
            "merge_result"
                if r.entry
                    .body
                    .get("merged")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true) =>
            {
                status = "merged".to_string();
            }
            _ => {}
        }
    }
    status
}

#[derive(Default)]
struct BoardWorkContext {
    owner: String,
    worktree: String,
    branch: String,
    work: String,
}

/// Work context is inherited across status moves: a `needs-review` entry that
/// only sets `status` keeps the owner/worktree/branch/work from the previous
/// status. An explicit empty string clears a field; `work` falls back to the
/// same status entry's `note` when absent.
fn card_work_context(ordered: &[&EntryRef]) -> BoardWorkContext {
    let mut ctx = BoardWorkContext::default();
    for r in ordered {
        if r.entry.kind != "status" {
            continue;
        }
        let body = &r.entry.body;
        for (key, slot) in [
            ("owner", &mut ctx.owner),
            ("worktree", &mut ctx.worktree),
            ("branch", &mut ctx.branch),
        ] {
            if let Some(v) = body.get(key).and_then(serde_json::Value::as_str) {
                *slot = v.to_string();
            }
        }
        if let Some(v) = body.get("work").and_then(serde_json::Value::as_str) {
            ctx.work = v.to_string();
        } else if let Some(v) = body.get("note").and_then(serde_json::Value::as_str) {
            ctx.work = v.to_string();
        }
    }
    ctx
}

/// First-match-wins against the column predicate (`docs/D1_PROTOCOL.md` §7.4/§8).
fn card_matches(card: &BoardCard, col: &BoardColumnDef) -> bool {
    if !col.kind.is_empty() && !card.kinds.iter().any(|k| k == &col.kind) {
        return false;
    }
    if !col.status.is_empty() && card.status != col.status {
        return false;
    }
    if !col.merge.is_empty() {
        let hit = match (&card.merge, col.merge.as_str()) {
            (Some(m), "allowed") => m.allowed,
            (Some(m), "blocked") => !m.allowed,
            _ => false,
        };
        if !hit {
            return false;
        }
    }
    !(col.unverified && card.unverified == 0)
}

fn sort_cards(cards: &mut [BoardCard], sort: BoardSort) {
    cards.sort_by(|a, b| {
        let field = match sort.by {
            BoardSortBy::Ts => a.last_ts.cmp(&b.last_ts),
            BoardSortBy::Id => a.id.cmp(&b.id),
        };
        let field = match sort.direction {
            BoardSortDirection::Asc => field,
            BoardSortDirection::Desc => field.reverse(),
        };
        field.then_with(|| a.id.cmp(&b.id))
    });
}

/// The human-written prose of an entry: the first non-empty of the fields the
/// write paths use (issue #112 — mirrors the SPA thread page's extraction, so
/// the board card and the thread page show the same text).
fn entry_prose(body: &serde_json::Value) -> String {
    ["text", "body", "note", "message", "summary"]
        .iter()
        .find_map(|k| {
            body.get(*k)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

/// The board projection: a pure function `(entries, principals, rules, board
/// definition) → columns`. Threads group by entry `id` exactly as
/// `build_report` does; each becomes at most one card in the first column whose
/// predicate it satisfies, sorted by the definition's sort. Two clients reading
/// the same refs compute byte-identical boards — that property is the e2e
/// acceptance (CLI offline aggregation vs the server endpoint).
pub fn build_board(
    entries: &[&EntryRef],
    principals: &HashMap<String, String, impl std::hash::BuildHasher>,
    rules: &MergeRules,
    board: &BoardDef,
) -> Board {
    let mut by_thread: BTreeMap<&str, Vec<&EntryRef>> = BTreeMap::new();
    for e in entries {
        by_thread.entry(e.entry.id.as_str()).or_default().push(e);
    }
    let mut columns: Vec<(BoardColumnDef, BoardColumn)> = board
        .columns
        .iter()
        .map(|def| {
            (
                def.clone(),
                BoardColumn {
                    name: def.name.clone(),
                    cards: Vec::new(),
                },
            )
        })
        .collect();
    for (id, group) in &by_thread {
        // CI run threads are not work-unit cards (docs/D1_CI_PROTOCOL.md
        // D1-CI §8.3): their surfaces are `walgit ci status`, the report's CI
        // section and the SPA thread badge — on a board they would only ever
        // be untitled "open" cards.
        if group
            .iter()
            .all(|r| r.entry.kind == crate::ci::CI_CLAIM_KIND || r.entry.kind == crate::ci::CI_RESULT_KIND)
        {
            continue;
        }
        let ordered = thread(group);
        let verified = ordered.iter().filter(|r| r.is_verified(principals)).count();
        let mut kinds: Vec<String> = group.iter().map(|r| r.entry.kind.clone()).collect();
        kinds.sort();
        kinds.dedup();
        let root = canonical_root(&ordered, principals);
        let merge = group.iter().any(|r| r.entry.kind == "patch").then(|| {
            let pr = pr_view(group, principals);
            let eval = merge_rule_eval(rules, &pr);
            BoardMergeCard {
                allowed: eval.allowed,
                reason: eval.reason,
            }
        });
        let work = card_work_context(&ordered);
        let card = BoardCard {
            id: (*id).to_string(),
            title: root_title(&ordered, principals),
            prose: root.map_or(String::new(), |r| entry_prose(&r.entry.body)),
            actor: root.map(|r| r.entry.actor.clone()).unwrap_or_default(),
            status: card_status(&ordered),
            owner: work.owner,
            worktree: work.worktree,
            branch: work.branch,
            work: work.work,
            created_ts: root.map_or(0, |r| r.entry.ts),
            last_ts: group.iter().map(|r| r.entry.ts).max().unwrap_or(0),
            entries: group.len(),
            verified,
            unverified: group.len() - verified,
            kinds,
            last_oid: ordered.last().map(|r| r.oid.clone()).unwrap_or_default(),
            merge,
        };
        if let Some((_, col)) = columns.iter_mut().find(|(def, _)| card_matches(&card, def)) {
            col.cards.push(card);
        }
    }
    for (_, col) in &mut columns {
        sort_cards(&mut col.cards, board.sort);
    }
    Board {
        columns: columns.into_iter().map(|(_, col)| col).collect(),
    }
}

#[cfg(test)]
mod board_tests {
    use super::*;
    use std::hash::{Hash, Hasher};

    fn keypair() -> (SigningKey, String) {
        let sk = SigningKey::from_bytes(&[11u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        (sk, base64::engine::general_purpose::STANDARD.encode(pk))
    }

    fn signed(sk: &SigningKey, e: &mut Entry) {
        e.sig = sign_entry(e, sk);
    }

    fn entry(id: &str, kind: &str, actor: &str, parent: &str, ts: i64, body: serde_json::Value) -> Entry {
        Entry {
            version: 1,
            kind: kind.into(),
            id: id.into(),
            actor: actor.into(),
            ts,
            parent: parent.into(),
            refs: None,
            body,
            sig: String::new(),
        }
    }

    /// The content-derived oid `refs_of` assigns (a stand-in for the real blob
    /// sha): permuting the input must not change any card's oid, which feeds
    /// `last_oid`, the thread order tie-break and the merge evaluation.
    fn test_oid(e: &Entry) -> String {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(e).unwrap_or_default().hash(&mut h);
        format!("oid{:016x}", h.finish())
    }

    fn refs_of(entries: &[Entry]) -> Vec<EntryRef> {
        entries
            .iter()
            .map(|e| EntryRef {
                oid: test_oid(e),
                principal: e.actor.clone(),
                entry: e.clone(),
            })
            .collect()
    }

    fn column_of<'a>(board: &'a Board, name: &str) -> &'a BoardColumn {
        board
            .columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    }

    #[test]
    fn board_skips_ci_run_threads() {
        let (_sk, pk) = keypair();
        let mut principals = HashMap::new();
        principals.insert("ci-a".to_string(), pk);
        let claim = entry(
            "ci-deadbeef",
            crate::ci::CI_CLAIM_KIND,
            "ci-a",
            "",
            900,
            serde_json::json!({"task": "build", "ttl": 300, "attempt": 1}),
        );
        let result = entry(
            "ci-deadbeef",
            crate::ci::CI_RESULT_KIND,
            "ci-a",
            &test_oid(&claim),
            950,
            serde_json::json!({"task": "build", "conclusion": "success", "claim": "x"}),
        );
        let issue = entry("pr1", "issue", "alice", "", 900, serde_json::json!({"title": "add thing"}));
        let owned = refs_of(&[claim, result, issue]);
        let refs: Vec<&EntryRef> = owned.iter().collect();
        let board = build_board(&refs, &principals, &MergeRules::default(), &default_board());
        assert!(
            board
                .columns
                .iter()
                .all(|c| c.cards.iter().all(|card| card.id != "ci-deadbeef")),
            "a ci run thread must not become a board card"
        );
        assert!(
            column_of(&board, "open").cards.iter().any(|c| c.id == "pr1"),
            "work units still board"
        );
    }

    #[test]
    fn board_toml_parses_and_validates() {
        let doc = r#"
version = 1

[sort]
by = "ts"
direction = "desc"

[[column]]
name = "review"
status = "needs-review"

[[column]]
name = "mergeable"
kind = "patch"
merge = "allowed"

[[column]]
name = "suspect"
unverified = true

[[column]]
name = "everything else"
"#;
        let def = parse_board_def(doc).expect("parses");
        assert_eq!(def.columns.len(), 4);
        assert_eq!(def.sort.by, BoardSortBy::Ts);
        assert_eq!(def.sort.direction, BoardSortDirection::Desc);
        assert_eq!(def.columns[1].kind, "patch");
        assert!(def.columns[2].unverified);

        // Fails closed on a broken definition — every one of these must error,
        // not silently fold cards into a wrong lane.
        assert!(parse_board_def("version = 2\n[[column]]\nname = \"x\"\n").is_err(), "unknown version");
        assert!(parse_board_def("[sort]\n[[column]]\nname = \"x\"\n").is_err(), "version required");
        assert!(parse_board_def("version = 1\n").is_err(), "no columns");
        assert!(parse_board_def("version = 1\n[[column]]\nname = \"\"\n").is_err(), "empty name");
        assert!(
            parse_board_def("version = 1\n[[column]]\nname = \"a\"\n[[column]]\nname = \"a\"\n").is_err(),
            "duplicate names"
        );
        assert!(
            parse_board_def("version = 1\n[[column]]\nname = \"a\"\nmerge = \"maybe\"\n").is_err(),
            "unknown merge verdict"
        );
        assert!(
            parse_board_def("version = 1\n[[column]]\nname = \"a\"\nstatuz = \"open\"\n").is_err(),
            "a typo must not silently no-op (deny_unknown_fields)"
        );
    }

    #[test]
    fn same_refs_project_to_identical_bytes_regardless_of_read_order() {
        let (sk, pk) = keypair();
        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), pk.clone());
        principals.insert("bob".to_string(), pk);

        // Specified with symbolic keys; parents resolve to the content-derived
        // oids below, exactly like a real thread chains on blob shas.
        let mut patch = entry("t1", "patch", "alice", "a4", 5, serde_json::json!({"message": "the change"}));
        patch.refs = Some(EntryRefs {
            base: Some("refs/heads/main".into()),
            head: Some("refs/heads/topic".into()),
        });
        let specs: Vec<(&str, Entry)> = vec![
            ("a1", entry("t1", "issue", "alice", "", 1, serde_json::json!({"title": "add thing"}))),
            ("a2", entry("t1", "comment", "bob", "a1", 2, serde_json::json!({"text": "looks right"}))),
            ("a3", entry("t1", "status", "alice", "a2", 3, serde_json::json!({"status": "needs-review"}))),
            ("a4", entry("t1", "review", "bob", "a3", 4, serde_json::json!({"decision": "approve"}))),
            ("a5", patch),
            // t2/t3: unsigned entries stay unverified.
            ("b1", entry("t2", "issue", "bob", "", 5, serde_json::json!({"title": "other thing"}))),
            ("c1", entry("t3", "issue", "alice", "", 6, serde_json::json!({"title": "third"}))),
            ("c2", entry("t3", "status", "alice", "c1", 7, serde_json::json!({"status": "closed"}))),
        ];
        // Content addressing, bottom-up like git: sign the entry, resolve its
        // parent to the already-computed oid, then hash the final bytes.
        // Signed: a1 (issue), a3 (status), a4 (bob's approve review — the
        // human approval the merge rule needs), a5 (patch). a2 stays unsigned
        // so the thread carries an unverified entry.
        let mut oid_of: HashMap<String, String> = HashMap::new();
        let mut es: Vec<Entry> = Vec::new();
        for (k, mut e) in specs {
            // Parent first: the signature covers the entry's final bytes,
            // parent pointer included.
            if let Some(p) = oid_of.get(&e.parent) {
                let p = p.clone();
                e.parent = p;
            } else if !e.parent.is_empty() {
                panic!("spec parent unknown");
            }
            if matches!(k, "a1" | "a3" | "a4" | "a5") {
                let sig = sign_entry(&mut e, &sk);
                e.sig = sig;
            }
            oid_of.insert(k.to_string(), test_oid(&e));
            es.push(e);
        }
        let rules = MergeRules {
            protect: vec!["refs/heads/main".into()],
            require_human_approvals: 1,
        };
        let board_def = parse_board_def(
            "version = 1\n[[column]]\nname = \"review\"\nstatus = \"needs-review\"\n\
             [[column]]\nname = \"done\"\nstatus = \"closed\"\n\
             [[column]]\nname = \"suspect\"\nunverified = true\n\
             [[column]]\nname = \"mergeable\"\nkind = \"patch\"\nmerge = \"allowed\"\n\
             [[column]]\nname = \"everything else\"\n",
        )
        .expect("board def");

        let project = |list: &[Entry]| {
            let refs = refs_of(list);
            let borrowed: Vec<&EntryRef> = refs.iter().collect();
            serde_json::to_vec(&build_board(&borrowed, &principals, &rules, &board_def))
                .expect("serialize")
        };
        let bytes = project(&es);
        // Same input, again: identical bytes.
        assert_eq!(bytes, project(&es));
        // Refs read in a different order (fetch/clone timing, page boundaries):
        // still byte-identical — the projection is a function of the set.
        let mut permuted = es.clone();
        permuted.reverse();
        assert_eq!(bytes, project(&permuted));

        let board: Board = serde_json::from_slice(&bytes).expect("board json");
        let t1 = &column_of(&board, "review").cards[0];
        // First match wins: t1 (needs-review AND patch-mergeable AND mostly
        // verified) lands in "review", not in a later matching lane.
        assert_eq!(column_of(&board, "review").cards.len(), 1);
        assert_eq!(t1.id, "t1");
        assert_eq!(t1.status, "needs-review");
        assert_eq!(t1.last_oid, oid_of["a5"], "tip of the parent chain");
        assert_eq!(t1.verified, 4);
        assert_eq!(t1.unverified, 1, "bob's unsigned comment");
        assert_eq!(t1.actor, "alice");
        assert_eq!(t1.created_ts, 1);
        assert_eq!(t1.last_ts, 5, "max ts over the thread");
        assert!(t1.merge.as_ref().expect("patch verdict").allowed);
        assert_eq!(column_of(&board, "done").cards[0].id, "t3");
        assert_eq!(column_of(&board, "suspect").cards[0].id, "t2");
        assert_eq!(column_of(&board, "suspect").cards[0].unverified, 1);
        assert!(column_of(&board, "mergeable").cards.is_empty(), "first match wins");
        assert!(column_of(&board, "everything else").cards.is_empty());
    }

    #[test]
    fn status_entries_move_cards_and_the_default_board_covers_every_status() {
        let (sk, pk) = keypair();
        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), pk);
        let mut es = vec![
            entry("t1", "issue", "alice", "", 1, serde_json::json!({"title": "move me"})),
            entry("t1", "status", "alice", "a1", 2, serde_json::json!({"status": "in-progress"})),
        ];
        signed(&sk, &mut es[0]);
        signed(&sk, &mut es[1]);
        let refs = refs_of(&es);
        let borrowed: Vec<&EntryRef> = refs.iter().collect();
        let rules = MergeRules::default();
        let def = default_board();
        def.validate().expect("default board is valid");

        let board = build_board(&borrowed, &principals, &rules, &def);
        assert_eq!(column_of(&board, "open").cards.len(), 0, "status moved it out of open");
        assert_eq!(
            column_of(&board, "other").cards.len(),
            1,
            "statuses without a lane fall to the catch-all"
        );
        assert_eq!(column_of(&board, "other").cards[0].status, "in-progress");

        // The move: one more signed `status` entry — the only write the board
        // ever needs — and the card is in "merged" for the next projection.
        let mut done = entry("t1", "status", "alice", "a2", 3, serde_json::json!({"status": "merged"}));
        signed(&sk, &mut done);
        let mut es2 = es.clone();
        es2.push(done);
        let refs2 = refs_of(&es2);
        let borrowed2: Vec<&EntryRef> = refs2.iter().collect();
        let board2 = build_board(&borrowed2, &principals, &rules, &def);
        assert_eq!(column_of(&board2, "merged").cards.len(), 1);
        assert_eq!(column_of(&board2, "merged").cards[0].id, "t1");
        // Newest first within a column; the id breaks ties. Both threads sit
        // in the catch-all (neither is "open"), both last active at ts 2.
        let mut tie = es.clone();
        let mut t9_issue = entry("t9", "issue", "alice", "", 1, serde_json::json!({"title": "tie"}));
        signed(&sk, &mut t9_issue);
        let mut t9_status = entry("t9", "status", "alice", "", 2, serde_json::json!({"status": "in-progress"}));
        signed(&sk, &mut t9_status);
        tie.push(t9_issue);
        tie.push(t9_status);
        let refs3 = refs_of(&tie);
        let borrowed3: Vec<&EntryRef> = refs3.iter().collect();
        let board3 = build_board(&borrowed3, &principals, &rules, &def);
        let other_col: Vec<&str> = column_of(&board3, "other").cards.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(other_col, vec!["t1", "t9"], "equal ts: id ascending");
    }

    #[test]
    fn board_card_carries_and_inherits_work_context() {
        let principals = HashMap::new();
        let mut entries = vec![
            entry("t1", "issue", "alice", "", 1, serde_json::json!({"title": "owned"})),
            entry(
                "t1",
                "status",
                "alice",
                "a1",
                2,
                serde_json::json!({
                    "status": "in-progress",
                    "owner": "agent-a",
                    "worktree": "wt-a",
                    "branch": "feat/a",
                    "work": "first"
                }),
            ),
            entry(
                "t1",
                "status",
                "alice",
                "a2",
                3,
                serde_json::json!({
                    "status": "needs-review",
                    "owner": "agent-b",
                    "work": "reviewing"
                }),
            ),
        ];
        let refs = refs_of(&entries);
        let borrowed: Vec<&EntryRef> = refs.iter().collect();
        let board = build_board(&borrowed, &principals, &MergeRules::default(), &default_board());
        let card = &column_of(&board, "other").cards[0];
        assert_eq!(card.owner, "agent-b");
        assert_eq!(card.worktree, "wt-a", "worktree inherits across status moves");
        assert_eq!(card.branch, "feat/a", "branch inherits across status moves");
        assert_eq!(card.work, "reviewing");

        let clear = entry(
            "t1",
            "status",
            "alice",
            "a3",
            4,
            serde_json::json!({
                "status": "in-progress",
                "owner": "",
                "worktree": "",
                "branch": "",
                "note": "fixing"
            }),
        );
        entries.push(clear);
        let refs = refs_of(&entries);
        let borrowed: Vec<&EntryRef> = refs.iter().collect();
        let board = build_board(&borrowed, &principals, &MergeRules::default(), &default_board());
        let card = &column_of(&board, "other").cards[0];
        assert_eq!(card.owner, "");
        assert_eq!(card.worktree, "");
        assert_eq!(card.branch, "");
        assert_eq!(card.work, "fixing", "note is the work fallback");
    }

    #[test]
    fn cards_matching_no_column_are_not_on_the_board() {
        let principals = HashMap::new();
        let refs = refs_of(&[entry("t1", "issue", "alice", "", 1, serde_json::json!({"title": "x"}))]);
        let borrowed: Vec<&EntryRef> = refs.iter().collect();
        let def = parse_board_def("version = 1\n[[column]]\nname = \"only blocked\"\nstatus = \"blocked\"\n").expect("def");
        let board = build_board(&borrowed, &principals, &MergeRules::default(), &def);
        assert!(board.columns.len() == 1 && board.columns[0].cards.is_empty());
    }

    /// Issue #131: the report's thread and PR projections carry the root
    /// entry's title with the one board rule — titled, untitled, and a title
    /// that lives only on a child (then: no title) all read the same way.
    #[test]
    fn report_threads_and_prs_project_the_root_title() {
        let principals = HashMap::new();
        let mut patch = entry(
            "pr-1",
            "patch",
            "alice",
            "",
            2,
            serde_json::json!({"title": "child title"}),
        );
        patch.refs = Some(EntryRefs {
            base: Some("refs/heads/main".into()),
            head: Some("refs/heads/topic".into()),
        });
        let owned = refs_of(&[
            entry(
                "pr-1",
                "issue",
                "alice",
                "",
                1,
                serde_json::json!({"title": "ship the report"}),
            ),
            patch,
            entry("ci-9", "comment", "bob", "", 3, serde_json::json!({"text": "no title here"})),
            entry("solo-7", "comment", "bob", "", 4, serde_json::json!({"text": "root has no title"})),
            entry("solo-7", "comment", "bob", "", 5, serde_json::json!({"title": "title only below the root"})),
        ]);
        let refs: Vec<&EntryRef> = owned.iter().collect();
        let report = build_report(&refs, &principals, &MergeRules::default(), 10);

        let title_of = |id: &str| {
            report
                .threads
                .iter()
                .find(|t| t.id == id)
                .unwrap_or_else(|| panic!("thread {id} in {:?}", report.threads))
                .title
                .clone()
        };
        assert_eq!(title_of("pr-1"), "ship the report", "root title wins over a child's");
        assert_eq!(title_of("ci-9"), "", "untitled thread is \"\"");
        assert_eq!(title_of("solo-7"), "", "a title below the root is not the thread's");
        assert_eq!(report.prs.len(), 1);
        assert_eq!(report.prs[0].id, "pr-1");
        assert_eq!(
            report.prs[0].title, "ship the report",
            "the PR projection carries the same root title"
        );
        // One rule for all three projections: the board card agrees.
        let board = build_board(&refs, &principals, &MergeRules::default(), &default_board());
        let card = board
            .columns
            .iter()
            .flat_map(|c| &c.cards)
            .find(|c| c.id == "pr-1")
            .expect("pr-1 card");
        assert_eq!(card.title, "ship the report");
        // Missing root (empty thread): "" — never a panic.
        assert_eq!(root_title(&[], &HashMap::new()), "");
    }

    /// Issue #131 follow-up: the projection owns the order — threads newest
    /// activity first (id ascending breaks ties), PRs `open` before `merged`
    /// before `closed`, then newest activity first. Clients render arrival
    /// order, so CLI and API agree byte-for-byte.
    #[test]
    fn report_lists_arrive_ordered_from_the_projection() {
        let principals = HashMap::new();
        let mk_patch = |id: &str, ts: i64| {
            let mut p = entry(id, "patch", "alice", "", ts, serde_json::json!({"title": id}));
            p.refs = Some(EntryRefs {
                base: Some("refs/heads/main".into()),
                head: Some(format!("refs/heads/{id}")),
            });
            p
        };
        let entries = vec![
            entry("t-mid", "comment", "bob", "", 20, serde_json::json!({})),
            entry("t-late", "comment", "bob", "", 30, serde_json::json!({})),
            entry("t-tie-b", "comment", "bob", "", 10, serde_json::json!({})),
            entry("t-tie-a", "comment", "bob", "", 10, serde_json::json!({})),
            mk_patch("pr-merged", 50),
            entry(
                "pr-merged",
                "status",
                "alice",
                "",
                51,
                serde_json::json!({"status": "merged"}),
            ),
            mk_patch("pr-cold", 100),
            entry(
                "pr-cold",
                "status",
                "alice",
                "",
                101,
                serde_json::json!({"status": "closed"}),
            ),
            mk_patch("pr-open-old", 1),
            mk_patch("pr-open-new", 5),
        ];
        let owned = refs_of(&entries);
        let refs: Vec<&EntryRef> = owned.iter().collect();
        let report = build_report(&refs, &principals, &MergeRules::default(), 200);
        // Every thread is in the threads list (the PR threads are threads too):
        // pr-cold's last activity is ts 101 (the status entry), pr-merged 51.
        let thread_ids: Vec<&str> = report.threads.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            thread_ids,
            [
                "pr-cold",
                "pr-merged",
                "t-late",
                "t-mid",
                "t-tie-a",
                "t-tie-b",
                "pr-open-new",
                "pr-open-old",
            ],
            "last_ts descending, id ascending on ties"
        );
        let pr_ids: Vec<&str> = report.prs.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            pr_ids,
            ["pr-open-new", "pr-open-old", "pr-merged", "pr-cold"],
            "open before merged before closed, then newest activity"
        );
    }

    /// A chain built in chain order: each entry's `parent` is the previous
    /// entry's content-derived oid. `descending_ts` makes the root the newest
    /// entry, so the sort key `(ts, actor, oid)` is the exact reverse of the
    /// parent chain — the input class the old shrinking guard truncated.
    fn chain(n: usize, descending_ts: bool) -> Vec<EntryRef> {
        let mut out: Vec<EntryRef> = Vec::new();
        let mut parent = String::new();
        for i in 0..n {
            let ts = if descending_ts { (n - i) as i64 } else { i as i64 };
            let e = entry("t-chain", "comment", "alice", &parent, ts, serde_json::json!({}));
            let oid = test_oid(&e);
            out.push(EntryRef {
                oid: oid.clone(),
                principal: "alice".into(),
                entry: e,
            });
            parent = oid;
        }
        out
    }

    /// cc-ai-d1-protocol-followups P0: the guard evaluated against the
    /// shrinking pending list truncated at n>=7 and appended the tail in
    /// sort order, reversing the chain. The progress-based loop resolves it.
    #[test]
    fn thread_resolves_descending_ts_chains_without_truncation() {
        for n in [6usize, 7, 10, 12] {
            let refs = chain(n, true);
            let borrowed: Vec<&EntryRef> = refs.iter().collect();
            let ordered = thread(&borrowed);
            let got: Vec<&str> = ordered.iter().map(|e| e.oid.as_str()).collect();
            let want: Vec<&str> = refs.iter().map(|e| e.oid.as_str()).collect();
            assert_eq!(got, want, "n={n}: descending-ts chain must stay in parent order");
            assert_eq!(
                ordered.last().map(|e| e.oid.as_str()),
                Some(refs.last().expect("chain is non-empty").oid.as_str()),
                "n={n}: the chain tip is last"
            );
        }
    }

    /// Under the old guard this appended child reordered an already-emitted
    /// entry to the tail; the fixed order is the pure chain order and the
    /// child stays the tip.
    #[test]
    fn thread_keeps_chain_order_for_entries_appended_after_a_long_chain() {
        let refs = chain(10, true);
        let tip = refs.last().expect("chain is non-empty").oid.clone();
        let follow = entry(
            "t-chain",
            "status",
            "alice",
            &tip,
            1,
            serde_json::json!({"status": "needs-review"}),
        );
        let follow_oid = test_oid(&follow);
        let mut all = refs.clone();
        all.push(EntryRef {
            oid: follow_oid,
            principal: "alice".into(),
            entry: follow,
        });
        let borrowed: Vec<&EntryRef> = all.iter().collect();
        let ordered = thread(&borrowed);
        let got: Vec<&str> = ordered.iter().map(|e| e.oid.as_str()).collect();
        let want: Vec<&str> = all.iter().map(|e| e.oid.as_str()).collect();
        assert_eq!(got, want);
        assert_eq!(ordered.last().expect("non-empty").entry.kind, "status");
    }

    /// Identity comes from the canonical root: the first *verified* root wins
    /// over an unverified backdated one; with no verified root the legacy
    /// first-entry rule still applies (unsigned repos keep working).
    #[test]
    fn board_identity_prefers_a_verified_root() {
        let (sk, pk) = keypair();
        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), pk);
        let mut real = entry(
            "t1",
            "issue",
            "alice",
            "",
            10,
            serde_json::json!({"title": "real", "body": "real body"}),
        );
        signed(&sk, &mut real);
        let hijack = entry(
            "t1",
            "issue",
            "mallory",
            "",
            1,
            serde_json::json!({"title": "hijack", "body": "x"}),
        );
        let owned = refs_of(&[real, hijack]);
        let refs: Vec<&EntryRef> = owned.iter().collect();
        let board = build_board(&refs, &principals, &MergeRules::default(), &default_board());
        let card = board
            .columns
            .iter()
            .flat_map(|c| &c.cards)
            .find(|c| c.id == "t1")
            .expect("card");
        assert_eq!(card.title, "real");
        assert_eq!(card.actor, "alice");
        assert_eq!(card.prose, "real body");

        // No verified root: the legacy first-entry identity is preserved.
        let legacy = refs_of(&[
            entry("t2", "issue", "alice", "", 10, serde_json::json!({"title": "late"})),
            entry("t2", "issue", "bob", "", 1, serde_json::json!({"title": "early"})),
        ]);
        let refs: Vec<&EntryRef> = legacy.iter().collect();
        let board = build_board(&refs, &principals, &MergeRules::default(), &default_board());
        let card = board
            .columns
            .iter()
            .flat_map(|c| &c.cards)
            .find(|c| c.id == "t2")
            .expect("card");
        assert_eq!(card.title, "early");
        assert_eq!(card.actor, "bob");
    }
}

#[cfg(test)]
mod snapshot_tests {
    //! D45 / `docs/D1_PROTOCOL.md` §9: the fold. A snapshot carries every folded entry
    //! verbatim (oid + inbox principal + raw signed bytes); aggregation over
    //! `snapshot ∪ tail` must be byte-identical to aggregation over the
    //! unfolded inbox — that equality is the acceptance property.
    use super::*;

    fn keypair(seed: u8) -> (SigningKey, String) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let pk = sk.verifying_key().to_bytes();
        (sk, base64::engine::general_purpose::STANDARD.encode(pk))
    }

    /// Serialize an entry the two write paths use (pretty like the CLI,
    /// compact like the thin API) and derive the record the fold would write:
    /// the oid is the git blob id of the exact bytes.
    fn record(e: &Entry, principal: &str, pretty: bool) -> SnapshotRecord {
        let json = if pretty {
            serde_json::to_string_pretty(e).unwrap()
        } else {
            serde_json::to_string(e).unwrap()
        };
        SnapshotRecord {
            oid: git_blob_oid(json.as_bytes(), ObjectFormat::Sha1),
            principal: principal.to_string(),
            json,
        }
    }

    fn entry(id: &str, kind: &str, actor: &str, parent: &str, ts: i64, body: serde_json::Value) -> Entry {
        Entry {
            version: 1,
            kind: kind.into(),
            id: id.into(),
            actor: actor.into(),
            ts,
            parent: parent.into(),
            refs: None,
            body,
            sig: String::new(),
        }
    }

    fn signed(sk: &SigningKey, mut e: Entry) -> Entry {
        e.sig = sign_entry(&mut e, sk);
        e
    }

    /// The aggregation fingerprint: every projection's bytes over the same
    /// input set. Pre/post fold, this fingerprint must not move.
    fn fingerprint(refs: &[&EntryRef], principals: &HashMap<String, String>) -> Vec<u8> {
        let rules = MergeRules {
            protect: vec!["refs/heads/main".into()],
            require_human_approvals: 1,
        };
        let mut out = serde_json::to_vec(&build_report(refs, principals, &rules, i64::MAX)).unwrap();
        out.extend_from_slice(&serde_json::to_vec(&build_board(refs, principals, &rules, &default_board())).unwrap());
        let mut ids: Vec<&str> = refs.iter().map(|r| r.entry.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            let group: Vec<&EntryRef> = refs.iter().filter(|r| r.entry.id == id).copied().collect();
            for r in thread(&group) {
                out.extend_from_slice(
                    serde_json::to_string(&serde_json::json!({
                        "oid": r.oid,
                        "principal": r.principal,
                        "verified": r.is_verified(principals),
                        "entry": r.entry,
                    }))
                    .unwrap()
                    .as_bytes(),
                );
            }
            if group.iter().any(|r| r.entry.kind == "patch") {
                out.extend_from_slice(&serde_json::to_vec(&pr_view(&group, principals)).unwrap());
            }
        }
        out
    }

    /// A realistic mixed history: signed + unsigned entries, issue/patch/
    /// review/status/comment + `ci_claim`/`ci_result`, cross-thread references,
    /// entries written by both write paths (pretty and compact bytes).
    /// Parents chain on real blob oids, bottom-up like git, and the signature
    /// covers the final bytes — parent pointers must survive the fold.
    fn history() -> (Vec<SnapshotRecord>, HashMap<String, String>) {
        let (alice_sk, alice_pk) = keypair(7);
        let (bob_sk, bob_pk) = keypair(8);
        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), alice_pk);
        principals.insert("bob".to_string(), bob_pk.clone());
        principals.insert("ci-runner-a".to_string(), bob_pk);

        let mut records: Vec<SnapshotRecord> = Vec::new();
        // Append an entry to the history: resolve `parent` is already an oid
        // (or ""), sign unless told not to, derive the record's oid from the
        // exact stored bytes. Returns the oid for chaining.
        let mut push = |principal: &str,
                        pretty: bool,
                        sign_with: Option<&SigningKey>,
                        e: Entry|
         -> String {
            let mut e = e;
            if let Some(sk) = sign_with {
                e.sig = sign_entry(&mut e, sk);
            }
            let rec = record(&e, principal, pretty);
            let oid = rec.oid.clone();
            records.push(rec);
            oid
        };

        // pr1: issue -> comment -> status(needs-review) -> review(approve) ->
        // patch -> an unsigned comment by the unregistered carol.
        let o1 = push("alice", true, Some(&alice_sk), entry("pr1", "issue", "alice", "", 1, serde_json::json!({"title": "add thing"})));
        let o2 = push("bob", true, Some(&bob_sk), entry("pr1", "comment", "bob", &o1, 2, serde_json::json!({"text": "looks right"})));
        let o3 = push("alice", false, Some(&alice_sk), entry("pr1", "status", "alice", &o2, 3, serde_json::json!({"status": "needs-review", "owner": "svc-a"})));
        let o4 = push("bob", true, Some(&bob_sk), entry("pr1", "review", "bob", &o3, 4, serde_json::json!({"decision": "approve"})));
        let mut patch = entry("pr1", "patch", "alice", &o4, 5, serde_json::json!({"message": "the change"}));
        patch.refs = Some(EntryRefs {
            base: Some("refs/heads/main".into()),
            head: Some("refs/heads/topic".into()),
        });
        let o5 = push("alice", true, Some(&alice_sk), patch);
        let _o6 = push("carol", true, None, entry("pr1", "comment", "carol", &o5, 6, serde_json::json!({"text": "unsigned"})));
        // t2: references pr1's comment via `related` (issue #75 ③) and closes.
        let t2 = push("alice", true, None, entry("t2", "issue", "alice", "", 7, serde_json::json!({"title": "second", "related": [o2]})));
        let _t2s = push("alice", true, Some(&alice_sk), entry("t2", "status", "alice", &t2, 8, serde_json::json!({"status": "closed"})));
        // A CI run thread (docs/D1_CI_PROTOCOL.md): claim -> result.
        let claim = push(
            "ci-runner-a",
            true,
            Some(&bob_sk),
            entry(
                "ci-deadbeef",
                crate::ci::CI_CLAIM_KIND,
                "ci-runner-a",
                "",
                40,
                serde_json::json!({"task": "test", "ref": "refs/heads/main", "commit": "c0ffee", "ttl": 300, "attempt": 1}),
            ),
        );
        let _result = push(
            "ci-runner-a",
            true,
            Some(&bob_sk),
            entry(
                "ci-deadbeef",
                crate::ci::CI_RESULT_KIND,
                "ci-runner-a",
                &claim,
                41,
                serde_json::json!({"task": "test", "conclusion": "success", "claim": claim, "exit_code": 0, "duration_ms": 5, "log_summary": "ok", "log_sha256": ""}),
            ),
        );
        (records, principals)
    }

    #[test]
    fn git_blob_oid_matches_git_known_answers() {
        // `printf <bytes> | git hash-object --stdin`
        assert_eq!(
            git_blob_oid(b"hello", ObjectFormat::Sha1),
            "b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0"
        );
        assert_eq!(
            git_blob_oid(b"", ObjectFormat::Sha1),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            git_blob_oid(b"{\"a\":1}", ObjectFormat::Sha1),
            "daa5053ecf5f9a37b2de733d0751cc1ab53ac010"
        );
        assert_eq!(
            git_blob_oid(b"hello", ObjectFormat::Sha256),
            "8aec4e4876f854f688d0ebfc8f37598f38e5fd6903cccc850ca36591175aeb60"
        );
    }

    #[test]
    fn fold_and_snapshot_plus_tail_are_byte_identical_to_the_unfolded_inbox() {
        let (records, principals) = history();
        let unfolded: Vec<EntryRef> = records
            .iter()
            .filter_map(|record| record.entry_ref(ObjectFormat::Sha1))
            .collect();
        let before: Vec<&EntryRef> = unfolded.iter().collect();
        let fp_before = fingerprint(&before, &principals);

        let (sk, _) = keypair(9);
        // Full fold: every entry into the snapshot, empty tail.
        let snap = build_snapshot("alice", 100, records.clone(), &sk);
        let parsed = parse_snapshot(&serde_json::to_vec(&snap).unwrap()).expect("snapshot parses");
        let folded: Vec<EntryRef> = parsed
            .entries
            .iter()
            .filter_map(|record| record.entry_ref(ObjectFormat::Sha1))
            .collect();
        let after: Vec<&EntryRef> = folded.iter().collect();
        assert_eq!(
            fp_before,
            fingerprint(&after, &principals),
            "a full fold must not move any projection (incl. verified flags)"
        );

        // Partial fold: snapshot covers a prefix of the set, the rest is tail —
        // the union dedups by oid and the answer is the same set.
        let cut = records.len() / 2;
        let snap = build_snapshot("alice", 100, records[..cut].to_vec(), &sk);
        let parsed = parse_snapshot(&serde_json::to_vec(&snap).unwrap()).expect("snapshot parses");
        let mut union: Vec<EntryRef> = parsed
            .entries
            .iter()
            .filter_map(|record| record.entry_ref(ObjectFormat::Sha1))
            .collect();
        let mut seen: std::collections::HashSet<String> = union.iter().map(|e| e.oid.clone()).collect();
        for r in &unfolded {
            if seen.insert(r.oid.clone()) {
                union.push(r.clone());
            }
        }
        let after: Vec<&EntryRef> = union.iter().collect();
        assert_eq!(fp_before, fingerprint(&after, &principals), "snapshot ∪ tail == unfolded");

        // The fold is a pure function of the set: same records, any input
        // order, produce the same snapshot bytes.
        let mut permuted = records.clone();
        permuted.reverse();
        let a = serde_json::to_vec(&build_snapshot("alice", 100, records, &sk)).unwrap();
        let b = serde_json::to_vec(&build_snapshot("alice", 100, permuted, &sk)).unwrap();
        assert_eq!(a, b, "the fold is deterministic");
    }

    #[test]
    fn snapshot_records_with_lying_oids_or_unparseable_bytes_are_skipped() {
        let (records, _) = history();
        let good = records[0].clone();
        let lying = SnapshotRecord {
            oid: "0".repeat(40),
            ..good.clone()
        };
        let garbage = SnapshotRecord {
            oid: git_blob_oid(b"not json", ObjectFormat::Sha1),
            principal: good.principal.clone(),
            json: "not json".to_string(),
        };
        assert!(good.entry_ref(ObjectFormat::Sha1).is_some());
        assert!(
            lying.entry_ref(ObjectFormat::Sha1).is_none(),
            "oid must recompute from the bytes"
        );
        assert!(
            garbage.entry_ref(ObjectFormat::Sha1).is_none(),
            "unparseable entries skip like corrupt inbox blobs"
        );
    }

    #[test]
    fn snapshot_document_fails_closed_on_wrong_version_or_kind() {
        let (sk, _) = keypair(9);
        let snap = build_snapshot("alice", 1, vec![], &sk);
        let mut doc = serde_json::to_value(&snap).unwrap();
        doc["version"] = serde_json::json!(2);
        assert!(parse_snapshot(doc.to_string().as_bytes()).is_err(), "unknown version");
        doc["version"] = serde_json::json!(1);
        doc["kind"] = serde_json::json!("something_else");
        assert!(parse_snapshot(doc.to_string().as_bytes()).is_err(), "unknown kind");
        assert!(parse_snapshot(b"{{{{").is_err(), "not json at all");
    }

    #[test]
    fn snapshot_signature_verifies_against_the_folder_key() {
        let (sk, pk) = keypair(9);
        let (records, _) = history();
        let snap = build_snapshot("alice", 42, records, &sk);
        assert!(verify_snapshot(&snap, &pk).is_ok());
        let mut tampered = snap.clone();
        tampered.actor = "mallory".into();
        assert!(verify_snapshot(&tampered, &pk).is_err());
        let (other_sk, _) = keypair(10);
        let signed_by_other = build_snapshot("alice", 42, snap.entries.clone(), &other_sk);
        assert!(verify_snapshot(&signed_by_other, &pk).is_err(), "wrong key");
    }

    #[test]
    fn entry_set_dedups_by_oid_and_prefers_the_legitimate_inbox() {
        let (sk, pk) = keypair(7);
        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), pk);
        let e = signed(&sk, entry("t", "issue", "alice", "", 1, serde_json::json!({"title": "x"})));
        let legit = record(&e, "alice", true);
        let planted = SnapshotRecord {
            principal: "bob".into(),
            ..legit.clone()
        };
        // Whichever order the refs were read in, the set holds the entry once,
        // attributed to its own actor's inbox (so it verifies).
        for order in [[legit.clone(), planted.clone()], [planted, legit]] {
            let mut set = EntrySet::new();
            for r in order {
                set.insert(r.entry_ref(ObjectFormat::Sha1).unwrap());
            }
            let entries = set.into_entries();
            assert_eq!(entries.len(), 1, "same oid = one entry");
            assert_eq!(entries[0].principal, "alice");
            assert!(entries[0].is_verified(&principals));
        }
    }

    /// The complete/dropped marker round-trips without breaking legacy
    /// snapshots: a complete fold serializes exactly like a pre-marker one
    /// (the fields are omitted), and a truncated fold is distinguishable and
    /// still verifies (the marker is signed with the fold).
    #[test]
    fn snapshot_completeness_marker_round_trips() {
        let (sk, pk) = keypair(9);
        let (records, _) = history();
        let complete = build_snapshot("alice", 42, records.clone(), &sk);
        let text = serde_json::to_string(&complete).expect("serialize");
        assert!(!text.contains("\"complete\""), "complete is the implicit default");
        assert!(!text.contains("\"dropped_entries\""));
        let parsed = parse_snapshot(text.as_bytes()).expect("parses");
        assert!(parsed.complete);
        assert_eq!(parsed.dropped_entries, 0);
        assert!(verify_snapshot(&parsed, &pk).is_ok());

        let truncated = build_snapshot_truncated("alice", 43, records[..1].to_vec(), 7, &sk);
        let text = serde_json::to_string(&truncated).expect("serialize");
        assert!(text.contains("\"complete\":false"));
        assert!(text.contains("\"dropped_entries\":7"));
        let parsed = parse_snapshot(text.as_bytes()).expect("parses");
        assert!(!parsed.complete);
        assert_eq!(parsed.dropped_entries, 7);
        assert!(verify_snapshot(&parsed, &pk).is_ok(), "the marker is signed with the fold");
    }
}

#[cfg(test)]
mod transition_tests {
    use super::*;

    fn entry(kind: &str, id: &str, actor: &str, oid: &str, ts: i64, body: serde_json::Value) -> EntryRef {
        EntryRef {
            oid: oid.to_string(),
            principal: actor.to_string(),
            entry: Entry {
                version: 1,
                kind: kind.to_string(),
                id: id.to_string(),
                actor: actor.to_string(),
                ts,
                parent: String::new(),
                refs: None,
                body,
                sig: String::new(),
            },
        }
    }

    fn make_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[42u8; 32])
    }

    fn signed_entry(key: &ed25519_dalek::SigningKey, kind: &str, id: &str, actor: &str, oid: &str, ts: i64, body: serde_json::Value) -> EntryRef {
        let mut e = Entry {
            version: 1,
            kind: kind.to_string(),
            id: id.to_string(),
            actor: actor.to_string(),
            ts,
            parent: String::new(),
            refs: None,
            body,
            sig: String::new(),
        };
        e.sig = sign_entry(&mut e, key);
        EntryRef { oid: oid.to_string(), principal: actor.to_string(), entry: e }
    }

    #[test]
    fn done_requires_needs_review_and_verified_approve() {
        let key = make_key();
        let pub_b64 = base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes());
        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), pub_b64);

        // thread: issue -> review(approve) -> status(needs-review)
        let e1 = signed_entry(&key, "issue", "t1", "alice", "e1", 1, serde_json::json!({"title": "x"}));
        let e3 = signed_entry(&key, "review", "t1", "alice", "e3", 3, serde_json::json!({"decision": "approve"}));
        let e4 = signed_entry(&key, "status", "t1", "alice", "e4", 4, serde_json::json!({"status": "needs-review"}));
        let entries = [e1, e3, e4];
        let refs: Vec<&EntryRef> = entries.iter().collect();
        assert!(validate_status_transition(&refs, &principals).is_ok());

        // open -> done: no needs-review prerequisite -> reject
        let entries_open = [
            entry("issue", "t2", "alice", "f1", 1, serde_json::json!({"title": "x"})),
        ];
        let refs2: Vec<&EntryRef> = entries_open.iter().collect();
        assert!(validate_status_transition(&refs2, &principals).is_err());
    }

    #[test]
    fn multi_status_thread_needs_thread_ordering_before_the_verdict() {
        // issue #104 观察 A:card_status 按给定顺序重放。真实链序(ts)以
        // in-progress 结尾,但 refs 收集顺序可能把 needs-review 排最后——
        // 不先 thread() 就会误放行 done。本测试锁「thread() 先行」的语义:
        // 危险顺序下旧行为误判放行,thread() 后按链序正确拒绝。
        let key = make_key();
        let pub_b64 = base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes());
        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), pub_b64);

        let issue = signed_entry(&key, "issue", "t9", "alice", "i1", 1, serde_json::json!({"title": "x"}));
        let approve = signed_entry(&key, "review", "t9", "alice", "a1", 3, serde_json::json!({"decision": "approve"}));
        let in_progress = signed_entry(&key, "status", "t9", "alice", "s2", 4, serde_json::json!({"status": "in-progress"}));
        let needs_review = signed_entry(&key, "status", "t9", "alice", "s1", 2, serde_json::json!({"status": "needs-review"}));

        // 反收集顺序:needs-review 排最后——不 thread() 的旧行为会误判当前
        // 状态为 needs-review 且有 verified approve → 错误放行。
        let collected = [&issue, &approve, &in_progress, &needs_review];
        assert!(
            validate_status_transition(&collected, &principals).is_ok(),
            "危险顺序构造无效:该顺序下不 thread() 应误判放行"
        );
        let ordered = thread(&collected);
        assert!(
            validate_status_transition(&ordered, &principals).is_err(),
            "thread() 后按链序判定:当前 in-progress,拒绝 done"
        );
    }

    #[test]
    fn done_requires_verified_approve() {
        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), "fake".to_string());

        let entries = [
            entry("issue", "t3", "alice", "g1", 1, serde_json::json!({"title": "x"})),
            entry("status", "t3", "alice", "g2", 2, serde_json::json!({"status": "needs-review"})),
        ];
        let refs: Vec<&EntryRef> = entries.iter().collect();
        assert!(validate_status_transition(&refs, &principals).is_err());
    }

    #[test]
    fn card_prose_is_the_thread_page_field_walk() {
        // issue #112: the card's prose is the first non-empty of
        // text/body/note/message/summary — the same walk the SPA renders.
        assert_eq!(entry_prose(&serde_json::json!({})), "");
        assert_eq!(entry_prose(&serde_json::json!({"text": "web"})), "web");
        assert_eq!(
            entry_prose(&serde_json::json!({"text": "", "body": "cli"})),
            "cli"
        );
        assert_eq!(
            entry_prose(&serde_json::json!({"text": "", "body": "", "note": "rev"})),
            "rev"
        );
        assert_eq!(
            entry_prose(&serde_json::json!({"message": "patch", "summary": "sum"})),
            "patch"
        );
        assert_eq!(
            entry_prose(&serde_json::json!({"text": "  trimmed  "})),
            "trimmed"
        );
    }

    /// The merge rule counts distinct non-`svc-` approvers and never the
    /// patch's own author.
    #[test]
    fn merge_rule_counts_distinct_non_author_approvers() {
        let mut pr = pr_view(&[], &HashMap::new());
        pr.base = Some("refs/heads/main".into());
        pr.authors = vec!["author".into()];
        pr.human_approvals = vec![
            Review { actor: "author".into(), decision: "approve".into(), ts: 1, oid: "a".into() },
            Review { actor: "alice".into(), decision: "approve".into(), ts: 2, oid: "b".into() },
            Review { actor: "alice".into(), decision: "approve".into(), ts: 3, oid: "c".into() },
            Review { actor: "svc-bot".into(), decision: "approve".into(), ts: 4, oid: "d".into() },
        ];
        let rules = MergeRules {
            protect: vec!["refs/heads/main".into()],
            require_human_approvals: 1,
        };
        let eval = merge_rule_eval(&rules, &pr);
        assert!(eval.allowed);
        assert_eq!(
            eval.satisfied_by,
            vec!["alice".to_string()],
            "author + duplicate alice + svc-bot = one distinct human approver"
        );
        let rules = MergeRules {
            protect: vec!["refs/heads/main".into()],
            require_human_approvals: 2,
        };
        assert!(!merge_rule_eval(&rules, &pr).allowed, "one approver is not two");
    }

    #[test]
    fn pr_view_collects_verified_patch_authors_only() {
        let key = make_key();
        let pub_b64 =
            base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes());
        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), pub_b64);
        let patch = signed_entry(
            &key,
            "patch",
            "t",
            "alice",
            "p1",
            1,
            serde_json::json!({"message": "x"}),
        );
        let unsigned = entry("patch", "t", "bob", "p2", 2, serde_json::json!({"message": "y"}));
        let pr = pr_view(&[&patch, &unsigned], &principals);
        assert_eq!(pr.authors, vec!["alice".to_string()], "only verified patches carry authority");
    }

    #[test]
    fn done_gate_rejects_self_approval_by_the_patch_author() {
        let key = make_key();
        let pub_b64 =
            base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes());
        let mut principals = HashMap::new();
        principals.insert("alice".to_string(), pub_b64);
        let issue = signed_entry(&key, "issue", "t9", "alice", "i1", 1, serde_json::json!({"title": "x"}));
        let status = signed_entry(&key, "status", "t9", "alice", "s1", 2, serde_json::json!({"status": "needs-review"}));
        let patch = signed_entry(&key, "patch", "t9", "alice", "p1", 3, serde_json::json!({"message": "x"}));
        let self_review = signed_entry(&key, "review", "t9", "alice", "r1", 4, serde_json::json!({"decision": "approve"}));
        let refs = vec![&issue, &status, &patch, &self_review];
        assert!(validate_status_transition(&refs, &principals).is_err(), "the author cannot approve their own change into done");

        let key2 = ed25519_dalek::SigningKey::from_bytes(&[43u8; 32]);
        let pub2 =
            base64::engine::general_purpose::STANDARD.encode(key2.verifying_key().to_bytes());
        principals.insert("bob".to_string(), pub2);
        let review = signed_entry(&key2, "review", "t9", "bob", "r2", 5, serde_json::json!({"decision": "approve"}));
        let refs = vec![&issue, &status, &patch, &self_review, &review];
        assert!(validate_status_transition(&refs, &principals).is_ok(), "a second, non-author reviewer unblocks it");
    }
}

#[cfg(test)]
mod golden_tests {
    //! Cross-language golden vector for the canonical signature
    //! (`docs/D1_PROTOCOL.md` §5.3). The same constants are asserted by
    //! `web/src/collab-canonical.test.ts` (SDK/WebCrypto) and posted through
    //! the thin API by the server test `collab_sdk_golden_entry_verifies_end_to_end`,
    //! so the SDK, the verifier and the aggregation are pinned to one byte
    //! string (cc-ai-d1-protocol-followups P0).
    use super::*;

    const GOLDEN_CANONICAL: &str = r#"{"actor":"alice","body":{"title":"golden vector"},"id":"golden","kind":"issue","parent":"","sig":"","ts":1786500000,"version":1}"#;
    const GOLDEN_SIG_B64: &str = "VFROsCUBDR4Sj1eFoMdDI/iRfV0A0jgRSGFGjAB91MVh2oh3IwnohAxj7Mq55x+uvpyrhM2tlq6x3WYuT9f5DQ==";
    const GOLDEN_PUB_B64: &str = "6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=";

    fn golden_entry() -> Entry {
        Entry {
            version: 1,
            kind: "issue".into(),
            id: "golden".into(),
            actor: "alice".into(),
            ts: 1_786_500_000,
            parent: String::new(),
            refs: None,
            body: serde_json::json!({"title": "golden vector"}),
            sig: String::new(),
        }
    }

    #[test]
    fn sdk_and_rust_sign_the_same_golden_bytes() {
        let mut e = golden_entry();
        assert_eq!(entry_canonical(&e), GOLDEN_CANONICAL);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pub_b64 =
            base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().to_bytes());
        assert_eq!(pub_b64, GOLDEN_PUB_B64);
        e.sig = sign_entry(&mut e, &sk);
        assert_eq!(e.sig, format!("ed25519:{GOLDEN_SIG_B64}"));
        assert!(verify_entry(&e, GOLDEN_PUB_B64).is_ok());
    }
}
