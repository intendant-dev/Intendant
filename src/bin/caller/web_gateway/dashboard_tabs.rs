//! Live dashboard-connection registry: which dashboard tabs (and peer
//! daemons) hold a connection to this daemon right now, over which
//! transport lane, and which of them currently hold the voice presence
//! or display input authority.
//!
//! Both event lanes register here at their existing lifecycle seams —
//! the `/ws` accept path in `listener.rs` and the dashboard-control
//! registry's `answer_offer`/`close` pair — keyed by the same ids the
//! presence slot (`ActivePresence.connection_id`) and the input
//! authority map (`DisplayInputHolder`) already carry, so the snapshot
//! joins voice/input ownership at query time instead of mirroring it.
//!
//! Tab identity is client-declared: the SPA mints a per-tab id in
//! `sessionStorage` and sends it on both lanes (`?tab=` on the WS URL,
//! `tab_id` in the control-tunnel offer), so one browser tab holding
//! both lanes groups into one entry client-side. The id is opaque,
//! sanitized on ingest, and grants nothing — it exists purely so the
//! Access pane can say "this tab" and count distinct tabs.
//!
//! Deliberately NOT in the wire payload: the server-internal connection
//! / control-session ids. The input-authority design keeps holder ids
//! off the browser wire (personalized `you|other|unclaimed` instead) —
//! this surface follows that decision and identifies entries by their
//! client-declared tab id, label, and lane only.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DashboardTabLane {
    /// The legacy `/ws` WebSocket event lane.
    LegacyWs,
    /// A dashboard-control (WebRTC datachannel) session.
    ControlTunnel,
}

impl DashboardTabLane {
    fn as_str(self) -> &'static str {
        match self {
            Self::LegacyWs => "ws",
            Self::ControlTunnel => "tunnel",
        }
    }
}

/// One live connection, as registered by its transport edge.
#[derive(Clone, Debug)]
pub(crate) struct DashboardTabConnection {
    pub(crate) lane: DashboardTabLane,
    /// Coarse provenance from the connection's `DashboardControlGrant`:
    /// `"local"` (owner surface), `"client"` (enrolled browser key), or
    /// `"peer"` (federated daemon / delegation lane).
    pub(crate) kind: &'static str,
    /// The grant label (principal label / peer label / "trusted-local").
    pub(crate) label: String,
    /// Client-declared per-tab id (sanitized); `None` when the client
    /// didn't send one (pre-stamp SPA builds, peer daemons, hosted
    /// Connect relays).
    pub(crate) tab_id: Option<String>,
    /// Remote host as observed by the transport, when known.
    pub(crate) remote: Option<String>,
    /// The connection's User-Agent header, when the lane carries one.
    pub(crate) user_agent: Option<String>,
    pub(crate) connected_at_unix_ms: u64,
}

/// Client-declared strings cross a trust boundary: cap and constrain
/// them on ingest so the Access pane never renders unbounded or
/// control-character junk. Tab ids are machine-matched (the SPA mints
/// UUIDs), so the charset is strict; user agents are display-only, so
/// they keep printable ASCII up to a cap.
fn sanitize_tab_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let ok = (8..=64).contains(&trimmed.len())
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then(|| trimmed.to_string())
}

