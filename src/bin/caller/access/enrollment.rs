//! Pending browser-key enrollment requests.
//!
//! Phase 3 of the trust architecture (docs/src/trust-architecture.md):
//! when a *verified* browser identity key reaches this daemon but has no
//! local grant, the refusal also records a pending enrollment. The owner
//! approves or denies it from Access → People & Devices in an
//! already-trusted session; approval writes an ordinary IAM grant through
//! the normal upsert path. The queue is advisory and in-memory: it grants
//! nothing by itself, loses nothing important on restart (the requesting
//! browser retries on its next offer), and is capped and TTL'd so an
//! unauthenticated route cannot grow daemon state unboundedly.

use serde::Serialize;
use std::sync::{Mutex, OnceLock};

/// Advisory cap: newest requests win; the queue is a doorbell, not a log.
const MAX_PENDING: usize = 32;
/// Requests older than this are pruned on read.
const PENDING_TTL_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PendingClientKeyEnrollment {
    /// base64url(sha256(raw point)) — the IAM binding value.
    pub fingerprint: String,
    /// base64url raw public key, kept so approval can store it for audit.
    pub public_key_b64u: String,
    /// The hosted origin the route implies (recorded onto the grant so role
    /// ceilings treat the key as hosted-provenance until re-enrolled from an
    /// anchor origin). Empty when unknown.
    pub origin: String,
    /// Transport the refused offer arrived on.
    pub transport: String,
    /// Human hint from the offer's account identity, e.g. "@alice". Display
    /// only — never an authorization input.
    pub account_hint: String,
    pub first_seen_unix_ms: i64,
    pub last_seen_unix_ms: i64,
    pub attempts: u32,
}

fn registry() -> &'static Mutex<Vec<PendingClientKeyEnrollment>> {
    static REGISTRY: OnceLock<Mutex<Vec<PendingClientKeyEnrollment>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

fn prune_locked(pending: &mut Vec<PendingClientKeyEnrollment>, now_unix_ms: i64) {
    pending.retain(|entry| now_unix_ms - entry.last_seen_unix_ms <= PENDING_TTL_MS);
}

/// Record a refused-but-verified key. Repeated attempts collapse into one
/// entry with a bumped counter; overflow evicts the stalest entry.
pub fn record_refused_client_key(
    fingerprint: &str,
    public_key_b64u: &str,
    origin: &str,
    transport: &str,
    account_hint: &str,
    now_unix_ms: i64,
) {
    let fingerprint = fingerprint.trim();
    if fingerprint.is_empty() {
        return;
    }
    let mut pending = registry().lock().expect("enrollment registry poisoned");
    prune_locked(&mut pending, now_unix_ms);
    if let Some(entry) = pending
        .iter_mut()
        .find(|entry| entry.fingerprint == fingerprint)
    {
        entry.last_seen_unix_ms = now_unix_ms;
        entry.attempts = entry.attempts.saturating_add(1);
        if !account_hint.trim().is_empty() {
            entry.account_hint = account_hint.trim().to_string();
        }
        if !origin.trim().is_empty() {
            entry.origin = origin.trim().to_string();
        }
        return;
    }
    if pending.len() >= MAX_PENDING {
        // Evict the entry that has waited longest without a retry.
        if let Some(stalest) = pending
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| entry.last_seen_unix_ms)
            .map(|(index, _)| index)
        {
            pending.remove(stalest);
        }
    }
    pending.push(PendingClientKeyEnrollment {
        fingerprint: fingerprint.to_string(),
        public_key_b64u: public_key_b64u.trim().to_string(),
        origin: origin.trim().to_string(),
        transport: transport.trim().to_string(),
        account_hint: account_hint.trim().to_string(),
        first_seen_unix_ms: now_unix_ms,
        last_seen_unix_ms: now_unix_ms,
        attempts: 1,
    });
}

/// Snapshot the queue, newest activity first.
pub fn pending_enrollments(now_unix_ms: i64) -> Vec<PendingClientKeyEnrollment> {
    let mut pending = registry().lock().expect("enrollment registry poisoned");
    prune_locked(&mut pending, now_unix_ms);
    let mut out = pending.clone();
    out.sort_by_key(|entry| std::cmp::Reverse(entry.last_seen_unix_ms));
    out
}

/// Remove and return an entry when the owner decides on it.
pub fn take_enrollment(fingerprint: &str) -> Option<PendingClientKeyEnrollment> {
    let fingerprint = fingerprint.trim();
    let mut pending = registry().lock().expect("enrollment registry poisoned");
    let index = pending
        .iter()
        .position(|entry| entry.fingerprint == fingerprint)?;
    Some(pending.remove(index))
}

#[cfg(test)]
pub fn clear_for_tests() {
    registry()
        .lock()
        .expect("enrollment registry poisoned")
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    // The registry is process-global, so exercise the full lifecycle in one
    // test to avoid cross-test interference under the parallel runner.
    #[test]
    fn enrollment_queue_lifecycle() {
        clear_for_tests();
        let now = 1_000_000;
        record_refused_client_key("fp-a", "pk-a", "https://connect.intendant.dev", "connect-dashboard-control", "@alice", now);
        record_refused_client_key("fp-a", "pk-a", "", "connect-dashboard-control", "", now + 10);
        let pending = pending_enrollments(now + 20);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].attempts, 2);
        assert_eq!(pending[0].account_hint, "@alice");
        assert_eq!(pending[0].origin, "https://connect.intendant.dev");
        assert_eq!(pending[0].first_seen_unix_ms, now);
        assert_eq!(pending[0].last_seen_unix_ms, now + 10);

        // TTL prunes stale entries.
        assert!(pending_enrollments(now + PENDING_TTL_MS + 11).is_empty());

        // Cap evicts the stalest entry, and take() removes on decide.
        clear_for_tests();
        for i in 0..(MAX_PENDING + 1) {
            record_refused_client_key(
                &format!("fp-{i}"),
                "pk",
                "",
                "connect-dashboard-control",
                "",
                now + i as i64,
            );
        }
        let pending = pending_enrollments(now + 1_000);
        assert_eq!(pending.len(), MAX_PENDING);
        assert!(!pending.iter().any(|entry| entry.fingerprint == "fp-0"));
        assert!(take_enrollment("fp-5").is_some());
        assert!(take_enrollment("fp-5").is_none());
        clear_for_tests();
    }
}
