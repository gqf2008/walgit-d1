//! The decentralized CI protocol (`docs/D1_CI_PROTOCOL.md`): the deterministic
//! read-side over signed `ci_claim` / `ci_result` collab entries. walgit runs
//! no CI (principle X) — a client runner claims a run, executes the tested
//! commit's `.walgit/ci.toml` and publishes a signed result into its inbox;
//! this module is what **every** client (CLI, server API, dashboard) evaluates
//! to agree on who is running what and which result is effective. Pure
//! functions of (entries, principals, `now`), like `crate::collab`.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

use crate::collab::EntryRef;

// ---- protocol constants (docs/D1_CI_PROTOCOL.md §6/§8, normative) --------------

pub const CI_CLAIM_KIND: &str = "ci_claim";
pub const CI_RESULT_KIND: &str = "ci_result";
/// §6.1: claim TTL bounds, seconds.
pub const CLAIM_TTL_MIN_SECS: u64 = 1;
pub const CLAIM_TTL_MAX_SECS: u64 = 86_400;
/// §7.4: attempts are bounded like ci.toml's `max_attempts` (V7).
pub const ATTEMPT_MAX: u32 = 10;
/// §11: the read side treats an oversized entry body as **malformed** (visible
/// red, never a convergence input). The write side's §6.1/§8.2 field limits
/// keep honest bodies under ~30 KiB (a 4096-byte `log_summary` worst-case
/// JSON-escaped plus 32 artifacts); this cap is a generous bound above that,
/// not a second schema.
pub const CI_BODY_MAX_BYTES: usize = 256 * 1024;

/// §8.2 storage convention (issue #161): artifact and log bytes are ordinary
/// git blobs, pushed through the same receive-pack channel as everything
/// else, content-addressed under this ref namespace:
/// `refs/collab/ci-artifacts/<actor>/<sha256>`. Any git client fetches them;
/// the web API serves them by hash.
pub const CI_ARTIFACT_REF_PREFIX: &str = "refs/collab/ci-artifacts/";
/// §8.2: at most this many artifacts per result (write-side; the read side
/// truncates a longer list rather than rejecting the entry).
pub const CI_ARTIFACTS_PER_RESULT_MAX: usize = 32;
/// §8.2 storage convention: one artifact/log object's byte cap. Bounded so a
/// runner pass's upload batch stays small and the read side can refuse to
/// materialize absurd blobs.
pub const CI_ARTIFACT_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// 64-bit FNV-1a — the §5 run-id digest.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The §5 identity byte stream: `utf8(task) || 0x1f || utf8(ref) || 0x1f ||
/// utf8(commit)`.
fn run_id_bytes(task: &str, repo_ref: &str, commit: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(task.len() + repo_ref.len() + commit.len() + 2);
    buf.extend_from_slice(task.as_bytes());
    buf.push(0x1f);
    buf.extend_from_slice(repo_ref.as_bytes());
    buf.push(0x1f);
    buf.extend_from_slice(commit.as_bytes());
    buf
}

/// §5: `run_id = "ci-" + hex16(fnv1a64(task || 0x1f || ref || 0x1f || commit))`.
/// Deterministic and computable by any client; the collab thread id of both
/// the claim and the result entries of a run.
pub fn run_id(task: &str, repo_ref: &str, commit: &str) -> String {
    format!("ci-{:016x}", fnv1a64(&run_id_bytes(task, repo_ref, commit)))
}

/// §5 scheduled variant (§4.3, issue #161): a cron-triggered run of the same
/// `(task, ref, commit)` is a **new** run — the slot's fire epoch keeps the
/// identity deterministic and recomputable by any client that knows the
/// schedule, without touching the entry schema or the state machine:
/// `scheduled_run_id = "ci-" + hex16(fnv1a64(task || 0x1f || ref || 0x1f ||
/// commit || 0x1f || decimal(slot_epoch)))`.
pub fn scheduled_run_id(task: &str, repo_ref: &str, commit: &str, slot_epoch: i64) -> String {
    let mut buf = run_id_bytes(task, repo_ref, commit);
    buf.push(0x1f);
    buf.extend_from_slice(slot_epoch.to_string().as_bytes());
    format!("ci-{:016x}", fnv1a64(&buf))
}

// ---- entry bodies (§6.1 claim / §8.2 result) ------------------------------------

/// The task's verdict (§8.2). `error` is a runner-side failure: it publishes
/// no task conclusion and voids the claim it references (§6.3).
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    Success,
    Failure,
    Timeout,
    Error,
}

impl Conclusion {
    pub fn as_str(&self) -> &'static str {
        match self {
            Conclusion::Success => "success",
            Conclusion::Failure => "failure",
            Conclusion::Timeout => "timeout",
            Conclusion::Error => "error",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "success" => Some(Conclusion::Success),
            "failure" => Some(Conclusion::Failure),
            "timeout" => Some(Conclusion::Timeout),
            "error" => Some(Conclusion::Error),
            _ => None,
        }
    }
}

/// A verified, well-formed `ci_claim` entry (§6.1).
#[derive(Serialize, Clone, Debug)]
pub struct CiClaim {
    pub oid: String,
    pub actor: String,
    /// The entry's signed timestamp — the claim time the convergence uses.
    pub ts: i64,
    pub task: String,
    pub repo_ref: String,
    pub commit: String,
    pub ttl_secs: u64,
    pub attempt: u32,
}

impl CiClaim {
    /// §6.3: a claim is live until `ts + ttl`; a claim referenced by an `error`
    /// result is void regardless (§7.2 `valid`).
    pub fn live(&self, now: i64) -> bool {
        now <= self.ts.saturating_add_unsigned(self.ttl_secs)
    }
}