fn sanitize_user_agent(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(200)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

pub(crate) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The registry: live connections keyed by the transport's internal id
/// (the `/ws` `connection_id` or the dashboard-control `session_id` —
/// the same ids the presence slot and authority map hold), plus the two
/// shared-state handles the snapshot joins against.
#[derive(Clone)]
pub(crate) struct DashboardTabsRegistry {
    inner: Arc<Mutex<HashMap<String, DashboardTabConnection>>>,
    active_presence: Arc<Mutex<Option<ActivePresence>>>,
    display_input_authority: Arc<DisplayInputAuthority>,
}

impl DashboardTabsRegistry {
    pub(crate) fn new(
        active_presence: Arc<Mutex<Option<ActivePresence>>>,
        display_input_authority: Arc<DisplayInputAuthority>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            active_presence,
            display_input_authority,
        }
    }

    pub(crate) fn register(&self, id: &str, mut conn: DashboardTabConnection) {
        conn.tab_id = conn.tab_id.as_deref().and_then(sanitize_tab_id);
        conn.user_agent = conn.user_agent.as_deref().and_then(sanitize_user_agent);
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), conn);
    }

    /// Annotate an already-registered connection with its client-declared
    /// tab id (the control-tunnel offer paths learn the id after the
    /// session is registered). No-op for unknown ids or junk values.
    pub(crate) fn note_tab_id(&self, id: &str, tab_id: &str) {
        let Some(clean) = sanitize_tab_id(tab_id) else {
            return;
        };
        if let Some(conn) = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(id)
        {
            conn.tab_id = Some(clean);
        }
    }

    pub(crate) fn unregister(&self, id: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    /// The wire payload: every live connection with voice/input
    /// ownership joined in, ordered oldest-first (deterministic
    /// label/tab tiebreak). Internal connection ids stay internal.
    pub(crate) fn snapshot(&self) -> serde_json::Value {
        let voice_holder: Option<String> = self
            .active_presence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|active| active.connection_id.clone());
        let mut input_by_holder: HashMap<String, Vec<u32>> = HashMap::new();
        {
            let map = self
                .display_input_authority
                .read()
                .unwrap_or_else(|e| e.into_inner());
            for (display_id, holder) in map.iter() {
                let key = match holder {
                    DisplayInputHolder::LocalWs { connection_id, .. } => connection_id.clone(),
                    DisplayInputHolder::DashboardControl { session_id } => session_id.clone(),
                    // Federated holders belong to peer-daemon display
                    // sessions, not to a connection this registry tracks.
                    DisplayInputHolder::FederatedWebRtc { .. } => continue,
                };
                input_by_holder.entry(key).or_default().push(*display_id);
            }
        }

        let mut rows: Vec<(u64, String, serde_json::Value)> = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner
                .iter()
                .map(|(id, conn)| {
                    let mut input_display_ids =
                        input_by_holder.get(id).cloned().unwrap_or_default();
                    input_display_ids.sort_unstable();
                    let tiebreak = format!(
                        "{}|{}|{}",
                        conn.label,
                        conn.tab_id.as_deref().unwrap_or(""),
                        conn.lane.as_str()
                    );
                    let row = serde_json::json!({
                        "lane": conn.lane.as_str(),
                        "kind": conn.kind,
                        "label": conn.label,
                        "tab_id": conn.tab_id,
                        "remote": conn.remote,
                        "user_agent": conn.user_agent,
                        "connected_at_unix_ms": conn.connected_at_unix_ms,
                        "voice_active": voice_holder.as_deref() == Some(id.as_str()),
                        "input_display_ids": input_display_ids,
                    });
                    (conn.connected_at_unix_ms, tiebreak, row)
                })
                .collect()
        };
        rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        serde_json::json!({
            "connections": rows.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>(),
        })
    }
}

