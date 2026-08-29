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
//!   principal is refused.
//! - Delivery is SESSION-SCOPED for agent-session callers (security
//!   review P1): a supervised backend using its session-bound MCP token
//!   sees only its gate-bound session's events — the
//!   `get_pending_approval_scoped` discipline — with sessionless events
//!   failing closed; every other caller class keeps the daemon-wide
//!   view its `session.inspect` grant already reads. Cursor positions
//!   stay global (coarse volume metadata, not content).
//! - `since` omitted = start at NOW: the first call returns the current
//!   cursor (optionally waiting for the next event), never the
//!   backlog.

use super::*;
use crate::event_ring::{EventRing, RingEntry};
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

/// Which delivered events a name filter keeps. The `event_gap` loss
/// marker is exempt: a filtering caller must still learn it missed
/// events.
fn passes_filter(entry: &RingEntry, filter: &Option<Vec<String>>) -> bool {
    let Some(names) = filter else {
        return true;
    };
    entry.event == "event_gap" || names.iter().any(|n| n == &entry.event)
}

/// Session scoping at delivery (security review P1): an agent-session
/// caller — a supervised backend using its session-bound MCP token —
/// sees only its gate-bound session's events, mirroring
/// `get_pending_approval_scoped`'s discipline; sessionless events fail
/// closed for it, and a scope whose gate bound no id sees nothing.
/// The synthetic `event_gap` loss marker is exempt — it carries no
/// content beyond "events were missed", and a scoped caller must still
/// learn its view lost events. Every other caller class keeps the
/// daemon-wide view its `session.inspect` grant already reads.
fn visible_to_scope(entry: &RingEntry, scope: &McpToolScope<'_>) -> bool {
    match scope {
        McpToolScope::Unrestricted => true,
        McpToolScope::AgentSession { session_id } => {
            if entry.event == "event_gap" {
                return true;
            }
            match (entry.session_id.as_deref(), session_id) {
                (Some(entry_session), Some(bound)) => entry_session == *bound,
                _ => false,
            }
        }
    }
}

fn events_error(message: &str) -> String {
    serde_json::json!({ "ok": false, "error": message }).to_string()
}