/// A verified, well-formed `ci_result` entry (§8.2).
#[derive(Serialize, Clone, Debug)]
pub struct CiResult {
    pub oid: String,
    pub actor: String,
    pub ts: i64,
    pub task: String,
    pub repo_ref: String,
    pub commit: String,
    pub attempt: u32,
    /// The oid of the claim entry this result answers.
    pub claim: String,
    pub conclusion: Conclusion,
    /// `None` on timeout / when no exit code exists.
    pub exit_code: Option<i64>,
    pub duration_ms: Option<u64>,
    pub log_summary: String,
    pub log_sha256: String,
    /// True when the published log blob is the retained tail of output larger
    /// than `CI_ARTIFACT_MAX_BYTES`; the hash covers the retained bytes only.
    pub log_truncated: bool,
    /// §8.2: declared output files (bytes in the repo's object store under
    /// `CI_ARTIFACT_REF_PREFIX`, or at an external `url` the writer set).
    pub artifacts: Vec<CiArtifact>,
}

/// §8.2: one declared output file. Integrity is the `sha256`; consumers
/// verify after fetching, wherever the bytes came from.
#[derive(Serialize, Clone, Debug)]
pub struct CiArtifact {
    pub name: String,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub url: Option<String>,
}

fn body_str(body: &serde_json::Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn body_u32(body: &serde_json::Value, key: &str, min: u32, max: u32) -> Option<u32> {
    body.get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| (min..=max).contains(v))
}

/// §11 read side: is the body's compact JSON serialization within
/// `CI_BODY_MAX_BYTES`? Computed without building the string, bailing out the
/// moment the budget is blown — an abusive body costs a bounded walk, not an
/// allocation of its full size. The byte count matches `serde_json`'s compact
/// form exactly (its escaping rule: only `"`, `\` and control characters
/// expand; everything else is raw UTF-8).
fn body_within_cap(body: &serde_json::Value) -> bool {
    json_fits(body, CI_BODY_MAX_BYTES)
}

/// Whether `v`'s compact serialization is at most `budget` bytes.
fn json_fits(v: &serde_json::Value, budget: usize) -> bool {
    fn walk(v: &serde_json::Value, remaining: &mut usize) -> Option<()> {
        use serde_json::Value;
        let cost = match v {
            Value::Null => 4,
            Value::Bool(b) => {
                if *b {
                    4 // true
                } else {
                    5 // false
                }
            }
            Value::Number(n) => n.to_string().len(),
            Value::String(s) => {
                2 + s
                    .chars()
                    .map(|c| match c {
                        // serde_json escapes `"`, `\` and the short forms
                        // \b \t \n \f \r as two bytes…
                        '"' | '\\' | '\u{8}' | '\u{9}' | '\u{a}' | '\u{c}' | '\u{d}' => 2,
                        c if (c as u32) < 0x20 => 6, // …other controls as \u00XX
                        c => c.len_utf8(),
                    })
                    .sum::<usize>()
            }
            Value::Array(a) => 2 + a.len().saturating_sub(1),
            Value::Object(o) => 2 + o.len().saturating_sub(1) + o.len(), // braces, commas, colons
        };
        *remaining = remaining.checked_sub(cost)?;
        match v {
            Value::Array(a) => a.iter().try_for_each(|x| walk(x, remaining))?,
            Value::Object(o) => {
                o.values()
                    .try_for_each(|x| walk(x, remaining))
                    .and(o.keys().try_for_each(|k| {
                        // Each key serializes as a JSON string.
                        walk(&Value::String(k.clone()), remaining)
                    }))?;
            }
            _ => {}
        }
        Some(())
    }
    let mut remaining = budget;
    walk(v, &mut remaining).is_some()
}

/// Parse a `ci_claim` body (§6.1). Strict: wrong/missing fields → `None`
/// (the entry stays visible in the thread but never drives convergence).
pub fn parse_claim(r: &EntryRef) -> Option<CiClaim> {
    if r.entry.kind != CI_CLAIM_KIND {
        return None;
    }
    let body = &r.entry.body;
    if !body_within_cap(body) {
        return None; // §11: oversized body = malformed
    }
    let ttl_secs = body_u32(
        body,
        "ttl",
        u32::try_from(CLAIM_TTL_MIN_SECS).ok()?,
        u32::try_from(CLAIM_TTL_MAX_SECS).ok()?,
    )?;
    Some(CiClaim {
        oid: r.oid.clone(),
        actor: r.entry.actor.clone(),
        ts: r.entry.ts,
        task: body_str(body, "task")?,
        repo_ref: body_str(body, "ref")?,
        commit: body_str(body, "commit")?,
        ttl_secs: u64::from(ttl_secs),
        attempt: body_u32(body, "attempt", 1, ATTEMPT_MAX)?,
    })
}

