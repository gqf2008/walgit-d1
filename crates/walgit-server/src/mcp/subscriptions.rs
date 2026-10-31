//! `/mcp` subscriptions: one bounded poller per (caller, session, URI) (D64).
//!
//! `resources/subscribe` does not hold a request open (D55 rules the long-poll
//! lane out): it starts a task that re-reads the resource's version every
//! `mcp.subscribe_interval` and emits `notifications/resources/updated` when it
//! moves. The version read *is* the observation lane every other walgit client
//! uses — a refs-level conditional `manifest.pb` GET — so a subscription costs
//! the same per tick as `git ls-remote`.
//!
//! Lifecycle: `unsubscribe`, a terminated session ([`Mcp::forget_session`]), a
//! closed transport and the poller's own failure all end a subscription, and
//! every one of those paths removes its registry entry (see [`Unregister`]).
//! The registry is in-memory and per-instance: a restart loses the pollers and
//! nothing else (D52).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{ErrorData, ResourceUpdatedNotificationParam};
use rmcp::service::{Peer, RoleServer};

use super::resources;
use crate::AppState;

/// Subscriptions: `(principal, session, uri)` → the poller watching it.
///
/// All three parts are in the key because all three are part of the answer. The
/// principal, because authorization decides it. The session, because the
/// notification travels down *that* stream: keying without it made a reconnect
/// get `ok` for a poller still bound to the old, dead peer (silently no
/// notifications), and made one session's `unsubscribe` abort another's poller.
/// And the URI, because that is what is watched.
///
/// One lock covers the cap check, the reservation and the abort-handle install,
/// and every entry carries a generation so a stale handle can never remove the
/// entry that replaced it. The registry is per-instance and disposable (D52).
#[derive(Default)]
pub struct Mcp {
    subs: parking_lot::Mutex<HashMap<Key, Subscription>>,
    generation: AtomicU64,
}

/// `(principal, session, uri)`.
type Key = (String, String, String);

struct Subscription {
    /// The refs-level seq the client was last told about.
    version: String,
    /// Identifies this poller, so `unsubscribe` and the poller's own guard only
    /// ever touch the entry they own.
    generation: u64,
    /// `None` only between "slot reserved" and "task spawned" (both under the
    /// same lock, so no other request can observe it).
    abort: Option<tokio::task::AbortHandle>,
}

impl Mcp {
    /// How many subscriptions are live (tests assert the reap).
    #[cfg_attr(not(test), allow(dead_code, reason = "asserted by the tests"))]
    pub fn len(&self) -> usize {
        self.subs.lock().len()
    }

    /// Nothing is watching anything — the state a terminated session leaves.
    #[cfg_attr(not(test), allow(dead_code, reason = "asserted by the tests"))]
    pub fn is_empty(&self) -> bool {
        self.subs.lock().is_empty()
    }
    /// Drop every subscription of one session. Called when that session is
    /// terminated: the key carries the session id precisely because the
    /// notification travels down *that* stream, so a dead session's pollers
    /// have nobody to tell and must not keep paying a bucket read per interval
    /// (or keep counting against `mcp.max_subscriptions`).
    pub fn forget_session(&self, session: &str) {
        let mut subs = self.subs.lock();
        let gone: Vec<Key> = subs
            .keys()
            .filter(|k| k.1 == session)
            .cloned()
            .collect();
        for key in gone {
            if let Some(abort) = subs.remove(&key).and_then(|s| s.abort) {
                abort.abort();
            }
        }
    }
    /// Stop watching one URI for one session (a no-op when nothing watches it).
    pub fn unsubscribe(&self, principal: &str, session: &str, uri: &str) {
        let key: Key = (principal.to_string(), session.to_string(), uri.to_string());
        if let Some(abort) = self.subs.lock().remove(&key).and_then(|s| s.abort) {
            abort.abort();
        }
    }
}

