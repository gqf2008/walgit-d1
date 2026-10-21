//! Cron schedule parsing and due evaluation for bundle strategies.
//!
//! Supports the `cron` crate's 6/7-field syntax plus the `@hourly`, `@daily`,
//! `@weekly` (and `@monthly`, `@yearly`) shorthand aliases natively parsed by
//! the `cron` crate.

use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use cron::Schedule;

use crate::BundleError;

/// Parse a cron expression or shorthand alias into a [`Schedule`].
///
/// The `cron` crate already handles `@hourly`/`@daily`/`@weekly`/`@monthly`/
/// `@yearly`, so this is a thin wrapper that surfaces a useful error.
pub fn parse_schedule(expr: &str) -> Result<Schedule, BundleError> {
    Schedule::from_str(expr.trim())
        .map_err(|e| BundleError::InvalidSchedule(format!("{expr}: {e}")))
}

/// Convert [`SystemTime`] to a chrono UTC datetime.
fn to_chrono(t: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(t)
}

/// Convert a chrono UTC datetime back to [`SystemTime`].
fn to_system(dt: DateTime<Utc>) -> SystemTime {
    // A pre-epoch timestamp (negative seconds) maps to the epoch — exactly
    // the old `.max(0) as u64` clamp: try_from fails only for negatives,
    // which default to 0.
    UNIX_EPOCH + Duration::from_secs(u64::try_from(dt.timestamp()).unwrap_or_default())
}

/// Next fire time of `schedule` strictly after `after`, or `None` if the
/// schedule is exhausted (e.g. a bounded year range has passed).
pub fn next_fire_after(schedule: &Schedule, after: SystemTime) -> Option<SystemTime> {
    let dt = to_chrono(after);
    schedule.after(&dt).next().map(to_system)
}

/// Whether a strategy is due at `now`.
///
/// * `last_built` = `None` (never built) → due.
/// * `last_built` = `Some(t)` → due iff the next scheduled fire after `t`
///   is at or before `now`.
/// * If the schedule is exhausted → not due.
pub fn is_due(schedule: &Schedule, last_built: Option<SystemTime>, now: SystemTime) -> bool {
    match last_built {
        None => true,
        Some(last) => {
            let next = next_fire_after(schedule, last);
            match next {
                Some(fire) => fire <= now,
                None => false,
            }
        }
    }
}

/// Current Unix timestamp in seconds (for `creation_token` computation).
pub fn unix_now(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Coalescing bound for [`due_slot`]: at most this many fires are walked per
/// evaluation; a deeper backlog advances the bookkeeping without firing (a
/// runner that was down for days must not burst-run every missed slot).
const COALESCE_SCAN_MAX: u32 = 4096;

/// What a periodic trigger owes at `now`, given the last slot it processed
/// (D1-CI §4.3 — the same coalescing philosophy as refs-level polling: missed
/// slots fold into the newest due one, never a replay of history).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DueSlot {
    /// No fire in `(last_slot, now]`.
    NotDue,
    /// The newest fire in `(last_slot, now]` — run exactly this slot.
    Run(i64),
    /// Fires remain due even past this one (backlog deeper than the scan
    /// bound): advance the bookkeeping to this slot without running.
    Skip(i64),
}