/// Parse a `ci_result` body (§8.2).
pub fn parse_result(r: &EntryRef) -> Option<CiResult> {
    if r.entry.kind != CI_RESULT_KIND {
        return None;
    }
    let body = &r.entry.body;
    if !body_within_cap(body) {
        return None; // §11: oversized body = malformed
    }
    let conclusion = Conclusion::parse(body_str(body, "conclusion")?.as_str())?;
    Some(CiResult {
        oid: r.oid.clone(),
        actor: r.entry.actor.clone(),
        ts: r.entry.ts,
        task: body_str(body, "task")?,
        repo_ref: body_str(body, "ref")?,
        commit: body_str(body, "commit")?,
        attempt: body_u32(body, "attempt", 1, ATTEMPT_MAX)?,
        claim: body_str(body, "claim")?,
        conclusion,
        exit_code: body.get("exit_code").and_then(serde_json::Value::as_i64),
        duration_ms: body.get("duration_ms").and_then(serde_json::Value::as_u64),
        log_summary: body_str(body, "log_summary").unwrap_or_default(),
        log_sha256: body_str(body, "log_sha256").unwrap_or_default(),
        log_truncated: body
            .get("log_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        artifacts: parse_artifacts(body)?,
    })
}

/// §8.2: `artifacts` is optional, but present-and-not-an-array makes the whole
/// entry malformed (a writer claiming the field must mean it). Items are
/// shape-checked one by one — a bad item drops, it does not condemn the
/// entry; the list is truncated to the per-result cap.
fn parse_artifacts(body: &serde_json::Value) -> Option<Vec<CiArtifact>> {
    let Some(val) = body.get("artifacts") else {
        return Some(Vec::new()); // absent = no artifacts, not malformed
    };
    let arr = val.as_array()?;
    let mut out = Vec::with_capacity(arr.len().min(CI_ARTIFACTS_PER_RESULT_MAX));
    for item in arr.iter().take(CI_ARTIFACTS_PER_RESULT_MAX) {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let (Some(name), Some(path), Some(sha256), Some(bytes)) = (
            obj.get("name").and_then(serde_json::Value::as_str),
            obj.get("path").and_then(serde_json::Value::as_str),
            obj.get("sha256").and_then(serde_json::Value::as_str),
            obj.get("bytes").and_then(serde_json::Value::as_u64),
        ) else {
            continue;
        };
        if name.is_empty() || name.len() > 128 || path.is_empty() {
            continue;
        }
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let url = obj
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        out.push(CiArtifact {
            name: name.to_string(),
            path: path.to_string(),
            sha256: sha256.to_string(),
            bytes,
            url,
        });
    }
    Some(out)
}

// ---- §7 aggregation: one run's view ----------------------------------------------

/// One attempt of a run: its claims, the deterministic winner, its results and
/// the one effective result (§7.2).
#[derive(Serialize, Clone, Debug)]
pub struct AttemptView {
    pub attempt: u32,
    pub claims: Vec<CiClaim>,
    /// min by `(ts, actor, oid)` over live claims — the runner that may execute.
    pub winner: Option<CiClaim>,
    pub results: Vec<CiResult>,
    /// The one result that counts (§7.2): references the winner, else the
    /// fallback that keeps a settled run settled.
    pub effective: Option<CiResult>,
    pub state: RunState,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// No claims at all — anyone may claim.
    Pending,
    /// A live winner holds the run.
    Claimed,
    /// Claims exist but none is live (TTL passed, or voided by an `error`
    /// result) — re-claimable (§6.3).
    Stale,
    /// An effective result exists.
    Done,
}

impl RunState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunState::Pending => "pending",
            RunState::Claimed => "claimed",
            RunState::Stale => "stale",
            RunState::Done => "done",
        }
    }
}

/// One run (= one collab thread): every attempt, the latest attempt's state.
#[derive(Serialize, Clone, Debug)]
pub struct RunView {
    pub id: String,
    pub task: String,
    pub repo_ref: String,
    pub commit: String,
    /// Sorted by attempt number.
    pub attempts: Vec<AttemptView>,
    pub latest_attempt: u32,
    pub state: RunState,
    /// The effective result's conclusion on the latest attempt, if any.
    pub conclusion: Option<Conclusion>,
    /// The actor holding (or having answered) the latest attempt.
    pub runner: Option<String>,
    pub last_ts: i64,
    /// Entries for this run that failed verification or parsing — visible red,
    /// never part of convergence (§7.1).
    pub unverified: usize,
}

/// §7.2, normative. `error` results void the claim they reference; the winner
/// is the earliest live claim; the effective result is the one referencing the
/// winner, falling back to the earliest result so a settled run never reverts.
fn attempt_view(
    attempt: u32,
    claims: Vec<CiClaim>,
    results: Vec<CiResult>,
    now: i64,
) -> AttemptView {
    let voided: Vec<&str> = results
        .iter()
        .filter(|r| r.conclusion == Conclusion::Error)
        .map(|r| r.claim.as_str())
        .collect();
    let winner: Option<CiClaim> = claims
        .iter()
        .filter(|c| c.live(now) && !voided.contains(&c.oid.as_str()))
        .min_by_key(|c| (c.ts, c.actor.clone(), c.oid.clone()))
        .cloned();
    let answered: Vec<&CiResult> = match &winner {
        Some(w) => results.iter().filter(|r| r.claim == w.oid).collect(),
        None => results.iter().collect(),
    };
    let effective = answered
        .iter()
        .min_by_key(|r| (r.ts, r.actor.clone(), r.oid.clone()))
        .copied()
        .cloned();
    // A settled run means a task verdict (success/failure/timeout); an
    // `error` result is displayed but its claim is void, so the attempt is
    // stale and re-claimable (§6.3/§7.3).
    let settled = effective
        .as_ref()
        .is_some_and(|r| r.conclusion != Conclusion::Error);
    let state = if settled {
        RunState::Done
    } else if winner.is_some() {
        RunState::Claimed
    } else if claims.is_empty() {
        RunState::Pending
    } else {
        RunState::Stale
    };
    AttemptView {
        attempt,
        claims,
        winner,
        results,
        effective,
        state,
    }
}

