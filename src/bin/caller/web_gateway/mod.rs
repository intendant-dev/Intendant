use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::presence::{self, AgentStateSnapshot};
use crate::types::{LogLevel, SessionGoal};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
// Phase 5a.1: the display input authority map is read from a synchronous
// `Fn() -> bool` closure on the WebRTC data-channel input hot path, so
// it can't live behind a `tokio::sync::RwLock` (no `.read().await` from
// sync code).  `StdRwLock` is the local alias to keep that map's type
// distinct at every callsite from the unrelated `tokio::sync::RwLock`
// uses in this file.  All access goes through `unwrap_or_else(|e| e.into_inner())`
// to remain poison-tolerant, matching the rest of the file's std-lock idiom.
use std::sync::RwLock as StdRwLock;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

mod static_assets;
pub(crate) use static_assets::*;

mod http;
pub(crate) use http::*;

mod session_catalog;
pub(crate) use session_catalog::*;

mod routes_files;
pub(crate) use routes_files::*;

mod routes_sessions;
pub(crate) use routes_sessions::*;

mod routes_peers;
pub(crate) use routes_peers::*;

mod routes_access;
pub(crate) use routes_access::*;

mod mcp_gate;
pub(crate) use mcp_gate::*;
mod listener;
pub(crate) use listener::*;
mod dashboard_presence;
pub(crate) use dashboard_presence::*;
mod input_authority;
pub(crate) use input_authority::*;
mod connect_bootstrap;
pub(crate) use connect_bootstrap::*;
mod settings;
pub(crate) use settings::*;
mod peer_requests;
pub(crate) use peer_requests::*;
mod access_gates;
pub(crate) use access_gates::*;
mod agent_card;
pub(crate) use agent_card::*;


/// Monotonically increasing counter for assigning unique peer IDs to WebSocket
/// connections.  Used for WebRTC signaling so that each browser tab gets a
/// stable identity within a display session.
static NEXT_PEER_ID: AtomicU64 = AtomicU64::new(1);
static SESSION_LIST_LIMITED_RESPONSE_CACHE: OnceLock<
    Mutex<HashMap<usize, SessionListResponseCacheEntry>>,
> = OnceLock::new();
static SESSION_LIST_ROW_CACHE: OnceLock<Mutex<HashMap<String, SessionListRowCacheEntry>>> =
    OnceLock::new();
static CODEX_SESSION_LIST_CACHE: OnceLock<Mutex<HashMap<String, CodexSessionListCacheEntry>>> =
    OnceLock::new();
static CODEX_PARENT_USAGE_BASELINE_CACHE: OnceLock<
    Mutex<HashMap<String, CodexParentUsageBaselineCacheEntry>>,
> = OnceLock::new();
static INTENDANT_SESSION_LIST_CACHE: OnceLock<
    Mutex<HashMap<String, IntendantSessionListCacheEntry>>,
> = OnceLock::new();

pub const DEFAULT_PORT: u16 = 8765;

/// Session-specific state that changes when a new agent session starts.
/// Wrapped in `Arc<tokio::sync::RwLock<...>>` so the web gateway can observe
/// session changes without restarting.
pub struct ActiveSessionState {
    /// Stable identity for the long-lived Intendant process. This is distinct
    /// from `session_log`, which may point at a currently active worker session
    /// and may be cleared while the dashboard waits for new tasks.
    pub daemon_session_id: Option<String>,
    pub query_ctx: Option<WebQueryCtx>,
    pub frame_registry: Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    pub session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    pub recording_registry: Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>>,
    pub session_registry: Option<crate::display::SharedSessionRegistry>,
    pub snapshot_dir: Option<PathBuf>,
    pub project_root_for_changes: Option<PathBuf>,
    /// Runtime-only daemon settings that may differ from persisted
    /// `intendant.toml` because of CLI flags such as `--agent` or
    /// `--no-presence`.
    pub runtime_settings: RuntimeSettingsState,
    /// Shared handle to the live `FileWatcher`, used to serve the per-round
    /// history endpoints (GET history, POST rollback/redo/prune). The same
    /// mutex guards snapshot creation so concurrent rollback from the web
    /// gateway and snapshot-on-round-complete can't race.
    pub file_watcher: Option<crate::file_watcher::SharedFileWatcher>,
}

impl ActiveSessionState {
    #[allow(dead_code)]
    pub fn empty() -> SharedActiveSession {
        Arc::new(tokio::sync::RwLock::new(Self {
            daemon_session_id: None,
            query_ctx: None,
            frame_registry: None,
            session_log: None,
            recording_registry: None,
            session_registry: None,
            snapshot_dir: None,
            project_root_for_changes: None,
            runtime_settings: RuntimeSettingsState::default(),
            file_watcher: None,
        }))
    }
}

pub type SharedActiveSession = Arc<tokio::sync::RwLock<ActiveSessionState>>;

#[derive(Clone, Default)]
pub struct RuntimeSettingsState {
    pub external_agent:
        Option<Arc<tokio::sync::RwLock<Option<crate::external_agent::AgentBackend>>>>,
    pub presence_enabled: Option<bool>,
}

/// Context for answering presence tool queries from browser-side live models.
/// Shared across all WebSocket connections (read-only for query tools).
#[derive(Clone)]
pub struct WebQueryCtx {
    pub agent_state: Arc<Mutex<AgentStateSnapshot>>,
    pub project_root: PathBuf,
    pub log_dir: PathBuf,
    pub knowledge_path: PathBuf,
    /// Server-authoritative presence session (event window + checkpoint state).
    pub presence_session: Option<Arc<Mutex<crate::presence::PresenceSession>>>,
    /// Shared context injection queue for mid-task interjections.
    pub context_injection: Option<crate::event::ContextInjectionQueue>,
}


/// Debug state for the voice model, tracked server-side from WebSocket messages.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VoiceDebugState {
    pub connected: bool,
    pub voice_log_count: u32,
    pub last_voice_log: String,
}

/// Voice + WebRTC runtime config sent to the web frontend via `/config`.
///
/// Scoped to *runtime config only* — the voice provider, the active
/// model, audio sample rates, and WebRTC ICE servers. Identity-shaped
/// fields (host label, version, git sha) moved out of `/config` and
/// into the Agent Card served at `/.well-known/agent-card.json`: see
/// [`crate::peer::AgentCard`] and [`crate::peer::AgentCard::local_intendant`].
/// That's the single source of truth for who this daemon is and what
/// it can do, and keeping `/config` narrow makes it less likely that
/// future runtime config additions re-blur the boundary.
#[derive(Clone, Debug, Serialize)]
pub struct WebGatewayConfig {
    pub provider: String,
    pub model: String,
    /// Effective server-side presence state for this running daemon. This is
    /// intentionally runtime-scoped, because `--no-presence` can override the
    /// persisted `[presence] enabled` setting.
    #[serde(default)]
    pub presence_enabled: bool,
    /// Effective external-agent backend selected for this daemon at startup.
    /// The voice provider/model above remain scoped to browser live audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_agent: Option<String>,
    pub input_sample_rate: u32,
    pub output_sample_rate: u32,
    /// Whether server-side transcription is enabled (browser should send user_audio).
    #[serde(default)]
    pub transcription_enabled: bool,
    /// ICE servers for WebRTC peer connections (STUN/TURN).
    /// Empty by default (local-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ice_servers: Vec<crate::display::IceServer>,
    /// Whether the *federated* (peer-to-peer) display path may negotiate
    /// H.264. Default false ⇒ the browser pins VP8 for federation (the
    /// safe default for lossy TURN-relayed paths). When true the browser
    /// prefers H.264, allowing the peer's federated H.264 layer
    /// (quarter-resolution, capped bitrate, periodic IDRs, same-SSRC NACK,
    /// small slices) to be selected. Does NOT affect the *local* DisplaySlot
    /// path, which already defaults codec order. Sourced from
    /// `[webrtc].federation_allow_h264` in intendant.toml.
    #[serde(default)]
    pub federation_allow_h264: bool,
    /// Public peer access-request hardening. This is gateway runtime state,
    /// not browser config, so `/config` intentionally omits it.
    #[serde(skip)]
    pub peer_access_requests: crate::project::PeerAccessRequestConfig,
    /// Experimental Connect rendezvous client config. This is daemon runtime
    /// state, not browser config, so `/config` intentionally omits it.
    #[serde(skip)]
    pub connect: crate::project::ConnectConfig,
}

impl Default for WebGatewayConfig {
    fn default() -> Self {
        Self {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash-native-audio-preview-12-2025".to_string(),
            presence_enabled: false,
            external_agent: None,
            input_sample_rate: 16000,
            output_sample_rate: 24000,
            transcription_enabled: false,
            ice_servers: Vec::new(),
            federation_allow_h264: false,
            peer_access_requests: crate::project::PeerAccessRequestConfig::default(),
            connect: crate::project::ConnectConfig::default(),
        }
    }
}


// Deliberately no Access-Control-Allow-Origin here: API responses are
// same-origin by default. Cross-origin readability is opt-in — the fleet
// Access APIs echo allowlisted origins (`with_fleet_cors`) and the public
// bootstrap surfaces use `with_public_cors`. A blanket wildcard would let
// any website read cert-authenticated responses through a visitor's
// browser (see docs/src/trust-architecture.md).


// ── Persistent session-list index ──
// The per-session caches below already carry exact invalidation
// (len/mtime/ctime/dev/ino fingerprints); persisting the entries makes
// that validity survive daemon restarts, so a cold start re-parses only
// sessions that actually changed instead of every log in every store
// (~tens of seconds on a real corpus). One small JSON file per entry,
// written atomically via tempfile+rename: daemons sharing a HOME can only
// race toward equivalent content, never corrupt an entry. External
// stores (~/.codex, ~/.claude, ~/.gemini) are never written — the index
// mirrors them under ~/.intendant/cache/session_index/.
//
// Entries carry a per-namespace schema stamp: when the value shape changes
// in a way serde would accept silently (a removed or defaulted field),
// bumping the namespace schema turns every old entry into a cache miss so
// it is rebuilt in place under the same slot filename. Entries whose
// source path no longer exists are pruned during the preload sweep —
// deleted sessions otherwise accumulate dead index files forever.


// Stale-while-revalidate: within the TTL a cached list is fresh; past it
// (up to the stale ceiling) the cached body is served IMMEDIATELY and one
// background refresh is kicked, so an interactive dashboard never blocks
// on a rescan. Only a very stale (or absent) entry rebuilds inline.


// ---------------------------------------------------------------------------
// Per-round file snapshot history endpoints
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// File upload endpoints
// ---------------------------------------------------------------------------


