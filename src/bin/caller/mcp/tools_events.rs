//! The `events` facade verb: a cursor-addressed long-poll over the
//! daemon event ring (docs/design-mcp-control-lane.md, M2) — the
//! transport-agnostic answer to "wait for something to happen" that
//! `ctl` never had either. Push semantics only: the ring's ingest
//! allowlist ([`crate::event_ring::app_event_rides_event_ring`]) keeps
//! the stream to content the `session.inspect`-gated read tools already
//! serve, so this verb can never widen what its operation reads.
//!
//! Cursor contract:
//! - Opaque `"{epoch:x}.{seq}.{tag:x}"` strings. The epoch is the
//!   ring's per-boot identity — a cursor from a previous daemon boot
//!   fails loudly instead of silently reading wrong positions.
//! - Cursors are PRINCIPAL-BOUND (the design-review amendment): the tag
//!   commits to the acting principal, and a cursor minted under another
//!   principal is refused. Visibility is uniform today (everything in
//!   the ring is `session.inspect`-class), so the binding is
//!   forward-compatibility for per-principal filtering, not a secrecy
//!   boundary by itself.
//! - `since` omitted = start at NOW: the first call returns the current
//!   cursor (optionally waiting for the next event), never the
//!   backlog.

use super::*;
use crate::event_ring::EventRing;
use std::sync::Arc;

/// Long-poll ceiling — the `remote wait` chunk contract.
const EVENTS_WAIT_MAX_S: u64 = 60;
const EVENTS_PAGE_DEFAULT: usize = 100;
const EVENTS_PAGE_MAX: usize = 500;

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct EventsParams {
    /// Cursor from a previous response's `next_cursor`. Omit to start
    /// at NOW (no backlog replay).
    #[serde(default)]
    pub since: Option<String>,
    /// Seconds to wait for an event when nothing is newer than the
    /// cursor (default 0 = return immediately; clamp 0–60). Chunk
    /// longer waits client-side and re-poll with the returned cursor.
    #[serde(default)]
    pub wait_s: Option<u64>,
    /// Comma-separated event names to keep (e.g.
    /// "approval_required,session_ended"). Omit for all. The synthetic
    /// `event_gap` loss marker always passes the filter.
    #[serde(default)]
    pub filter: Option<String>,
    /// Max events per page (default 100, cap 500).
    #[serde(default)]
    pub max_events: Option<usize>,
}

/// The principal half of a cursor: root surfaces share one tag; every
/// scoped caller commits to its bound principal id (the unattributed
/// fallback included — same fail-closed identity the terminal actor
/// derivation uses).
fn cursor_principal_tag(trust: ToolCallerTrust, actor: &crate::access::actor::ActorBinding) -> u64 {
    use std::hash::{Hash, Hasher};
    // DefaultHasher::new() is fixed-key SipHash — stable for the life
    // of the boot, which is exactly a cursor's validity (the epoch
    // already invalidates cursors across restarts).
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match trust {
        ToolCallerTrust::OwnerSurface => "root".hash(&mut hasher),
        ToolCallerTrust::Scoped => actor
            .principal_id
            .as_deref()
            .unwrap_or("principal:unattributed")
            .hash(&mut hasher),
    }
    hasher.finish()
}

fn encode_cursor(epoch: u64, seq: u64, tag: u64) -> String {
    format!("{epoch:x}.{seq}.{tag:x}")
}

fn decode_cursor(cursor: &str) -> Option<(u64, u64, u64)> {
    let mut parts = cursor.split('.');
    let epoch = u64::from_str_radix(parts.next()?, 16).ok()?;
    let seq = parts.next()?.parse::<u64>().ok()?;
    let tag = u64::from_str_radix(parts.next()?, 16).ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((epoch, seq, tag))
}

/// Which delivered events a filter keeps. The `event_gap` loss marker
/// is exempt: a filtering caller must still learn it missed events.
fn passes_filter(json: &str, filter: &Option<Vec<String>>) -> bool {
    let Some(names) = filter else {
        return true;
    };
    let tag = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.get("event").and_then(|e| e.as_str()).map(String::from));
    match tag {
        Some(tag) => tag == "event_gap" || names.iter().any(|n| n == &tag),
        // An unparsable entry is delivered rather than silently eaten.
        None => true,
    }
}

fn events_error(message: &str) -> String {
    serde_json::json!({ "ok": false, "error": message }).to_string()
}