/// S6 neutral core for `GET /api/dashboard/tabs` and its
/// `api_dashboard_tabs` tunnel twin.
pub(crate) fn dashboard_tabs_api_response(tabs: &DashboardTabsRegistry) -> ApiResponse {
    ApiResponse::json(200, JsonBody::Value(tabs.snapshot()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_registry() -> DashboardTabsRegistry {
        DashboardTabsRegistry::new(
            Arc::new(Mutex::new(None)),
            Arc::new(DisplayInputAuthority::default()),
        )
    }

    fn conn(lane: DashboardTabLane, label: &str, at: u64) -> DashboardTabConnection {
        DashboardTabConnection {
            lane,
            kind: "local",
            label: label.to_string(),
            tab_id: None,
            remote: None,
            user_agent: None,
            connected_at_unix_ms: at,
        }
    }

    #[test]
    fn register_snapshot_unregister_roundtrip() {
        let reg = empty_registry();
        reg.register(
            "ws-1",
            conn(DashboardTabLane::LegacyWs, "trusted-local", 10),
        );
        reg.register(
            "sess-1",
            conn(DashboardTabLane::ControlTunnel, "trusted-local", 20),
        );
        let snap = reg.snapshot();
        let rows = snap["connections"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        // Oldest first; internal ids never appear anywhere in the payload.
        assert_eq!(rows[0]["lane"], "ws");
        assert_eq!(rows[1]["lane"], "tunnel");
        assert!(!snap.to_string().contains("ws-1"));
        assert!(!snap.to_string().contains("sess-1"));

        reg.unregister("ws-1");
        let snap = reg.snapshot();
        assert_eq!(snap["connections"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn tab_id_and_user_agent_are_sanitized_on_ingest() {
        let reg = empty_registry();
        let mut c = conn(DashboardTabLane::LegacyWs, "trusted-local", 1);
        c.tab_id = Some("  bad id with spaces  ".to_string());
        c.user_agent = Some(format!("Safari\u{7}/605 {}", "x".repeat(400)));
        reg.register("ws-1", c);
        let snap = reg.snapshot();
        let row = &snap["connections"][0];
        assert!(row["tab_id"].is_null());
        let ua = row["user_agent"].as_str().unwrap();
        assert!(ua.len() <= 200);
        assert!(!ua.contains('\u{7}'));
        assert!(ua.starts_with("Safari/605"));
    }

    #[test]
    fn note_tab_id_annotates_only_valid_ids_on_known_connections() {
        let reg = empty_registry();
        reg.register("sess-1", conn(DashboardTabLane::ControlTunnel, "l", 1));
        reg.note_tab_id("sess-1", "not valid!");
        assert!(reg.snapshot()["connections"][0]["tab_id"].is_null());
        reg.note_tab_id("sess-1", "0d9a4c2e-11ab-4c1d-9e2f-aa55bb66cc77");
        assert_eq!(
            reg.snapshot()["connections"][0]["tab_id"],
            "0d9a4c2e-11ab-4c1d-9e2f-aa55bb66cc77"
        );
        // Unknown connection: silently ignored.
        reg.note_tab_id("sess-2", "0d9a4c2e-11ab-4c1d-9e2f-aa55bb66cc77");
        assert_eq!(reg.snapshot()["connections"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn snapshot_joins_voice_and_input_authority_by_internal_id() {
        let active = Arc::new(Mutex::new(None));
        let authority = Arc::new(DisplayInputAuthority::default());
        let reg = DashboardTabsRegistry::new(active.clone(), authority.clone());
        reg.register("ws-1", conn(DashboardTabLane::LegacyWs, "a", 1));
        reg.register("sess-1", conn(DashboardTabLane::ControlTunnel, "b", 2));

        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        *active.lock().unwrap() = Some(ActivePresence {
            connection_id: "ws-1".to_string(),
            direct_tx: tx.clone(),
        });
        {
            let mut map = authority.write().unwrap();
            map.insert(
                7,
                DisplayInputHolder::DashboardControl {
                    session_id: "sess-1".to_string(),
                },
            );
            map.insert(
                3,
                DisplayInputHolder::DashboardControl {
                    session_id: "sess-1".to_string(),
                },
            );
            // A federated holder joins no registry entry and is skipped.
            map.insert(
                9,
                DisplayInputHolder::FederatedWebRtc {
                    federation_connection_id: "f-1".to_string(),
                    session_id: "fs-1".to_string(),
                },
            );
        }

        let snap = reg.snapshot();
        let rows = snap["connections"].as_array().unwrap();
        assert_eq!(rows[0]["voice_active"], true);
        assert_eq!(rows[0]["input_display_ids"].as_array().unwrap().len(), 0);
        assert_eq!(rows[1]["voice_active"], false);
        assert_eq!(
            rows[1]["input_display_ids"],
            serde_json::json!([3u32, 7u32])
        );
    }

    #[test]
    fn api_response_is_the_neutral_ok_envelope() {
        let reg = empty_registry();
        match dashboard_tabs_api_response(&reg) {
            ApiResponse::Json { status, .. } => assert_eq!(status, 200),
            _ => panic!("unexpected response shape"),
        }
    }
}