/// Decode and validate a caller cursor against the live ring: shape,
/// boot epoch, principal binding, and position. The position check
/// matters twice over — a forged future sequence would otherwise park
/// the poll until the counter catches up, and it must never reach the
/// ring's arithmetic at all (review P2).
fn validate_cursor(cursor: &str, ring: &EventRing, expected_tag: u64) -> Result<u64, &'static str> {
    let Some((epoch, seq, tag)) = decode_cursor(cursor) else {
        return Err("invalid cursor — omit `since` to start at now and mint a fresh one");
    };
    if epoch != ring.epoch() {
        return Err(
            "cursor is from a previous daemon boot — the stream restarted; omit `since`, resync state via the read commands, and continue from the fresh cursor",
        );
    }
    if tag != expected_tag {
        // Principal-bound (design-review amendment): a cursor minted
        // under another principal is refused.
        return Err("cursor was minted for a different principal — omit `since` to mint your own");
    }
    if seq > ring.current_seq() {
        // Real cursors are minted from the monotonic counter, so a
        // position ahead of it was never minted by this ring.
        return Err(
            "cursor is ahead of the stream — it was not minted by this ring; omit `since` to start at now",
        );
    }
    Ok(seq)
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
            Some(cursor) => match validate_cursor(cursor, &ring, tag) {
                Ok(seq) => seq,
                Err(message) => return events_error(message),
            },
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
        let scope = McpToolScope::from_actor(actor);

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
                if visible_to_scope(&entry, &scope) && passes_filter(&entry, &filter) {
                    delivered.push(entry.json);
                }
                if delivered.len() >= max_events {
                    break;
                }
            }
            // Return on the first delivered batch (long-poll contract)
            // or on a gap (the caller must resync).
            if !delivered.is_empty() || gap {
                break;
            }
            // A filter can reject a whole raw page while a match sits
            // further along the buffered window — and nothing already
            // buffered will ever fire the notify. Drain to the ring's
            // head before considering any wait (review P2); the window
            // is bounded, so this is at most a few pages.
            if scan_seq < ring.current_seq() {
                continue;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                // Lapse: loop once more for a final read — an event
                // recorded moments ago is delivered instead of being
                // reported as a quiet timeout (the ask-tool ledger
                // discipline); the deadline check above ends the loop
                // right after it.
                continue;
            }
            // Woken: loop back and read from the advanced cursor.
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

    /// Every refusal branch of cursor validation, including the forged
    /// future position (review P2 — it would park the poll and, worse,
    /// must never reach the ring's arithmetic).
    #[test]
    fn cursor_validation_refuses_foreign_stale_and_future_cursors() {
        let ring = EventRing::new();
        ring.push("a".into(), None, "{\"event\":\"a\"}".into());
        let tag = 0x77;
        let ok = encode_cursor(ring.epoch(), 1, tag);
        assert_eq!(validate_cursor(&ok, &ring, tag), Ok(1));
        assert!(validate_cursor("garbage", &ring, tag).is_err());
        let wrong_epoch = encode_cursor(ring.epoch().wrapping_add(1), 1, tag);
        assert!(validate_cursor(&wrong_epoch, &ring, tag)
            .unwrap_err()
            .contains("previous daemon boot"));
        let wrong_principal = encode_cursor(ring.epoch(), 1, tag + 1);
        assert!(validate_cursor(&wrong_principal, &ring, tag)
            .unwrap_err()
            .contains("different principal"));
        let future = encode_cursor(ring.epoch(), u64::MAX, tag);
        assert!(validate_cursor(&future, &ring, tag)
            .unwrap_err()
            .contains("ahead of the stream"));
    }

    /// A filter that rejects whole raw pages must not park the poll
    /// while a match already sits further along the buffered window
    /// (review P2): with wait_s=0 the scan drains to the ring's head
    /// and returns the buried match immediately.
    #[tokio::test]
    async fn filtered_polls_drain_the_buffered_window() {
        let state = crate::mcp::tests::test_state();
        let ring = Arc::new(EventRing::new());
        for i in 0..250 {
            ring.push(
                "session_started".into(),
                Some("sess-x".into()),
                format!("{{\"event\":\"session_started\",\"n\":{i}}}"),
            );
        }
        ring.push(
            "approval_required".into(),
            Some("sess-x".into()),
            "{\"event\":\"approval_required\",\"id\":9}".into(),
        );
        state.write().await.event_ring = Some(ring.clone());
        let server = IntendantServer::new(state, crate::event::EventBus::new());

        let unattributed = crate::access::actor::ActorBinding::unattributed();
        let start = encode_cursor(
            ring.epoch(),
            0,
            cursor_principal_tag(ToolCallerTrust::OwnerSurface, &unattributed),
        );
        let result = server
            .events_tool(
                EventsParams {
                    since: Some(start),
                    wait_s: Some(0),
                    filter: Some("approval_required".into()),
                    max_events: Some(10),
                },
                ToolCallerTrust::OwnerSurface,
                &unattributed,
            )
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(parsed["ok"], true, "{result}");
        let events = parsed["events"].as_array().expect("events array");
        assert_eq!(events.len(), 1, "{result}");
        assert_eq!(events[0]["event"], "approval_required");
        assert_eq!(
            parsed["current_seq"].as_u64(),
            Some(251),
            "cursor drained to the head"
        );
    }

    /// End-to-end session scoping through the tool (security review
    /// P1): an agent-session caller polling the full stream receives
    /// only its gate-bound session's events — another session's events
    /// and sessionless approvals are withheld, while the cursor still
    /// advances past them.
    #[tokio::test]
    async fn agent_session_callers_see_only_their_session_through_the_tool() {
        let state = crate::mcp::tests::test_state();
        let ring = Arc::new(EventRing::new());
        ring.push(
            "turn_started".into(),
            Some("sess-a".into()),
            "{\"event\":\"turn_started\",\"session_id\":\"sess-a\"}".into(),
        );
        ring.push(
            "turn_started".into(),
            Some("sess-b".into()),
            "{\"event\":\"turn_started\",\"session_id\":\"sess-b\"}".into(),
        );
        ring.push(
            "approval_required".into(),
            None,
            "{\"event\":\"approval_required\",\"id\":1}".into(),
        );
        state.write().await.event_ring = Some(ring.clone());
        let server = IntendantServer::new(state, crate::event::EventBus::new());

        let actor = crate::access::actor::ActorBinding::agent_session(
            Some("principal:agent".into()),
            "sess-a".into(),
        );
        let start = encode_cursor(
            ring.epoch(),
            0,
            cursor_principal_tag(ToolCallerTrust::Scoped, &actor),
        );
        let result = server
            .events_tool(
                EventsParams {
                    since: Some(start),
                    wait_s: Some(0),
                    filter: None,
                    max_events: None,
                },
                ToolCallerTrust::Scoped,
                &actor,
            )
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(parsed["ok"], true, "{result}");
        let events = parsed["events"].as_array().expect("events array");
        assert_eq!(events.len(), 1, "{result}");
        assert_eq!(events[0]["session_id"], "sess-a");
        assert_eq!(
            parsed["current_seq"].as_u64(),
            Some(3),
            "the cursor advances past withheld events"
        );
    }

    fn entry(event: &str, session_id: Option<&str>) -> RingEntry {
        RingEntry {
            seq: 1,
            event: event.to_string(),
            session_id: session_id.map(String::from),
            json: format!("{{\"event\":\"{event}\"}}"),
        }
    }

    /// The `event_gap` loss marker always passes a filter — a filtering
    /// caller must still learn it missed events.
    #[test]
    fn filters_keep_gap_markers_and_matching_names() {
        let filter = Some(vec!["approval_required".to_string()]);
        assert!(passes_filter(&entry("approval_required", None), &filter));
        assert!(!passes_filter(&entry("session_started", None), &filter));
        assert!(passes_filter(&entry("event_gap", None), &filter));
        assert!(passes_filter(&entry("anything", None), &None));
    }

    /// Session scoping at delivery (security review P1): an
    /// agent-session caller sees only its gate-bound session's events;
    /// sessionless events fail closed, an unbound scope sees nothing,
    /// the loss marker stays visible, and every other caller class
    /// keeps the daemon-wide view.
    #[test]
    fn agent_session_scope_confines_delivery_to_the_bound_session() {
        let mine = McpToolScope::AgentSession {
            session_id: Some("sess-a"),
        };
        assert!(visible_to_scope(
            &entry("turn_started", Some("sess-a")),
            &mine
        ));
        assert!(!visible_to_scope(
            &entry("turn_started", Some("sess-b")),
            &mine
        ));
        assert!(
            !visible_to_scope(&entry("approval_required", None), &mine),
            "sessionless events fail closed for session-scoped callers"
        );
        assert!(
            visible_to_scope(&entry("event_gap", None), &mine),
            "the loss marker stays visible"
        );
        let unbound = McpToolScope::AgentSession { session_id: None };
        assert!(
            !visible_to_scope(&entry("turn_started", Some("sess-a")), &unbound),
            "a scope whose gate bound no id sees nothing"
        );
        assert!(visible_to_scope(
            &entry("turn_started", Some("sess-b")),
            &McpToolScope::Unrestricted
        ));
    }
}