impl IntendantServer {
    pub(crate) async fn events_tool(
        &self,
        params: EventsParams,
        trust: ToolCallerTrust,
        actor: &crate::access::actor::ActorBinding,
    ) -> String {
        let ring: Option<Arc<EventRing>> = self.state.read().await.event_ring.clone();
        let Some(ring) = ring else {
            return events_error(
                "event stream unavailable on this server shape (bare stdio --mcp has no daemon event ring)",
            );
        };
        let tag = cursor_principal_tag(trust, actor);
        let mut scan_seq = match params
            .since
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => ring.current_seq(),
            Some(cursor) => {
                let Some((epoch, seq, cursor_tag)) = decode_cursor(cursor) else {
                    return events_error(
                        "invalid cursor — omit `since` to start at now and mint a fresh one",
                    );
                };
                if epoch != ring.epoch() {
                    return events_error(
                        "cursor is from a previous daemon boot — the stream restarted; omit `since`, resync state via the read commands, and continue from the fresh cursor",
                    );
                }
                if cursor_tag != tag {
                    // Principal-bound (design-review amendment): a cursor
                    // minted under another principal is refused.
                    return events_error(
                        "cursor was minted for a different principal — omit `since` to mint your own",
                    );
                }
                seq
            }
        };
        let filter = params.filter.as_deref().map(|f| {
            f.split(',')
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect::<Vec<_>>()
        });
        let max_events = params
            .max_events
            .unwrap_or(EVENTS_PAGE_DEFAULT)
            .clamp(1, EVENTS_PAGE_MAX);
        let wait_s = params.wait_s.unwrap_or(0).min(EVENTS_WAIT_MAX_S);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait_s);

        let mut delivered: Vec<String> = Vec::new();
        let mut gap = false;
        loop {
            // Notify discipline: arm the wakeup BEFORE reading, so a
            // push between the read and the await cannot be missed.
            let notified = ring.notified();
            let (entries, gap_now) = ring.since(scan_seq, max_events);
            gap |= gap_now;
            for entry in entries {
                scan_seq = entry.seq;
                if passes_filter(&entry.json, &filter) {
                    delivered.push(entry.json);
                }
                if delivered.len() >= max_events {
                    break;
                }
            }
            // Return on the first delivered batch (long-poll contract),
            // on a gap (the caller must resync), or at the deadline.
            if !delivered.is_empty() || gap || tokio::time::Instant::now() >= deadline {
                break;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                // Lapse: fall through to one final read — an event
                // recorded moments ago is delivered instead of being
                // reported as a quiet timeout (the ask-tool ledger
                // discipline).
                let (entries, gap_now) = ring.since(scan_seq, max_events);
                gap |= gap_now;
                for entry in entries {
                    scan_seq = entry.seq;
                    if passes_filter(&entry.json, &filter) {
                        delivered.push(entry.json);
                    }
                    if delivered.len() >= max_events {
                        break;
                    }
                }
                break;
            }
        }

        let events_json = delivered.join(",");
        format!(
            "{{\"ok\":true,\"events\":[{events_json}],\"next_cursor\":{next_cursor},\"current_seq\":{current_seq},\"gap\":{gap}}}",
            next_cursor =
                serde_json::json!(encode_cursor(ring.epoch(), scan_seq, tag)),
            current_seq = ring.current_seq(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_reject_malformed() {
        let cursor = encode_cursor(0xabc, 42, 0xdef);
        assert_eq!(decode_cursor(&cursor), Some((0xabc, 42, 0xdef)));
        for bad in ["", "1.2", "x.2.3", "1.y.3", "1.2.z", "1.2.3.4"] {
            assert_eq!(decode_cursor(bad), None, "{bad:?} must be refused");
        }
    }

    /// Cursors are principal-bound: root surfaces share one tag, each
    /// scoped principal gets its own, and the unattributed fallback is
    /// its own identity rather than colliding with root.
    #[test]
    fn principal_tags_separate_root_and_principals() {
        let unattributed = crate::access::actor::ActorBinding::unattributed();
        let root = cursor_principal_tag(ToolCallerTrust::OwnerSurface, &unattributed);
        let scoped_unattributed = cursor_principal_tag(ToolCallerTrust::Scoped, &unattributed);
        assert_ne!(root, scoped_unattributed);
        // The tag is deterministic within a boot — the same caller can
        // reuse its cursor across calls.
        assert_eq!(
            root,
            cursor_principal_tag(ToolCallerTrust::OwnerSurface, &unattributed)
        );
    }

    /// The `event_gap` loss marker always passes a filter — a filtering
    /// caller must still learn it missed events.
    #[test]
    fn filters_keep_gap_markers_and_matching_names() {
        let filter = Some(vec!["approval_required".to_string()]);
        assert!(passes_filter(
            "{\"event\":\"approval_required\",\"id\":1}",
            &filter
        ));
        assert!(!passes_filter("{\"event\":\"session_started\"}", &filter));
        assert!(passes_filter(
            "{\"event\":\"event_gap\",\"skipped\":9}",
            &filter
        ));
        assert!(passes_filter("{\"event\":\"anything\"}", &None));
    }
}