/// Start watching one URI for this caller's session (idempotent: a second
/// `subscribe` for the same session and URI keeps the existing poller).
pub(super) async fn watch(
    st: &Arc<AppState>,
    principal: &str,
    session: &str,
    uri_str: &str,
    peer: Peer<RoleServer>,
) -> Result<(), ErrorData> {
        let uri = resources::parse(uri_str)?;
        let cfg = &st.cfg.mcp;
        let interval = cfg.subscribe_interval;
        if interval.is_zero() {
            return Err(ErrorData::internal_error(
                "mcp.subscribe_interval is zero",
                None,
            ));
        }
        // The version is read *outside* the registry lock: it is a bucket
        // round trip, and subscribing must not serialize every other client.
        let version = resources::version(st, &uri)
            .await
            .map_err(resources::PollError::into_error_data)?;
        let key: Key = (principal.to_string(), session.to_string(), uri_str.to_string());
        // From here to the end of the function is one critical section: the cap
        // check, the reservation and the abort handle. Two racing `subscribe`s
        // can then never both pass the cap, and no `unsubscribe` can slip
        // between "reserved" and "watching".
        let mut subs = st.mcp.subs.lock();
        if subs.contains_key(&key) {
            return Ok(());
        }
        // Counted per caller across its sessions: the budget is what one caller
        // costs the bucket per interval, not one stream's.
        let mine = subs.keys().filter(|k| k.0 == principal).count();
        if mine >= cfg.max_subscriptions {
            return Err(ErrorData::invalid_params(
                format!(
                    "at most {} subscriptions per caller (mcp.max_subscriptions)",
                    cfg.max_subscriptions
                ),
                None,
            ));
        }
        let generation = st.mcp.generation.fetch_add(1, Ordering::Relaxed) + 1;
        subs.insert(
            key.clone(),
            Subscription {
                version,
                generation,
                abort: None,
            },
        );
        let task = tokio::spawn(poll(
            st.clone(),
            key.clone(),
            generation,
            uri,
            interval,
            peer,
        ));
        match subs.get_mut(&key) {
            Some(slot) if slot.generation == generation => {
                slot.abort = Some(task.abort_handle());
            }
            // Unreachable while the entry cannot be removed under this lock;
            // aborting is the only safe answer if that ever stops being true.
            _ => task.abort(),
        }
        Ok(())
    }
/// One subscription's poller. Bounded by `interval`, ended by `unsubscribe`, by
/// a session that went away, or by its own failure — and [`Unregister`] is what
/// keeps the registry honest however it ends.
async fn poll(
    st: Arc<AppState>,
    key: Key,
    generation: u64,
    uri: resources::Uri,
    interval: Duration,
    peer: rmcp::service::Peer<RoleServer>,
) {
    let _guard = Unregister {
        st: st.clone(),
        key: key.clone(),
        generation,
    };
    loop {
        tokio::time::sleep(interval).await;
        // The peer's channel closes when rmcp drops the session (its
        // `keep_alive` timeout, a DELETE, a crashed client). Checking it here
        // rather than only on a failed send matters: a notification is only
        // sent when the version *moved*, which an idle repository never does,
        // so a send-failure test was unreachable and the poller leaked.
        if peer.is_transport_closed() {
            return;
        }
        let next = match resources::version(&st, &uri).await {
            Ok(v) => v,
            Err(resources::PollError::Gone) => {
                // The repository no longer exists: the resource is gone, which
                // is the one thing `notifications/resources/list_changed`
                // means. This is where that capability earns its place — it is
                // never sent "just because".
                let _ = peer.notify_resource_list_changed().await;
                return;
            }
            Err(resources::PollError::Other(e)) => {
                tracing::debug!(uri = %key.2, error = %e, "resource poll failed; unsubscribing");
                return;
            }
        };
        let moved = {
            let mut subs = st.mcp.subs.lock();
            match subs.get_mut(&key) {
                Some(s) if s.generation == generation && s.version != next => {
                    s.version = next;
                    true
                }
                Some(_) => false,
                // Someone else removed it (unsubscribe, or a newer poller): stop.
                None => return,
            }
        };
        if moved
            && peer
                .notify_resource_updated(ResourceUpdatedNotificationParam::new(key.2.clone()))
                .await
                .is_err()
        {
            // A failed send means the session is gone: this poller has nobody
            // to tell, so it stops and the guard cleans up.
            return;
        }
    }
}

/// Registry cleanup for one poller. `Drop` is sync, and the registry lock is a
/// `parking_lot` mutex, so no `await` is needed. The generation is what stops a
/// dying poller from deleting the entry that replaced it.
struct Unregister {
    st: Arc<AppState>,
    key: Key,
    generation: u64,
}

impl Drop for Unregister {
    fn drop(&mut self) {
        let mut subs = self.st.mcp.subs.lock();
        if subs
            .get(&self.key)
            .is_some_and(|s| s.generation == self.generation)
        {
            subs.remove(&self.key);
        }
    }
}