/// The view of one run over the entries carrying its `id` (§7.1): only
/// verified entries drive the state; the rest count in `unverified`.
pub fn run_view(
    id: &str,
    entries: &[&EntryRef],
    principals: &HashMap<String, String, impl std::hash::BuildHasher>,
    now: i64,
) -> Option<RunView> {
    let mut attempts: BTreeMap<u32, (Vec<CiClaim>, Vec<CiResult>)> = BTreeMap::new();
    let mut unverified = 0usize;
    let mut last_ts = 0i64;
    let mut seen = false;
    for r in entries {
        if r.entry.id != id {
            continue;
        }
        seen = true;
        last_ts = last_ts.max(r.entry.ts);
        let verified = r.is_verified(principals);
        let claim = if verified { parse_claim(r) } else { None };
        let result = if verified { parse_result(r) } else { None };
        if claim.is_none() && result.is_none() {
            // Unverified, or a verified entry that does not parse (§6.1/§8.2):
            // a visible red, not a state input.
            if r.entry.kind == CI_CLAIM_KIND || r.entry.kind == CI_RESULT_KIND {
                unverified += 1;
            }
            continue;
        }
        let attempt = claim
            .as_ref()
            .map(|c| c.attempt)
            .or(result.as_ref().map(|x| x.attempt));
        let entry = attempts
            .entry(attempt.unwrap_or(1))
            .or_insert_with(|| (Vec::new(), Vec::new()));
        if let Some(c) = claim {
            entry.0.push(c);
        }
        if let Some(x) = result {
            entry.1.push(x);
        }
    }
    if !seen {
        return None;
    }
    let mut views: Vec<AttemptView> = attempts
        .into_iter()
        .map(|(attempt, (claims, results))| attempt_view(attempt, claims, results, now))
        .collect();
    views.sort_by_key(|a| a.attempt);
    // Header fields come from the latest attempt only, deterministically:
    // effective result, else the winner claim, else the first parsed claim.
    let (state, conclusion, runner, task, repo_ref, commit) = match views.last() {
        Some(a) => {
            let source = a
                .effective
                .as_ref()
                .map(|r| (r.task.as_str(), r.repo_ref.as_str(), r.commit.as_str()))
                .or_else(|| {
                    a.winner
                        .as_ref()
                        .map(|c| (c.task.as_str(), c.repo_ref.as_str(), c.commit.as_str()))
                })
                .or_else(|| {
                    a.claims
                        .first()
                        .map(|c| (c.task.as_str(), c.repo_ref.as_str(), c.commit.as_str()))
                })
                .unwrap_or(("", "", ""));
            (
                a.state.clone(),
                a.effective.as_ref().map(|r| r.conclusion.clone()),
                a.winner.as_ref().map(|c| c.actor.clone()),
                source.0.to_string(),
                source.1.to_string(),
                source.2.to_string(),
            )
        }
        None => (
            RunState::Pending,
            None,
            None,
            String::new(),
            String::new(),
            String::new(),
        ),
    };
    Some(RunView {
        id: id.to_string(),
        task,
        repo_ref,
        commit,
        latest_attempt: views.last().map_or(1, |a| a.attempt),
        attempts: views,
        state,
        conclusion,
        runner,
        last_ts,
        unverified,
    })
}

// ---- §7.1/§8.3 the whole log, aggregated ------------------------------------------

/// The CI-relevant slice of a log: every `ci_claim`/`ci_result` entry,
/// verified or not — the reds stay visible (§7.1/§8.3), everything else is
/// not this protocol's business.
pub fn ci_entries<'a>(entries: &[&'a EntryRef]) -> Vec<&'a EntryRef> {
    entries
        .iter()
        .filter(|e| e.entry.kind == CI_CLAIM_KIND || e.entry.kind == CI_RESULT_KIND)
        .copied()
        .collect()
}

/// Every run in the log, by id, as of `now` (§7.1). The one aggregation
/// behind `walgit ci status`, the collab report's CI section and the SPA's
/// thread display — three surfaces, one answer, no second semantics.
pub fn collect_runs(
    entries: &[&EntryRef],
    principals: &HashMap<String, String, impl std::hash::BuildHasher>,
    now: i64,
) -> BTreeMap<String, RunView> {
    let mut by_id: BTreeMap<&str, Vec<&EntryRef>> = BTreeMap::new();
    for e in entries {
        by_id.entry(e.entry.id.as_str()).or_default().push(e);
    }
    by_id
        .into_iter()
        .filter_map(|(id, es)| {
            let view = run_view(id, &es, principals, now)?;
            Some((id.to_string(), view))
        })
        .collect()
}

// ---- §7.3 decide: the runner's normative decision --------------------------------
/// What a runner must do for a run right now (§6.2 step 2 / §7.3).
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum Decision {
    /// The run has an effective result on its latest attempt; nothing to do.
    Settled {
        conclusion: Conclusion,
        attempt: u32,
    },
    /// Publish a claim for this attempt (fresh run, re-claim after TTL, or the
    /// next attempt after a failure with retry budget left).
    Claim { attempt: u32 },
    /// Our own unexpired claim already wins — execute now (crash recovery).
    Resume { claim_oid: String, attempt: u32 },
    /// Another runner holds a live winning claim.
    StandDown { holder: String },
}