/// The slot a periodic trigger should process at `now`, or the bookkeeping
/// advance for a backlog too deep to run through. `last_slot` is a fire epoch
/// previously processed (first sight is handled by the caller seeding
/// `last_slot = now`).
pub fn due_slot(schedule: &Schedule, last_slot: i64, now: i64) -> DueSlot {
    let after = UNIX_EPOCH + Duration::from_secs(u64::try_from(last_slot).unwrap_or(0));
    let mut latest: Option<i64> = None;
    let mut fire = next_fire_after(schedule, after);
    let mut steps = 0u32;
    while let Some(f) = fire {
        let epoch = i64::try_from(unix_now(f)).unwrap_or(i64::MAX);
        if epoch > now {
            break;
        }
        latest = Some(epoch);
        steps += 1;
        if steps >= COALESCE_SCAN_MAX {
            break;
        }
        fire = next_fire_after(schedule, f);
    }
    match latest {
        None => DueSlot::NotDue,
        Some(slot) => {
            // Is anything still due past this slot (either the scan hit its
            // bound or the next fire is already in the past)? Then this pass
            // only advances the bookkeeping.
            let after_slot = UNIX_EPOCH + Duration::from_secs(u64::try_from(slot).unwrap_or(0));
            let more = next_fire_after(schedule, after_slot)
                .is_some_and(|f| i64::try_from(unix_now(f)).unwrap_or(i64::MAX) <= now);
            if more {
                DueSlot::Skip(slot)
            } else {
                DueSlot::Run(slot)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_aliases() {
        assert!(parse_schedule("@hourly").is_ok());
        assert!(parse_schedule("@daily").is_ok());
        assert!(parse_schedule("@weekly").is_ok());
        assert!(parse_schedule("@monthly").is_ok());
        assert!(parse_schedule("@yearly").is_ok());
    }

    #[test]
    fn parse_6field() {
        assert!(parse_schedule("0 0 * * * *").is_ok()); // hourly
        assert!(parse_schedule("0 0 0 * * *").is_ok()); // daily
        assert!(parse_schedule("0 0 0 * * 1").is_ok()); // weekly (Sunday, 6 fields)
    }

    #[test]
    fn parse_7field() {
        assert!(parse_schedule("0 30 9 * * * 2024").is_ok());
        assert!(parse_schedule("0 0 0 * * 1 *").is_ok()); // weekly (Sunday, 7 fields)
    }

    #[test]
    fn parse_bad() {
        assert!(parse_schedule("not a cron").is_err());
        assert!(parse_schedule("").is_err());
    }

    #[test]
    fn due_never_built() {
        let s = parse_schedule("@hourly").unwrap();
        let now = SystemTime::now();
        assert!(is_due(&s, None, now));
    }

    #[test]
    fn due_after_fire_time() {
        let s = parse_schedule("@hourly").unwrap();
        let now = SystemTime::now();
        // Last built 2 hours ago → next fire was 1 hour ago → due.
        let last = now - Duration::from_hours(2);
        assert!(is_due(&s, Some(last), now));
    }

    #[test]
    fn not_due_before_fire_time() {
        let s = parse_schedule("@daily").unwrap();
        let now = SystemTime::now();
        // Last built 1 second ago → next fire is ~24h away → not due.
        let last = now - Duration::from_secs(1);
        assert!(!is_due(&s, Some(last), now));
    }

    #[test]
    fn next_fire_advances() {
        let s = parse_schedule("@hourly").unwrap();
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let next = next_fire_after(&s, t).unwrap();
        // Next fire should be after t.
        assert!(next > t);
        // And within 1 hour (hourly schedule).
        assert!(next <= t + Duration::from_secs(3600));
    }

    #[test]
    fn due_slot_runs_only_the_newest_due_fire() {
        let every_5s = parse_schedule("*/5 * * * * *").unwrap();
        // Not due: the next fire is in the future.
        assert_eq!(
            due_slot(&every_5s, 1_700_000_000, 1_700_000_004),
            DueSlot::NotDue
        );
        // Due: exactly one fire in the window — run it.
        assert_eq!(
            due_slot(&every_5s, 1_700_000_000, 1_700_000_006),
            DueSlot::Run(1_700_000_005)
        );
        // Coalesce: three fires due — run only the newest.
        assert_eq!(
            due_slot(&every_5s, 1_700_000_000, 1_700_000_016),
            DueSlot::Run(1_700_000_015)
        );
        // A daily schedule evaluated right at the fire is due exactly then.
        let daily = parse_schedule("0 0 0 * * *").unwrap();
        let midnight = 1_700_092_800i64; // 2023-11-16T00:00:00Z (86400-aligned)
        assert_eq!(
            due_slot(&daily, midnight - 86_400, midnight),
            DueSlot::Run(midnight)
        );
        assert_eq!(due_slot(&daily, midnight, midnight), DueSlot::NotDue);
    }

    #[test]
    fn due_slot_skips_a_backlog_deeper_than_the_scan_bound() {
        // Every-second schedule, last processed 5000 s ago: more fires remain
        // due past the scan bound, so the pass only advances the bookkeeping.
        let per_second = parse_schedule("* * * * * *").unwrap();
        let last = 1_700_000_000i64;
        let now = last + 5000;
        let due = due_slot(&per_second, last, now);
        let slot = match due {
            DueSlot::Skip(s) => s,
            other => panic!("deep backlog must skip, got {other:?}"),
        };
        assert_eq!(slot, last + i64::from(COALESCE_SCAN_MAX));
        // After the skip, the remaining backlog drains one scan per pass and
        // the final pass runs the newest slot (per-second fire: `now` itself).
        let due = due_slot(&per_second, slot, now);
        assert_eq!(due, DueSlot::Run(now), "{due:?}");
    }
}