// Same-origin by default; see `json_response_body` for the CORS rationale.


/// Check whether it is safe to mutate the project tree (rollback/redo) right
/// now. Returns `Ok(())` if idle, or an `(status_code, body_json)` pair to
/// send back as-is.
fn ensure_idle(
    agent_state: Option<&Arc<Mutex<AgentStateSnapshot>>>,
) -> Result<(), (&'static str, String)> {
    if let Some(state) = agent_state {
        let phase = state.lock().map(|g| g.phase.clone()).unwrap_or_default();
        if !presence::is_agent_idle(&phase) {
            let body = serde_json::json!({
                "error": "agent is busy, stop the turn before rolling back",
                "phase": phase,
            })
            .to_string();
            return Err(("409 Conflict", body));
        }
    }
    Ok(())
}


pub(crate) async fn displays_response_body(
    session_registry: &Option<crate::display::SharedSessionRegistry>,
) -> String {
    let displays = crate::display::enumerate_displays_with_sessions(session_registry).await;
    serde_json::to_string(&displays).unwrap_or_else(|_| "[]".to_string())
}

async fn handle_diagnostics_visual_freshness(
    mut stream: DemuxStream,
    body_text: String,
    request_line: &str,
) {
    // **Phase 0 visual-freshness transcript sink** (task #83).
    // Body is browser-emitted NDJSON (one JSON record per
    // `\n`-terminated line); server appends verbatim to
    // `~/.intendant/diagnostics/visual-freshness/<session>.ndjson`.
    // No parsing or schema validation here — that's
    // browser-side or post-hoc analysis on the
    // transcript. Session id arrives via `?session_id=…`
    // query param; we sanitize aggressively (alnum + `-`
    // + `_` only) and reject anything that collapses
    // empty so a missing param can't accidentally
    // produce a bare-`.ndjson` write.
    use tokio::io::AsyncWriteExt;
    let session_id_raw: String = request_line
        .split('?')
        .nth(1)
        .and_then(|qs| qs.split_whitespace().next())
        .map(|qs| {
            qs.split('&')
                .find_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    if k == "session_id" {
                        Some(v.to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let (status, body) =
        match crate::diagnostics::append_visual_freshness_record(
            &session_id_raw,
            body_text.as_bytes(),
        ) {
            Ok(written) => (
                "200 OK",
                serde_json::json!({"ok": true, "written": written}).to_string(),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => (
                "400 Bad Request",
                serde_json::json!({"error": e.to_string()}).to_string(),
            ),
            Err(e) => (
                "500 Internal Server Error",
                serde_json::json!({"error": e.to_string()}).to_string(),
            ),
        };
    let response = HttpResponse::with_content(status, "application/json", body)
        .header("Access-Control-Allow-Origin", "*")
        .header("Connection", "close")
        .into_string();
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}


#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        pub(crate) fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }


    pub(crate) async fn next_ws_json_matching<S, F>(ws_rx: &mut S, mut matches: F) -> serde_json::Value
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
        F: FnMut(&serde_json::Value) -> bool,
    {
        let mut seen = Vec::new();
        for _ in 0..20 {
            let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
                .await
                .expect("timeout")
                .expect("websocket closed")
                .expect("websocket error");
            let Message::Text(text) = msg else {
                continue;
            };
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            if matches(&json) {
                return json;
            }
            seen.push(json);
        }
        panic!("expected websocket message not found; seen: {seen:?}");
    }

    #[test]
    fn test_default_port() {
        assert_eq!(DEFAULT_PORT, 8765);
    }


    #[test]
    fn list_sessions_joins_external_context_from_debug_thread_log() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        let command_cwd = repo.join(".worktrees").join("feature");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&command_cwd).unwrap();

        let intendant_id = "intendant-wrapper-session";
        let log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join(intendant_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": intendant_id,
                "created_at": "2026-05-17T20:44:00",
                "project_root": repo.to_string_lossy(),
                "task": "Dashboard-started Codex task",
                "status": "running"
            })
            .to_string(),
        )
        .unwrap();

        let codex_id = "019e37ae-dashboard-started";
        let intendant_lines = [
            serde_json::json!({
                "ts": "2026-05-17T20:44:01",
                "event": "debug",
                "message": "Mode: external agent (Codex)"
            }),
            serde_json::json!({
                "ts": "2026-05-17T20:44:01.500Z",
                "event": "session_capabilities",
                "data": {
                    "session_id": intendant_id,
                    "capabilities": {
                        "follow_up": true,
                        "steer": true,
                        "interrupt": true,
                        "codex_thread_actions": ["compact", "fork", "side"],
                        "codex_managed_context": "managed",
                        "codex_command": "/tmp/codex-managed"
                    }
                }
            }),
            serde_json::json!({
                "ts": "2026-05-17T20:44:02",
                "event": "debug",
                "message": format!("External agent thread: {codex_id}")
            }),
        ];
        std::fs::write(
            log_dir.join("session.jsonl"),
            intendant_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let codex_lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": codex_id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": repo.to_string_lossy()
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {
                    "type": "exec_command_end",
                    "cwd": command_cwd.to_string_lossy()
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{codex_id}.jsonl")),
            codex_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions: Vec<serde_json::Value> =
            serde_json::from_str(&list_sessions_from_home(home.path())).unwrap();
        assert!(
            sessions.iter().all(|s| {
                !(s.get("source").and_then(|v| v.as_str()) == Some("intendant")
                    && s.get("session_id").and_then(|v| v.as_str()) == Some(intendant_id))
            }),
            "intendant wrapper should be merged into the native external session row"
        );
        let wrapped = sessions
            .iter()
            .find(|s| {
                s.get("source").and_then(|v| v.as_str()) == Some("codex")
                    && s.get("session_id").and_then(|v| v.as_str()) == Some(codex_id)
            })
            .expect("native Codex session should be listed");
        let expected_project_root = repo.to_string_lossy().to_string();
        let expected_cwd = command_cwd.to_string_lossy().to_string();
        assert_eq!(
            wrapped.get("project_root").and_then(|v| v.as_str()),
            Some(expected_project_root.as_str())
        );
        assert_eq!(
            wrapped.get("cwd").and_then(|v| v.as_str()),
            Some(expected_cwd.as_str())
        );
        assert_eq!(
            wrapped.get("backend_source").and_then(|v| v.as_str()),
            Some("codex")
        );
        assert_eq!(
            wrapped.get("backend_source_label").and_then(|v| v.as_str()),
            Some("Codex")
        );
        assert_eq!(
            wrapped.get("backend_session_id").and_then(|v| v.as_str()),
            Some(codex_id)
        );
        assert_eq!(
            wrapped.get("intendant_session_id").and_then(|v| v.as_str()),
            Some(intendant_id)
        );
        let capabilities = wrapped
            .get("capabilities")
            .and_then(|v| v.as_object())
            .expect("capabilities should be merged from wrapper session");
        assert_eq!(
            capabilities
                .get("codex_managed_context")
                .and_then(|v| v.as_str()),
            Some("managed")
        );
        assert_eq!(
            capabilities.get("codex_command").and_then(|v| v.as_str()),
            Some("/tmp/codex-managed")
        );
        assert_eq!(
            wrapped
                .get("codex_managed_context")
                .and_then(|v| v.as_str()),
            Some("managed")
        );
        assert_eq!(
            wrapped.get("agent_command").and_then(|v| v.as_str()),
            Some("/tmp/codex-managed")
        );
    }


    #[test]
    fn list_codex_sessions_exposes_usage_limited_goal() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("07");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e5c7a-4d05-78d3-a98a-29999cb9898e";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-06-07T15:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-06-07T15:00:00Z",
                    "cwd": "/repo"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-06-07T15:01:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "thread_goal_updated",
                    "threadId": id,
                    "goal": {
                        "threadId": id,
                        "objective": "Keep the Station goal moving",
                        "status": "usageLimited",
                        "tokensUsed": 39449760,
                        "timeUsedSeconds": 93019
                    }
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-06-07T15-00-00-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(
            session.pointer("/goal/objective").and_then(|v| v.as_str()),
            Some("Keep the Station goal moving")
        );
        assert_eq!(
            session.pointer("/goal/status").and_then(|v| v.as_str()),
            Some("usageLimited")
        );
        assert_eq!(
            session
                .pointer("/session_goal/tokens_used")
                .and_then(|v| v.as_u64()),
            Some(39449760)
        );
    }

    #[test]
    fn filtered_codex_sessions_hydrates_goal_outside_list_scan_window() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("07");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e5c7a-4d05-78d3-a98a-29999cb9898e";
        let filler = "x".repeat(4096);
        let mut lines = vec![serde_json::json!({
            "timestamp": "2026-06-07T15:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "timestamp": "2026-06-07T15:00:00Z",
                "cwd": "/repo"
            }
        })];
        for idx in 0..160 {
            lines.push(serde_json::json!({
                "timestamp": format!("2026-06-07T15:00:{idx:02}Z"),
                "type": "noop",
                "payload": { "blob": filler }
            }));
        }
        lines.push(serde_json::json!({
            "timestamp": "2026-06-07T15:05:00Z",
            "type": "event_msg",
            "payload": {
                "type": "thread_goal_updated",
                "threadId": id,
                "goal": {
                    "threadId": id,
                    "objective": "Keep the Station goal moving",
                    "status": "usageLimited",
                    "tokensUsed": 39449760,
                    "timeUsedSeconds": 93019
                }
            }
        }));
        for idx in 0..160 {
            lines.push(serde_json::json!({
                "timestamp": format!("2026-06-07T15:10:{idx:02}Z"),
                "type": "noop",
                "payload": { "blob": filler }
            }));
        }
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-06-07T15-00-00-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let body = list_sessions_from_home(home.path());
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(session.get("goal"), None);

        let filtered = filter_session_list_by_ids_with_codex_goal_hydration(
            home.path(),
            &body,
            &[id.to_string()],
        );
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&filtered).unwrap();
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should still be listed");
        assert_eq!(
            session.pointer("/goal/objective").and_then(|v| v.as_str()),
            Some("Keep the Station goal moving")
        );
        assert_eq!(
            session.pointer("/goal/status").and_then(|v| v.as_str()),
            Some("usageLimited")
        );
    }

    #[test]
    fn targeted_codex_session_list_accepts_prefix_and_hydrates_goal() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("07");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e5c7a-4d05-78d3-a98a-29999cb9898e";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-06-07T15:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-06-07T15:00:00Z",
                    "cwd": "/repo"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-06-07T15:01:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "thread_goal_updated",
                    "threadId": id,
                    "goal": {
                        "threadId": id,
                        "objective": "Keep the Station goal moving",
                        "status": "usageLimited"
                    }
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-06-07T15-00-00-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let body = cached_list_sessions_for_ids_from_home(home.path(), &["019e5c7a".to_string()]);
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.get("session_id").and_then(|v| v.as_str()), Some(id));
        assert_eq!(
            session.pointer("/goal/objective").and_then(|v| v.as_str()),
            Some("Keep the Station goal moving")
        );
    }


    #[test]
    fn external_codex_detail_limit_keeps_usage_limited_goal() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("07");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e5c7a-4d05-78d3-a98a-29999cb9898e";
        let mut lines = vec![
            serde_json::json!({
                "timestamp": "2026-06-07T15:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-06-07T15:00:00Z",
                    "cwd": "/repo"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-06-07T15:01:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "thread_goal_updated",
                    "threadId": id,
                    "goal": {
                        "threadId": id,
                        "objective": "Keep the Station goal moving",
                        "status": "usageLimited",
                        "tokensUsed": 39449760,
                        "timeUsedSeconds": 93019
                    }
                }
            }),
        ];
        for idx in 0..6 {
            lines.push(serde_json::json!({
                "timestamp": format!("2026-06-07T15:02:{idx:02}Z"),
                "type": "event_msg",
                "payload": {
                    "type": "agent_message",
                    "message": format!("later output {idx}")
                }
            }));
        }
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-06-07T15-00-00-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let detail =
            external_session_detail_from_home_with_limit(home.path(), "codex", id, Some(2))
                .expect("external detail should parse");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let entries = detail["entries"].as_array().unwrap();
        let goal = entries
            .iter()
            .find(|entry| entry["event"] == "session_goal")
            .expect("latest goal metadata should survive detail limiting");
        assert_eq!(
            goal.pointer("/data/goal/objective")
                .and_then(|v| v.as_str()),
            Some("Keep the Station goal moving")
        );
        assert_eq!(
            goal.pointer("/data/goal/status").and_then(|v| v.as_str()),
            Some("usageLimited")
        );

        let replay = external_session_activity_replay_from_home(home.path(), "codex", id, 2)
            .expect("external replay should parse");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        assert!(replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["event"] == "session_goal"
                && entry.pointer("/data/goal/status").and_then(|v| v.as_str())
                    == Some("usageLimited")));
    }


    #[test]
    fn list_codex_sessions_exposes_thread_name_separately_from_task() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let codex_dir = home.path().join(".codex");
        let sessions_dir = codex_dir
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37ae-thread-name";
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            serde_json::json!({
                "id": id,
                "updated_at": "2026-05-17T20:44:33Z",
                "thread_name": "Rehydration fix"
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": "/Users/vm/projects/intendant"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Fix activity replay"}
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl")),
            contents,
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(
            session.get("name").and_then(|v| v.as_str()),
            Some("Rehydration fix")
        );
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("Fix activity replay")
        );
    }

    #[test]
    fn codex_index_skeleton_reads_bounded_tail() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let codex_dir = home.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let old_id = "019ef734-old-index-row";
        let recent_id = "019ef734-recent-index-row";
        let old_line = serde_json::json!({
            "id": old_id,
            "updated_at": "2026-06-24T01:00:00Z",
            "thread_name": "Old index row"
        })
        .to_string();
        let recent_line = serde_json::json!({
            "id": recent_id,
            "updated_at": "2026-06-24T02:00:00Z",
            "thread_name": "Recent index row"
        })
        .to_string();
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            format!(
                "{old_line}\n{}\n{recent_line}\n",
                "x".repeat(CODEX_SESSION_INDEX_TAIL_READ_LIMIT as usize + 16)
            ),
        )
        .unwrap();

        let rows = list_codex_index_skeleton_sessions_with_limit(home.path(), 10);
        let ids = rows
            .iter()
            .filter_map(|row| value_str(row, "session_id"))
            .collect::<Vec<_>>();
        assert!(ids.contains(&recent_id.to_string()));
        assert!(!ids.contains(&old_id.to_string()));
    }

    #[test]
    fn quick_session_rows_use_external_wrapper_shape() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let codex_dir = home.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let backend_id = "019ef734-be3f-7882-b1f5-a8ed1dfe12be";
        let wrapper_id = "wrapper-session-for-codex";
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            serde_json::json!({
                "id": backend_id,
                "updated_at": "2026-06-24T02:00:00Z",
                "thread_name": "Wrapped Codex row"
            })
            .to_string()
                + "\n",
        )
        .unwrap();
        let wrapper_dir = home.path().join(".intendant").join("logs").join(wrapper_id);
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        std::fs::write(
            wrapper_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": wrapper_id,
                "created_at": "2026-06-24T01:59:00",
                "task": "Run through Intendant"
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home.path(),
            "codex",
            backend_id,
            wrapper_id,
            &wrapper_dir,
            None,
        )
        .unwrap();

        let mut rows = list_intendant_skeleton_sessions_with_limit(home.path(), 10);
        rows.extend(list_codex_index_skeleton_sessions_with_limit(
            home.path(),
            10,
        ));
        merge_quick_session_rows_with_wrapper_index(home.path(), &mut rows);

        assert_eq!(rows.len(), 1);
        assert_eq!(value_str(&rows[0], "source").as_deref(), Some("codex"));
        assert_eq!(
            value_str(&rows[0], "intendant_session_id").as_deref(),
            Some(wrapper_id)
        );
        assert_eq!(
            value_str(&rows[0], "session_id").as_deref(),
            Some(backend_id)
        );
        assert_eq!(rows[0]["total_bytes"].as_u64(), Some(0));
        assert!(value_str(&rows[0], "path").is_none());
    }

    #[test]
    fn list_codex_sessions_marks_subagent_lineage() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("24");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let parent_id = "019ef734-parent-thread";
        let child_id = "019ef734-child-thread";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-06-24T01:18:11Z",
                "type": "session_meta",
                "payload": {
                    "id": child_id,
                    "timestamp": "2026-06-24T01:18:09Z",
                    "cwd": "/repo",
                    "source": {
                        "subagent": {
                            "thread_spawn": {
                                "parent_thread_id": parent_id,
                                "agent_nickname": "Zeno"
                            }
                        }
                    }
                }
            }),
            serde_json::json!({
                "timestamp": "2026-06-24T01:18:12Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Run child lane"}
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-06-24T01-18-09-{child_id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(child_id))
            .expect("codex subagent session should be listed");
        assert_eq!(
            session.get("parent_session_id").and_then(|v| v.as_str()),
            Some(parent_id)
        );
        assert_eq!(
            session.get("relationship_kind").and_then(|v| v.as_str()),
            Some("subagent")
        );
        assert_eq!(
            session.get("thread_source").and_then(|v| v.as_str()),
            Some("subagent")
        );
        assert_eq!(
            session.get("agent_nickname").and_then(|v| v.as_str()),
            Some("Zeno")
        );
    }


    #[test]
    fn list_codex_sessions_parses_large_prefix_and_daily_usage() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37c5-large-prefix-daily";
        let large_prompt = "x".repeat(EXTERNAL_SESSION_READ_LIMIT as usize + 1024);
        let filler = "y".repeat(EXTERNAL_SESSION_READ_LIMIT as usize + 1024);
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T10:00:00",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T10:00:00",
                    "cwd": "/Users/vm/projects/intendant",
                    "model_provider": "openai",
                    "base_instructions": {"text": large_prompt}
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-05-17T10:00:01",
                "type": "turn_context",
                "payload": {
                    "cwd": "/Users/vm/projects/intendant",
                    "model": "gpt-5.4"
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-05-17T10:00:02",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 80,
                            "cached_input_tokens": 20,
                            "output_tokens": 20,
                            "total_tokens": 100
                        }
                    }
                }
            })
            .to_string(),
            filler,
            serde_json::json!({
                "timestamp": "2026-05-18T10:00:02",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 200,
                            "cached_input_tokens": 50,
                            "output_tokens": 50,
                            "total_tokens": 250
                        }
                    }
                }
            })
            .to_string(),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T10-00-00-{id}.jsonl")),
            lines.join("\n"),
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(session["model"].as_str(), Some("gpt-5.4"));
        assert_eq!(session["prompt_tokens"].as_u64(), Some(200));
        assert_eq!(session["completion_tokens"].as_u64(), Some(50));
        assert_eq!(session["cached_tokens"].as_u64(), Some(50));
        assert_eq!(session["total_tokens"].as_u64(), Some(250));

        let daily = session["daily_usage"].as_array().expect("daily usage");
        let by_day = daily
            .iter()
            .map(|row| {
                (
                    row["day"].as_str().unwrap().to_string(),
                    row["total_tokens"].as_u64().unwrap(),
                )
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(by_day.get("2026-05-17"), Some(&100));
        assert_eq!(by_day.get("2026-05-18"), Some(&150));
    }


    #[test]
    fn codex_transcript_imports_function_call_output() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-function-output";
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_empty",
                        "output": ""
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00.500Z",
                    "type": "response_item",
                    "payload": {
                        "id": "call-item-output",
                        "type": "function_call",
                        "call_id": "call_output",
                        "name": "exec_command",
                        "arguments": "{\"cmd\":\"echo actual\",\"workdir\":\"/tmp\"}"
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:01Z",
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_output",
                        "output": "Chunk ID: abc123\nWall time: 0.0001 seconds\nProcess exited with code 0\nOriginal token count: 8\nOutput:\nTotal output lines: 1\n\nactual command output\n"
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let entries = external_session_entries_from_home(dir.path(), "codex", session_id)
            .expect("codex session should parse");
        let outputs: Vec<_> = entries
            .iter()
            .filter(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("agent_output"))
            .collect();

        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0]["session_id"], session_id);
        assert_eq!(outputs[0]["source"], "codex");
        assert_eq!(outputs[0]["kind"], "agent_output");
        assert_eq!(outputs[0]["output_id"], "call_output");
        assert_eq!(outputs[0]["stdout"], "actual command output\n");
        assert_eq!(outputs[0]["item_id"], "call-item-output");
        assert_eq!(outputs[0]["item_type"], "command_execution");
        assert_eq!(outputs[0]["command_item_id"], "call-item-output");
        assert_eq!(outputs[0]["turn_id"], "turn-unknown");
        assert_eq!(outputs[0]["delivery"], "lossless");
        assert!(outputs[0]["ts_ms"].as_i64().is_some());
        assert_eq!(outputs[0]["command_execution"]["status"], "completed");
        assert_eq!(outputs[0]["command_execution"]["command"], "echo actual");
        assert_eq!(outputs[0]["command_execution"]["cwd"], "/tmp");
        assert_eq!(outputs[0]["thread_item"]["type"], "command_execution");
        assert_eq!(
            outputs[0]["thread_history_change"]["changed_items"][0]["id"],
            "call-item-output"
        );

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let replay_output = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["kind"] == "agent_output")
            .expect("command output should replay as a log entry");
        assert_eq!(replay_output["content"], "actual command output\n");
        assert_eq!(replay_output["output_id"], "call_output");
        assert_eq!(
            replay_output["event_id"],
            format!("external:codex:{session_id}:item:call-item-output")
        );
        assert_eq!(replay_output["delivery"], "lossless");
        assert!(replay_output["ts_ms"].as_i64().is_some());
        assert_eq!(replay_output["item_type"], "command_execution");
        assert_eq!(replay_output["command_execution"]["id"], "call-item-output");
        assert_eq!(
            replay_output["thread_history_change"]["changed_items"][0]["id"],
            "call-item-output"
        );
    }


    #[test]
    fn external_activity_replay_uses_compact_session_transcript() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-e756-7461-9946-34b639448717";
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "id": "msg-user-refresh",
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "What happens on refresh?" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:04Z",
                    "type": "response_item",
                    "payload": {
                        "id": "msg-agent-refresh",
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "The task keeps running." }]
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let normalized = external_session_entries_from_home(dir.path(), "codex", session_id)
            .expect("codex session should parse");
        let normalized_user = normalized
            .iter()
            .find(|entry| entry["content"] == "What happens on refresh?")
            .expect("user entry should parse");
        assert_eq!(normalized_user["item_id"], "msg-user-refresh");
        let normalized_agent = normalized
            .iter()
            .find(|entry| entry["content"] == "The task keeps running.")
            .expect("agent entry should parse");
        assert_eq!(normalized_agent["item_id"], "msg-agent-refresh");

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        assert_eq!(replay["t"], "log_replay");
        assert_eq!(replay["replay_semantics"], EXTERNAL_TRANSCRIPT_SEMANTICS);

        let entries = replay["entries"].as_array().unwrap();
        assert_eq!(entries[0]["event"], "replay_start");
        assert_eq!(
            entries[0]["replay_semantics"],
            EXTERNAL_TRANSCRIPT_SEMANTICS
        );
        assert_eq!(entries[1]["event"], "session_attached");
        assert_eq!(entries[1]["session_id"], session_id);
        assert_eq!(entries[1]["source"], "codex");

        assert!(entries.iter().any(|entry| {
            entry["event"] == "log_entry"
                && entry["session_id"] == session_id
                && entry["level"] == "info"
                && entry["source"] == "user"
                && entry["content"] == "What happens on refresh?"
                && entry["user_turn_index"] == 1
                && entry["item_id"] == "msg-user-refresh"
        }));
        assert!(entries.iter().any(|entry| {
            entry["event"] == "log_entry"
                && entry["session_id"] == session_id
                && entry["level"] == "model"
                && entry["source"] == "codex"
                && entry["content"] == "The task keeps running."
                && entry["item_id"] == "msg-agent-refresh"
        }));
    }





    #[test]
    fn test_web_gateway_config_default() {
        let config = WebGatewayConfig::default();
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
        assert_eq!(config.output_sample_rate, 24000);
    }

    #[test]
    fn test_web_gateway_config_serialize() {
        let config = WebGatewayConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"provider\":\"gemini\""));
        assert!(json.contains("\"input_sample_rate\":16000"));
    }


    #[test]
    fn session_detail_limit_keeps_latest_goal_per_nested_session_id() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let entries = vec![
            serde_json::json!({
                "event": "session_goal",
                "data": {
                    "session_id": "target-session",
                    "goal": {
                        "objective": "old target goal",
                        "status": "active"
                    }
                }
            }),
            serde_json::json!({
                "event": "session_goal",
                "data": {
                    "session_id": "target-session",
                    "goal": {
                        "objective": "latest target goal",
                        "status": "active"
                    }
                }
            }),
            serde_json::json!({
                "event": "session_goal",
                "data": {
                    "session_id": "other-session",
                    "goal": {
                        "objective": "other goal",
                        "status": "active"
                    }
                }
            }),
            serde_json::json!({"event": "model_response", "summary": "tail 1"}),
            serde_json::json!({"event": "model_response", "summary": "tail 2"}),
        ];

        let limited = limited_session_detail_entries(entries, Some(2));
        let goals: Vec<_> = limited
            .iter()
            .filter(|entry| entry["event"] == "session_goal")
            .filter_map(|entry| {
                entry
                    .pointer("/data/goal/objective")
                    .and_then(|v| v.as_str())
            })
            .collect();
        assert_eq!(goals, vec!["latest target goal", "other goal"]);
    }

    #[test]
    fn websocket_bootstrap_replay_omits_context_and_caps_history() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let mut entries = vec![
            serde_json::json!({"event": "replay_start"}),
            serde_json::json!({
                "event": "context_snapshot",
                "raw": {"instructions": "large historical context"}
            }),
            serde_json::json!({
                "event": "session_goal",
                "data": {
                    "session_id": "target-session",
                    "goal": {
                        "objective": "latest goal",
                        "status": "active"
                    }
                }
            }),
        ];
        for n in 0..40 {
            entries.push(serde_json::json!({
                "event": "model_response",
                "summary": format!("tail {n}")
            }));
        }
        entries.push(serde_json::json!({
            "event": "model_response",
            "summary": "x".repeat(WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES + 1024)
        }));

        let limited = prepare_websocket_bootstrap_replay_entries(entries, 10);

        assert!(!limited.iter().any(|entry| {
            entry.get("event").and_then(|v| v.as_str()) == Some("context_snapshot")
        }));
        assert!(limited.len() <= 12);
        assert!(limited.iter().any(|entry| {
            entry
                .pointer("/data/goal/objective")
                .and_then(|v| v.as_str())
                == Some("latest goal")
        }));
        let oversized_summary = limited
            .last()
            .and_then(|entry| entry.get("summary"))
            .and_then(|v| v.as_str())
            .expect("tail summary should remain");
        assert!(oversized_summary.ends_with("..."));
        assert!(oversized_summary.len() < WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES + 16);
    }


    #[test]
    fn external_activity_replay_uses_wrapper_index_for_multiple_codex_attaches() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let session_id = "019e37b2-multiple-wrapper-index";
        let sessions_dir = home.join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "continue indexed thread" }]
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        for (wrapper_id, request_id, request_index) in [
            ("wrapper-before-daemon-restart", "req-before", 1_u64),
            ("wrapper-after-daemon-restart", "req-after", 2_u64),
        ] {
            let wrapper_log_dir = home.join(".intendant").join("logs").join(wrapper_id);
            let mut log = crate::session_log::SessionLog::open(wrapper_log_dir).unwrap();
            log.session_identity(wrapper_id, "codex", session_id);
            log.context_snapshot_for_session(
                Some(session_id),
                "codex",
                "Codex resolved request payload",
                Some(request_id),
                Some(request_index),
                Some(request_index as usize),
                "openai.responses.resolved_request.v1",
                Some(1200 + request_index),
                Some("backend_reported"),
                Some(128_000),
                Some(272_000),
                Some(1),
                &serde_json::json!({
                    "_intendant_context": {
                        "thread_id": session_id,
                        "request_id": request_id,
                        "request_index": request_index
                    },
                    "input": [{"role": "user", "content": request_id}]
                }),
            );
            drop(log);
        }

        let indexed = crate::external_wrapper_index::wrappers_for(home, "codex", session_id);
        assert_eq!(indexed.len(), 2);

        let replay = external_session_activity_replay_from_home(home, "codex", session_id, 80)
            .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let request_ids: HashSet<_> = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["event"] == "context_snapshot")
            .filter_map(|entry| entry["request_id"].as_str())
            .collect();
        assert!(request_ids.contains("req-before"));
        assert!(request_ids.contains("req-after"));

        let sessions: Vec<serde_json::Value> =
            serde_json::from_str(&list_sessions_from_home(home)).unwrap();
        let row = sessions
            .iter()
            .find(|session| session["session_id"] == session_id)
            .expect("codex session row should be present");
        assert_eq!(row["intendant_wrappers"].as_array().map(Vec::len), Some(2));
    }


    // ---- /api/peers endpoint tests ----

    /// Spawn a test gateway with the given peer registry option and
    /// return (port, gateway handle). Condensed helper to keep the
    /// /api/peers tests below compact.
    async fn spawn_test_gateway_with_registry(
        peer_registry: Option<crate::peer::PeerRegistry>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            peer_registry,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        (port, handle)
    }

    /// Fire a raw HTTP request and read the response bytes.
    async fn http_request_bytes(port: u16, request: &str) -> Vec<u8> {
        use tokio::io::AsyncWriteExt;
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;
        response
    }

    /// Fire a raw HTTP request and read the response. Small helper
    /// because the /api/peers tests all make a handful of these.
    pub(crate) async fn http_request(port: u16, request: &str) -> String {
        String::from_utf8_lossy(&http_request_bytes(port, request).await).into_owned()
    }

    #[tokio::test]
    async fn test_api_dashboard_targets_lists_local_root_target() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let resp = http_request(
            port,
            "GET /api/dashboard/targets HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let (head, body) = resp
            .split_once("\r\n\r\n")
            .expect("HTTP response has header/body split");
        assert!(
            head.starts_with("HTTP/1.1 200 OK\r\n"),
            "dashboard targets should return 200: {head}"
        );
        assert!(
            head.contains("Content-Type: application/json"),
            "dashboard targets should return JSON: {head}"
        );

        let payload: serde_json::Value =
            serde_json::from_str(body).expect("dashboard targets body is JSON");
        let targets = payload["targets"].as_array().expect("targets array");
        assert_eq!(targets.len(), 1, "test gateway has only the local target");
        let local = &targets[0];
        assert_eq!(local["local"], true);
        assert_eq!(local["access_domain"], "user_client");
        assert_eq!(local["route"], "current_dashboard");
        assert_eq!(local["effective_role"], "root");
        assert_eq!(local["connected"], true);

        handle.abort();
    }


    #[tokio::test]
    async fn test_api_origin_gate_refuses_foreign_pages() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        // Foreign origin on a fleet path: refused (not in the allowlist).
        let resp = http_request(
            port,
            "GET /api/access/overview HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.example\r\n\r\n",
        )
        .await;
        assert!(
            resp.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "foreign origin should be refused on fleet APIs: {}",
            resp.lines().next().unwrap_or("")
        );
        // Foreign origin on a non-fleet API: also refused.
        let resp = http_request(
            port,
            "GET /api/dashboard/targets HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.example\r\n\r\n",
        )
        .await;
        assert!(
            resp.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "foreign origin should be refused on non-fleet APIs: {}",
            resp.lines().next().unwrap_or("")
        );
        // The daemon's own origin sails through and is echoed on fleet paths.
        let resp = http_request(
            port,
            "GET /api/access/overview HTTP/1.1\r\nHost: localhost\r\nOrigin: http://localhost\r\n\r\n",
        )
        .await;
        assert!(
            resp.starts_with("HTTP/1.1 200 OK\r\n"),
            "own origin should be allowed: {}",
            resp.lines().next().unwrap_or("")
        );
        assert!(
            !resp.contains("Access-Control-Allow-Origin: *"),
            "fleet APIs must never be wildcard-readable"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn test_api_access_overview_lists_current_browser_root_grant() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let resp = http_request(
            port,
            "GET /api/access/overview HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let (head, body) = resp
            .split_once("\r\n\r\n")
            .expect("HTTP response has header/body split");
        assert!(
            head.starts_with("HTTP/1.1 200 OK\r\n"),
            "access overview should return 200: {head}"
        );
        assert!(
            head.contains("Content-Type: application/json"),
            "access overview should return JSON: {head}"
        );

        let payload: serde_json::Value =
            serde_json::from_str(body).expect("access overview body is JSON");
        assert_eq!(payload["schema_version"], 1);
        assert_eq!(payload["scope"]["kind"], "local_daemon");
        assert_eq!(payload["targets"].as_array().expect("targets").len(), 1);

        // Trusted local requests resolve through the shared IAM evaluator to
        // an actual root-session principal; the overview must report that
        // real subject rather than a synthetic "current browser" placeholder.
        let principals = payload["principals"].as_array().expect("principals");
        assert!(
            principals
                .iter()
                .any(|p| p["id"] == "principal:root:dashboard" && p["kind"] == "root_session"),
            "current root session principal should be present: {principals:?}"
        );
        let grants = payload["grants"].as_array().expect("grants");
        assert!(
            grants
                .iter()
                .any(|grant| grant["kind"] == "user_client_root"
                    && grant["role"] == "root"
                    && grant["policy_id"] == "policy:root"),
            "current browser root grant should be present"
        );
        let policies = payload["policies"].as_array().expect("policies");
        assert!(
            policies
                .iter()
                .any(|policy| policy["id"] == "policy:peer-profile"
                    && policy["status"] == "enforced"),
            "peer profile policy should be visible in the overview"
        );
        let permissions = payload["permissions"].as_array().expect("permissions");
        for expected in [
            "access.inspect",
            "access.manage",
            "peer.inspect",
            "peer.manage",
        ] {
            assert!(
                permissions
                    .iter()
                    .any(|permission| permission["id"].as_str() == Some(expected)),
                "{expected} permission should be visible in the overview"
            );
        }
        assert_eq!(
            payload["iam"]["capabilities"]["enforce_user_client_grants"],
            true
        );
        assert_eq!(
            payload["iam"]["capabilities"]["enforce_root_and_peer_grants"],
            true
        );
        assert_eq!(
            payload["iam"]["enforcement"]["principal_binding"],
            "root_peer_and_local_user_client"
        );

        handle.abort();
    }


    /// End-to-end exercise of the static-asset arms through a real
    /// gateway socket: exact-path routing (the `/api/...?path=<asset>`
    /// shadowing regression), conditional requests, gzip negotiation,
    /// the `?v=` cache policy, and HEAD.
    #[tokio::test]
    async fn test_static_asset_serving_end_to_end() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;

        // Exact path serves the wasm with ETag + CORS + revalidation
        // caching (no current `?v=` buster on the request).
        let resp = http_request_bytes(
            port,
            "GET /wasm-station/station_web_bg.wasm HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "got: {head}");
        assert!(head.contains("Content-Type: application/wasm\r\n"));
        assert!(head.contains("Access-Control-Allow-Origin: *\r\n"));
        assert!(head.contains("Cache-Control: no-cache, must-revalidate\r\n"));
        assert_eq!(&resp[split..], WASM_STATION_BIN);
        let etag_line = head
            .lines()
            .find(|l| l.starts_with("ETag: "))
            .expect("ETag header on asset response")
            .to_string();

        // The same asset path mentioned inside an API query parameter is
        // no longer shadowed by the asset arm: /api/fs/stat answers JSON.
        let resp = http_request(
            port,
            "GET /api/fs/stat?path=/wasm-station/station_web_bg.wasm HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let head = resp.split("\r\n\r\n").next().unwrap_or("");
        assert!(
            head.contains("Content-Type: application/json"),
            "fs API must answer JSON, not the wasm asset; got: {head}"
        );

        // Conditional revalidation: matching If-None-Match → 304, no body.
        let etag = etag_line.trim_start_matches("ETag: ").trim();
        let req = format!(
            "GET /wasm-station/station_web_bg.wasm HTTP/1.1\r\nHost: localhost\r\nIf-None-Match: {etag}\r\n\r\n"
        );
        let resp = http_request_bytes(port, &req).await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();
        assert!(
            head.starts_with("HTTP/1.1 304 Not Modified\r\n"),
            "got: {head}"
        );
        assert!(head.contains(&etag_line));
        assert!(head.contains("Access-Control-Allow-Origin: *\r\n"));
        assert_eq!(resp.len(), split, "304 must carry no body");

        // Current-version buster + gzip: immutable caching and a gzip
        // body that round-trips to the original bytes.
        let req = format!(
            "GET /wasm-station/station_web_bg.wasm?v={} HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip, deflate\r\n\r\n",
            asset_version()
        );
        let resp = http_request_bytes(port, &req).await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();
        assert!(head.contains("Cache-Control: public, max-age=31536000, immutable\r\n"));
        assert!(head.contains("Content-Encoding: gzip\r\n"));
        assert!(head.contains("Vary: Accept-Encoding\r\n"));
        use std::io::Read as _;
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(&resp[split..])
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, WASM_STATION_BIN);

        // HEAD: status + headers only.
        let resp = http_request_bytes(
            port,
            "HEAD /wasm-station/station_web_bg.wasm HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "got: {head}");
        assert_eq!(resp.len(), split, "HEAD must carry no body");

        handle.abort();
    }

    #[tokio::test]
    async fn test_dashboard_fs_read_serves_file_bytes() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("download.txt");
        std::fs::write(&file, b"download through dashboard").unwrap();
        let req = format!(
            "GET /api/fs/read?path={} HTTP/1.1\r\nHost: localhost\r\n\r\n",
            file.display()
        );

        let resp = http_request_bytes(port, &req).await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();

        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "got: {head}");
        assert!(
            head.contains("Content-Type: text/plain; charset=utf-8\r\n"),
            "got: {head}"
        );
        assert!(head.contains("Accept-Ranges: bytes\r\n"), "got: {head}");
        assert!(
            head.contains("Content-Disposition: attachment; filename=\"download.txt\"\r\n"),
            "got: {head}"
        );
        assert_eq!(&resp[split..], b"download through dashboard");

        handle.abort();
    }

    #[tokio::test]
    async fn test_dashboard_fs_read_serves_byte_ranges() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("download.txt");
        std::fs::write(&file, b"download through dashboard").unwrap();
        let req = format!(
            "GET /api/fs/read?path={} HTTP/1.1\r\nHost: localhost\r\nRange: bytes=9-15\r\n\r\n",
            file.display()
        );

        let resp = http_request_bytes(port, &req).await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();

        assert!(
            head.starts_with("HTTP/1.1 206 Partial Content\r\n"),
            "got: {head}"
        );
        assert!(
            head.contains("Content-Range: bytes 9-15/26\r\n"),
            "got: {head}"
        );
        assert!(head.contains("Accept-Ranges: bytes\r\n"), "got: {head}");
        assert_eq!(&resp[split..], b"through");

        handle.abort();
    }

    #[tokio::test]
    async fn test_dashboard_fs_read_rejects_unsatisfiable_byte_range() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("download.txt");
        std::fs::write(&file, b"download").unwrap();
        let req = format!(
            "GET /api/fs/read?path={} HTTP/1.1\r\nHost: localhost\r\nRange: bytes=99-100\r\n\r\n",
            file.display()
        );

        let resp = http_request_bytes(port, &req).await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();
        let body = String::from_utf8_lossy(&resp[split..]).into_owned();

        assert!(
            head.starts_with("HTTP/1.1 416 Range Not Satisfiable\r\n"),
            "got: {head}"
        );
        assert!(head.contains("Content-Range: bytes */8\r\n"), "got: {head}");
        assert!(body.contains("range is not satisfiable"), "body: {body}");

        handle.abort();
    }

    /// Same as `spawn_test_gateway_with_registry` but also wires an
    /// inbound bearer token. Used by the federation auth tests.
    async fn spawn_test_gateway_with_auth(
        peer_registry: Option<crate::peer::PeerRegistry>,
        bearer_token: Option<String>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            peer_registry,
            Vec::new(),
            bearer_token,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        (port, handle)
    }

    /// Spawn a gateway with a self-signed TLS acceptor wired in (strict
    /// HTTPS/WSS mode) plus an optional inbound bearer token. Used by the
    /// strict-TLS demux tests and the TLS variant of the /ws bearer test
    /// (audit F2), which only manifests over TLS — rustls buffers the
    /// response ciphertext, so a missing flush truncates it to empty.
    async fn spawn_test_gateway_tls(
        bearer_token: Option<String>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        spawn_test_gateway_tls_with_client_cert_requirement(bearer_token, false).await
    }

    async fn spawn_test_gateway_tls_with_client_cert_requirement(
        bearer_token: Option<String>,
        tls_client_cert_required: bool,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Self-signed cert with localhost / 127.0.0.1 in the SAN list, the
        // same construction the production `--tls` self-signed path uses.
        let acceptor = crate::web_tls::build_acceptor(&crate::web_tls::TlsCertSource::SelfSigned {
            bind_ip: Some("127.0.0.1".parse().unwrap()),
            hostname: None,
        })
        .expect("self-signed acceptor builds");
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            bearer_token,
            crate::peer::AuthRequirements::none(),
            tls_client_cert_required,
            Some(acceptor),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        (port, handle)
    }

    /// Test-only `ServerCertVerifier` that accepts any certificate. The
    /// gateway serves a self-signed cert with no chain to a trust anchor,
    /// so a real verifier would reject it; tests only care that the bytes
    /// flow over an encrypted channel, not that the cert is trusted.
    /// Signature verification still delegates to the ring provider so the
    /// handshake's signed-transcript check is genuine.
    #[derive(Debug)]
    struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

    impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    /// Fire a raw request over a TLS client connection to a `--tls` gateway
    /// and read the full decrypted response. The TLS analogue of
    /// `http_request`: connects with `AcceptAnyCert` (the gateway's cert is
    /// self-signed), writes the request as cleartext into the TLS session,
    /// and reads to EOF. Returns the decrypted bytes as a lossy string.
    async fn https_request(port: u16, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let tcp = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let mut tls = connector.connect(server_name, tcp).await.unwrap();
        tls.write_all(request.as_bytes()).await.unwrap();
        // Read to EOF under one generous deadline. The old 2s timeout with
        // its result discarded turned this into a load lottery: ~2.8 MB of
        // dashboard over local TLS can exceed 2s on a busy CI box, and the
        // caller then trips the body-truncation assertion (the dell flake
        // tax). Both callers expect complete responses terminated by the
        // server closing the connection, so read_to_end is the right shape
        // — the deadline only bounds a hung server.
        let mut response = Vec::new();
        if tokio::time::timeout(
            tokio::time::Duration::from_secs(30),
            tls.read_to_end(&mut response),
        )
        .await
        .is_err()
        {
            eprintln!(
                "https_request: read timed out after 30s with {} bytes buffered",
                response.len()
            );
        }
        String::from_utf8_lossy(&response).into_owned()
    }



    /// Routes served by the legacy dispatch chain (the non-API surface:
    /// connect bootstrap, recordings, frames, debug, config, static/SPA)
    /// must never match the table — a route is served by exactly one of
    /// the two.
    #[test]
    fn route_table_does_not_shadow_legacy_chain_routes() {
        let legacy_served: &[(&str, &str)] = &[
            ("GET", "/connect/bootstrap"),
            ("GET", "/connect/status"),
            ("POST", "/connect/dashboard/offer"),
            ("POST", "/connect/dashboard/ice"),
            ("POST", "/connect/dashboard/close"),
            ("GET", "/frames/f1"),
            ("POST", "/session"),
            ("GET", "/recordings"),
            ("GET", "/recordings/stream1/meta"),
            ("GET", "/debug"),
            ("GET", "/config"),
            ("GET", "/.well-known/agent-card.json"),
            ("GET", "/index.html"),
            ("GET", "/"),
        ];
        for (method, path) in legacy_served {
            assert!(
                crate::gateway_routes::match_route(method, path).is_none(),
                "{method} {path} is still served by the legacy chain but \
                 matches the route table — port the family (removing its \
                 chain arm) before declaring it, then move it out of this \
                 list",
            );
        }
    }


    // -----------------------------------------------------------------
    // End-to-end: federation REST auth enforcement
    // -----------------------------------------------------------------

    /// With `inbound_bearer_token` configured, a federation request
    /// without an Authorization header is rejected 401.
    #[tokio::test]
    async fn test_federation_endpoint_rejects_missing_bearer() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        // Request without auth — should 401, NOT pass through to the
        // 503-no-registry response that would happen otherwise.
        let resp = http_request(port, "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(resp.contains("missing Authorization"));
        assert!(
            resp.contains("WWW-Authenticate: Bearer"),
            "WWW-Authenticate header signals the auth scheme"
        );
        handle.abort();
    }

    /// Wrong bearer token → 401 with "invalid bearer token".
    #[tokio::test]
    async fn test_federation_endpoint_rejects_wrong_bearer() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(
            port,
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n",
        )
        .await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(resp.contains("invalid bearer"));
        handle.abort();
    }

    /// Correct bearer token → request flows through to the normal
    /// handler (which then returns 503 because no registry was
    /// configured — proves auth passed and dispatch ran).
    #[tokio::test]
    async fn test_federation_endpoint_accepts_correct_bearer() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(
            port,
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer test-token\r\n\r\n",
        )
        .await;
        // Auth passed; handler returned its 503 (no registry).
        assert!(
            resp.contains("503"),
            "expected 503 (auth passed, registry missing), got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    /// /config is exempt — even when bearer is required for
    /// federation endpoints, the dashboard bootstrap continues to work
    /// without auth. This is how the dashboard remains usable on the
    /// loopback / trusted-network case where the operator has set a
    /// bearer for WAN federation.
    #[tokio::test]
    async fn test_config_endpoint_unauthenticated_when_bearer_set() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(port, "GET /config HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(
            resp.contains("200 OK"),
            "config should serve unauthenticated, got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    #[tokio::test]
    async fn test_favicon_routes_serve_png() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;

        for path in ["/icon-128.png", "/favicon.ico"] {
            let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n");
            let resp = http_request_bytes(port, &request).await;
            let response_str = String::from_utf8_lossy(&resp);
            assert!(
                response_str.starts_with("HTTP/1.1 200 OK"),
                "expected 200 for {path}, got: {response_str}"
            );
            assert!(
                response_str.contains("Content-Type: image/png"),
                "expected PNG content type for {path}, got: {response_str}"
            );
            assert!(
                !response_str.contains("<!DOCTYPE html>"),
                "{path} should not fall through to app HTML"
            );

            let body_offset = resp
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|idx| idx + 4)
                .expect("HTTP response should contain a body separator");
            assert!(
                resp[body_offset..].starts_with(b"\x89PNG\r\n\x1a\n"),
                "{path} should serve PNG bytes"
            );
        }

        handle.abort();
    }


    /// Real /ws upgrade through `spawn_test_gateway_with_auth`:
    /// connecting without a token gets a plain HTTP 401 *before* the
    /// WebSocket handshake completes — the dashboard sees a 401 page,
    /// not a successful upgrade then immediate close.
    #[tokio::test]
    async fn test_ws_upgrade_rejects_missing_bearer() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("ws-token".into())).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(
            resp.contains("WWW-Authenticate: Bearer"),
            "WWW-Authenticate signals scheme"
        );
        // Critically, the upgrade did NOT complete.
        assert!(
            !resp.contains("101 Switching Protocols"),
            "must reject before WS handshake completes"
        );
        handle.abort();
    }

    /// The same /ws-without-token rejection, but over a real TLS connection
    /// (audit F2). The /ws bearer-reject arm writes the 401 then returns,
    /// dropping the stream; over TLS the 401's ciphertext can sit in the
    /// rustls session buffer and be discarded on an abortive close, so the
    /// client reads an *empty* response instead of the 401. The
    /// `finalize_http_stream` flush+shutdown on that arm closes the session
    /// cleanly (close_notify + FIN) so the response always lands. This test
    /// exercises the TLS path end to end; it's the cross-platform companion
    /// to the plain-TCP `test_ws_upgrade_rejects_missing_bearer` and is the
    /// regression net for the empty-response symptom on platforms whose
    /// socket close discards queued TX (e.g. Windows).
    #[tokio::test]
    async fn test_ws_upgrade_rejects_missing_bearer_over_tls() {
        let (port, handle) = spawn_test_gateway_tls(Some("ws-token".into())).await;
        let resp = https_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(
            !resp.is_empty(),
            "TLS 401 must not be truncated to empty (audit F2)"
        );
        assert!(resp.contains("401"), "expected 401 over TLS, got: {resp}");
        assert!(
            resp.contains("WWW-Authenticate: Bearer"),
            "WWW-Authenticate signals scheme"
        );
        assert!(
            !resp.contains("101 Switching Protocols"),
            "must reject before WS handshake completes"
        );
        handle.abort();
    }

    /// Strict TLS: with a TLS acceptor configured, a *cleartext* HTTP
    /// connection to the secure port is refused with a 426 Upgrade Required
    /// hint and closed — never served over plain HTTP (audit F3 "no
    /// unencrypted traffic"). Uses a plain `http_request` (no TLS) against
    /// the TLS gateway.
    #[tokio::test]
    async fn test_strict_tls_rejects_cleartext_http() {
        let (port, handle) = spawn_test_gateway_tls(None).await;
        let resp = http_request(port, "GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(
            resp.contains("426"),
            "cleartext HTTP to a --tls gateway must get 426, got: {resp}"
        );
        assert!(
            !resp.contains("200 OK"),
            "must not serve the dashboard over cleartext, got: {resp}"
        );
        handle.abort();
    }

    /// Strict TLS: a cleartext WebSocket upgrade to the secure port is
    /// likewise refused (426) and never upgraded — the WS-over-cleartext
    /// path is closed off the same way as plain HTTP.
    #[tokio::test]
    async fn test_strict_tls_rejects_cleartext_ws() {
        let (port, handle) = spawn_test_gateway_tls(None).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("426"),
            "cleartext WS to a --tls gateway must get 426, got: {resp}"
        );
        assert!(
            !resp.contains("101 Switching Protocols"),
            "must not upgrade a cleartext WS on the secure port, got: {resp}"
        );
        handle.abort();
    }

    /// Managed child agents connect to Intendant's Streamable HTTP MCP
    /// endpoint over loopback. That local control channel must continue to
    /// work when the operator enables TLS/mTLS for the dashboard; otherwise
    /// child CLIs cannot report tool calls or receive session-scoped MCP
    /// context. The exception is path- and peer-scoped to loopback `/mcp`.
    #[tokio::test]
    async fn test_strict_tls_allows_loopback_cleartext_mcp_when_mtls_required() {
        let (port, handle) = spawn_test_gateway_tls_with_client_cert_requirement(None, true).await;
        let token = loopback_mcp_auth_token();
        let resp = http_request(
            port,
            &format!(
                "POST /mcp?session_id=child&mcp_token={token} HTTP/1.1\r\n\
                 Host: localhost\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: 2\r\n\
                 \r\n\
                 {{}}"
            ),
        )
        .await;
        assert!(
            resp.contains("503 Service Unavailable"),
            "loopback cleartext /mcp should reach the MCP handler, got: {resp}"
        );
        assert!(
            !resp.contains("426"),
            "loopback cleartext /mcp must not be rejected by strict TLS, got: {resp}"
        );
        assert!(
            !resp.contains("mTLS client certificate required"),
            "loopback cleartext /mcp must not be rejected by dashboard mTLS, got: {resp}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn test_strict_tls_rejects_loopback_cleartext_mcp_without_token() {
        let (port, handle) = spawn_test_gateway_tls_with_client_cert_requirement(None, true).await;
        let resp = http_request(
            port,
            "POST /mcp?session_id=child HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Type: application/json\r\n\
             Content-Length: 2\r\n\
             \r\n\
             {}",
        )
        .await;
        assert!(
            resp.contains("426"),
            "loopback cleartext /mcp without token should stay on the strict TLS reject path, got: {resp}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn test_strict_tls_rejects_browser_origin_cleartext_mcp() {
        let (port, handle) = spawn_test_gateway_tls_with_client_cert_requirement(None, true).await;
        let token = loopback_mcp_auth_token();
        let resp = http_request(
            port,
            &format!(
                "POST /mcp?session_id=child&mcp_token={token} HTTP/1.1\r\n\
                 Host: localhost\r\n\
                 Origin: https://example.test\r\n\
                 Content-Type: text/plain\r\n\
                 Content-Length: 2\r\n\
                 \r\n\
                 {{}}"
            ),
        )
        .await;
        assert!(
            resp.contains("426"),
            "browser-origin cleartext /mcp should stay on the strict TLS reject path, got: {resp}"
        );
        assert!(
            !resp.contains("503 Service Unavailable"),
            "browser-origin cleartext /mcp must not reach the MCP handler, got: {resp}"
        );
        handle.abort();
    }

    /// Strict TLS sanity + truncation guard: a *real* TLS request to the
    /// secure port serves the full dashboard (the rejection above is
    /// specific to cleartext, not a blanket closure). The body-length
    /// assertion guards the audit-F2 truncation class: the ~871 KB
    /// `app.html` far exceeds one synchronous rustls record, so a missing
    /// `finalize_http_stream` flush+shutdown can drop the buffered tail and
    /// truncate the body. Whether that manifests is platform-dependent
    /// (Windows' abortive socket close discards queued TX; macOS loopback
    /// happens to drain it), so this is a cross-platform regression net —
    /// strongest on the Windows build. We assert the decrypted body length
    /// matches `Content-Length` and that the closing `</html>` survived.
    #[tokio::test]
    async fn test_strict_tls_serves_https() {
        let (port, handle) = spawn_test_gateway_tls(None).await;
        let resp = https_request(port, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(
            resp.contains("200 OK"),
            "HTTPS request to a --tls gateway should serve the dashboard, got first 200 bytes: {}",
            &resp.chars().take(200).collect::<String>()
        );
        // Body must arrive intact, not truncated mid-buffer (audit F2).
        let content_length: usize = resp
            .split("\r\n")
            .find_map(|line| {
                line.strip_prefix("Content-Length: ")
                    .and_then(|v| v.trim().parse().ok())
            })
            .expect("response carries a Content-Length");
        let body = resp
            .split_once("\r\n\r\n")
            .map(|(_, b)| b)
            .expect("response has a header/body separator");
        assert_eq!(
            body.len(),
            content_length,
            "TLS body truncated: got {} bytes, Content-Length promised {content_length}",
            body.len()
        );
        assert!(
            body.contains("</html>"),
            "TLS body must include the closing </html> (not cut off mid-record)"
        );
        handle.abort();
    }

    /// The dispatch chain routes on parsed `(method, path)`, matching the
    /// IAM/Origin gates. The old `request_line.contains(...)` dispatch let
    /// a request whose path merely EMBEDDED an API route reach its handler
    /// while the gates — which classify by parsed path — never saw it as
    /// that route (`POST /x/api/api-keys` reached the API-key writer).
    #[tokio::test]
    async fn dispatch_routes_on_parsed_paths_not_substrings() {
        let (port, handle) = spawn_test_gateway_with_auth(None, None).await;

        // Path embedding /api/api-keys must NOT reach the API-key writer;
        // it falls through to the SPA shell like any unknown route.
        let resp = http_request(
            port,
            "POST /x/api/api-keys HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert!(
            resp.contains("Content-Type: text/html"),
            "off-path api-keys request must fall through, got: {}",
            &resp.chars().take(120).collect::<String>()
        );

        // The exact route still reaches the writer (a JSON responder).
        let resp = http_request(
            port,
            "POST /api/api-keys HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert!(
            resp.contains("Content-Type: application/json"),
            "exact api-keys route must reach the writer, got: {}",
            &resp.chars().take(120).collect::<String>()
        );

        // A query string mentioning another route must not shadow the
        // requested one. /api/project-root dispatches well before /debug,
        // so under substring routing this query would have answered as
        // project-root; parsed routing serves the /debug state object.
        // (Both are cheap and machine-independent, unlike the session
        // list, which scans the real home directory.)
        let resp = http_request(
            port,
            "GET /debug?note=/api/project-root HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("\"agent_state\"") && !resp.contains("\"project_root\""),
            "query mention must not shadow the routed path, got: {}",
            &resp.chars().take(200).collect::<String>()
        );

        // Look-alike longer paths are not the route.
        let resp = http_request(port, "GET /api/sessionsfoo HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(
            resp.contains("Content-Type: text/html"),
            "look-alike path must fall through, got: {}",
            &resp.chars().take(120).collect::<String>()
        );

        // Per-file diff subpaths ARE the route (regression: the parsed-path
        // refactor briefly matched only the exact list endpoint, dropping
        // /api/session/current/changes/{path}).
        let resp = http_request(
            port,
            "GET /api/session/current/changes/src/main.rs HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(
            !resp.contains("Content-Type: text/html"),
            "per-file changes subpath must hit the changes handler, got: {}",
            &resp.chars().take(200).collect::<String>()
        );

        handle.abort();
    }

    /// /ws with a matching Authorization header completes the upgrade
    /// (101 Switching Protocols). This is the daemon-to-daemon path
    /// that IntendantWsTransport uses.
    #[tokio::test]
    async fn test_ws_upgrade_accepts_matching_authorization_header() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("ws-token".into())).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Authorization: Bearer ws-token\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("101 Switching Protocols"),
            "expected upgrade success, got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    /// /ws with `?token=` query parameter completes the upgrade. This
    /// is the dashboard-browser path (browsers can't set arbitrary
    /// headers on `WebSocket` opens, so the token rides on the URL).
    #[tokio::test]
    async fn test_ws_upgrade_accepts_matching_query_token() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("ws-token".into())).await;
        let resp = http_request(
            port,
            "GET /ws?token=ws-token HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("101 Switching Protocols"),
            "expected upgrade success, got: {resp}"
        );
        handle.abort();
    }

    /// /ws with no token still works when the gateway has no bearer
    /// configured (the common case for trusted-LAN deployments).
    #[tokio::test]
    async fn test_ws_upgrade_accepts_when_no_bearer_configured() {
        let (port, handle) = spawn_test_gateway_with_auth(None, None).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("101 Switching Protocols"),
            "expected upgrade success, got: {resp}"
        );
        handle.abort();
    }

    /// /.well-known/agent-card.json is exempt — discovery must work
    /// before any auth handshake. Connecting peers fetch the card to
    /// see what auth they need to satisfy.
    #[tokio::test]
    async fn test_agent_card_endpoint_unauthenticated_when_bearer_set() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(
            port,
            "GET /.well-known/agent-card.json HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("200 OK"),
            "agent card should serve unauthenticated, got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    /// `GET /api/peers` returns 503 when the web gateway was spawned
    /// without a peer registry. This lets the dashboard distinguish
    /// "peers not configured" from "no peers yet" and render
    /// differently.
    #[tokio::test]
    async fn test_api_peers_returns_503_without_registry() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let resp = http_request(port, "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(resp.contains("503"), "expected 503, got: {resp}");
        assert!(resp.contains("peer registry not configured"));
        handle.abort();
    }



    /// `GET /api/peers` on a registry with no peers returns
    /// `{"peers":[]}`. Baseline for the list endpoint shape.
    #[tokio::test]
    async fn test_api_peers_list_empty_registry() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;
        let resp = http_request(port, "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(resp.contains("200 OK"));
        // Split body from headers.
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert_eq!(body.trim(), r#"{"peers":[]}"#);
        handle.abort();
    }

    /// End-to-end: spawn a "target" gateway (gateway A) and a
    /// "dashboard" gateway (gateway B) with a peer registry. POST
    /// A's card URL to B's /api/peers. Assert the peer is added,
    /// GET /api/peers shows it, DELETE removes it. This exercises
    /// the full path from HTTP request through PeerRegistry,
    /// IntendantWsTransport, the Agent Card fetch, WebSocket
    /// connect, and event drain.
    #[tokio::test]
    async fn test_api_peers_add_list_remove_end_to_end() {
        // Gateway A: the target peer this dashboard will federate with.
        let (target_port, target_handle) = spawn_test_gateway_with_registry(None).await;

        // Gateway B: the dashboard, with its own peer registry.
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(64);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (dash_port, dash_handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        // POST A's Agent Card URL to B's /api/peers.
        let card_url = format!("http://127.0.0.1:{target_port}/.well-known/agent-card.json");
        let body = serde_json::json!({"card_url": card_url}).to_string();
        let req = format!(
            "POST /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "add failed: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        let peer_id = parsed["peer_id"]
            .as_str()
            .expect("peer_id missing")
            .to_string();
        assert!(peer_id.starts_with("intendant:"));

        // GET /api/peers should now show the added peer.
        let list_resp = http_request(
            dash_port,
            "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(list_resp.contains("200 OK"));
        let list_body = list_resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let list: serde_json::Value = serde_json::from_str(list_body).unwrap();
        let peers_arr = list["peers"].as_array().unwrap();
        assert_eq!(peers_arr.len(), 1);
        assert_eq!(peers_arr[0]["id"].as_str().unwrap(), peer_id);
        // The "id" field should match the peer_id returned from POST.
        // The "version" should be the local build's version.
        assert_eq!(
            peers_arr[0]["version"].as_str().unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        // The dashboard panel rebuild relies on `ws_url` being
        // present so the browser can open a secondary WASM
        // connection without re-fetching the card. Guard against
        // the field being dropped or renamed.
        let ws_url = peers_arr[0]["ws_url"]
            .as_str()
            .expect("ws_url field must be present in the API response");
        assert!(
            ws_url.starts_with("ws://") && ws_url.ends_with("/ws"),
            "ws_url should be a native Intendant WebSocket URL: {ws_url}"
        );
        // The dashboard renders capability badges from this list,
        // so it must be present and contain the always-on phase 1
        // capabilities the test peer advertises.
        let caps = peers_arr[0]["capabilities"]
            .as_array()
            .expect("capabilities must be a JSON array");
        assert!(!caps.is_empty(), "expected at least one capability");

        // DELETE /api/peers with the peer_id.
        let del_body = serde_json::json!({"peer_id": peer_id}).to_string();
        let del_req = format!(
            "DELETE /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            del_body.len(),
            del_body
        );
        let del_resp = http_request(dash_port, &del_req).await;
        assert!(del_resp.contains("200 OK"), "delete failed: {del_resp}");

        // GET should now be empty.
        let empty_resp = http_request(
            dash_port,
            "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let empty_body = empty_resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert_eq!(empty_body.trim(), r#"{"peers":[]}"#);

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers` with an invalid body returns 400 with a
    /// diagnostic error message.
    #[tokio::test]
    async fn test_api_peers_post_invalid_body() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "not json";
        let req = format!(
            "POST /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        handle.abort();
    }

    /// `DELETE /api/peers` for an unknown peer id returns 404.
    #[tokio::test]
    async fn test_api_peers_delete_unknown_returns_404() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = r#"{"peer_id":"intendant:ghost"}"#;
        let req = format!(
            "DELETE /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        handle.abort();
    }

    // -----------------------------------------------------------------
    // Per-peer outbound op endpoints — `/api/peers/{id}/{op}`
    // -----------------------------------------------------------------

    /// Poll the registry until the peer transitions to
    /// `ConnectionState::Connected`, or `timeout` elapses. Returns
    /// whether the peer connected in time. Used by the routing tests
    /// below to avoid sending ops at a peer whose transport is still
    /// in handshake (which would bounce off as `NotConnected` → 502
    /// and obscure the actual code path under test).
    async fn wait_for_connected(
        registry: &crate::peer::PeerRegistry,
        peer_id: &crate::peer::PeerId,
        timeout: tokio::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if let Some(h) = registry.get(peer_id) {
                if h.is_connected() {
                    return true;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
        }
        false
    }

    /// Boilerplate: spawn target gateway A, register it as a peer on
    /// dashboard gateway B via HTTP, wait for the transport to connect,
    /// return everything the per-peer op tests need: the dashboard's
    /// port (where ops are POSTed) plus the peer id (the path
    /// parameter for every op endpoint) plus all four task handles to
    /// abort at end of test. Cuts ~30 lines of setup per test.
    pub(crate) async fn setup_peer_op_test() -> (
        u16,
        String,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        // Gateway A: the target peer this dashboard will federate with.
        let (target_port, target_handle) = spawn_test_gateway_with_registry(None).await;

        // Gateway B: the dashboard, with its own peer registry.
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(64);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let registry_for_wait = registry.clone();
        let (dash_port, dash_handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        // POST A's Agent Card URL to B's /api/peers.
        let card_url = format!("http://127.0.0.1:{target_port}/.well-known/agent-card.json");
        let body = serde_json::json!({"card_url": card_url}).to_string();
        let req = format!(
            "POST /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "register failed: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        let peer_id = parsed["peer_id"].as_str().unwrap().to_string();

        // Wait for the IntendantWsTransport to finish its handshake so
        // the op ack distinguishes "handler+routing works" from
        // "transport not ready yet".
        let pid = crate::peer::PeerId(peer_id.clone());
        assert!(
            wait_for_connected(
                &registry_for_wait,
                &pid,
                tokio::time::Duration::from_secs(3),
            )
            .await,
            "peer never reached Connected"
        );

        (dash_port, peer_id, target_handle, dash_handle)
    }

    /// `POST /api/peers/{id}/message` with a `{text}` shorthand body
    /// returns 200 + a `message_id`. Verifies the path-parameter
    /// routing, the JSON shorthand parsing, and the dispatch into
    /// `PeerHandle::send_message`. The wire-level encoding (this
    /// becomes a `ControlMsg::FollowUp` over the WebSocket) is covered
    /// by `peer::transport::intendant::tests::send_message_writes_followup_control_msg`.
    #[tokio::test]
    async fn test_api_peers_send_message_text_shorthand_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({"text": "hello peer"}).to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "send_message failed: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert!(
            parsed["message_id"].as_str().is_some(),
            "expected message_id in response: {resp_body}"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// Peer ids contain `:` and browsers commonly percent-encode
    /// path segments (`intendant:e2e` -> `intendant%3Ae2e`). The
    /// shared `/api/peers/{id}/{op}` route must decode the id before
    /// looking it up in the registry.
    #[tokio::test]
    async fn test_api_peers_encoded_peer_id_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let encoded_peer_id = peer_id.replace(':', "%3A");
        let body = serde_json::json!({"text": "hello encoded peer"}).to_string();
        let req = format!(
            "POST /api/peers/{encoded_peer_id}/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "encoded peer id failed: {resp}");

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{id}/task` with `{instructions}` returns 200 +
    /// `task_id`. Wire-level encoding covered by
    /// `peer::transport::intendant::tests::delegate_task_writes_start_task_control_msg`.
    #[tokio::test]
    async fn test_api_peers_delegate_task_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "instructions": "do the thing",
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/task HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "delegate_task failed: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert!(
            parsed["task_id"].as_str().is_some(),
            "expected task_id in response: {resp_body}"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{id}/approval` with `{request_id, decision}`
    /// returns 200. Wire-level encoding covered by
    /// `peer::transport::intendant::tests::resolve_approval_maps_each_decision_to_its_control_msg`.
    #[tokio::test]
    async fn test_api_peers_resolve_approval_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "request_id": "42",
            "decision": "accept",
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/approval HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "resolve_approval failed: {resp}");

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{unknown}/message` returns 404 with a
    /// diagnostic body. Doesn't require setup — exercises only the
    /// peer lookup path before any transport interaction.
    #[tokio::test]
    async fn test_api_peers_op_unknown_peer_returns_404() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = serde_json::json!({"text": "hi"}).to_string();
        let req = format!(
            "POST /api/peers/intendant:ghost/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("intendant:ghost"),
            "404 body should mention the missing id: {resp_body}"
        );
        handle.abort();
    }

    /// `POST /api/peers/{id}/message` with malformed JSON returns 400.
    #[tokio::test]
    async fn test_api_peers_send_message_invalid_body_returns_400() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "not json";
        let req = format!(
            "POST /api/peers/intendant:any/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        handle.abort();
    }

    /// `POST /api/peers/{id}/message` with neither `text` nor
    /// `content` returns 400. Verifies the `into_message` validation
    /// rejects empty bodies before the peer lookup runs.
    #[tokio::test]
    async fn test_api_peers_send_message_requires_text_or_content() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = serde_json::json!({"session": "scratch"}).to_string();
        let req = format!(
            "POST /api/peers/intendant:any/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("text") && resp_body.contains("content"),
            "error body should mention the missing fields: {resp_body}"
        );
        handle.abort();
    }

    /// Unknown sub-op (e.g. `/api/peers/{id}/bogus`) returns 404 with
    /// a diagnostic body. Guards the dispatch arm that distinguishes
    /// "supported op" from "unrecognized verb".
    #[tokio::test]
    async fn test_api_peers_unknown_op_returns_404() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "{}";
        let req = format!(
            "POST /api/peers/intendant:any/bogus HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("bogus"),
            "404 body should name the unknown op: {resp_body}"
        );
        handle.abort();
    }

    // -----------------------------------------------------------------
    // Coordinator endpoints — capability discovery + delegation
    // -----------------------------------------------------------------

    /// `GET /api/peers/eligible` returns 503 with no registry,
    /// matching the rest of /api/peers.
    #[tokio::test]
    async fn test_api_peers_eligible_returns_503_without_registry() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let resp = http_request(
            port,
            "GET /api/peers/eligible?capability=display HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("503"), "expected 503, got: {resp}");
        handle.abort();
    }

    /// Missing `?capability=...` query param returns 400 with a
    /// hint that at least one is required.
    #[tokio::test]
    async fn test_api_peers_eligible_requires_capability_param() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let resp = http_request(
            port,
            "GET /api/peers/eligible HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("capability"),
            "400 body should mention capability: {resp_body}"
        );
        handle.abort();
    }

    /// Unknown capability strings return 400 with the offending
    /// values surfaced (not silently dropped, which would let an
    /// /api/peers/eligible?capability=typo through and return all
    /// peers).
    #[tokio::test]
    async fn test_api_peers_eligible_rejects_unknown_capability() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let resp = http_request(
            port,
            "GET /api/peers/eligible?capability=display&capability=nope HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("nope"),
            "400 body should name the unknown capability: {resp_body}"
        );
        handle.abort();
    }

    /// `POST /api/coordinator/route` with required_capabilities the
    /// connected peer satisfies returns 200 + peer_id + task_id.
    /// Wire encoding to ControlMsg::StartTask is covered by
    /// peer::transport::intendant::tests.
    #[tokio::test]
    async fn test_api_coordinator_route_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "required_capabilities": ["computer-use"],
            "task": {
                "instructions": "do the thing",
                "context": {"file": "src/main.rs"},
            },
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert_eq!(
            parsed["peer_id"].as_str().expect("peer_id present"),
            peer_id
        );
        assert!(
            parsed["task_id"].as_str().is_some(),
            "task_id should be present in response: {resp_body}"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// Bad JSON body returns 400.
    #[tokio::test]
    async fn test_api_coordinator_route_invalid_body_returns_400() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "not json";
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        handle.abort();
    }

    /// Empty `required_capabilities` returns 400 — would otherwise
    /// match every peer and route to the first lexicographically,
    /// which is almost never what the caller meant.
    #[tokio::test]
    async fn test_api_coordinator_route_rejects_empty_capabilities() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = serde_json::json!({
            "required_capabilities": [],
            "task": {"instructions": "anything"},
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("required_capabilities"),
            "400 body should mention required_capabilities: {resp_body}"
        );
        handle.abort();
    }

    /// GET on the route endpoint returns 405 — only POST is allowed.
    #[tokio::test]
    async fn test_api_coordinator_route_get_returns_405() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let resp = http_request(
            port,
            "GET /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("405"), "expected 405, got: {resp}");
        handle.abort();
    }

    /// Insert a `DisplayInputHolder` directly into the map for tests
    /// that need to seed a holder without going through the full
    /// `apply_grant_input_authority` flow.  The inserted holder owns
    /// a fresh dummy `direct_tx` whose receiver is dropped — sends to
    /// it return `Err`, which the production code already tolerates
    /// (the WS-close path would have cleared this entry in real life).
    pub(crate) fn seed_holder(
        map: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
        display_id: u32,
        connection_id: &str,
    ) {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            display_id,
            DisplayInputHolder::LocalWs {
                connection_id: connection_id.to_string(),
                direct_tx: tx,
            },
        );
    }


}