/// §7.3, normative. `max_attempts` is the task's retry bound (ci.toml V7): a
/// failed (or timed-out) latest attempt with budget left becomes the next
/// attempt's `Claim`; `error` conclusions void their claim, so the same
/// attempt is re-claimable at once (§6.3). The view already carries the
/// evaluation instant (`run_view(.., now)`), so decide itself is time-free.
pub fn decide(run: &RunView, actor: &str, max_attempts: u32) -> Decision {
    let Some(latest) = run.attempts.last() else {
        return Decision::Claim { attempt: 1 };
    };
    match latest.state {
        RunState::Done => match &latest.effective {
            Some(eff) => {
                let budget = max_attempts.clamp(1, ATTEMPT_MAX);
                let retriable = matches!(eff.conclusion, Conclusion::Failure | Conclusion::Timeout)
                    && eff.attempt < budget;
                if retriable {
                    Decision::Claim {
                        attempt: eff.attempt + 1,
                    }
                } else {
                    Decision::Settled {
                        conclusion: eff.conclusion.clone(),
                        attempt: eff.attempt,
                    }
                }
            }
            // Unreachable by construction (Done implies an effective result);
            // the total match keeps decide total without unwrapping.
            None => Decision::Claim {
                attempt: latest.attempt,
            },
        },
        RunState::Claimed => match latest.winner.as_ref() {
            Some(w) if w.actor == actor => Decision::Resume {
                claim_oid: w.oid.clone(),
                attempt: latest.attempt,
            },
            Some(w) => Decision::StandDown {
                holder: w.actor.clone(),
            },
            // A claimed attempt always has a winner; total match over the
            // invariant keeps this function total without unwrapping.
            None => Decision::Claim { attempt: 1 },
        },
        RunState::Pending => Decision::Claim { attempt: 1 },
        RunState::Stale => Decision::Claim {
            attempt: latest.attempt,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collab::{Entry, EntryRefs, sign_entry};
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;

    fn keypair(seed: u8) -> (SigningKey, String) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let pk = base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().to_bytes());
        (sk, pk)
    }

    /// A signed entry with a caller-built body; `oid` encodes (seed, n) for
    /// deterministic tie-breaks in the tests.
    fn signed_entry(
        sk: &SigningKey,
        id: &str,
        kind: &str,
        actor: &str,
        oid: &str,
        ts: i64,
        body: serde_json::Value,
    ) -> EntryRef {
        let mut e = Entry {
            version: 1,
            kind: kind.to_string(),
            id: id.to_string(),
            actor: actor.to_string(),
            ts,
            parent: String::new(),
            refs: None::<EntryRefs>,
            body,
            sig: String::new(),
        };
        e.sig = sign_entry(&mut e, sk);
        EntryRef {
            oid: oid.to_string(),
            principal: actor.to_string(),
            entry: e,
        }
    }

    fn claim_body(
        task: &str,
        repo_ref: &str,
        commit: &str,
        ttl: u64,
        attempt: u32,
    ) -> serde_json::Value {
        serde_json::json!({"task": task, "ref": repo_ref, "commit": commit, "ttl": ttl, "attempt": attempt})
    }

    fn result_body(
        task: &str,
        repo_ref: &str,
        commit: &str,
        attempt: u32,
        claim_oid: &str,
        conclusion: &str,
    ) -> serde_json::Value {
        serde_json::json!({"task": task, "ref": repo_ref, "commit": commit, "attempt": attempt,
            "claim": claim_oid, "conclusion": conclusion, "exit_code": 0, "duration_ms": 5,
            "log_summary": "ok", "log_sha256": "abc"})
    }

    const NOW: i64 = 1_000;

    fn run_ref(id: &str) -> String {
        id.to_string()
    }

    #[test]
    fn run_id_is_deterministic_and_discriminating() {
        let a = run_id("test", "refs/heads/main", "abc");
        let b = run_id("test", "refs/heads/main", "abd");
        let c = run_id("test", "refs/heads/main", "abc");
        assert_eq!(a, c, "same run -> same id");
        assert_ne!(a, b, "different commit -> different id");
        assert_ne!(
            a,
            run_id("test", "refs/heads/mainx", "abc"),
            "field separator matters"
        );
        assert!(a.starts_with("ci-"));
        assert_eq!(a.len(), 19, "ci- + 16 hex");
        let hex = a.strip_prefix("ci-").unwrap_or(&a);
        assert!(hex.len() == 16 && hex.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn claim_and_result_parse_strictly() {
        let (sk, pk) = keypair(3);
        let mut principals = HashMap::new();
        principals.insert("ci-1".to_string(), pk);

        let good = signed_entry(
            &sk,
            "r",
            CI_CLAIM_KIND,
            "ci-1",
            "c1",
            NOW,
            claim_body("test", "refs/heads/main", "abc", 300, 1),
        );
        let parsed = parse_claim(&good).expect("parses");
        assert_eq!(parsed.ttl_secs, 300);
        assert_eq!(parsed.attempt, 1);
        assert!(parsed.live(NOW));
        assert!(!parsed.live(NOW + 301), "expired after ttl");

        for bad_ttl in [0u64, 86_401] {
            let e = signed_entry(
                &sk,
                "r",
                CI_CLAIM_KIND,
                "ci-1",
                "cx",
                NOW,
                claim_body("test", "refs/heads/main", "abc", bad_ttl, 1),
            );
            assert!(parse_claim(&e).is_none(), "ttl {bad_ttl} out of bounds");
        }
        let e = signed_entry(
            &sk,
            "r",
            CI_CLAIM_KIND,
            "ci-1",
            "cx",
            NOW,
            serde_json::json!({"task": "test", "ref": "refs/heads/main", "commit": "abc"}),
        );
        assert!(parse_claim(&e).is_none(), "missing fields");

        let res = signed_entry(
            &sk,
            "r",
            CI_RESULT_KIND,
            "ci-1",
            "x1",
            NOW,
            result_body("test", "refs/heads/main", "abc", 1, "c1", "success"),
        );
        let parsed = parse_result(&res).expect("parses");
        assert_eq!(parsed.conclusion, Conclusion::Success);
        assert_eq!(parsed.exit_code, Some(0));
        assert!(!parsed.log_truncated);
        let mut truncated = result_body("test", "refs/heads/main", "abc", 1, "c1", "success");
        truncated["log_truncated"] = serde_json::json!(true);
        let e = signed_entry(
            &sk,
            "r",
            CI_RESULT_KIND,
            "ci-1",
            "x-truncated",
            NOW,
            truncated,
        );
        assert!(parse_result(&e).expect("parses").log_truncated);
        let bad = signed_entry(
            &sk,
            "r",
            CI_RESULT_KIND,
            "ci-1",
            "x2",
            NOW,
            result_body("test", "refs/heads/main", "abc", 1, "c1", "green"),
        );
        assert!(parse_result(&bad).is_none(), "unknown conclusion");
    }

    #[test]
    fn earliest_claim_wins_and_other_stands_down() {
        let (sk_a, pk_a) = keypair(1);
        let (sk_b, pk_b) = keypair(2);
        let mut principals = HashMap::new();
        principals.insert("ci-a".to_string(), pk_a);
        principals.insert("ci-b".to_string(), pk_b);

        let id = run_ref("run1");
        let ca = signed_entry(
            &sk_a,
            &id,
            CI_CLAIM_KIND,
            "ci-a",
            "a1",
            900,
            claim_body("t", "refs/heads/main", "abc", 300, 1),
        );
        let cb = signed_entry(
            &sk_b,
            &id,
            CI_CLAIM_KIND,
            "ci-b",
            "b1",
            910,
            claim_body("t", "refs/heads/main", "abc", 300, 1),
        );
        let entries = vec![&ca, &cb];
        let view = run_view(&id, &entries, &principals, NOW).expect("view");
        assert_eq!(view.state, RunState::Claimed);
        assert_eq!(view.runner.as_deref(), Some("ci-a"), "earliest ts wins");
        assert_eq!(
            decide(&view, "ci-b", 1),
            Decision::StandDown {
                holder: "ci-a".into()
            },
            "later claimant stands down"
        );
        assert_eq!(
            decide(&view, "ci-a", 1),
            Decision::Resume {
                claim_oid: "a1".into(),
                attempt: 1
            },
            "winner resumes (crash recovery)"
        );
    }

    #[test]
    fn claim_tie_breaks_by_actor_then_oid() {
        let (sk_a, pk_a) = keypair(1);
        let (sk_b, pk_b) = keypair(2);
        let mut principals = HashMap::new();
        principals.insert("ci-a".to_string(), pk_a);
        principals.insert("ci-b".to_string(), pk_b);
        let id = run_ref("run2");
        let ca = signed_entry(
            &sk_a,
            &id,
            CI_CLAIM_KIND,
            "ci-a",
            "zz",
            900,
            claim_body("t", "r", "c", 300, 1),
        );
        let cb = signed_entry(
            &sk_b,
            &id,
            CI_CLAIM_KIND,
            "ci-b",
            "aa",
            900,
            claim_body("t", "r", "c", 300, 1),
        );
        let entries = vec![&cb, &ca];
        let view = run_view(&id, &entries, &principals, NOW).expect("view");
        assert_eq!(
            view.runner.as_deref(),
            Some("ci-a"),
            "same ts -> actor order"
        );
    }

    #[test]
    fn expired_claim_is_reclaimable_and_error_voids_its_claim() {
        let (sk_a, pk_a) = keypair(1);
        let (sk_b, pk_b) = keypair(2);
        let mut principals = HashMap::new();
        principals.insert("ci-a".to_string(), pk_a);
        principals.insert("ci-b".to_string(), pk_b);
        let id = run_ref("run3");

        // TTL passed: stale, anyone may re-claim the same attempt.
        let ca = signed_entry(
            &sk_a,
            &id,
            CI_CLAIM_KIND,
            "ci-a",
            "a1",
            500,
            claim_body("t", "r", "c", 100, 1),
        );
        let entries = vec![&ca];
        let view = run_view(&id, &entries, &principals, NOW).expect("view");
        assert_eq!(view.state, RunState::Stale);
        assert_eq!(decide(&view, "ci-b", 1), Decision::Claim { attempt: 1 });

        // An error result voids its own claim immediately (§6.3): stale at once.
        let ca2 = signed_entry(
            &sk_a,
            &id,
            CI_CLAIM_KIND,
            "ci-a",
            "a2",
            990,
            claim_body("t", "r", "c", 300, 1),
        );
        let err = signed_entry(
            &sk_b,
            &id,
            CI_RESULT_KIND,
            "ci-b",
            "e1",
            995,
            result_body("t", "r", "c", 1, "a2", "error"),
        );
        let entries = vec![&ca2, &err];
        let view = run_view(&id, &entries, &principals, NOW).expect("view");
        assert_eq!(
            view.state,
            RunState::Stale,
            "voided claim = stale, not claimed"
        );
        assert_eq!(decide(&view, "ci-b", 1), Decision::Claim { attempt: 1 });
    }

    #[test]
    fn effective_result_references_the_winning_claim_and_survives_expiry() {
        let (sk_a, pk_a) = keypair(1);
        let (sk_b, pk_b) = keypair(2);
        let mut principals = HashMap::new();
        principals.insert("ci-a".to_string(), pk_a);
        principals.insert("ci-b".to_string(), pk_b);
        let id = run_ref("run4");

        // Two results (partition race): only the one answering the winning
        // claim is effective.
        let ca = signed_entry(
            &sk_a,
            &id,
            CI_CLAIM_KIND,
            "ci-a",
            "a1",
            900,
            claim_body("t", "r", "c", 300, 1),
        );
        let cb = signed_entry(
            &sk_b,
            &id,
            CI_CLAIM_KIND,
            "ci-b",
            "b1",
            910,
            claim_body("t", "r", "c", 300, 1),
        );
        let ra = signed_entry(
            &sk_a,
            &id,
            CI_RESULT_KIND,
            "ci-a",
            "ra",
            950,
            result_body("t", "r", "c", 1, "a1", "success"),
        );
        let rb = signed_entry(
            &sk_b,
            &id,
            CI_RESULT_KIND,
            "ci-b",
            "rb",
            951,
            result_body("t", "r", "c", 1, "b1", "failure"),
        );
        let entries = vec![&ca, &cb, &ra, &rb];
        let view = run_view(&id, &entries, &principals, NOW).expect("view");
        assert_eq!(view.state, RunState::Done);
        assert_eq!(
            view.conclusion,
            Some(Conclusion::Success),
            "a's result answers the winner"
        );
        let latest = view.attempts.last().expect("attempt");
        assert_eq!(latest.effective.as_ref().expect("eff").oid, "ra");

        // Every claim expires later: the run stays done (fallback rule §7.2).
        let view = run_view(&id, &entries, &principals, NOW + 10_000).expect("view");
        assert_eq!(view.state, RunState::Done, "settled stays settled");
        assert_eq!(view.conclusion, Some(Conclusion::Success));
    }

    #[test]
    fn failure_retries_up_to_max_attempts() {
        let (sk_a, pk_a) = keypair(1);
        let mut principals = HashMap::new();
        principals.insert("ci-a".to_string(), pk_a);
        let id = run_ref("run5");
        let ca = signed_entry(
            &sk_a,
            &id,
            CI_CLAIM_KIND,
            "ci-a",
            "a1",
            900,
            claim_body("t", "r", "c", 300, 1),
        );
        let ra = signed_entry(
            &sk_a,
            &id,
            CI_RESULT_KIND,
            "ci-a",
            "ra",
            950,
            result_body("t", "r", "c", 1, "a1", "failure"),
        );

        let entries = vec![&ca, &ra];
        let view = run_view(&id, &entries, &principals, NOW).expect("view");
        assert_eq!(
            decide(&view, "ci-a", 3),
            Decision::Claim { attempt: 2 },
            "budget left"
        );
        assert_eq!(
            decide(&view, "ci-a", 1),
            Decision::Settled {
                conclusion: Conclusion::Failure,
                attempt: 1
            },
            "no budget -> settled failure"
        );

        // Attempt 2 succeeds: settled, no attempt 3 even with budget.
        let ca2 = signed_entry(
            &sk_a,
            &id,
            CI_CLAIM_KIND,
            "ci-a",
            "a2",
            960,
            claim_body("t", "r", "c", 300, 2),
        );
        let ra2 = signed_entry(
            &sk_a,
            &id,
            CI_RESULT_KIND,
            "ci-a",
            "ra2",
            970,
            result_body("t", "r", "c", 2, "a2", "success"),
        );
        let entries = vec![&ca, &ra, &ca2, &ra2];
        let view = run_view(&id, &entries, &principals, NOW).expect("view");
        assert_eq!(view.latest_attempt, 2);
        assert_eq!(view.conclusion, Some(Conclusion::Success));
        assert_eq!(
            decide(&view, "ci-a", 3),
            Decision::Settled {
                conclusion: Conclusion::Success,
                attempt: 2
            }
        );
    }

    #[test]
    fn unverified_entries_count_red_but_never_drive_state() {
        let (sk_a, pk_a) = keypair(1);
        let (sk_b, _pk_b) = keypair(2);
        let mut principals = HashMap::new();
        principals.insert("ci-a".to_string(), pk_a);
        // ci-b is not registered: its entries never verify.
        let id = run_ref("run6");
        let good = signed_entry(
            &sk_a,
            &id,
            CI_CLAIM_KIND,
            "ci-a",
            "a1",
            900,
            claim_body("t", "r", "c", 300, 1),
        );
        let evil = signed_entry(
            &sk_b,
            &id,
            CI_CLAIM_KIND,
            "ci-b",
            "z1",
            100,
            claim_body("t", "r", "c", 300, 1),
        );
        let entries = vec![&good, &evil];
        let view = run_view(&id, &entries, &principals, NOW).expect("view");
        assert_eq!(view.unverified, 1, "the unverified claim is a visible red");
        assert_eq!(
            view.runner.as_deref(),
            Some("ci-a"),
            "unverified cannot win"
        );
    }

    #[test]
    fn parse_never_trusts_the_wrong_inbox() {
        let (sk_a, pk_a) = keypair(1);
        let mut principals = HashMap::new();
        principals.insert("ci-a".to_string(), pk_a);
        let id = run_ref("run7");
        // Signed by ci-a but planted in ci-b's inbox: the docs/D1_PROTOCOL.md §4.5 inbox
        // invariant makes it unverified, so it drives nothing.
        let mut smuggled = signed_entry(
            &sk_a,
            &id,
            CI_CLAIM_KIND,
            "ci-a",
            "a1",
            900,
            claim_body("t", "r", "c", 300, 1),
        );
        smuggled.principal = "ci-b".to_string();
        let entries = vec![&smuggled];
        let view = run_view(&id, &entries, &principals, NOW).expect("view");
        assert_eq!(
            view.state,
            RunState::Pending,
            "smuggled claim is not a claim"
        );
        assert_eq!(view.unverified, 1);
    }

    #[test]
    fn scheduled_run_id_is_a_fresh_identity_per_slot() {
        // §5 scheduled variant: same (task, ref, commit), different slot → a
        // different run; same slot → the same run; and a scheduled id never
        // collides with the ref-triggered id.
        let plain = run_id("test", "refs/heads/main", "abc");
        let s1 = scheduled_run_id("test", "refs/heads/main", "abc", 1_700_000_000);
        let s2 = scheduled_run_id("test", "refs/heads/main", "abc", 1_700_000_001);
        assert_ne!(plain, s1, "slot in the identity");
        assert_ne!(s1, s2, "one run per slot");
        assert_eq!(
            s1,
            scheduled_run_id("test", "refs/heads/main", "abc", 1_700_000_000)
        );
        assert!(s1.starts_with("ci-") && s1.len() == 19, "{s1}");
        // The slot is decimal ASCII in the stream: "…+1s" ≠ "…+10s" ≠ another
        // tuple that could textually collide through the separator.
        assert_ne!(
            scheduled_run_id("test", "refs/heads/main", "abc1", 700_000_000),
            scheduled_run_id("test", "refs/heads/main", "abc", 1_700_000_000)
        );
    }

    #[test]
    fn artifacts_parse_shape_checked_and_truncated() {
        // §8.2: `artifacts` is optional, but present-not-an-array is malformed
        // (the whole entry); items are shape-checked one by one — conforming
        // ones survive, the rest drop; the list is truncated to the cap.
        let (sk, _pk) = keypair(1);
        let hex64 = "ab".repeat(32);

        // Absent key: parses, empty list.
        let e = signed_entry(
            &sk,
            "run",
            CI_RESULT_KIND,
            "ci-a",
            "r0",
            900,
            result_body("t", "r", "c", 1, "a0", "success"),
        );
        let r = parse_result(&e).expect("parses");
        assert!(r.artifacts.is_empty());

        // Present but not an array: the whole entry is malformed.
        let mut bad = result_body("t", "r", "c", 1, "a0", "success");
        bad["artifacts"] = serde_json::json!("not-an-array");
        let e = signed_entry(&sk, "run", CI_RESULT_KIND, "ci-a", "r1", 900, bad);
        assert!(parse_result(&e).is_none(), "not an array = malformed");

        // Mixed items: conforming ones survive, the rest drop.
        let mut mixed = result_body("t", "r", "c", 1, "a0", "success");
        mixed["artifacts"] = serde_json::json!([
            {"name": "report.html", "path": "target/report.html", "sha256": &hex64, "bytes": 12},
            {"name": "no-sha", "path": "x", "bytes": 1},
            {"name": "bad-sha", "path": "x", "sha256": "zz", "bytes": 1},
            {"name": "x".repeat(129), "path": "x", "sha256": &hex64, "bytes": 1},
            "junk",
            {"name": "with-url", "path": "p", "sha256": &hex64, "bytes": 1, "url": "https://x"}
        ]);
        let e = signed_entry(&sk, "run", CI_RESULT_KIND, "ci-a", "r2", 900, mixed);
        let r = parse_result(&e).expect("parses");
        assert_eq!(r.artifacts.len(), 2, "{:?}", r.artifacts);
        assert_eq!(r.artifacts[0].name, "report.html");
        assert_eq!(r.artifacts[0].path, "target/report.html");
        assert_eq!(r.artifacts[0].bytes, 12);
        assert_eq!(r.artifacts[0].url, None);
        assert_eq!(r.artifacts[1].url.as_deref(), Some("https://x"));

        // Over the per-result cap: truncated, still parses.
        let mut many = result_body("t", "r", "c", 1, "a0", "success");
        many["artifacts"] = serde_json::json!(
            (0..40)
                .map(|i| serde_json::json!({"name": format!("f{i}"), "path": "p", "sha256": &hex64, "bytes": 1}))
                .collect::<Vec<_>>()
        );
        let e = signed_entry(&sk, "run", CI_RESULT_KIND, "ci-a", "r3", 900, many);
        let r = parse_result(&e).expect("parses");
        assert_eq!(r.artifacts.len(), CI_ARTIFACTS_PER_RESULT_MAX);
    }

    #[test]
    fn oversized_body_is_malformed_and_never_drives_state() {
        // §11: the write side's field limits bound honest bodies; the read side
        // treats anything over CI_BODY_MAX_BYTES as malformed — a visible red
        // that cannot win a claim or settle a run.
        let (sk, pk) = keypair(1);
        let mut principals = HashMap::new();
        principals.insert("ci-a".to_string(), pk);
        let id = run_ref("runBig");

        let huge = "x".repeat(CI_BODY_MAX_BYTES);
        let big_claim = signed_entry(
            &sk,
            &id,
            CI_CLAIM_KIND,
            "ci-a",
            "a1",
            900,
            serde_json::json!({"task": "t", "ref": "r", "commit": "c", "ttl": 300, "attempt": 1,
                "runner": huge}),
        );
        assert!(
            parse_claim(&big_claim).is_none(),
            "an oversized claim body is malformed"
        );
        let entries = vec![&big_claim];
        let view = run_view(&id, &entries, &principals, NOW).expect("view");
        assert_eq!(
            view.state,
            RunState::Pending,
            "oversized claim is not a claim"
        );
        assert_eq!(view.unverified, 1, "…but it stays a visible red");
        assert_eq!(
            decide(&view, "ci-a", 1),
            Decision::Claim { attempt: 1 },
            "anyone may (re)claim — the blob cannot wedge the run"
        );

        let big_result = signed_entry(
            &sk,
            &id,
            CI_RESULT_KIND,
            "ci-a",
            "r1",
            950,
            serde_json::json!({"task": "t", "ref": "r", "commit": "c", "attempt": 1,
                "claim": "a1", "conclusion": "success",
                "log_summary": "x".repeat(CI_BODY_MAX_BYTES), "log_sha256": "abc"}),
        );
        assert!(parse_result(&big_result).is_none());

        // The boundary: a body just under the cap still parses.
        let mut ok_body =
            serde_json::json!({"task": "t", "ref": "r", "commit": "c", "ttl": 300, "attempt": 1});
        let base_len = serde_json::to_string(&ok_body).unwrap().len();
        let pad = CI_BODY_MAX_BYTES - base_len - 15; // ,"pad":"…" framing
        ok_body["pad"] = serde_json::Value::String("y".repeat(pad));
        assert!(serde_json::to_string(&ok_body).unwrap().len() <= CI_BODY_MAX_BYTES);
        let ok_entry = signed_entry(&sk, &id, CI_CLAIM_KIND, "ci-a", "a2", 900, ok_body);
        assert!(
            parse_claim(&ok_entry).is_some(),
            "a body within the cap parses"
        );

        // The estimator agrees with the real serializer on tricky content
        // (escapes, multibyte, nesting) — the cap cannot be gamed by encoding.
        for v in [
            serde_json::json!({"a": "q\"ü\\中\u{1}", "b": [1, true, null, {"c": "d"}]}),
            serde_json::json!({"k": "x".repeat(1000)}),
            serde_json::json!(["\u{7f}", "é", 3.5]),
        ] {
            let exact = serde_json::to_string(&v).unwrap().len();
            assert!(json_fits(&v, exact), "at the exact size it fits ({exact})");
            assert!(
                !json_fits(&v, exact - 1),
                "one byte short of the exact size it does not ({exact})"
            );
        }
    }

    #[test]
    fn collect_runs_aggregates_the_whole_log_by_id() {
        let (sk_a, pk_a) = keypair(1);
        let mut principals = HashMap::new();
        principals.insert("ci-a".to_string(), pk_a);
        let id1 = run_ref("runA");
        let id2 = run_ref("runB");
        let claim = signed_entry(
            &sk_a,
            &id1,
            CI_CLAIM_KIND,
            "ci-a",
            "a1",
            900,
            claim_body("t", "r", "c", 300, 1),
        );
        let result = signed_entry(
            &sk_a,
            &id1,
            CI_RESULT_KIND,
            "ci-a",
            "r1",
            950,
            result_body("t", "r", "c", 1, "a1", "success"),
        );
        let pending = signed_entry(
            &sk_a,
            &id2,
            CI_CLAIM_KIND,
            "ci-a",
            "a2",
            960,
            claim_body("u", "r2", "c2", 300, 1),
        );
        let noise = signed_entry(
            &sk_a,
            &id2,
            "issue",
            "ci-a",
            "n1",
            970,
            serde_json::json!({ "title": "not a CI entry" }),
        );
        let all: Vec<EntryRef> = vec![claim, result, pending, noise];
        let refs: Vec<&EntryRef> = all.iter().collect();
        let runs = collect_runs(&refs, &principals, NOW);
        assert_eq!(runs.len(), 2, "two run ids");
        let done = runs.get(&id1).expect("runA");
        assert_eq!(done.state, RunState::Done);
        assert_eq!(done.conclusion, Some(Conclusion::Success));
        let claimed = runs.get(&id2).expect("runB");
        assert_eq!(
            claimed.state,
            RunState::Claimed,
            "the issue entry is not a claim"
        );
        assert_eq!(
            claimed.unverified, 0,
            "non-CI kinds are simply not CI entries"
        );
        // `ci_entries` is the slice every caller aggregates over.
        let only_ci = ci_entries(&refs);
        assert_eq!(only_ci.len(), 3, "the issue entry is filtered out");
    }
}
