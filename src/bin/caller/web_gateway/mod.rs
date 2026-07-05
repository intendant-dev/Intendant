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

const TLS_FAILURE_LOG_INTERVAL_SECS: u64 = 30;

#[derive(Debug)]
struct TlsFailureLogEntry {
    last_logged: std::time::Instant,
    suppressed: u64,
}

type TlsFailureLogState = Arc<Mutex<HashMap<String, TlsFailureLogEntry>>>;

fn log_tls_failure_rate_limited(state: &TlsFailureLogState, peer: &str, kind: &str, detail: &str) {
    let now = std::time::Instant::now();
    let key = format!("{kind}|{peer}|{detail}");
    let mut map = state.lock().unwrap_or_else(|e| e.into_inner());
    match map.get_mut(&key) {
        Some(entry)
            if now.duration_since(entry.last_logged)
                < std::time::Duration::from_secs(TLS_FAILURE_LOG_INTERVAL_SECS) =>
        {
            entry.suppressed = entry.suppressed.saturating_add(1);
        }
        Some(entry) => {
            let suppressed = entry.suppressed;
            entry.last_logged = now;
            entry.suppressed = 0;
            drop(map);
            if suppressed > 0 {
                eprintln!(
                    "[web_gateway] {kind} from {peer}: {detail} \
                     (suppressed {suppressed} repeats in the last {TLS_FAILURE_LOG_INTERVAL_SECS}s)"
                );
            } else {
                eprintln!("[web_gateway] {kind} from {peer}: {detail}");
            }
        }
        None => {
            map.insert(
                key,
                TlsFailureLogEntry {
                    last_logged: now,
                    suppressed: 0,
                },
            );
            drop(map);
            eprintln!("[web_gateway] {kind} from {peer}: {detail}");
        }
    }
}

// Exact fork baselines are a synchronous `/api/sessions` refinement. The scanner
// below parses compact Codex token lines without materializing full JSON values.
// Exact fork baselines come from scanning the parent's log (results
// persist per file-state in the codex-parent-baseline namespace, so each
// scan happens once). The per-file cap covers the largest observed
// rollouts; parents past the per-build budget pick up their baseline on a
// later list pass.

















/// Tracks which WebSocket connection currently owns the voice model (is "active").
/// Only one connection can be active at a time; all others are "passive" (TUI-only).
struct ActivePresence {
    connection_id: String,
    direct_tx: mpsc::UnboundedSender<String>,
}

fn dashboard_control_presence_sender(
    control_tx: mpsc::UnboundedSender<serde_json::Value>,
) -> mpsc::UnboundedSender<String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            let payload = serde_json::from_str::<serde_json::Value>(&text)
                .unwrap_or_else(|_| serde_json::json!({"t": "raw", "text": text}));
            let _ = control_tx.send(serde_json::json!({
                "t": "event",
                "payload": payload,
            }));
        }
    });
    tx
}

fn send_dashboard_control_presence_event(
    control_tx: &mpsc::UnboundedSender<serde_json::Value>,
    payload: serde_json::Value,
) {
    let _ = control_tx.send(serde_json::json!({
        "t": "event",
        "payload": payload,
    }));
}

async fn dashboard_control_presence_connect(
    request: crate::dashboard_control::DashboardPresenceConnectRequest,
    active_presence: Arc<Mutex<Option<ActivePresence>>>,
    voice_debug: Arc<Mutex<VoiceDebugState>>,
    shared_session: SharedActiveSession,
    bus: EventBus,
    default_provider: String,
    default_model: String,
) {
    voice_debug
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .connected = true;

    let provider = request.provider.unwrap_or(default_provider);
    let model = request.model.unwrap_or(default_model);
    let sender = dashboard_control_presence_sender(request.control_tx.clone());
    let (becomes_active, was_already_active) = {
        let mut slot = active_presence.lock().unwrap_or_else(|e| e.into_inner());
        let was_already_active = slot
            .as_ref()
            .map(|active| active.connection_id == request.session_id)
            .unwrap_or(false);
        let becomes_active = !request.passive && (slot.is_none() || was_already_active);
        if becomes_active {
            *slot = Some(ActivePresence {
                connection_id: request.session_id.clone(),
                direct_tx: sender,
            });
        }
        (becomes_active, was_already_active)
    };

    let active = shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);

    if let Some(ctx) = &query_ctx {
        let conversation_ctx = presence::build_conversation_context(&ctx.log_dir, 20);
        if let Some(ps) = &ctx.presence_session {
            let mut session = ps.lock().unwrap_or_else(|e| e.into_inner());
            if becomes_active {
                session.set_connected(true);
            }
            let state = ctx
                .agent_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let welcome = session.build_welcome(request.last_event_seq, &state);
            send_dashboard_control_presence_event(
                &request.control_tx,
                serde_json::json!({
                    "t": "presence_welcome",
                    "session_id": welcome.session_id,
                    "state": welcome.state,
                    "events": welcome.events,
                    "last_checkpoint_summary": welcome.last_checkpoint_summary,
                    "current_seq": welcome.current_seq,
                    "is_active": becomes_active,
                    "conversation_context": conversation_ctx,
                }),
            );
        } else {
            send_dashboard_control_presence_event(
                &request.control_tx,
                serde_json::json!({
                    "t": "presence_welcome",
                    "is_active": becomes_active,
                    "conversation_context": conversation_ctx,
                }),
            );
        }
    } else {
        send_dashboard_control_presence_event(
            &request.control_tx,
            serde_json::json!({
                "t": "presence_welcome",
                "is_active": becomes_active,
            }),
        );
    }

    if becomes_active && !was_already_active {
        if let Some(sl) = session_log {
            if let Ok(mut log) = sl.lock() {
                log.presence_connected(Some(&provider), Some(&model));
            }
        }
        bus.send(AppEvent::PresenceConnected {
            server_session_id: request.server_session_id,
            last_event_seq: request.last_event_seq,
            live_provider: Some(provider),
            live_model: Some(model),
        });
    }
}

async fn dashboard_control_presence_disconnect(
    request: crate::dashboard_control::DashboardPresenceDisconnectRequest,
    active_presence: Arc<Mutex<Option<ActivePresence>>>,
    voice_debug: Arc<Mutex<VoiceDebugState>>,
    shared_session: SharedActiveSession,
    bus: EventBus,
) {
    voice_debug
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .connected = false;
    let active = shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);
    if let Some(ctx) = query_ctx {
        if let Some(ps) = ctx.presence_session {
            ps.lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_connected(false);
        }
    }
    let was_active = {
        let mut slot = active_presence.lock().unwrap_or_else(|e| e.into_inner());
        if slot
            .as_ref()
            .map(|active| active.connection_id == request.session_id)
            .unwrap_or(false)
        {
            *slot = None;
            true
        } else {
            false
        }
    };
    if was_active {
        if let Some(sl) = session_log {
            if let Ok(mut log) = sl.lock() {
                log.presence_disconnected();
            }
        }
        bus.send(AppEvent::PresenceDisconnected);
    }
}

async fn dashboard_control_presence_make_active(
    request: crate::dashboard_control::DashboardPresenceMakeActiveRequest,
    active_presence: Arc<Mutex<Option<ActivePresence>>>,
    voice_debug: Arc<Mutex<VoiceDebugState>>,
    shared_session: SharedActiveSession,
    bus: EventBus,
    default_provider: String,
    default_model: String,
) {
    let provider = request.provider.unwrap_or(default_provider);
    let model = request.model.unwrap_or(default_model);
    let sender = dashboard_control_presence_sender(request.control_tx.clone());

    let previous_active = {
        let slot = active_presence.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().map(|active| active.connection_id.clone())
    };
    let active = shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);

    if let Some(sl) = &session_log {
        if let Ok(mut log) = sl.lock() {
            log.voice_diagnostic(
                "make_active_received_gateway",
                &format!(
                    "request from connection={} previous_active={}",
                    request.session_id,
                    previous_active.as_deref().unwrap_or("none"),
                ),
            );
        }
    }

    {
        let mut slot = active_presence.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = slot.as_ref() {
            if old.connection_id != request.session_id {
                let _ = old.direct_tx.send(
                    serde_json::json!({
                        "t": "force_disconnect_voice",
                        "reason": "handover",
                    })
                    .to_string(),
                );
                if let Some(sl) = &session_log {
                    if let Ok(mut log) = sl.lock() {
                        log.voice_diagnostic(
                            "make_active_force_disconnect_gateway",
                            &format!(
                                "old_active={} new_active={}",
                                old.connection_id, request.session_id,
                            ),
                        );
                    }
                }
            } else if let Some(sl) = &session_log {
                if let Ok(mut log) = sl.lock() {
                    log.voice_diagnostic(
                        "make_active_noop_gateway",
                        &format!(
                            "request from already-active connection={}",
                            request.session_id,
                        ),
                    );
                }
            }
        }
        *slot = Some(ActivePresence {
            connection_id: request.session_id.clone(),
            direct_tx: sender,
        });
    }

    voice_debug
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .connected = true;

    let handover_context = query_ctx
        .as_ref()
        .and_then(|ctx| ctx.presence_session.as_ref())
        .and_then(|ps| {
            let session = ps.lock().unwrap_or_else(|e| e.into_inner());
            session.last_checkpoint_summary()
        })
        .unwrap_or_default();
    let conversation_ctx = query_ctx
        .as_ref()
        .and_then(|ctx| presence::build_conversation_context(&ctx.log_dir, 20));
    let has_handover_context = !handover_context.is_empty();
    let has_conversation_context = conversation_ctx
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    send_dashboard_control_presence_event(
        &request.control_tx,
        serde_json::json!({
            "t": "active_granted",
            "is_active": true,
            "handover_context": handover_context,
            "conversation_context": conversation_ctx,
        }),
    );

    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.voice_diagnostic(
                "make_active_granted_gateway",
                &format!(
                    "connection={} handover_context={} conversation_context={}",
                    request.session_id,
                    if has_handover_context { "yes" } else { "no" },
                    if has_conversation_context {
                        "yes"
                    } else {
                        "no"
                    },
                ),
            );
            log.presence_connected(Some(&provider), Some(&model));
        }
    }
    bus.send(AppEvent::PresenceConnected {
        server_session_id: None,
        last_event_seq: 0,
        live_provider: Some(provider),
        live_model: Some(model),
    });
}

async fn dashboard_control_presence_cleanup(
    session_id: String,
    active_presence: Arc<Mutex<Option<ActivePresence>>>,
    voice_debug: Arc<Mutex<VoiceDebugState>>,
    shared_session: SharedActiveSession,
    bus: EventBus,
) {
    let was_active = {
        let mut slot = active_presence.lock().unwrap_or_else(|e| e.into_inner());
        if slot
            .as_ref()
            .map(|active| active.connection_id == session_id)
            .unwrap_or(false)
        {
            *slot = None;
            true
        } else {
            false
        }
    };
    if !was_active {
        return;
    }
    voice_debug
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .connected = false;
    let active = shared_session.read().await;
    let query_ctx = active.query_ctx.clone();
    let session_log = active.session_log.clone();
    drop(active);
    if let Some(ctx) = query_ctx {
        if let Some(ps) = ctx.presence_session {
            ps.lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_connected(false);
        }
    }
    if let Some(sl) = session_log {
        if let Ok(mut log) = sl.lock() {
            log.presence_disconnected();
        }
    }
    bus.send(AppEvent::PresenceDisconnected);
}

/// Identity of who currently holds input authority for one display.
///
/// Three provenance kinds, with explicit identity per kind so the
/// arbitration / gate / cleanup paths can match on the source of the
/// hold without resorting to string-shape inference:
///
/// - **`LocalWs`**: holder is a WebSocket connection on this gateway.
///   Carries the WS connection id (identity) plus the connection's
///   `direct_tx` for the local-only `display_input_authority_revoked`
///   confirmation that fires when this holder is preempted by another
///   grant. Federated holders do NOT get a direct revoke — federated
///   state always flows through the personalized authority-state
///   broadcast on each federated WebRtcPeer's `display_input_authority`
///   data channel.
///
/// - **`FederatedWebRtc`**: holder is a federated `PeerDisplayConnection`
///   on a peer primary. Identified by `(federation_connection_id,
///   session_id)`. `federation_connection_id` is the gateway-WS
///   `connection_id` of the federation transport (one per primary's
///   federation client); `session_id` distinguishes multiple
///   `PeerDisplayConnection` tabs from the same primary. Field name
///   spelled out so it's not confused with the local-browser
///   `LocalWs::connection_id`.
///
///   The design doc originally specified `peer_id: PeerId`, but the
///   stable federation `PeerId` isn't carried in the
///   `ControlMsg::WebRtcSignal` wire format — it's implicit in which
///   `/ws` connection delivered the message. F-1.3b uses the federation
///   WS `connection_id` as the holder identity instead; it's
///   authenticated by the federation WS connection, unique per
///   primary's federation transport, and already covered by WS-close
///   cleanup. `connection_id` changes across federation WS reconnect
///   (a stable `PeerId` would survive); WS-close cleanup releases any
///   held authority on each disconnect, so the trade-off is a UX
///   nicety, not correctness. See
///   `docs/design-federated-input-authority.md` for the full note.
///
/// - **`DashboardControl`**: holder is one daemon-scoped dashboard
///   control DataChannel session. It has no WS `direct_tx`; state
///   changes are personalized and pushed through the control tunnel's
///   event stream.
///
/// The map is `HashMap<u32, DisplayInputHolder>` — no `Option`, no
/// wrapper struct. Entry absence = unclaimed; that's the pre-phase-5
/// backwards-compat state where every connection's input flowed
/// through (now: only the holder's input flows through; everyone
/// else's is dropped at the gate, federated input is dropped
/// unconditionally until F-2 lights up the federated input gate).
#[derive(Clone, Debug)]
pub(crate) enum DisplayInputHolder {
    LocalWs {
        connection_id: String,
        /// Outbound channel for sending this WS connection's
        /// `display_input_authority_revoked` confirmation when a
        /// later grant preempts this holder. Local-only — the
        /// federated path uses the personalized authority-state
        /// broadcast for the same notification.
        direct_tx: mpsc::UnboundedSender<String>,
    },
    FederatedWebRtc {
        federation_connection_id: String,
        session_id: String,
    },
    DashboardControl {
        session_id: String,
    },
}

impl DisplayInputHolder {
    /// True iff this holder is `LocalWs` with the given `connection_id`.
    /// Used by local gate / personalization sites; deliberately returns
    /// false for `FederatedWebRtc` rather than panicking, so a future
    /// caller that mistakenly passes a connection id from the federated
    /// side gets a silent drop rather than mis-authorization.
    fn matches_local_ws(&self, connection_id: &str) -> bool {
        match self {
            Self::LocalWs {
                connection_id: c, ..
            } => c == connection_id,
            Self::FederatedWebRtc { .. } | Self::DashboardControl { .. } => false,
        }
    }

    /// True iff this holder is `FederatedWebRtc` with the given
    /// `(federation_connection_id, session_id)` pair. Used by the
    /// federated input gate (in F-2) and the federated close-cleanup
    /// path.
    fn matches_federated(&self, federation_connection_id: &str, session_id: &str) -> bool {
        match self {
            Self::FederatedWebRtc {
                federation_connection_id: c,
                session_id: s,
            } => c == federation_connection_id && s == session_id,
            Self::LocalWs { .. } | Self::DashboardControl { .. } => false,
        }
    }

    /// True iff this holder is a daemon-scoped dashboard control
    /// session with the given `session_id`.
    fn matches_dashboard_control(&self, session_id: &str) -> bool {
        match self {
            Self::DashboardControl { session_id: s } => s == session_id,
            Self::LocalWs { .. } | Self::FederatedWebRtc { .. } => false,
        }
    }

    /// True iff `self` and `other` identify the same holder
    /// (provenance + identity). Used by release / preempt sites where
    /// we need to compare the requesting holder against the current
    /// one without unwrapping the variant manually. Deliberately
    /// ignores `direct_tx` (which isn't equality-comparable and isn't
    /// part of identity — it's a notification handle that can change
    /// if the same WS connection rebuilds its outbound queue).
    ///
    /// Distinct from a `PartialEq` impl on purpose: spelled-out method
    /// at call sites makes intent explicit and prevents accidental
    /// equality-comparison pitfalls in collections / `.contains()` /
    /// pattern guards.
    ///
    /// Production callers don't need this yet — every F-1 / F-2
    /// release-or-preempt site already knows which provenance kind it's
    /// matching against and uses `matches_local_ws` /
    /// `matches_federated` directly. The method is pinned by unit
    /// tests as the documented identity-equality contract for future
    /// arbitration work (e.g. F-2's per-primary multi-operator
    /// scoping, where the comparison is against an opaque
    /// `DisplayInputHolder` snapshot).
    #[allow(dead_code)]
    fn same_identity(&self, other: &DisplayInputHolder) -> bool {
        match (self, other) {
            (
                Self::LocalWs {
                    connection_id: a, ..
                },
                Self::LocalWs {
                    connection_id: b, ..
                },
            ) => a == b,
            (
                Self::FederatedWebRtc {
                    federation_connection_id: ca,
                    session_id: sa,
                },
                Self::FederatedWebRtc {
                    federation_connection_id: cb,
                    session_id: sb,
                },
            ) => ca == cb && sa == sb,
            (
                Self::DashboardControl { session_id: a },
                Self::DashboardControl { session_id: b },
            ) => a == b,
            _ => false,
        }
    }
}

/// Phase 5a.1: dedicated internal broadcast event for display input
/// authority transitions.
///
/// Carries the holder's *server-internal* identity (or `None` for
/// unclaimed) so each WS outbound task can personalize this for its
/// own browser as `you | other | unclaimed` without ever shipping
/// holder IDs to browsers.  Personalization happens in the
/// per-connection outbound select arm where `connection_id_outbound`
/// is in scope.
///
/// Distinct from [`AppEvent`] on purpose: the generic outbound
/// broadcast carries already-serialized JSON strings, which would leak
/// holder IDs if we routed authority through it.  A dedicated typed
/// channel keeps the holder identity inside the gateway and forces
/// every per-connection consumer to compute its own personalized state.
#[derive(Clone, Debug)]
pub(crate) struct DisplayInputAuthorityChange {
    display_id: u32,
    holder: Option<DisplayInputHolder>,
}

/// Build the per-peer "may this connection inject input now?" closure
/// for the local `/ws` display-offer path (Phase 5a.1).
///
/// Returns a closure that consults the live authority map every time
/// it's called, so a grant or release elsewhere takes effect on the
/// very next data-channel input event without needing to reconstruct
/// the closure or rebuild the peer connection.
///
/// Semantics:
/// - `auth.get(display_id) == Some(entry)` and
///   `entry.matches_local_ws(this_id)` → `true`
///   (this WS connection holds authority)
/// - `auth.get(display_id) == Some(entry)` and
///   `!entry.matches_local_ws(this_id)` → `false`
///   (someone else — local or, once the variant lands, federated —
///   holds it; silent drop)
/// - `auth.get(display_id) == None`
///   → `true` (unclaimed = pre-phase-5 default; any connection can
///   input)
///
/// The federated path does NOT call this; it has its own deny-by-
/// default authorizer that becomes a `FederatedWebRtc` registry
/// lookup in F-1's later commits.
fn build_local_ws_input_authorizer(
    display_id: u32,
    connection_id: String,
    authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(move || {
        let auth = authority.read().unwrap_or_else(|e| e.into_inner());
        match auth.get(&display_id) {
            Some(entry) => entry.matches_local_ws(&connection_id),
            None => true,
        }
    })
}

/// Capacity of the [`DisplayInputAuthorityChange`] broadcast channel.
///
/// Sized to comfortably absorb a burst of grants/releases across a few
/// dozen connected browsers — typically 1-3 events per user action,
/// fanned out across all WS connections.  64 is plenty of headroom and
/// cheap; lagged subscribers fall back to a fresh personalized snapshot
/// path (see the `Lagged` arm in the outbound select).
const AUTHORITY_CHANGE_CAPACITY: usize = 64;

/// F-2: federated path's input-authorization closure. Returns `true`
/// iff the current holder for `display_id` is `FederatedWebRtc` matching
/// THIS peer's `(federation_connection_id, session_id)`. Anything else
/// — no holder, a `LocalWs` holder, a `FederatedWebRtc` with a different
/// session id (e.g. another tab from the same primary), or a different
/// connection — returns `false` and the federated input handler drops
/// the event silently.
///
/// Symmetric in shape to [`build_local_ws_input_authorizer`], but with
/// strict deny-by-default for the unclaimed case: local 5c treats `None`
/// as "anyone may input" for pre-phase-5 backwards compatibility, while
/// the federated path has no such legacy and treats `None` as "nobody
/// holds this — drop everything." A federated browser only sends input
/// when its chip is `'you'` (UX-side guard); receiving input here under
/// any other condition is a protocol bug or a stale post-release race
/// and silent drop is correct.
///
/// The closure is the entire boundary: `display/mod.rs` invokes it per
/// event and never sees the registry, the holder identity, or the
/// connection/session IDs. F-2's gate flip is the single semantic change
/// from F-1's `Arc::new(|| false)` deny-everything stub.
fn build_federated_input_authorizer(
    display_id: u32,
    federation_connection_id: String,
    session_id: String,
    authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(move || {
        let auth = authority.read().unwrap_or_else(|e| e.into_inner());
        match auth.get(&display_id) {
            Some(entry) => entry.matches_federated(&federation_connection_id, &session_id),
            None => false,
        }
    })
}

/// Apply a `RequestDisplayInputAuthority`.  Inserts the new holder,
/// returns the prior holder if any, sends `display_input_authority_revoked`
/// to the prior holder (if displaced), and emits the personalized
/// authority change for fan-out.  Caller is responsible for the
/// `display_input_authority_granted` confirm to `requester_direct_tx`
/// and the bus log message — both stay at the call site to keep the
/// helper's surface narrow (no logging dependency, no second send to
/// the same channel).
///
/// Lock discipline: the `authority` write guard is dropped before any
/// `direct_tx.send` or `authority_change_tx.send` call.
fn apply_grant_input_authority(
    display_id: u32,
    requester_connection_id: String,
    requester_direct_tx: mpsc::UnboundedSender<String>,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Option<DisplayInputHolder> {
    let new_holder = DisplayInputHolder::LocalWs {
        connection_id: requester_connection_id.clone(),
        direct_tx: requester_direct_tx,
    };
    // Clone for the broadcast — broadcast recipients personalize
    // from holder identity (the channel-clone in LocalWs is unused
    // downstream but cheap because mpsc::UnboundedSender is
    // Arc-backed).
    let broadcast_holder = new_holder.clone();
    let prior = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        map.insert(display_id, new_holder)
    };
    // Only `LocalWs` prior holders get the direct revoke confirmation
    // — `direct_tx` is local-only by design (see `DisplayInputHolder`
    // doc). A `FederatedWebRtc` prior holder learns of the preempt
    // through the personalized authority-state broadcast on its own
    // `display_input_authority` data channel.
    if let Some(DisplayInputHolder::LocalWs {
        connection_id: prior_id,
        direct_tx: prior_tx,
    }) = prior.as_ref()
    {
        if prior_id != &requester_connection_id {
            let notify = serde_json::json!({
                "t": "display_input_authority_revoked",
                "display_id": display_id,
                "reason": "another connection requested control",
            })
            .to_string();
            let _ = prior_tx.send(notify);
        }
    }
    let _ = authority_change_tx.send(DisplayInputAuthorityChange {
        display_id,
        holder: Some(broadcast_holder),
    });
    prior
}

/// Apply a `ReleaseDisplayInputAuthority`.  No-op if the calling
/// connection isn't the holder (prevents A from unclaiming B's slot).
/// Returns `true` iff the slot was actually released.  Emits the
/// personalized authority change with `None` only when the release
/// took effect — a no-op release does not flip anyone's UI state.
///
/// Lock discipline: matches [`apply_grant_input_authority`].
fn apply_release_input_authority(
    display_id: u32,
    releaser_connection_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> bool {
    let removed = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        match map.get(&display_id) {
            Some(entry) if entry.matches_local_ws(releaser_connection_id) => {
                map.remove(&display_id);
                true
            }
            _ => false,
        }
    };
    if removed {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id,
            holder: None,
        });
    }
    removed
}

/// F-1.3b: federated grant. Constructs a `FederatedWebRtc` holder
/// from `(federation_connection_id, session_id)`, inserts it into
/// the registry, returns the prior holder if any, and emits the
/// personalized authority change for fan-out.
///
/// Mirrors [`apply_grant_input_authority`] for the local path but
/// is provenance-distinct: federated holders carry no `direct_tx`
/// (federated state always flows through the personalized
/// authority-state broadcast on the federated WebRtcPeer's
/// `display_input_authority` data channel — see the F-1 design
/// note in `DisplayInputHolder`).
///
/// Prior holder revocation:
/// - If prior is `LocalWs`, send the existing
///   `display_input_authority_revoked` notification on the prior
///   holder's `direct_tx`. Same protocol as a local→local handover.
/// - If prior is `FederatedWebRtc` with a different identity, no
///   direct revoke — the broadcast-driven personalized state
///   `"other"` reaches that prior federated holder via its own
///   authority data channel and updates its chip.
///
/// Lock discipline: matches [`apply_grant_input_authority`].
fn apply_grant_input_authority_federated(
    display_id: u32,
    federation_connection_id: String,
    session_id: String,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Option<DisplayInputHolder> {
    let new_holder = DisplayInputHolder::FederatedWebRtc {
        federation_connection_id: federation_connection_id.clone(),
        session_id: session_id.clone(),
    };
    let broadcast_holder = new_holder.clone();
    let prior = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        map.insert(display_id, new_holder)
    };
    // Prior LocalWs holder gets the legacy direct revoke; prior
    // FederatedWebRtc gets nothing here because the personalized
    // broadcast below carries `"other"` to it on its own data channel.
    if let Some(DisplayInputHolder::LocalWs {
        direct_tx: prior_tx,
        ..
    }) = prior.as_ref()
    {
        let notify = serde_json::json!({
            "t": "display_input_authority_revoked",
            "display_id": display_id,
            "reason": "another connection requested control",
        })
        .to_string();
        let _ = prior_tx.send(notify);
    }
    let _ = authority_change_tx.send(DisplayInputAuthorityChange {
        display_id,
        holder: Some(broadcast_holder),
    });
    prior
}

/// F-1.3b: federated release. No-op if the calling
/// `(federation_connection_id, session_id)` doesn't match the
/// current holder (prevents one federated session from unclaiming
/// another's slot — even from the same primary, distinct
/// `PeerDisplayConnection` tabs have distinct `session_id`s).
/// Returns `true` iff the slot was actually released.
///
/// Lock discipline: matches [`apply_grant_input_authority_federated`].
fn apply_release_input_authority_federated(
    display_id: u32,
    federation_connection_id: &str,
    session_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> bool {
    let removed = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        match map.get(&display_id) {
            Some(entry) if entry.matches_federated(federation_connection_id, session_id) => {
                map.remove(&display_id);
                true
            }
            _ => false,
        }
    };
    if removed {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id,
            holder: None,
        });
    }
    removed
}

fn dashboard_control_authority_state_frame(
    session_id: &str,
    display_id: u32,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
) -> serde_json::Value {
    let state = {
        let auth = authority.read().unwrap_or_else(|e| e.into_inner());
        match auth.get(&display_id) {
            Some(entry) if entry.matches_dashboard_control(session_id) => "you",
            Some(_) => "other",
            None => "unclaimed",
        }
    };
    serde_json::json!({
        "t": "display_input_authority_state",
        "display_id": display_id,
        "state": state,
    })
}

fn dashboard_control_authority_snapshot_frames(
    session_id: &str,
    display_ids: &[u32],
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
) -> Vec<serde_json::Value> {
    display_ids
        .iter()
        .map(|display_id| {
            dashboard_control_authority_state_frame(session_id, *display_id, authority)
        })
        .collect()
}

fn apply_grant_input_authority_dashboard_control(
    display_id: u32,
    session_id: String,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Option<DisplayInputHolder> {
    let new_holder = DisplayInputHolder::DashboardControl {
        session_id: session_id.clone(),
    };
    let broadcast_holder = new_holder.clone();
    let prior = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        map.insert(display_id, new_holder)
    };
    if let Some(DisplayInputHolder::LocalWs {
        direct_tx: prior_tx,
        ..
    }) = prior.as_ref()
    {
        let notify = serde_json::json!({
            "t": "display_input_authority_revoked",
            "display_id": display_id,
            "reason": "another connection requested control",
        })
        .to_string();
        let _ = prior_tx.send(notify);
    }
    let _ = authority_change_tx.send(DisplayInputAuthorityChange {
        display_id,
        holder: Some(broadcast_holder),
    });
    prior
}

fn apply_release_input_authority_dashboard_control(
    display_id: u32,
    session_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> bool {
    let removed = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        match map.get(&display_id) {
            Some(entry) if entry.matches_dashboard_control(session_id) => {
                map.remove(&display_id);
                true
            }
            _ => false,
        }
    };
    if removed {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id,
            holder: None,
        });
    }
    removed
}

fn apply_dashboard_control_close_input_authority(
    session_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Vec<u32> {
    let released: Vec<u32> = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        map.retain(|did, entry| {
            if entry.matches_dashboard_control(session_id) {
                out.push(*did);
                false
            } else {
                true
            }
        });
        out
    };
    for did in &released {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id: *did,
            holder: None,
        });
    }
    released
}

fn dashboard_control_input_authorized(
    session_id: &str,
    display_id: u32,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
) -> bool {
    let auth = authority.read().unwrap_or_else(|e| e.into_inner());
    match auth.get(&display_id) {
        Some(entry) => entry.matches_dashboard_control(session_id),
        None => true,
    }
}

/// F-1.3b: federated WS-close cleanup. Releases every
/// `FederatedWebRtc` entry whose `federation_connection_id` matches
/// the dropping federation transport, regardless of `session_id`
/// (the WS drop kills every `PeerDisplayConnection` session multiplexed
/// over that primary's federation transport). Emits one `None`-holder
/// authority change per affected display so other viewers' chips
/// flip back to `unclaimed`.
///
/// Distinct from [`apply_ws_close_input_authority`] which targets
/// `LocalWs` entries: a single `connection_id` is either acting as
/// a local browser or a federation transport but not both, so the
/// two cleanup paths address disjoint registry entries. Both fire
/// from the same WS-close hook (the gateway calls them in sequence).
///
/// Lock discipline: matches [`apply_grant_input_authority_federated`].
fn apply_federated_ws_close_input_authority(
    federation_connection_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Vec<u32> {
    let released: Vec<u32> = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        map.retain(|did, entry| match entry {
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: c,
                ..
            } if c == federation_connection_id => {
                out.push(*did);
                false
            }
            _ => true,
        });
        out
    };
    for did in &released {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id: *did,
            holder: None,
        });
    }
    released
}

// ---------------------------------------------------------------------------
// F-1.3b3: federated authority subscriber registry + helpers
//
// The federated counterpart to local 5c's per-WS subscriber model.
// Local 5c has no shared subscriber registry — each WS outbound task
// subscribes to `authority_change_tx` directly and personalizes for
// its own `connection_id`. Federated needs a registry because the
// send target is `WebRtcPeer::send_authority_state`, not a WS
// `direct_tx`: the gateway must hold an `Arc<WebRtcPeer>` to push to,
// and that handle isn't available until `handle_offer` returns and
// the peer is stored in the session.
//
// One entry per `(federation_connection_id, session_id, display_id)` —
// uniquely identifies one federated `PeerDisplayConnection`'s
// subscription to one display's authority state. Each entry owns a
// fanout task + a `CancellationToken` for clean teardown on the two
// distinct cleanup edges:
//
// 1. `WebRtcSignal::Close` / `DisplaySession::remove_peer(peer_id)`:
//    unregister this exact `(federation_connection_id, session_id,
//    display_id)` entry. Identity-matched authority release runs
//    alongside via `apply_release_input_authority_federated`.
// 2. Federation WS close: unregister all entries for that
//    `federation_connection_id`. Bulk authority release runs
//    alongside via `apply_federated_ws_close_input_authority`.
// ---------------------------------------------------------------------------

/// One federated authority subscriber. Holds the cancellation token
/// that terminates the per-subscriber fanout task on cleanup.
///
/// The `Arc<WebRtcPeer>` push target lives entirely inside the
/// fanout task spawned by [`register_federated_authority_subscriber`];
/// the registry doesn't carry a second copy because nothing reads
/// it back. The Drop chain is: cleanup edge calls `shutdown.cancel()`
/// → fanout task exits → its captured peer Arc drops → reference
/// count to the `WebRtcPeer` decrements. Any peer-teardown work that
/// the registry needs (e.g. tearing down WebRtcPeers on federation
/// WS-close) lives separately at the gateway level via
/// [`peer_id_for_federated_session`] + `DisplaySession::remove_peer`,
/// not by holding a duplicate Arc here.
pub(crate) struct FederatedAuthoritySubscriber {
    shutdown: tokio_util::sync::CancellationToken,
}

/// Stable mapping from a federated `session_id` (the
/// browser-supplied per-`PeerDisplayConnection` id round-tripped in
/// `ControlMsg::WebRtcSignal`) to the [`crate::display::PeerId`]
/// (`u64`) used as the `WebRtcPeer` key inside `DisplaySession`.
///
/// Used in two places that must agree exactly:
/// 1. [`handle_federated_webrtc_signal`] — derives the key on
///    Offer/IceCandidate/Close so subsequent signals route to the
///    same peer.
/// 2. WS-close cleanup — derives the key from each `(session_id,
///    display_id)` returned by
///    [`unregister_all_federated_subscribers_for_connection`] so
///    the federation WS-close can call `DisplaySession::remove_peer`
///    on every WebRtcPeer owned by the dropping connection.
///
/// A divergence between the two callers would leak peers (cleanup
/// would target a different key than was inserted on Offer), which
/// is exactly the bug fixed by extracting this helper.
fn peer_id_for_federated_session(session_id: &str) -> crate::display::PeerId {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    session_id.hash(&mut h);
    h.finish()
}

/// Gateway-side registry of federated authority subscribers, keyed by
/// `(federation_connection_id, session_id, display_id)`. Owned by the
/// gateway listener task; cloned per-WS for the inbound handler so
/// every per-connection branch can register/unregister without
/// passing the registry through every helper signature.
type FederatedAuthoritySubscribers =
    Arc<StdRwLock<HashMap<(String, String, u32), FederatedAuthoritySubscriber>>>;

/// Compute the personalized authority state for one federated
/// subscriber from a `Option<&DisplayInputHolder>`. Returns `You` if
/// the holder is a `FederatedWebRtc` matching this subscriber's
/// `(federation_connection_id, session_id)`, `Other` if any other
/// holder exists, `Unclaimed` if no one holds. Mirrors the local 5c
/// outbound personalization logic at the per-WS subscriber loop.
fn personalize_authority_for_federated(
    holder: Option<&DisplayInputHolder>,
    federation_connection_id: &str,
    session_id: &str,
) -> crate::display::webrtc::DisplayInputAuthorityState {
    use crate::display::webrtc::DisplayInputAuthorityState;
    match holder {
        Some(h) if h.matches_federated(federation_connection_id, session_id) => {
            DisplayInputAuthorityState::You
        }
        Some(_) => DisplayInputAuthorityState::Other,
        None => DisplayInputAuthorityState::Unclaimed,
    }
}

/// Build the federated authority data-channel handler closure.
///
/// The handler is invoked by the WebRTC driver on every parsed
/// [`crate::display::webrtc::AuthorityChannelMessage`] received on the
/// `display_input_authority` channel. Identity is captured at
/// construction time, so messages from this peer always apply
/// authority changes against this peer's
/// `(federation_connection_id, session_id)` — there's no way for one
/// federated session to act on behalf of another, even from the same
/// primary.
///
/// Display-ID mismatches are silently dropped: the federated peer's
/// `PeerDisplayConnection` is bound to one display, so a request for
/// any other display is a protocol bug on the browser side rather
/// than a recoverable condition. Authority gating still applies on
/// the input-injection path (F-2's job), so a misdirected message
/// here can't bypass anything.
fn build_federated_authority_handler(
    display_id: u32,
    federation_connection_id: String,
    session_id: String,
    authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: broadcast::Sender<DisplayInputAuthorityChange>,
    allow_input_authority: bool,
) -> crate::display::webrtc::AuthorityChannelHandler {
    use crate::display::webrtc::AuthorityChannelMessage;
    Arc::new(move |msg| match msg {
        AuthorityChannelMessage::Request {
            display_id: req_did,
        } if req_did == display_id && allow_input_authority => {
            apply_grant_input_authority_federated(
                display_id,
                federation_connection_id.clone(),
                session_id.clone(),
                &authority,
                &authority_change_tx,
            );
        }
        AuthorityChannelMessage::Request { .. } if !allow_input_authority => {
            // Read-only peer profile: view signaling is allowed, input authority
            // requests are ignored. The input-injection authorizer below is also
            // deny-all, so a malformed client cannot bypass this by sending input
            // without first becoming the holder.
        }
        AuthorityChannelMessage::Release {
            display_id: req_did,
        } if req_did == display_id => {
            apply_release_input_authority_federated(
                display_id,
                &federation_connection_id,
                &session_id,
                &authority,
                &authority_change_tx,
            );
        }
        AuthorityChannelMessage::Request { .. } | AuthorityChannelMessage::Release { .. } => {
            // Display-ID mismatch — drop silently. See doc comment.
        }
    })
}

/// Register a federated authority subscriber and start its fanout
/// task. Called from the federated `Offer` arm after a successful
/// `DisplaySession::handle_offer` and `get_peer` lookup.
///
/// Behavior, in order:
/// 1. Subscribe to `authority_change_tx` FIRST. Doing this before
///    the snapshot read closes the race where a holder change
///    arrives between the registry read and the subscribe — without
///    this ordering, that change would land on neither the snapshot
///    nor the fanout, and the chip would end up stale until the
///    next change.
/// 2. Compute the initial personalized snapshot from the current
///    registry state and send it via `peer.send_authority_state`.
///    F-1.2's pending-authority queue absorbs the case where the
///    `display_input_authority` data channel hasn't opened yet on
///    the federated browser side — the queued state flushes on
///    `OnDataChannel(OnOpen)` so the chip cannot start stuck on
///    `unknown`.
/// 3. Spawn the fanout task with the rx from step 1. It
///    personalizes each inbound change for this subscriber's
///    identity and pushes via `peer.send_authority_state`. Lagged
///    subscribers re-snapshot from the registry — same recovery
///    pattern as the local 5c lagged path so a momentary catch-up
///    cannot leave the chip on stale state.
/// 4. Insert the entry into `subscribers` keyed by
///    `(federation_connection_id, session_id, display_id)` so
///    cleanup edges can reach it.
///
/// Snapshot-vs-change ordering across the wire is FIFO via
/// `WebRtcPeer::send_authority_state`'s underlying `Command`
/// channel. If a change races the initial snapshot, both land on
/// the channel in the order they were enqueued; the more recent
/// one wins on the browser side, so the chip ends up correct
/// regardless of which arrives last.
fn register_federated_authority_subscriber(
    federation_connection_id: String,
    session_id: String,
    display_id: u32,
    peer: Arc<crate::display::webrtc::WebRtcPeer>,
    authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: broadcast::Sender<DisplayInputAuthorityChange>,
    subscribers: FederatedAuthoritySubscribers,
) {
    // 1. Subscribe BEFORE snapshot — closes the race window where a
    //    change between snapshot read and subscribe lands on neither
    //    path.
    let mut auth_rx = authority_change_tx.subscribe();

    // 2. Initial snapshot. F-1.2's queue handles "channel not open yet."
    let initial_state = {
        let map = authority.read().unwrap_or_else(|e| e.into_inner());
        personalize_authority_for_federated(
            map.get(&display_id),
            &federation_connection_id,
            &session_id,
        )
    };
    let peer_for_initial = Arc::clone(&peer);
    tokio::spawn(async move {
        let _ = peer_for_initial
            .send_authority_state(display_id, initial_state)
            .await;
    });

    // 3. Fanout task.
    let shutdown = tokio_util::sync::CancellationToken::new();
    let task_shutdown = shutdown.clone();
    let task_authority = Arc::clone(&authority);
    let task_fcid = federation_connection_id.clone();
    let task_sid = session_id.clone();
    let task_did = display_id;
    let task_peer = peer; // moved — registry doesn't keep a copy.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = task_shutdown.cancelled() => break,
                msg = auth_rx.recv() => match msg {
                    Ok(change) if change.display_id == task_did => {
                        let state = personalize_authority_for_federated(
                            change.holder.as_ref(),
                            &task_fcid,
                            &task_sid,
                        );
                        let _ = task_peer
                            .send_authority_state(task_did, state)
                            .await;
                    }
                    Ok(_) => {} // change for a different display
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Re-snapshot from registry — same recovery
                        // pattern as the local 5c lagged subscriber so
                        // the chip is never left stuck on stale state.
                        let state = {
                            let map = task_authority
                                .read()
                                .unwrap_or_else(|e| e.into_inner());
                            personalize_authority_for_federated(
                                map.get(&task_did),
                                &task_fcid,
                                &task_sid,
                            )
                        };
                        let _ = task_peer
                            .send_authority_state(task_did, state)
                            .await;
                    }
                }
            }
        }
    });

    // 4. Insert into the registry. Replace-on-collision: a duplicate
    //    `(fcid, sid, did)` would mean a renegotiated peer for the
    //    same identity; cancel the prior shutdown to terminate its
    //    fanout task before the new entry takes over.
    if let Some(prior) = subscribers
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            (federation_connection_id, session_id, display_id),
            FederatedAuthoritySubscriber { shutdown },
        )
    {
        prior.shutdown.cancel();
    }
}

/// Unregister one federated authority subscriber by exact identity.
/// Called from the federated `Close` arm. Cancels the fanout task
/// and removes the entry. Returns `true` if an entry was removed.
///
/// Does NOT release authority — that's
/// `apply_release_input_authority_federated`'s responsibility, called
/// alongside this function. Splitting the two keeps each helper
/// single-purpose: this one manages subscriber lifecycle, the other
/// manages the holder map.
fn unregister_federated_authority_subscriber(
    federation_connection_id: &str,
    session_id: &str,
    display_id: u32,
    subscribers: &FederatedAuthoritySubscribers,
) -> bool {
    if let Some(sub) = subscribers
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(
            federation_connection_id.to_string(),
            session_id.to_string(),
            display_id,
        ))
    {
        sub.shutdown.cancel();
        true
    } else {
        false
    }
}

/// Tear down every federated `WebRtcPeer` listed in `released`.
/// Called from the federation WS-close cleanup hook AFTER
/// [`unregister_all_federated_subscribers_for_connection`] returns
/// the surviving entries' `(session_id, display_id)` pairs. Without
/// this, the WebRTC data channels on those peers would stay alive
/// past the federation WS drop and could keep dispatching
/// `display_input_authority_request` frames against the registry —
/// the authority handler closure captures the
/// `federation_connection_id` at construction time, so a request
/// arriving after the WS-close would re-grant the (already-released)
/// authority under a now-defunct identity.
///
/// Tearing the peers down here is the structural fix: the federation
/// WS identity is the only thing tying these peers to a real user;
/// once it's gone the peers must go too. `DisplaySession::remove_peer`
/// closes the underlying WebRTC peer connection cleanly, which causes
/// every data channel on it to close and the driver task to exit —
/// no further authority frames can be processed.
///
/// Returns the count of peers actually removed. Missing displays
/// (display session torn down between Offer and WS-close) and
/// missing peers (already removed by an earlier `WebRtcSignal::Close`
/// for the same session) both fall through silently as no-ops on
/// `remove_peer`.
async fn close_federated_peers_for_sessions(
    released: &[(String, u32)],
    session_registry: Option<&Arc<tokio::sync::RwLock<crate::display::SessionRegistry>>>,
) -> usize {
    if released.is_empty() {
        return 0;
    }
    let Some(sr) = session_registry else {
        return 0;
    };
    // Snapshot Arcs out of the read guard first so per-peer awaits
    // (remove_peer's `peer.close()` chain) don't hold the registry
    // lock — same lock-discipline rationale as the local
    // `display_ice` handler that fixed the original 5-20s mDNS
    // starvation. The registry's RwLock is read-only here so a
    // concurrent display deactivate isn't blocked by us either way,
    // but keeping the pattern consistent prevents future regressions
    // if the lock semantics change.
    let mut targets: Vec<(Arc<crate::display::DisplaySession>, crate::display::PeerId)> =
        Vec::with_capacity(released.len());
    {
        let reg = sr.read().await;
        for (sid, did) in released {
            if let Some(session) = reg.get(*did) {
                targets.push((session, peer_id_for_federated_session(sid)));
            }
        }
    }
    let count = targets.len();
    for (session, pid) in targets {
        session.remove_peer(pid).await;
    }
    count
}

/// Unregister every federated authority subscriber for a dropping
/// federation transport. Called from the WS-close cleanup hook
/// alongside [`apply_federated_ws_close_input_authority`]. Returns
/// the `(session_id, display_id)` pairs that were unregistered, for
/// caller logging and for the post-step
/// [`close_federated_peers_for_sessions`] which actually tears down
/// the WebRtcPeers.
fn unregister_all_federated_subscribers_for_connection(
    federation_connection_id: &str,
    subscribers: &FederatedAuthoritySubscribers,
) -> Vec<(String, u32)> {
    let mut released = Vec::new();
    let mut map = subscribers.write().unwrap_or_else(|e| e.into_inner());
    map.retain(|(fcid, sid, did), sub| {
        if fcid == federation_connection_id {
            released.push((sid.clone(), *did));
            sub.shutdown.cancel();
            false
        } else {
            true
        }
    });
    released
}

/// Apply WS-close cleanup for a dropping connection.  Removes every
/// authority entry held by `connection_id` and emits one `None`-holder
/// authority change per affected display so observers move from
/// `you/other` back to `unclaimed`.  Returns the list of released
/// display ids for caller logging / tests.
///
/// Lock discipline: matches [`apply_grant_input_authority`].
fn apply_ws_close_input_authority(
    connection_id: &str,
    authority: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
    authority_change_tx: &broadcast::Sender<DisplayInputAuthorityChange>,
) -> Vec<u32> {
    let released: Vec<u32> = {
        let mut map = authority.write().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        map.retain(|did, entry| {
            if entry.matches_local_ws(connection_id) {
                out.push(*did);
                false
            } else {
                true
            }
        });
        out
    };
    for did in &released {
        let _ = authority_change_tx.send(DisplayInputAuthorityChange {
            display_id: *did,
            holder: None,
        });
    }
    released
}

/// Phase 5a.1 / 5c.2: build the personalized
/// `display_input_authority_state` snapshot a freshly-connecting browser
/// needs to bootstrap its chip from `unknown` to the authoritative state.
///
/// One entry per active display id, with `state` resolved against this
/// connection's id:
/// - `"you"` if `connection_id` currently holds the slot;
/// - `"other"` if some other connection holds it;
/// - `"unclaimed"` if no one holds it.
///
/// Holder connection ids never leave the daemon — the caller serializes
/// only the resolved `&'static str` into the `display_input_authority_state`
/// frame.
///
/// The frames built from this snapshot must be sent to `direct_tx`
/// **after** the `log_replay` block: replayed historical `display_ready` /
/// `user_display_revoked` events re-trigger `addDisplaySlot` /
/// `removeDisplaySlot` on the browser, which destroys the bootstrap slot
/// and creates a fresh one whose chip starts at `unknown`. Sending the
/// authority snapshot after replay guarantees it lands on the *final*
/// slot, so a late-connecting browser never gets stranded at `unknown`
/// for a display that already exists. See the
/// `bootstrap_authority_snapshots_*` tests for the regression coverage.
fn compute_bootstrap_authority_snapshots(
    active_display_ids: impl IntoIterator<Item = u32>,
    authority: &HashMap<u32, DisplayInputHolder>,
    connection_id: &str,
) -> Vec<(u32, &'static str)> {
    active_display_ids
        .into_iter()
        .map(|did| {
            let state = match authority.get(&did) {
                Some(entry) if entry.matches_local_ws(connection_id) => "you",
                Some(_) => "other",
                None => "unclaimed",
            };
            (did, state)
        })
        .collect()
}

pub const DEFAULT_PORT: u16 = 8765;

/// Mint a short-lived vendor session token server-side so the browser
/// never handles (or stores) a long-lived API key.
pub(crate) async fn mint_session_token(provider: &str, model: &str) -> Result<String, String> {
    match provider {
        "openai" => {
            let api_key = crate::credential_leases::provider_api_key("OPENAI_API_KEY")
                .ok_or_else(|| "OPENAI_API_KEY not set on server".to_string())?;
            let body = serde_json::json!({
                "model": model,
            });
            let resp = reqwest::Client::new()
                .post("https://api.openai.com/v1/realtime/sessions")
                .header("Authorization", format!("Bearer {}", api_key))
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("OpenAI request failed: {}", e))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("OpenAI HTTP {}: {}", status, text));
            }
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("OpenAI parse failed: {}", e))?;
            // Response may have token at top level or nested under client_secret
            let token = data["client_secret"]["value"]
                .as_str()
                .or_else(|| data["value"].as_str())
                .ok_or_else(|| format!("No token in OpenAI response: {}", data))?;
            let expires_at = data["client_secret"]["expires_at"]
                .as_i64()
                .or_else(|| data["expires_at"].as_i64())
                .unwrap_or(0);
            Ok(serde_json::json!({
                "client_secret": { "value": token },
                "expires_at": expires_at
            })
            .to_string())
        }
        "gemini" => {
            let api_key = crate::credential_leases::provider_api_key("GEMINI_API_KEY")
                .ok_or_else(|| "GEMINI_API_KEY not set on server".to_string())?;
            let body = serde_json::json!({
                "uses": 1,
                "bidi_generate_content_setup": {
                    "model": format!("models/{}", model),
                    "generation_config": {
                        "response_modalities": ["AUDIO"],
                        "speech_config": {
                            "voice_config": {
                                "prebuilt_voice_config": {
                                    "voice_name": "Aoede"
                                }
                            }
                        }
                    }
                }
            });
            let url = format!(
                "https://generativelanguage.googleapis.com/v1alpha/auth_tokens?key={}",
                api_key
            );
            let resp = reqwest::Client::new()
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("Gemini request failed: {}", e))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("Gemini HTTP {}: {}", status, text));
            }
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("Gemini parse failed: {}", e))?;
            let token = data["name"]
                .as_str()
                .ok_or("No 'name' in Gemini response")?;
            Ok(serde_json::json!({ "token": token }).to_string())
        }
        _ => Err(format!("Unknown provider: {}", provider)),
    }
}

// Browser-facing external replay is a live UI bootstrap, not an archival export.
// Keep it bounded; native rollout files and session search remain the audit source.

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




async fn connect_dashboard_offer_response(
    dashboard_control: &Arc<crate::dashboard_control::DashboardControlRegistry>,
    body_text: &str,
    agent_card: &serde_json::Value,
) -> String {
    let body = match serde_json::from_str::<serde_json::Value>(body_text) {
        Ok(body) => body,
        Err(e) => return json_error("400 Bad Request", format!("invalid JSON: {e}")),
    };
    let sdp = body.get("sdp").and_then(|v| v.as_str()).unwrap_or("");
    if sdp.trim().is_empty() {
        return json_error("400 Bad Request", "missing sdp");
    }
    let client_nonce = body
        .get("client_nonce")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|nonce| !nonce.is_empty())
        .map(str::to_string);
    // Reaching this path already required a trusted transport (mTLS or local
    // loopback), so sessions stay root-compatible. A signed browser identity
    // key refines that: a key with a local IAM grant binds the session to
    // that scoped principal, and an ungranted key keeps root authority but
    // surfaces its fingerprint so the Access UI can offer enrollment. An
    // invalid signature fails closed.
    let client_key_fields: crate::access::client_key::ClientKeyOfferFields =
        serde_json::from_value(body.clone()).unwrap_or_default();
    let verified_client_key = match client_key_fields.verify(
        "",
        client_nonce.as_deref().unwrap_or(""),
        sdp,
        crate::access::client_key::now_unix_ms(),
    ) {
        Ok(verified) => verified,
        Err(e) => {
            return json_error(
                "400 Bad Request",
                format!("client key verification failed: {e}"),
            )
        }
    };
    // Org-grant ride-along (phase 6 step 4), same as the rendezvous path:
    // materialize before grant resolution so a member's first offer binds
    // its scoped principal instead of falling back to trusted-transport
    // root. Failure changes nothing here — the transport already earned
    // its authority — but the error is surfaced for the console.
    let org_grant_error = body
        .get("org_grant")
        .filter(|doc| !doc.is_null())
        .and_then(|doc| {
            crate::access::org::present_org_grant_value(
                doc,
                &org_target_agent_card_ids(agent_card),
                crate::access::client_key::now_unix_ms() as u64,
            )
            .err()
        });
    let grant = match verified_client_key {
        Some(key) => {
            let cert_dir = crate::access::backend::select_backend().cert_dir();
            let loaded = load_local_iam_state_for_request(&cert_dir).ok().flatten();
            let bound = loaded.as_ref().and_then(|state| {
                crate::access::iam::principal_for_client_key(
                    state,
                    &key.fingerprint,
                    "local-dashboard-control",
                )
                .or_else(|| {
                    crate::access::iam::principal_for_client_key_any_status(
                        state,
                        &key.fingerprint,
                        "local-dashboard-control",
                    )
                })
                .map(|principal| (principal, state.clone()))
            });
            match bound {
                Some((principal, iam_state)) => {
                    crate::dashboard_control::DashboardControlGrant::UserClient {
                        principal,
                        iam_state,
                    }
                }
                None => crate::dashboard_control::DashboardControlGrant::UserClientRoot {
                    principal:
                        crate::access::iam::AccessPrincipal::root_dashboard_session_with_client_key(
                            "dashboard-control",
                            "local-dashboard-control",
                            &key.fingerprint,
                            &key.public_key_b64u,
                        ),
                },
            }
        }
        None => crate::dashboard_control::DashboardControlGrant::TrustedLocal,
    };
    match dashboard_control
        .answer_offer_with_grant(sdp.to_string(), None, client_nonce, grant)
        .await
    {
        Ok(answer) => {
            let mut response = serde_json::json!({
                "ok": true,
                "signaling": "connect-bootstrap-local",
                "session_id": answer.session_id,
                "sdp": answer.sdp,
                "binding": answer.binding,
            });
            if let Some(org_error) = org_grant_error {
                response["org_grant_error"] = serde_json::Value::String(org_error);
            }
            json_ok(response)
        }
        Err(e) => json_error("500 Internal Server Error", e),
    }
}

async fn connect_dashboard_ice_response(
    dashboard_control: &Arc<crate::dashboard_control::DashboardControlRegistry>,
    body_text: &str,
) -> String {
    let body = match serde_json::from_str::<serde_json::Value>(body_text) {
        Ok(body) => body,
        Err(e) => return json_error("400 Bad Request", format!("invalid JSON: {e}")),
    };
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if session_id.is_empty() {
        return json_error("400 Bad Request", "missing session_id");
    }
    let candidate = body
        .get("candidate")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    match dashboard_control
        .add_ice_candidate(session_id, &candidate)
        .await
    {
        Ok(true) => json_ok(serde_json::json!({ "ok": true })),
        Ok(false) => json_error("404 Not Found", "dashboard control session not found"),
        Err(e) => json_error("500 Internal Server Error", e),
    }
}

async fn connect_dashboard_close_response(
    dashboard_control: &Arc<crate::dashboard_control::DashboardControlRegistry>,
    body_text: &str,
) -> String {
    let body = match serde_json::from_str::<serde_json::Value>(body_text) {
        Ok(body) => body,
        Err(e) => return json_error("400 Bad Request", format!("invalid JSON: {e}")),
    };
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if session_id.is_empty() {
        return json_error("400 Bad Request", "missing session_id");
    }
    dashboard_control.close(session_id).await;
    json_ok(serde_json::json!({ "ok": true }))
}

fn connect_bootstrap_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Intendant Connect Bootstrap</title>
  <style>
    :root { color-scheme: dark; font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background: #11111b; color: #cdd6f4; }
    body { margin: 0; min-height: 100vh; display: grid; place-items: center; }
    main { width: min(760px, calc(100vw - 32px)); }
    h1 { font-size: 22px; margin: 0 0 16px; }
    pre { white-space: pre-wrap; overflow-wrap: anywhere; padding: 16px; background: #181825; border: 1px solid #45475a; border-radius: 8px; }
    .ok { color: #a6e3a1; }
    .err { color: #f38ba8; }
  </style>
</head>
<body>
  <main>
    <h1>Intendant Connect Bootstrap</h1>
    <pre id="status">starting</pre>
  </main>
  <script>
(() => {
	  const statusEl = document.getElementById('status');
	  const MAX_CHUNKED_RESPONSE_BYTES = 128 * 1024 * 1024;
	  const MAX_BYTE_STREAM_BYTES = 128 * 1024 * 1024;
	  const UPLOAD_CHUNK_BYTES = 16 * 1024;
	  const UPLOAD_BUFFER_HIGH_BYTES = 1024 * 1024;
	  function paint(message, kind = '') {
	    statusEl.textContent = typeof message === 'string' ? message : JSON.stringify(message, null, 2);
	    statusEl.className = kind;
	  }

  function abortError(message = 'dashboard control request aborted') {
    try { return new DOMException(message, 'AbortError'); } catch {
      const err = new Error(message);
      err.name = 'AbortError';
      return err;
    }
  }

  function bytesToBase64Url(bytes) {
    let binary = '';
    for (const b of bytes) binary += String.fromCharCode(b);
    return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/g, '');
  }

  function base64UrlToBytes(value) {
    const padded = String(value || '').replace(/-/g, '+').replace(/_/g, '/').padEnd(Math.ceil(String(value || '').length / 4) * 4, '=');
    const binary = atob(padded);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
  }

	  function base64ToBytes(value) {
	    const binary = atob(String(value || ''));
	    const bytes = new Uint8Array(binary.length);
	    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
	    return bytes;
	  }

	  function bytesToBase64(bytes) {
	    let binary = '';
	    for (let i = 0; i < bytes.byteLength; i++) binary += String.fromCharCode(bytes[i]);
	    return btoa(binary);
	  }

  async function sha256B64u(text) {
    const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(String(text)));
    return bytesToBase64Url(new Uint8Array(digest));
  }

  function randomB64u(byteLength = 32) {
    const bytes = new Uint8Array(byteLength);
    crypto.getRandomValues(bytes);
    return bytesToBase64Url(bytes);
  }

  function bindingPayload(binding) {
    const parts = [
      binding.protocol || '',
      binding.session_id || '',
      binding.daemon_public_key || '',
      String(binding.created_unix_ms || ''),
      String(binding.expires_unix_ms || ''),
      binding.offer_sha256 || '',
      binding.answer_sha256 || '',
    ];
    if (binding.client_nonce) parts.push(binding.client_nonce);
    if (binding.session_grant_sha256) parts.push(binding.session_grant_sha256);
    return parts.join('\n');
  }

  async function verifyEd25519(publicKeyBytes, signatureBytes, payloadBytes) {
    let key;
    try {
      key = await crypto.subtle.importKey('raw', publicKeyBytes, { name: 'Ed25519' }, false, ['verify']);
    } catch (firstErr) {
      try {
        key = await crypto.subtle.importKey('raw', publicKeyBytes, 'Ed25519', false, ['verify']);
      } catch {
        throw firstErr;
      }
    }
    return crypto.subtle.verify({ name: 'Ed25519' }, key, signatureBytes, payloadBytes);
  }

  async function verifyBinding(binding, sessionId, offerSdp, answerSdp, clientNonce = '') {
    if (!binding || typeof binding !== 'object') return { ok: false, error: 'missing binding' };
    if (binding.protocol !== 'intendant-dashboard-control-v1') return { ok: false, error: 'unexpected protocol' };
    if (String(binding.session_id || '') !== String(sessionId || '')) return { ok: false, error: 'session mismatch' };
    if (!crypto?.subtle) return { ok: false, error: 'WebCrypto unavailable' };
    const createdUnixMs = Number(binding.created_unix_ms || 0);
    const expiresUnixMs = Number(binding.expires_unix_ms || 0);
    if (!Number.isFinite(createdUnixMs) || createdUnixMs <= 0) return { ok: false, error: 'missing binding creation time' };
    if (!Number.isFinite(expiresUnixMs) || expiresUnixMs <= 0) return { ok: false, error: 'missing binding expiry' };
    const nowUnixMs = Date.now();
    if (expiresUnixMs + 30000 < nowUnixMs) return { ok: false, error: 'binding expired' };
    if (createdUnixMs - 30000 > nowUnixMs) return { ok: false, error: 'binding timestamp from future' };
    if (binding.offer_sha256 !== await sha256B64u(offerSdp || '')) return { ok: false, error: 'offer hash mismatch' };
    if (binding.answer_sha256 !== await sha256B64u(answerSdp || '')) return { ok: false, error: 'answer hash mismatch' };
    const nonce = String(clientNonce || '');
    if (nonce) {
      if (String(binding.client_nonce || '') !== nonce) return { ok: false, error: 'client nonce mismatch' };
    } else if (binding.client_nonce) {
      return { ok: false, error: 'unexpected client nonce binding' };
    }
    const verified = await verifyEd25519(
      base64UrlToBytes(binding.daemon_public_key || ''),
      base64UrlToBytes(binding.signature || ''),
      new TextEncoder().encode(bindingPayload(binding))
    );
    if (!verified) return { ok: false, error: 'signature invalid' };
    return {
      ok: true,
      daemonPublicKey: binding.daemon_public_key,
      createdUnixMs,
      expiresUnixMs,
      clientNonce: binding.client_nonce || '',
    };
  }

  const connect = {
    pc: null,
    channel: null,
    sessionId: '',
    binding: null,
    verifiedBinding: null,
    clientNonce: '',
    expiresUnixMs: 0,
    localOfferSdp: '',
	    pendingIce: [],
	    pending: new Map(),
	    chunkedResponses: new Map(),
	    byteStreams: new Map(),
	    completedChunkedResponses: 0,
	    completedByteStreams: 0,
	    seq: 0,
	    started: false,

    async start() {
      if (this.started) return this.status();
      this.started = true;
	      this.pc = new RTCPeerConnection({});
	      this.channel = this.pc.createDataChannel('intendant-dashboard-control', { ordered: true });
	      this.channel.onopen = () => {
	        this.sendFrame({ t: 'hello', id: this.nextId(), features: ['response_credit', 'byte_streams', 'upload_frames'] });
	        paint(this.status(), 'ok');
	      };
      this.channel.onmessage = ev => this.handleMessage(ev.data);
      this.channel.onclose = () => paint(this.status());
      this.pc.onconnectionstatechange = () => paint(this.status(), this.pc.connectionState === 'failed' ? 'err' : '');
      this.pc.onicecandidate = ev => {
        if (!ev.candidate) return;
        const candidate = ev.candidate.toJSON ? ev.candidate.toJSON() : ev.candidate;
        if (!this.sessionId) {
          this.pendingIce.push(candidate);
          return;
        }
        this.sendIce(candidate).catch(err => console.warn('connect ice failed', err));
      };
      const offer = await this.pc.createOffer();
      await this.pc.setLocalDescription(offer);
      this.localOfferSdp = offer.sdp || '';
      this.clientNonce = randomB64u(32);
      const answer = await fetch('/connect/dashboard/offer', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ sdp: this.localOfferSdp, client_nonce: this.clientNonce }),
      }).then(async resp => {
        const body = await resp.json().catch(() => ({}));
        if (!resp.ok) throw new Error(body.error || `HTTP ${resp.status}`);
        return body;
      });
      this.sessionId = String(answer.session_id || '');
      this.binding = answer.binding || null;
      const verified = await verifyBinding(this.binding, this.sessionId, this.localOfferSdp, answer.sdp || '', this.clientNonce);
      if (!verified.ok) throw new Error(`binding rejected: ${verified.error || 'unknown'}`);
      this.verifiedBinding = verified;
      this.expiresUnixMs = verified.expiresUnixMs || 0;
      await this.pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
      for (const candidate of this.pendingIce.splice(0)) await this.sendIce(candidate);
      paint(this.status(), 'ok');
      return this.status();
    },

    async sendIce(candidate) {
      await fetch('/connect/dashboard/ice', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ session_id: this.sessionId, candidate }),
      });
    },

    handleMessage(data) {
      let msg;
      try { msg = JSON.parse(String(data)); } catch { return; }
      this.handleFrame(msg);
    },

    handleFrame(msg) {
      if (msg.t === 'hello_ack') {
        paint(this.status(), 'ok');
        return;
      }
      if (msg.t === 'response_start') {
        this.handleResponseStart(msg);
        return;
      }
      if (msg.t === 'response_chunk') {
        this.handleResponseChunk(msg);
        return;
      }
	      if (msg.t === 'response_end') {
	        this.handleResponseEnd(msg);
	        return;
	      }
	      if (msg.t === 'byte_stream_start') {
	        this.handleByteStreamStart(msg);
	        return;
	      }
	      if (msg.t === 'byte_stream_chunk') {
	        this.handleByteStreamChunk(msg);
	        return;
	      }
	      if (msg.t === 'byte_stream_end') {
	        this.handleByteStreamEnd(msg);
	        return;
	      }
	      if (msg.t !== 'pong' && msg.t !== 'response') return;
      const pending = this.pending.get(msg.id);
      if (!pending) return;
      this.pending.delete(msg.id);
      if (msg.cancelled) pending.reject(abortError(msg.error || 'dashboard control request cancelled'));
      else if (msg.t === 'response' && msg.ok === false) pending.reject(new Error(msg.error || 'dashboard control request failed'));
      else pending.resolve(msg.t === 'pong' ? msg : msg.result);
    },

    handleResponseStart(msg) {
      const id = String(msg.id || '');
      if (!id || !this.pending.has(id)) return;
      const totalBytes = Number(msg.total_bytes);
      const expectedChunks = Number(msg.chunks);
      if (
        msg.encoding !== 'base64-json-frame' ||
        !Number.isSafeInteger(totalBytes) ||
        totalBytes < 0 ||
        totalBytes > MAX_CHUNKED_RESPONSE_BYTES ||
        !Number.isSafeInteger(expectedChunks) ||
        expectedChunks < 0
      ) {
        this.rejectChunkedResponse(id, 'invalid connect dashboard chunked response header');
        return;
      }
      this.chunkedResponses.set(id, {
        totalBytes,
        expectedChunks,
        receivedBytes: 0,
        chunks: new Map(),
        ended: false,
      });
      paint(this.status(), 'ok');
    },

    handleResponseChunk(msg) {
      const id = String(msg.id || '');
      const state = this.chunkedResponses.get(id);
      if (!state) return;
      const seq = Number(msg.seq);
      if (!Number.isSafeInteger(seq) || seq < 0 || seq >= state.expectedChunks) {
        this.rejectChunkedResponse(id, 'invalid connect dashboard chunk sequence');
        return;
      }
      if (state.chunks.has(seq)) return;
      let bytes;
      try {
        bytes = base64ToBytes(msg.data);
      } catch {
        this.rejectChunkedResponse(id, 'invalid connect dashboard chunk encoding');
        return;
      }
      state.chunks.set(seq, bytes);
      state.receivedBytes += bytes.byteLength;
      if (state.receivedBytes > state.totalBytes) {
        this.rejectChunkedResponse(id, 'connect dashboard chunked response exceeded declared size');
        return;
      }
      const completed = this.maybeCompleteChunkedResponse(id);
      if (!completed && this.chunkedResponses.has(id)) {
        this.sendChunkCredit(id, 1);
      }
      paint(this.status(), 'ok');
    },

    handleResponseEnd(msg) {
      const id = String(msg.id || '');
      const state = this.chunkedResponses.get(id);
      if (!state) return;
      const finalChunks = Number(msg.chunks);
      if (!Number.isSafeInteger(finalChunks) || finalChunks !== state.expectedChunks) {
        this.rejectChunkedResponse(id, 'invalid connect dashboard chunked response footer');
        return;
      }
      state.ended = true;
      this.maybeCompleteChunkedResponse(id);
      paint(this.status(), 'ok');
    },

    maybeCompleteChunkedResponse(id) {
      const state = this.chunkedResponses.get(id);
      if (!state || !state.ended || state.chunks.size !== state.expectedChunks) return false;
      const merged = new Uint8Array(state.totalBytes);
      let offset = 0;
      for (let seq = 0; seq < state.expectedChunks; seq++) {
        const chunk = state.chunks.get(seq);
        if (!chunk) {
          this.rejectChunkedResponse(id, 'connect dashboard chunked response missed a chunk');
          return false;
        }
        merged.set(chunk, offset);
        offset += chunk.byteLength;
      }
      if (offset !== state.totalBytes) {
        this.rejectChunkedResponse(id, 'connect dashboard chunked response size mismatch');
        return false;
      }
      this.chunkedResponses.delete(id);
      let frame;
      try {
        frame = JSON.parse(new TextDecoder().decode(merged));
      } catch {
        this.rejectChunkedResponse(id, 'connect dashboard chunked response was not valid JSON');
        return false;
      }
      if (frame.t !== 'response' || String(frame.id || '') !== id) {
        this.rejectChunkedResponse(id, 'connect dashboard chunked response id mismatch');
        return false;
      }
      this.completedChunkedResponses += 1;
      this.handleFrame(frame);
      return true;
    },

    rejectChunkedResponse(id, message) {
      this.chunkedResponses.delete(id);
      const pending = this.pending.get(id);
      if (pending) {
        this.pending.delete(id);
        pending.reject(new Error(message));
      }
	      paint(this.status(), 'err');
	    },

	    handleByteStreamStart(msg) {
	      const id = String(msg.id || '');
	      const streamId = String(msg.stream_id || id);
	      if (!id || !streamId || !this.pending.has(id)) return;
	      const totalBytes = Number(msg.total_bytes);
	      const expectedChunks = Number(msg.chunks);
	      if (
	        msg.encoding !== 'base64' ||
	        !Number.isSafeInteger(totalBytes) ||
	        totalBytes < 0 ||
	        totalBytes > MAX_BYTE_STREAM_BYTES ||
	        !Number.isSafeInteger(expectedChunks) ||
	        expectedChunks < 0
	      ) {
	        this.rejectByteStream(streamId, 'invalid connect dashboard byte stream header', id);
	        return;
	      }
	      this.byteStreams.set(streamId, {
	        id,
	        streamId,
	        totalBytes,
	        expectedChunks,
	        receivedBytes: 0,
	        chunks: new Map(),
	        ended: false,
	        result: null,
	        contentType: String(msg.content_type || 'application/octet-stream'),
	        filename: msg.filename ? String(msg.filename) : '',
	      });
	      paint(this.status(), 'ok');
	    },

	    handleByteStreamChunk(msg) {
	      const id = String(msg.id || '');
	      const streamId = String(msg.stream_id || id);
	      const state = this.byteStreams.get(streamId);
	      if (!state) return;
	      const seq = Number(msg.seq);
	      if (!Number.isSafeInteger(seq) || seq < 0 || seq >= state.expectedChunks) {
	        this.rejectByteStream(streamId, 'invalid connect dashboard byte stream chunk sequence');
	        return;
	      }
	      if (state.chunks.has(seq)) return;
	      let bytes;
	      try {
	        bytes = base64ToBytes(msg.data);
	      } catch {
	        this.rejectByteStream(streamId, 'invalid connect dashboard byte stream encoding');
	        return;
	      }
	      state.chunks.set(seq, bytes);
	      state.receivedBytes += bytes.byteLength;
	      if (state.receivedBytes > state.totalBytes) {
	        this.rejectByteStream(streamId, 'connect dashboard byte stream exceeded declared size');
	        return;
	      }
	      const completed = this.maybeCompleteByteStream(streamId);
	      if (!completed && this.byteStreams.has(streamId)) {
	        this.sendChunkCredit(id, 1, streamId === id ? null : streamId);
	      }
	      paint(this.status(), 'ok');
	    },

	    handleByteStreamEnd(msg) {
	      const id = String(msg.id || '');
	      const streamId = String(msg.stream_id || id);
	      const state = this.byteStreams.get(streamId);
	      if (!state) return;
	      if (msg.ok === false) {
	        this.rejectByteStream(streamId, msg.error || 'connect dashboard byte stream failed');
	        return;
	      }
	      const finalChunks = Number(msg.chunks);
	      if (!Number.isSafeInteger(finalChunks) || finalChunks !== state.expectedChunks) {
	        this.rejectByteStream(streamId, 'invalid connect dashboard byte stream footer');
	        return;
	      }
	      state.ended = true;
	      state.result = msg.result || null;
	      this.maybeCompleteByteStream(streamId);
	      paint(this.status(), 'ok');
	    },

	    maybeCompleteByteStream(streamId) {
	      const state = this.byteStreams.get(streamId);
	      if (!state || !state.ended || state.chunks.size !== state.expectedChunks) return false;
	      const merged = new Uint8Array(state.totalBytes);
	      let offset = 0;
	      for (let seq = 0; seq < state.expectedChunks; seq++) {
	        const chunk = state.chunks.get(seq);
	        if (!chunk) {
	          this.rejectByteStream(streamId, 'connect dashboard byte stream missed a chunk');
	          return false;
	        }
	        merged.set(chunk, offset);
	        offset += chunk.byteLength;
	      }
	      if (offset !== state.totalBytes) {
	        this.rejectByteStream(streamId, 'connect dashboard byte stream size mismatch');
	        return false;
	      }
	      this.byteStreams.delete(streamId);
	      const pending = this.pending.get(state.id);
	      if (!pending) return true;
	      const result = state.result && typeof state.result === 'object' && !Array.isArray(state.result)
	        ? { ...state.result }
	        : {};
	      result.ok = result.ok !== false;
	      result.bytes = merged;
	      result.size = state.totalBytes;
	      result.content_type = result.content_type || state.contentType;
	      result.filename = result.filename || state.filename;
	      result.stream_id = state.streamId;
	      this.completedByteStreams += 1;
	      this.pending.delete(state.id);
	      this.chunkedResponses.delete(state.id);
	      pending.resolve(result);
	      paint(this.status(), 'ok');
	      return true;
	    },

	    rejectByteStream(streamId, message, requestId = '') {
	      const state = this.byteStreams.get(streamId);
	      const id = state?.id || requestId || streamId;
	      this.byteStreams.delete(streamId);
	      const pending = this.pending.get(id);
	      if (pending) {
	        this.pending.delete(id);
	        pending.reject(new Error(message));
	      }
	      paint(this.status(), 'err');
	    },

	    request(method, params = {}, options = {}) {
	      if (options.signal?.aborted) return Promise.reject(abortError());
	      if (!this.canUseRpc()) return Promise.reject(new Error('connect dashboard RPC is not connected'));
	      const id = this.nextId();
	      const promise = this.waitFor(id, options);
	      this.sendFrame({ t: 'request', id, method, params });
	      return promise;
	    },

	    requestBytes(method, params = {}, options = {}) {
	      if (options.signal?.aborted) return Promise.reject(abortError());
	      if (!this.canUseRpc()) return Promise.reject(new Error('connect dashboard byte stream is not connected'));
	      const id = this.nextId();
	      const promise = this.waitFor(id, options);
	      this.sendFrame({ t: 'request', id, method, params, bytes: true });
	      return promise;
	    },

	    async uploadBytes(method, params = {}, bytes, options = {}) {
	      if (options.signal?.aborted) return Promise.reject(abortError());
	      if (!this.canUseRpc()) return Promise.reject(new Error('connect dashboard upload is not connected'));
	      const id = this.nextId();
	      const totalBytes = Number(bytes?.size ?? bytes?.byteLength ?? bytes?.length ?? 0);
	      const chunkSize = options.chunkBytes || UPLOAD_CHUNK_BYTES;
	      const chunks = Math.ceil(totalBytes / chunkSize);
	      const promise = this.waitFor(id, options);
	      this.sendFrame({
	        t: 'upload_start',
	        id,
	        method,
	        params,
	        encoding: 'base64',
	        total_bytes: totalBytes,
	        chunks,
	      });
	      try {
	        for (let seq = 0, offset = 0; offset < totalBytes; seq += 1, offset += chunkSize) {
	          if (options.signal?.aborted) throw abortError();
	          if (!this.pending.has(id)) break;
	          const end = Math.min(offset + chunkSize, totalBytes);
	          let chunk;
	          if (bytes instanceof Blob) {
	            chunk = new Uint8Array(await bytes.slice(offset, end).arrayBuffer());
	          } else if (bytes instanceof Uint8Array) {
	            chunk = bytes.subarray(offset, end);
	          } else {
	            chunk = new Uint8Array(bytes.slice(offset, end));
	          }
	          this.sendFrame({
	            t: 'upload_chunk',
	            id,
	            seq,
	            data: bytesToBase64(chunk),
	          });
	          await this.waitForBufferedAmountLow(options.signal);
	        }
	        if (this.pending.has(id)) this.sendFrame({ t: 'upload_end', id, chunks });
	      } catch (err) {
	        if (this.pending.has(id)) this.sendFrame({ t: 'cancel', id });
	        throw err;
	      }
	      return promise;
	    },

	    async waitForBufferedAmountLow(signal = null) {
	      while (
	        this.channel &&
	        this.channel.readyState === 'open' &&
	        this.channel.bufferedAmount > UPLOAD_BUFFER_HIGH_BYTES
	      ) {
	        if (signal?.aborted) throw abortError();
	        await new Promise(resolve => setTimeout(resolve, 10));
	      }
	    },

    waitFor(id, options = {}) {
      return new Promise((resolve, reject) => {
        let settled = false;
        const signal = options.signal || null;
        const fail = (err, cancel = false) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
	          this.pending.delete(id);
	          this.chunkedResponses.delete(id);
	          this.deleteByteStreamsForRequest(id);
	          if (cancel) this.sendFrame({ t: 'cancel', id });
	          reject(err);
	        };
        const abortHandler = signal ? () => fail(abortError(), true) : null;
        const timeoutMs = Number.isFinite(Number(options.timeoutMs)) ? Number(options.timeoutMs) : 10000;
        const timer = setTimeout(() => fail(new Error('connect dashboard request timed out'), true), timeoutMs);
        if (signal && abortHandler) signal.addEventListener('abort', abortHandler, { once: true });
        this.pending.set(id, {
	          resolve: value => {
	            if (settled) return;
	            settled = true;
	            clearTimeout(timer);
	            if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
	            this.chunkedResponses.delete(id);
	            this.deleteByteStreamsForRequest(id);
	            resolve(value);
	          },
          reject: err => fail(err),
        });
      });
    },

    canUseRpc() {
      return Boolean(this.verifiedBinding && this.pc?.connectionState === 'connected' && this.channel?.readyState === 'open');
    },

    sendFrame(frame) {
      if (this.channel?.readyState === 'open') this.channel.send(JSON.stringify(frame));
    },

	    sendChunkCredit(id, chunks, chunkId = null) {
	      const frame = { t: 'credit', id, chunks };
	      if (chunkId) frame.chunk_id = chunkId;
	      this.sendFrame(frame);
	    },

	    deleteByteStreamsForRequest(id) {
	      for (const [streamId, state] of this.byteStreams) {
	        if (streamId === id || state?.id === id) this.byteStreams.delete(streamId);
	      }
	    },

    status() {
      return {
        connected: this.pc?.connectionState === 'connected',
        pcState: this.pc?.connectionState || '',
	        channelState: this.channel?.readyState || '',
	        sessionId: this.sessionId,
	        verifiedBinding: this.verifiedBinding,
	        clientNonce: this.clientNonce,
	        expiresUnixMs: this.expiresUnixMs,
	        pendingRequests: this.pending.size,
	        pendingChunkedResponses: this.chunkedResponses.size,
	        pendingByteStreams: this.byteStreams.size,
	        completedChunkedResponses: this.completedChunkedResponses,
	        completedByteStreams: this.completedByteStreams,
	      };
	    },

    close() {
      if (this.sessionId) {
        fetch('/connect/dashboard/close', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ session_id: this.sessionId }),
        }).catch(() => {});
	      }
	      this.chunkedResponses.clear();
	      this.byteStreams.clear();
	      try { this.channel?.close(); } catch {}
      try { this.pc?.close(); } catch {}
    },

    nextId() {
      this.seq += 1;
      return `connect-${Date.now()}-${this.seq}`;
    },
  };

  window.intendantConnectDashboard = connect;
  connect.start().catch(err => {
    console.error(err);
    paint(err?.message || String(err), 'err');
  });
})();
  </script>
</body>
</html>"#
        .to_string()
}






















































































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






/// Settings payload for GET/POST /api/settings.
/// Flattened view of intendant.toml sections relevant to the web dashboard.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SettingsPayload {
    // Computer Use
    pub cu_provider: Option<String>,
    pub cu_model: Option<String>,
    pub cu_backend: String,
    /// Read-only: `[experimental] cu_first_routing` from intendant.toml.
    /// The dashboard shows the CU provider/model rows only when the
    /// vaulted routing is enabled; the flag itself is file-only.
    #[serde(default)]
    pub cu_first_routing: bool,
    // Presence
    pub presence_enabled: bool,
    pub presence_provider: Option<String>,
    pub presence_model: Option<String>,
    pub presence_live_provider: Option<String>,
    pub presence_live_model: Option<String>,
    // Transcription
    pub transcription_enabled: bool,
    pub transcription_provider: String,
    pub transcription_model: String,
    pub transcription_endpoint: Option<String>,
    pub transcription_language: Option<String>,
    // Recording
    pub recording_enabled: bool,
    pub recording_framerate: u32,
    pub recording_quality: String,
    // Live Audio
    pub live_audio_enabled: bool,
    pub live_audio_timeout_secs: u64,
    // External agent default (persisted to `[agent] default_backend`).
    // Values: "codex" | "claude-code" | "gemini" | None (internal agent).
    #[serde(default)]
    pub external_agent: Option<String>,
    // Codex runtime config (persisted to `[agent.codex]`). Mirrored here so
    // the Activity → Control sub-tab can load in one fetch.
    #[serde(default)]
    pub codex_command: Option<String>,
    /// Managed-capable (Intendant-aware fork) codex binary; managed
    /// sessions spawn it instead of `codex_command`. Empty string clears.
    #[serde(default)]
    pub codex_managed_command: Option<String>,
    #[serde(default)]
    pub codex_sandbox: Option<String>,
    #[serde(default)]
    pub codex_approval_policy: Option<String>,
    #[serde(default)]
    pub codex_model: Option<String>,
    #[serde(default)]
    pub codex_reasoning_effort: Option<String>,
    // Empty / omitted = inherit Codex config; "standard" sends an explicit
    // normal/clear override for Intendant-managed Codex sessions.
    #[serde(default)]
    pub codex_service_tier: Option<String>,
    #[serde(default)]
    pub codex_web_search: bool,
    #[serde(default)]
    pub codex_network_access: bool,
    #[serde(default)]
    pub codex_writable_roots: Vec<String>,
    #[serde(default, alias = "codex_context_recovery")]
    pub codex_managed_context: Option<String>,
    #[serde(default)]
    pub codex_context_archive: Option<String>,
    // Other external-agent executable commands. The Settings pane does not
    // edit these today, but the New Session pane uses them as per-launch
    // command/path defaults.
    #[serde(default)]
    pub claude_command: Option<String>,
    // Claude Code runtime config (persisted to `[agent.claude_code]`).
    // Mirrors the Codex/Gemini fields for the Activity → Control sub-tab.
    #[serde(default)]
    pub claude_model: Option<String>,
    #[serde(default)]
    pub claude_permission_mode: Option<String>,
    #[serde(default)]
    pub claude_allowed_tools: Option<Vec<String>>,
    // Per-category approval rules (persisted to `[approval]`). Exposed here
    // for the dashboard's "Approval rules" controls to populate the selects.
    // Live edits flow through the `set_approval_rule` ControlMsg, not through
    // `apply_settings_payload`, so these are display/read-only in the payload.
    // Values: "auto" | "ask" | "deny".
    #[serde(default = "default_settings_approval_auto")]
    pub approval_file_read: String,
    #[serde(default = "default_settings_approval_ask")]
    pub approval_file_write: String,
    #[serde(default = "default_settings_approval_ask")]
    pub approval_file_delete: String,
    #[serde(default = "default_settings_approval_auto")]
    pub approval_command_exec: String,
    #[serde(default = "default_settings_approval_auto")]
    pub approval_network: String,
    #[serde(default = "default_settings_approval_ask")]
    pub approval_destructive: String,
    #[serde(default = "default_settings_approval_ask")]
    pub approval_display_control: String,
    #[serde(default = "default_settings_approval_auto")]
    pub approval_tool_call: String,
    // Env var overrides (read-only, shown in UI)
    #[serde(default)]
    pub env_overrides: std::collections::HashMap<String, String>,
}

fn default_settings_approval_auto() -> String {
    crate::autonomy::ApprovalRule::Auto.as_str().to_string()
}

fn default_settings_approval_ask() -> String {
    crate::autonomy::ApprovalRule::Ask.as_str().to_string()
}

fn normalize_settings_codex_command(input: Option<&str>) -> String {
    normalize_settings_agent_command(input, "codex")
}

fn normalize_settings_agent_command(input: Option<&str>, fallback: &str) -> String {
    let trimmed = input.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn settings_payload_from_config(config: &crate::project::ProjectConfig) -> SettingsPayload {
    let mut env_overrides = std::collections::HashMap::new();
    for (key, var) in [
        ("CU_PROVIDER", "CU_PROVIDER"),
        ("CU_MODEL", "CU_MODEL"),
        ("PRESENCE_PROVIDER", "PRESENCE_PROVIDER"),
        ("PRESENCE_MODEL", "PRESENCE_MODEL"),
        ("PROVIDER", "PROVIDER"),
        ("MODEL_NAME", "MODEL_NAME"),
    ] {
        if let Ok(val) = std::env::var(var) {
            env_overrides.insert(key.to_string(), val);
        }
    }
    SettingsPayload {
        cu_provider: config.computer_use.provider.clone(),
        cu_model: config.computer_use.model.clone(),
        cu_backend: config.computer_use.backend.clone(),
        cu_first_routing: config.experimental.cu_first_routing,
        presence_enabled: config.presence.enabled,
        presence_provider: config.presence.provider.clone(),
        presence_model: config.presence.model.clone(),
        presence_live_provider: config.presence.live_provider.clone(),
        presence_live_model: config.presence.live_model.clone(),
        transcription_enabled: config.transcription.enabled,
        transcription_provider: config.transcription.provider.clone(),
        transcription_model: config.transcription.model.clone(),
        transcription_endpoint: config.transcription.endpoint.clone(),
        transcription_language: config.transcription.language.clone(),
        recording_enabled: config.recording.enabled,
        recording_framerate: config.recording.framerate,
        recording_quality: config.recording.quality.clone(),
        live_audio_enabled: config.live_audio.enabled,
        live_audio_timeout_secs: config.live_audio.default_timeout_secs,
        external_agent: config.agent.default_backend.clone(),
        codex_command: Some(config.agent.codex.command.clone()),
        codex_managed_command: config.agent.codex.managed_command.clone(),
        codex_sandbox: Some(crate::project::normalize_sandbox_mode(
            &config.agent.codex.sandbox,
        )),
        codex_approval_policy: Some(crate::project::normalize_approval_policy(
            &config.agent.codex.approval_policy,
        )),
        codex_model: config.agent.codex.model.clone(),
        codex_reasoning_effort: crate::project::normalize_reasoning_effort(
            config.agent.codex.reasoning_effort.as_deref(),
        ),
        codex_service_tier: crate::project::normalize_codex_service_tier(
            config.agent.codex.service_tier.as_deref(),
        ),
        codex_web_search: config.agent.codex.web_search,
        codex_network_access: config.agent.codex.network_access,
        codex_writable_roots: config.agent.codex.writable_roots.clone(),
        codex_managed_context: Some(crate::project::normalize_codex_managed_context(
            &config.agent.codex.managed_context,
        )),
        codex_context_archive: Some(crate::project::normalize_codex_context_archive(
            &config.agent.codex.context_archive,
        )),
        claude_command: Some(config.agent.claude_code.command.clone()),
        claude_model: config.agent.claude_code.model.clone(),
        claude_permission_mode: Some(crate::project::normalize_claude_permission_mode(
            &config.agent.claude_code.permission_mode,
        )),
        claude_allowed_tools: Some(config.agent.claude_code.allowed_tools.clone()),
        approval_file_read: config.approval.file_read.as_str().to_string(),
        approval_file_write: config.approval.file_write.as_str().to_string(),
        approval_file_delete: config.approval.file_delete.as_str().to_string(),
        approval_command_exec: config.approval.command_exec.as_str().to_string(),
        approval_network: config.approval.network.as_str().to_string(),
        approval_destructive: config.approval.destructive.as_str().to_string(),
        approval_display_control: config.approval.display_control.as_str().to_string(),
        approval_tool_call: config.approval.tool_call.as_str().to_string(),
        env_overrides,
    }
}

async fn settings_payload_with_runtime_overrides(
    config: &crate::project::ProjectConfig,
    runtime: &RuntimeSettingsState,
) -> SettingsPayload {
    let mut payload = settings_payload_from_config(config);
    if let Some(presence_enabled) = runtime.presence_enabled {
        payload.presence_enabled = presence_enabled;
    }
    if let Some(shared_external_agent) = &runtime.external_agent {
        payload.external_agent = shared_external_agent
            .read()
            .await
            .as_ref()
            .map(|backend| backend.as_short_str().to_string());
    }
    payload
}

pub(crate) async fn settings_get_response_body(
    project_root: Option<&Path>,
    runtime_settings: &RuntimeSettingsState,
) -> String {
    match project_root {
        Some(root) => match crate::project::Project::from_root(root.to_path_buf()) {
            Ok(proj) => {
                let payload =
                    settings_payload_with_runtime_overrides(&proj.config, runtime_settings).await;
                serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        },
        None => serde_json::json!({"error": "No project root"}).to_string(),
    }
}

fn apply_settings_payload(config: &mut crate::project::ProjectConfig, payload: &SettingsPayload) {
    config.computer_use.provider = payload.cu_provider.clone();
    config.computer_use.model = payload.cu_model.clone();
    config.computer_use.backend = payload.cu_backend.clone();
    config.presence.enabled = payload.presence_enabled;
    config.presence.provider = payload.presence_provider.clone();
    config.presence.model = payload.presence_model.clone();
    config.presence.live_provider = payload.presence_live_provider.clone();
    config.presence.live_model = payload.presence_live_model.clone();
    config.transcription.enabled = payload.transcription_enabled;
    config.transcription.provider = payload.transcription_provider.clone();
    config.transcription.model = payload.transcription_model.clone();
    config.transcription.endpoint = payload.transcription_endpoint.clone();
    config.transcription.language = payload.transcription_language.clone();
    config.recording.enabled = payload.recording_enabled;
    config.recording.framerate = payload.recording_framerate;
    config.recording.quality = payload.recording_quality.clone();
    config.live_audio.enabled = payload.live_audio_enabled;
    config.live_audio.default_timeout_secs = payload.live_audio_timeout_secs;
    // Normalize empty strings to None so the TOML doesn't end up with
    // `default_backend = ""` — the loader treats "" as a valid override
    // and would try to resolve it to a backend.
    config.agent.default_backend =
        payload
            .external_agent
            .as_ref()
            .and_then(|s| if s.is_empty() { None } else { Some(s.clone()) });
    if payload.codex_command.is_some() {
        config.agent.codex.command =
            normalize_settings_codex_command(payload.codex_command.as_deref());
    }
    if payload.codex_managed_command.is_some() {
        // Empty clears the override (managed sessions fall back to
        // `command`); anything else is the fork binary path.
        config.agent.codex.managed_command = payload
            .codex_managed_command
            .as_deref()
            .map(str::trim)
            .filter(|cmd| !cmd.is_empty())
            .map(str::to_string);
    }
    if let Some(mode) = payload.codex_sandbox.as_deref() {
        config.agent.codex.sandbox = crate::project::normalize_sandbox_mode(mode);
    }
    if let Some(policy) = payload.codex_approval_policy.as_deref() {
        config.agent.codex.approval_policy = crate::project::normalize_approval_policy(policy);
    }
    if payload.codex_service_tier.is_some() {
        config.agent.codex.service_tier =
            crate::project::normalize_codex_service_tier(payload.codex_service_tier.as_deref());
    }
    if let Some(mode) = payload.codex_managed_context.as_deref() {
        config.agent.codex.managed_context = crate::project::normalize_codex_managed_context(mode);
    }
    if let Some(mode) = payload.codex_context_archive.as_deref() {
        config.agent.codex.context_archive = crate::project::normalize_codex_context_archive(mode);
    }
    if payload.claude_command.is_some() {
        config.agent.claude_code.command =
            normalize_settings_agent_command(payload.claude_command.as_deref(), "claude");
    }
    if payload.claude_model.is_some() {
        // Empty clears the override (claude picks its configured default).
        config.agent.claude_code.model = payload
            .claude_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
    }
    if let Some(mode) = payload.claude_permission_mode.as_deref() {
        config.agent.claude_code.permission_mode =
            crate::project::normalize_claude_permission_mode(mode);
    }
    if let Some(tools) = payload.claude_allowed_tools.as_ref() {
        config.agent.claude_code.allowed_tools = tools
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
    }
}

pub(crate) fn settings_post_result(
    body_text: &str,
    project_root: Option<&Path>,
    bus: &EventBus,
) -> (&'static str, String) {
    let Some(root) = project_root else {
        return (
            "400 Bad Request",
            serde_json::json!({"error": "No project root"}).to_string(),
        );
    };
    let payload = match serde_json::from_str::<SettingsPayload>(body_text) {
        Ok(payload) => payload,
        Err(e) => {
            return (
                "400 Bad Request",
                serde_json::json!({"error": format!("Invalid settings: {}", e)}).to_string(),
            );
        }
    };
    let mut proj = match crate::project::Project::from_root(root.to_path_buf()) {
        Ok(proj) => proj,
        Err(e) => {
            return (
                "500 Internal Server Error",
                serde_json::json!({"error": e.to_string()}).to_string(),
            );
        }
    };
    apply_settings_payload(&mut proj.config, &payload);
    match proj.save_config() {
        Ok(()) => {
            dispatch_codex_settings_control_msgs(bus, &payload);
            ("200 OK", serde_json::json!({"ok": true}).to_string())
        }
        Err(e) => (
            "500 Internal Server Error",
            serde_json::json!({"error": e.to_string()}).to_string(),
        ),
    }
}

/// Mirror just-persisted `[agent.codex]` settings into the live control plane.
///
/// `apply_settings_payload` + `save_config` update the TOML, but session
/// launches read the live `CodexRuntimeConfig`, which OVERLAYS the TOML
/// (`project_with_runtime_config`). Without this, an API client that POSTs
/// `codex_managed_context: "managed"` sees /api/settings echo the new value
/// while sessions keep launching with the stale live value until a daemon
/// restart. Frontends stay display-only, so we don't write shared state here:
/// we emit the same `ControlMsg`s a dashboard would, and the control plane
/// (the single writer) updates shared state, broadcasts `CodexConfigChanged`,
/// and re-persists the normalized value. That second persist is intentional
/// and idempotent — both paths run the same normalizers, and the gateway's
/// own synchronous TOML write (kept above) is what makes an immediate
/// GET /api/settings read back the saved values.
///
/// Only fields actually present in the payload are dispatched, mirroring
/// `apply_settings_payload`'s conditional writes; only codex fields with a
/// live control-plane setter are covered.
fn dispatch_codex_settings_control_msgs(bus: &EventBus, payload: &SettingsPayload) {
    if payload.codex_command.is_some() {
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexCommand {
            command: payload.codex_command.clone(),
        }));
    }
    if payload.codex_managed_command.is_some() {
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexManagedCommand {
                command: payload.codex_managed_command.clone(),
            },
        ));
    }
    if let Some(mode) = payload.codex_sandbox.clone() {
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexSandbox {
            mode,
        }));
    }
    if let Some(policy) = payload.codex_approval_policy.clone() {
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexApprovalPolicy { policy },
        ));
    }
    if payload.codex_service_tier.is_some() {
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexServiceTier {
            service_tier: payload.codex_service_tier.clone(),
        }));
    }
    if let Some(mode) = payload.codex_managed_context.clone() {
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexManagedContext { mode },
        ));
    }
    if let Some(mode) = payload.codex_context_archive.clone() {
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexContextArchive { mode },
        ));
    }
}

/// Return JSON with boolean flags indicating which API keys are usable —
/// an active credential lease counts the same as a configured .env key.
fn get_api_key_status_json() -> String {
    let openai = crate::credential_leases::provider_api_key("OPENAI_API_KEY").is_some();
    let anthropic = crate::credential_leases::provider_api_key("ANTHROPIC_API_KEY").is_some();
    let gemini = crate::credential_leases::provider_api_key("GEMINI_API_KEY").is_some();
    serde_json::json!({
        "openai": openai,
        "anthropic": anthropic,
        "gemini": gemini,
    })
    .to_string()
}

pub(crate) fn api_key_status_response_body() -> String {
    get_api_key_status_json()
}

/// Whether any provider credential is usable at all — the aggregate of
/// [`get_api_key_status_json`], safe to expose at presence level.
pub(crate) fn any_provider_credential_usable() -> bool {
    ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"]
        .iter()
        .any(|name| crate::credential_leases::provider_api_key(name).is_some())
}

pub(crate) fn project_root_response_body(project_root: Option<&Path>) -> String {
    serde_json::json!({
        "project_root": project_root.map(|root| root.to_string_lossy().to_string())
    })
    .to_string()
}

/// Availability of the external-agent backends (Codex, Claude Code):
/// the configured command, whether it resolves to an executable, and
/// when this daemon last ran a session with it. Deliberately independent
/// of provider fueling — external agents bring their own credentials, so
/// the dashboard pairs this with the `fueled` flag instead of letting the
/// first-run nudge claim an unfueled daemon can't do anything.
pub(crate) fn external_agents_response_body(project_root: Option<&Path>) -> String {
    let agent_config = project_root
        .and_then(|root| crate::project::Project::from_root(root.to_path_buf()).ok())
        .map(|project| project.config.agent)
        .unwrap_or_default();
    let home = crate::platform::home_dir();
    serde_json::json!({
        "external_agents":
            crate::external_agent::backend_availability_json(&agent_config, &home),
    })
    .to_string()
}

pub(crate) async fn displays_response_body(
    session_registry: &Option<crate::display::SharedSessionRegistry>,
) -> String {
    let displays = crate::display::enumerate_displays_with_sessions(session_registry).await;
    serde_json::to_string(&displays).unwrap_or_else(|_| "[]".to_string())
}

/// Payload for POST /api/api-keys.
#[derive(serde::Deserialize)]
struct SetApiKeysPayload {
    keys: std::collections::HashMap<String, String>,
}

/// Handle POST /api/api-keys: persist keys to ~/.config/intendant/.env and
/// set them in the current process.
pub(crate) fn handle_set_api_keys(body: &str) -> String {
    let payload: SetApiKeysPayload = match serde_json::from_str(body) {
        Ok(p) => p,
        Err(e) => {
            return serde_json::json!({"error": format!("Invalid payload: {}", e)}).to_string();
        }
    };

    // Only allow known key names.
    const ALLOWED: &[&str] = &["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"];
    for key in payload.keys.keys() {
        if !ALLOWED.contains(&key.as_str()) {
            return serde_json::json!({"error": format!("Unknown key: {}", key)}).to_string();
        }
    }

    // Resolve config dir.
    let config_dir = match dirs::config_dir() {
        Some(d) => d.join("intendant"),
        None => {
            return serde_json::json!({"error": "Cannot determine config directory"}).to_string();
        }
    };

    // Ensure the directory exists.
    if let Err(e) = std::fs::create_dir_all(&config_dir) {
        return serde_json::json!({"error": format!("Cannot create config dir: {}", e)})
            .to_string();
    }

    let env_path = config_dir.join(".env");

    // Read existing content (may not exist yet).
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();

    // Build updated content: replace existing lines, append new ones.
    let mut lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();
    let mut written_keys = std::collections::HashSet::new();

    for line in &mut lines {
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq_pos) = trimmed.find('=') {
            let var_name = trimmed[..eq_pos].trim().to_string();
            if let Some(new_val) = payload.keys.get(&var_name) {
                *line = format!("{}={}", var_name, new_val);
                written_keys.insert(var_name);
            }
        }
    }

    // Append keys that weren't already in the file.
    for (key, val) in &payload.keys {
        if !written_keys.contains(key.as_str()) {
            lines.push(format!("{}={}", key, val));
        }
    }

    let new_content = lines.join("\n") + "\n";

    if let Err(e) = crate::file_watcher::atomic_write(&env_path, new_content.as_bytes()) {
        return serde_json::json!({"error": format!("Write failed: {}", e)}).to_string();
    }

    // Set env vars in the current process so future provider instantiations
    // pick them up without requiring a restart.
    for (key, val) in &payload.keys {
        std::env::set_var(key, val);
    }

    serde_json::json!({"ok": true}).to_string()
}

// ---------------------------------------------------------------------------
// MCP-over-HTTP (Streamable HTTP) types
// ---------------------------------------------------------------------------
//
// rmcp's Streamable HTTP transport expects:
//   - Requests (with `id`):   200 OK + application/json body
//   - Notifications (no `id`): 202 Accepted + empty body
//
// Returning 200+JSON for notifications causes rmcp to try deserializing the
// body as ServerJsonRpcMessage, which fails because there's no valid `id`.








/// Consecutive "fatal-class" accept failures tolerated on the same socket
/// before it is dropped and rebound. EINVAL has been observed twice on
/// macOS (2026-07-04, both times within ~1s of an external-agent spawn)
/// on a listener that remained LISTEN at the kernel afterwards — treating
/// the first one as fatal is what actually broke the dashboard. A short
/// streak (~2s) absorbs the spurious case; a genuinely dead socket fails
/// every retry and reaches the rebind path.
pub(crate) const FATAL_ACCEPT_REBIND_THRESHOLD: u32 = 8;

pub(crate) fn should_continue_after_accept_error(error: &std::io::Error) -> bool {
    match error.kind() {
        std::io::ErrorKind::Interrupted
        | std::io::ErrorKind::WouldBlock
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::TimedOut => return true,
        std::io::ErrorKind::InvalidInput
        | std::io::ErrorKind::InvalidData
        | std::io::ErrorKind::NotFound
        | std::io::ErrorKind::PermissionDenied => return false,
        _ => {}
    }

    match error.raw_os_error() {
        // The listener file descriptor/socket is invalid or no longer a
        // listening socket (EBADF/EINVAL/ENOTSOCK). Retrying accept() on it
        // would spin forever — the caller rebinds a fresh listener instead.
        Some(9 | 22 | 38) => false,
        // Process/system descriptor pressure and socket buffer pressure are
        // recoverable after current connections close. Keep the gateway alive
        // so the dashboard recovers instead of becoming half-alive.
        Some(23 | 24 | 55) => true,
        // Unknown accept errors are safer to treat as per-connection failures:
        // losing one inbound connection is better than dropping the dashboard
        // listener while existing WebSocket tasks make the UI look alive.
        _ => true,
    }
}

/// Rebind a TCP listener on its original address after the previous
/// socket became unusable — seen in the wild on macOS as `accept()`
/// returning EINVAL a minute into an app-spawned daemon's life, which
/// used to kill the listener task and leave the dashboard half-alive
/// (established WebSockets kept flowing while every new connection —
/// session details, files, uploads, Station assets — failed). Mirrors
/// `bind_dual_stack_or_v4`: dual-stack for the IPv6 wildcard,
/// `SO_REUSEADDR` so lingering TIME_WAIT sockets don't block the port.
/// Shared by the dashboard gateway and the enrollment cert server.
pub(crate) fn rebind_dead_tcp_listener(
    addr: std::net::SocketAddr,
) -> std::io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() && addr.ip().is_unspecified() {
        let _ = socket.set_only_v6(false);
    }
    let _ = socket.set_reuse_address(true);
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    TcpListener::from_std(std_listener)
}





pub fn spawn_web_gateway(
    listener: TcpListener,
    bus: EventBus,
    broadcast_tx: broadcast::Sender<String>,
    config: WebGatewayConfig,
    shared_session: SharedActiveSession,
    transcriber: Option<Arc<dyn crate::transcription::Transcriber>>,
    web_tui_tx: Option<mpsc::UnboundedSender<crate::tui::web::WebTuiCommand>>,
    task_tx: Option<tokio::sync::mpsc::Sender<presence_core::TaskEnvelope>>,
    project_root: Option<std::path::PathBuf>,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    peer_registry: Option<crate::peer::PeerRegistry>,
    advertise_urls: Vec<String>,
    // Inbound bearer token enforcement. When `Some`, federation REST
    // endpoints (/api/peers*, /api/coordinator/*, /api/sessions)
    // require `Authorization: Bearer <token>` matching the configured
    // value; missing or wrong token returns 401. When `None`, no
    // application-layer auth is enforced — the operator's expected to
    // rely on transport security (mTLS proxy, tailnet, loopback).
    // Sourced from `[server.auth] bearer_token` in intendant.toml.
    //
    // /ws, /.well-known/agent-card.json, /config, the dashboard HTML,
    // and static assets are intentionally exempt in this slice — /ws
    // enforcement requires a parallel dashboard auth flow (browser
    // can't easily set headers on `WebSocket` opens) which lands in
    // slice 2d.
    inbound_bearer_token: Option<String>,
    // What to advertise in the local Agent Card's `auth` field —
    // tells connecting peers what wire-layer (transport) and
    // application-layer (bearer) auth they need to satisfy.
    // Built by `crate::main::build_local_advertised_auth` from
    // `[server.auth] advertised_transport` (`"none"` /
    // `"mutual-tls"` / `"pin-self-cert"`) and
    // `[server.auth] bearer_token`. The `pin-self-cert` path reads
    // the daemon's own `server.crt` from the access cert dir and
    // pre-fills the fingerprint so operators don't have to compute
    // it manually.
    //
    // Test call sites pass `AuthRequirements::none()` since they
    // don't exercise the advertise path; production call sites in
    // main.rs build the requirements from the project config.
    local_card_auth: crate::peer::AuthRequirements,
    // When true, the TLS layer may complete without a client certificate so
    // public peer-access requests can reach the doorbell endpoint, but every
    // other HTTP/WS path is rejected unless rustls verified a client cert.
    tls_client_cert_required: bool,
    // Native TLS for the dashboard. `Some(acceptor)` (built in main.rs
    // from `[server.tls]` / `--tls`) makes the per-connection demux wrap
    // any connection whose first bytes are a TLS ClientHello, serving the
    // dashboard over HTTPS/WSS. `None` (the default) preserves the
    // current plain-HTTP behavior. Either way the raw ICE-TCP (STUN-
    // framed) demux branch is unaffected — TLS is distinguished by its
    // `0x16` handshake-record first byte, which is disjoint from both the
    // STUN length-prefix and HTTP method bytes.
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
) -> tokio::task::JoinHandle<()> {
    let config_json = serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string());
    let peer_access_request_config = config.peer_access_requests.clone();
    // Cache the most recent worktree inventory scan. Scanning can walk
    // large worktree directories for disk-size accounting, so the
    // dashboard explicitly triggers refreshes instead of doing it on
    // every GET. Shared by HTTP and the dashboard WebRTC control tunnel.
    let worktree_inventory_cache: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Build the local Agent Card from live runtime state so
    // `/.well-known/agent-card.json` can serve it. The transport URLs
    // come from [`resolve_advertise_urls`], which uses operator
    // overrides verbatim when provided and otherwise falls back to a
    // single auto-detected URL derived from the listener's bind
    // address. Multiple URLs let one daemon advertise itself reachable
    // via several paths (LAN IP, Tailscale, host port-forward, etc.)
    // — the connecting peer probes them in order.
    //
    // TLS state drives the advertised scheme: when a TLS acceptor is
    // present the dashboard is HTTPS/WSS-only (strict demux below), so
    // auto-detected URLs must be `wss://`, not `ws://`, or peers handed a
    // `ws://` URL would be refused.
    let advertise_urls = resolve_advertise_urls(
        listener.local_addr().ok(),
        &advertise_urls,
        tls_acceptor.is_some(),
    );
    let agent_card = build_local_agent_card(advertise_urls, local_card_auth);
    let mut agent_card_value =
        serde_json::to_value(&agent_card).unwrap_or_else(|_| serde_json::json!({}));
    // Phase 7: the signed card names the rendezvous this daemon actually
    // polls, so browsers learn the signaling base from the daemon record
    // instead of assuming the default hosted instance.
    if config.connect.enabled {
        if let Some(base) = config
            .connect
            .rendezvous_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            agent_card_value["rendezvous_base"] =
                serde_json::Value::String(base.trim_end_matches('/').to_string());
            if let Some(id) = config
                .connect
                .daemon_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                agent_card_value["connect_daemon_id"] = serde_json::Value::String(id.to_string());
            }
        }
    }
    let agent_card_json =
        serde_json::to_string(&agent_card_value).unwrap_or_else(|_| "{}".to_string());
    let agent_card_value_for_targets = agent_card_value.clone();
    let bootstrap_caches = crate::dashboard_control::DashboardBootstrapCaches::default();

    // Warm the session list in the background so the first dashboard
    // request doesn't pay the initial scan. The persistent per-session
    // index makes this mostly stat calls + one parallel index sweep; the
    // results land in the ordinary response caches.
    tokio::task::spawn_blocking(|| {
        preload_session_index();
        // 600 matches the dashboard's default recent-list request size;
        // the unlimited list feeds the Stats tab's usage view. Sequential
        // so the second scan reuses everything the first one warmed.
        let _ = cached_list_sessions_with_limit(SESSION_LIST_STREAM_QUICK_LIMIT);
        let _ = cached_list_sessions();
    });

    // Pre-build ICE config for WebRTC display sessions from the gateway config.
    let ice_config = crate::display::IceConfig {
        ice_servers: config.ice_servers.clone(),
    };

    // Shared ICE-TCP peer registry + advertised TCP port.
    //
    // We multiplex ICE-TCP onto the HTTP listener port: the per-connection
    // accept handler (later in this function) peeks every accepted TCP
    // connection's first bytes to tell HTTP vs. WebSocket vs. STUN-framed
    // traffic apart. STUN traffic is read through one RFC 4571 frame and
    // handed to this registry, which demuxes to the matching peer by the
    // STUN USERNAME's local-ufrag half. The advertised TCP candidate port
    // is the HTTP port itself, so ICE-TCP flows through the exact same
    // tunnel/port-forward that already carries the dashboard — users
    // don't configure anything extra beyond what the dashboard already
    // requires.
    let http_port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let tcp_peer_registry = crate::display::webrtc::TcpPeerRegistry::new();
    let tcp_advertised_port: Option<u16> = if http_port != 0 {
        Some(http_port)
    } else {
        None
    };
    let peer_file_transfer_registry =
        Arc::new(crate::peer_file_transfer::PeerFileTransferRegistry::new(
            ice_config.clone(),
            bus.clone(),
            Arc::clone(&tcp_peer_registry),
        ));

    // Slice 3b: TCP relay registry for primary-as-media-relay. When
    // a federated WebRTC `Answer` flows from a peer back to the
    // browser, the translator (below) extracts the peer's ICE ufrag
    // from the SDP, resolves the peer's outbound TCP address, and
    // registers the mapping here. The accept loop (below) then
    // dispatches incoming STUN-framed TCP connections whose ufrag
    // matches an entry to the relay byte-forwarding path instead of
    // the local WebRtcPeer path — the primary opens a fresh TCP
    // connection to the peer and shuttles bytes between browser and
    // peer until either side closes. Browser ICE treats this as a
    // TCP candidate alongside the peer's direct candidate; direct
    // wins on reachable topologies, relay covers the browser-can-
    // only-reach-primary case (e.g. hypervisor-isolated VMs).
    let tcp_relay_registry = crate::display::webrtc::TcpRelayRegistry::new();

    // Primary's relay TCP URL, used to inject a relay candidate into
    // forwarded `Answer` SDPs. Derived from the agent card's first
    // IntendantWs transport — that's the URL the primary advertises
    // to peers, which on most deployments is also what browsers use
    // to reach the primary. Stored as a string so DNS resolution
    // happens lazily at per-Answer rewrite time rather than once at
    // startup (hostnames may not resolve at boot for Tailscale /
    // mDNS / etc).
    let relay_advertise_url: Option<String> = agent_card.transports.iter().find_map(|t| match t {
        crate::peer::TransportSpec::IntendantWs { url } => Some(url.clone()),
        _ => None,
    });

    // Inject content-hash version into WASM/JS URLs for cache-busting.
    let v = asset_version().to_string();
    let session_provider = config.provider.clone();
    let session_model = config.model.clone();
    let voice_debug = Arc::new(Mutex::new(VoiceDebugState::default()));
    let active_presence: Arc<Mutex<Option<ActivePresence>>> = Arc::new(Mutex::new(None));
    // Per-display input authority (phase 5).  Entry absence = unclaimed
    // (any connection can input — pre-phase-5 default); entry presence =
    // exclusive ownership by that one `connection_id`.
    //
    // Synchronous `StdRwLock` (5a.1): the WebRTC data-channel input
    // handler in `display/mod.rs::handle_offer_pool_mode` is an
    // `Arc<dyn Fn(InputEvent) + Send + Sync>` invoked from rtc's sync
    // receive context, and reads this map through the per-peer
    // `input_authorized` closure each time an event arrives.  Tokio's
    // RwLock can't be read from sync code without `block_on`; std's
    // can.  The map is small, write-rare (grant/release/WS-close only),
    // read-frequent on the hot input path; std::sync::RwLock is the
    // correct lock here.
    let display_input_authority: Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>> =
        Arc::new(StdRwLock::new(HashMap::new()));

    // Phase 5a.1 authority transition channel.  Each per-connection
    // outbound task subscribes; emit sites are the Request/Release
    // ControlMsg handlers, the WS-close cleanup, and the DisplayReady
    // listener that fires `holder: None` for freshly
    // created display sessions so already-connected browsers move
    // from `unknown` to `unclaimed`.
    let (authority_change_tx, _authority_change_rx0) =
        broadcast::channel::<DisplayInputAuthorityChange>(AUTHORITY_CHANGE_CAPACITY);

    let (dashboard_authority_change_tx, _dashboard_authority_change_rx0) =
        broadcast::channel::<u32>(AUTHORITY_CHANGE_CAPACITY);
    {
        let mut authority_change_rx = authority_change_tx.subscribe();
        let dashboard_authority_change_tx = dashboard_authority_change_tx.clone();
        tokio::spawn(async move {
            loop {
                match authority_change_rx.recv().await {
                    Ok(change) => {
                        let _ = dashboard_authority_change_tx.send(change.display_id);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
    }

    let dashboard_display_authority = {
        let snapshot_authority = Arc::clone(&display_input_authority);
        let state_authority = Arc::clone(&display_input_authority);
        let request_authority = Arc::clone(&display_input_authority);
        let request_change_tx = authority_change_tx.clone();
        let release_authority = Arc::clone(&display_input_authority);
        let release_change_tx = authority_change_tx.clone();
        let input_authority = Arc::clone(&display_input_authority);
        let cleanup_authority = Arc::clone(&display_input_authority);
        let cleanup_change_tx = authority_change_tx.clone();
        let subscribe_tx = dashboard_authority_change_tx.clone();
        crate::dashboard_control::DashboardDisplayAuthorityBridge::new(
            move |session_id, display_ids| {
                dashboard_control_authority_snapshot_frames(
                    session_id,
                    display_ids,
                    &snapshot_authority,
                )
            },
            move |session_id, display_id| {
                Some(dashboard_control_authority_state_frame(
                    session_id,
                    display_id,
                    &state_authority,
                ))
            },
            move |session_id, display_id| {
                apply_grant_input_authority_dashboard_control(
                    display_id,
                    session_id.to_string(),
                    &request_authority,
                    &request_change_tx,
                );
                vec![dashboard_control_authority_state_frame(
                    session_id,
                    display_id,
                    &request_authority,
                )]
            },
            move |session_id, display_id| {
                apply_release_input_authority_dashboard_control(
                    display_id,
                    session_id,
                    &release_authority,
                    &release_change_tx,
                );
                vec![dashboard_control_authority_state_frame(
                    session_id,
                    display_id,
                    &release_authority,
                )]
            },
            move |session_id, display_id| {
                dashboard_control_input_authorized(session_id, display_id, &input_authority)
            },
            move |session_id| {
                apply_dashboard_control_close_input_authority(
                    session_id,
                    &cleanup_authority,
                    &cleanup_change_tx,
                );
            },
            move || subscribe_tx.subscribe(),
        )
    };

    // Process-wide registry of standalone shell PTY sessions, keyed by
    // (host_id, terminal_id). Lives as long as the web gateway task and
    // is cloned into each per-connection handler so reconnects reattach
    // to existing shells. Keyed on host_id even though there's only one
    // host today so multi-host phase 1 can add siblings without refactor.
    let terminal_registry: Arc<crate::terminal::TerminalRegistry> = Arc::new(
        crate::terminal::TerminalRegistry::new(project_root.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })),
    );

    let dashboard_presence = {
        let connect_active_presence = active_presence.clone();
        let connect_voice_debug = voice_debug.clone();
        let connect_shared_session = shared_session.clone();
        let connect_bus = bus.clone();
        let connect_provider = config.provider.clone();
        let connect_model = config.model.clone();

        let disconnect_active_presence = active_presence.clone();
        let disconnect_voice_debug = voice_debug.clone();
        let disconnect_shared_session = shared_session.clone();
        let disconnect_bus = bus.clone();

        let make_active_presence = active_presence.clone();
        let make_voice_debug = voice_debug.clone();
        let make_shared_session = shared_session.clone();
        let make_bus = bus.clone();
        let make_provider = config.provider.clone();
        let make_model = config.model.clone();

        let cleanup_active_presence = active_presence.clone();
        let cleanup_voice_debug = voice_debug.clone();
        let cleanup_shared_session = shared_session.clone();
        let cleanup_bus = bus.clone();

        let log_voice_debug = voice_debug.clone();

        crate::dashboard_control::DashboardPresenceBridge::new(
            move |request| {
                Box::pin(dashboard_control_presence_connect(
                    request,
                    connect_active_presence.clone(),
                    connect_voice_debug.clone(),
                    connect_shared_session.clone(),
                    connect_bus.clone(),
                    connect_provider.clone(),
                    connect_model.clone(),
                ))
            },
            move |request| {
                Box::pin(dashboard_control_presence_disconnect(
                    request,
                    disconnect_active_presence.clone(),
                    disconnect_voice_debug.clone(),
                    disconnect_shared_session.clone(),
                    disconnect_bus.clone(),
                ))
            },
            move |request| {
                Box::pin(dashboard_control_presence_make_active(
                    request,
                    make_active_presence.clone(),
                    make_voice_debug.clone(),
                    make_shared_session.clone(),
                    make_bus.clone(),
                    make_provider.clone(),
                    make_model.clone(),
                ))
            },
            move |session_id| {
                Box::pin(dashboard_control_presence_cleanup(
                    session_id,
                    cleanup_active_presence.clone(),
                    cleanup_voice_debug.clone(),
                    cleanup_shared_session.clone(),
                    cleanup_bus.clone(),
                ))
            },
            move |text| {
                let mut vd = log_voice_debug.lock().unwrap_or_else(|e| e.into_inner());
                vd.voice_log_count += 1;
                vd.last_voice_log = text;
            },
        )
    };

    let dashboard_control = Arc::new(crate::dashboard_control::DashboardControlRegistry::new(
        config.clone(),
        broadcast_tx.clone(),
        bus.clone(),
        peer_registry.clone(),
        mcp_server.clone(),
        shared_session.clone(),
        project_root.clone(),
        worktree_inventory_cache.clone(),
        terminal_registry.clone(),
        web_tui_tx.clone(),
        task_tx.clone(),
        agent_card_value,
        bootstrap_caches.clone(),
        Some(dashboard_display_authority),
        Some(dashboard_presence),
        ice_config.clone(),
        Arc::clone(&tcp_peer_registry),
    ));
    let _connect_rendezvous_handle = crate::connect_rendezvous::spawn_connect_rendezvous_client(
        config.connect.clone(),
        dashboard_control.clone(),
    );

    // F-1.3b3 federated authority subscribers. Federated counterpart
    // to local 5c's per-WS subscriber loop: federated browsers don't
    // share the local 5c WS path, so the gateway needs an explicit
    // registry of `(federation_connection_id, session_id, display_id)`
    // → `WebRtcPeer` to fan personalized state out to. Owned here at
    // gateway scope so cleanup edges (federated `Close`, federation
    // WS close) can locate entries by either single-identity or
    // bulk-by-connection key. See the F-1.3b3 helpers above.
    let federated_authority_subscribers: FederatedAuthoritySubscribers =
        Arc::new(StdRwLock::new(HashMap::new()));

    // Spawn a listener that fires an "unclaimed" authority change for
    // every newly-created display session so already-connected browsers'
    // chips flip from `unknown` to `unclaimed` without waiting for the
    // first Request/Release.  Subscribes to the broadcast_tx event
    // stream (already serialized JSON) and pattern-matches on
    // `display_ready` rather than the typed AppEvent — same source the
    // existing `display_ready_cache` task uses, keeps the dependency
    // surface small.
    {
        let authority_change_tx = authority_change_tx.clone();
        let mut display_events_rx = broadcast_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match display_events_rx.recv().await {
                    Ok(line) => {
                        if line.contains("\"event\":\"display_ready\"") {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                let did = parsed["display_id"].as_u64().unwrap_or(0) as u32;
                                let _ = authority_change_tx.send(DisplayInputAuthorityChange {
                                    display_id: did,
                                    holder: None,
                                });
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
    }

    // Cache the latest usage_update JSON so late-connecting browsers get it
    // without sending ControlMsg (which would pollute the event log).
    let last_usage_json = bootstrap_caches.last_usage_json.clone();
    // Cache the latest live_usage_update JSON for late-connecting browsers.
    let last_live_usage_json = bootstrap_caches.last_live_usage_json.clone();
    // Cache the latest status event (has autonomy, session_id, task).
    let last_status_json = bootstrap_caches.last_status_json.clone();
    // Cache standalone autonomy changes so reconnecting dashboards do not
    // fall back to the stale autonomy value in the latest status event.
    let last_autonomy_json = bootstrap_caches.last_autonomy_json.clone();
    // Cache the latest external_agent_changed event so a refreshed
    // browser learns the current value without having to re-fetch
    // settings. Without this the dashboard dropdown snaps back to
    // "None (internal agent)" on every page refresh even though the
    // daemon still has the value in memory.
    let last_external_agent_json = bootstrap_caches.last_external_agent_json.clone();
    // Cache all currently externally-attached sessions so refreshed browsers
    // can rehydrate every open Activity window with the same compact
    // transcript shown in the Sessions tab. This must be a set, not "last
    // attached", because multiple Codex/Claude/Gemini session windows may be
    // open at once.
    let attached_external_sessions = bootstrap_caches.attached_external_sessions.clone();
    // Cache the latest user_display_granted event. The authoritative
    // state lives in AutonomyState.user_display_granted on the server,
    // but the dashboard only learns about it via the broadcast; without
    // this cache a refreshed browser shows "off" regardless of whether
    // the user has actually granted access. Cleared on user_display_revoked
    // so a stale grant doesn't get replayed after the user revokes.
    let last_user_display_json = bootstrap_caches.last_user_display_json.clone();
    // Cache display_ready JSON per display_id for late-connecting browsers.
    // Using a HashMap so multiple concurrent display sessions are all replayed.
    let display_ready_cache: Arc<Mutex<HashMap<u32, String>>> =
        Arc::new(Mutex::new(HashMap::new()));
    {
        let usage_cache = last_usage_json.clone();
        let live_usage_cache = last_live_usage_json.clone();
        let status_cache = last_status_json.clone();
        let autonomy_cache = last_autonomy_json.clone();
        let external_agent_cache = last_external_agent_json.clone();
        let session_attached_cache = attached_external_sessions.clone();
        let user_display_cache = last_user_display_json.clone();
        let display_cache = display_ready_cache.clone();
        let mut usage_rx = broadcast_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match usage_rx.recv().await {
                    Ok(line) => {
                        // Cache display_ready events per display_id for
                        // late-connecting browsers.
                        if line.contains("\"event\":\"display_ready\"") {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                let did = parsed["display_id"].as_u64().unwrap_or(0) as u32;
                                if let Ok(mut guard) = display_cache.lock() {
                                    guard.insert(did, line.clone());
                                }
                            }
                        }
                        // Evict display_ready cache when display is revoked.
                        if line.contains("\"event\":\"user_display_revoked\"")
                            || line.contains("\"event\":\"display_capture_lost\"")
                        {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                                let did = parsed["display_id"].as_u64().unwrap_or(0) as u32;
                                if let Ok(mut guard) = display_cache.lock() {
                                    guard.remove(&did);
                                }
                            }
                        }
                        // Cache user_display_granted for replay on reconnect.
                        // Clear the cache on user_display_revoked so a refreshed
                        // browser after a revoke doesn't re-enable the badge.
                        if line.contains("\"event\":\"user_display_granted\"") {
                            if let Ok(mut guard) = user_display_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"user_display_revoked\"") {
                            if let Ok(mut guard) = user_display_cache.lock() {
                                *guard = None;
                            }
                        }
                        if line.contains("\"event\":\"usage_update\"")
                            || line.contains("\"event\":\"usage\"")
                        {
                            if let Ok(mut guard) = usage_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"live_usage_update\"") {
                            if let Ok(mut guard) = live_usage_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"status\"") {
                            if let Ok(mut guard) = status_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"autonomy_changed\"") {
                            if let Ok(mut guard) = autonomy_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"external_agent_changed\"") {
                            if let Ok(mut guard) = external_agent_cache.lock() {
                                *guard = Some(line.clone());
                            }
                        }
                        if line.contains("\"event\":\"session_attached\"")
                            || line.contains("\"event\":\"session_identity\"")
                        {
                            if let Ok(mut guard) = session_attached_cache.lock() {
                                update_external_attached_sessions_from_wire(&mut guard, &line);
                            }
                        }
                        if line.contains("\"event\":\"session_ended\"") {
                            if let Ok(mut guard) = session_attached_cache.lock() {
                                update_external_attached_sessions_from_wire(&mut guard, &line);
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Peer registry → dashboard push translator.
    //
    // When the registry is wired (the daemon was started with
    // federation enabled), subscribe to its [`RegistryEvent`] stream
    // and translate each event into the matching wire-format
    // [`OutboundEvent`] variant, broadcast over the same channel as
    // every other dashboard event. The browser's existing primary
    // WebSocket pipeline picks them up and updates peer rows in-place
    // without polling `GET /api/peers`.
    //
    // Lagged events are skipped on purpose: the dashboard's recovery
    // path is to re-fetch `/api/peers`, which always returns ground
    // truth. Closed receiver = registry was dropped, exit cleanly.
    if let Some(reg) = peer_registry.as_ref() {
        let mut reg_rx = reg.subscribe();
        let push_tx = broadcast_tx.clone();
        let reg_for_task = reg.clone();
        let relay_registry_for_task = Arc::clone(&tcp_relay_registry);
        let relay_url_for_task = relay_advertise_url.clone();
        let bus_for_task = bus.clone();
        tokio::spawn(async move {
            loop {
                match reg_rx.recv().await {
                    Ok(event) => {
                        let outbound = match event {
                            crate::peer::RegistryEvent::PeerAdded(snap) => {
                                crate::types::OutboundEvent::PeerAdded { peer: snap }
                            }
                            crate::peer::RegistryEvent::PeerRemoved(id) => {
                                crate::types::OutboundEvent::PeerRemoved {
                                    id: id.as_str().to_string(),
                                }
                            }
                            crate::peer::RegistryEvent::PeerStateChanged(snap) => {
                                crate::types::OutboundEvent::PeerStateChanged { peer: snap }
                            }
                            crate::peer::RegistryEvent::PeerEventForwarded { peer, event } => {
                                // Slice 3b: when a federated Answer
                                // comes back toward the browser, rewrite
                                // the SDP to inject a TCP candidate
                                // pointing at the primary's own relay
                                // address, and register the peer's ufrag
                                // in the relay registry so incoming
                                // browser TCP connections with that
                                // ufrag get forwarded to the peer. Other
                                // event variants pass through verbatim.
                                let rewritten_event = maybe_rewrite_federated_answer(
                                    &peer,
                                    event,
                                    &reg_for_task,
                                    &relay_registry_for_task,
                                    relay_url_for_task.as_deref(),
                                    &bus_for_task,
                                )
                                .await;
                                crate::types::OutboundEvent::PeerEventForwarded {
                                    peer_id: peer.as_str().to_string(),
                                    payload: rewritten_event,
                                }
                            }
                        };
                        crate::control::broadcast_event(&push_tx, &outbound);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let app_html = Arc::new(rewrite_app_html_asset_urls(APP_HTML.to_string(), &v));
    // INTENDANT_APP_HTML_PATH (dev override): read once at spawn; when
    // set, every dashboard request re-reads that path instead of serving
    // the embedded `app_html` above.
    let app_html_override: Option<Arc<std::path::Path>> =
        app_html_override_path().map(Arc::from);
    if let Some(path) = &app_html_override {
        eprintln!(
            "[web_gateway] INTENDANT_APP_HTML_PATH: serving the dashboard from {} \
             (re-read per request; the embedded copy is ignored)",
            path.display()
        );
        if let Err(err) = std::fs::metadata(path) {
            eprintln!(
                "[web_gateway] WARNING: INTENDANT_APP_HTML_PATH is not readable right now: {err}"
            );
        }
    }
    // Lazily computed (ETag token, gzipped body) for the rewritten
    // app.html — once per gateway spawn, on the first page load. The
    // rewritten HTML is gateway-scoped (unlike the `include_*!` constants
    // behind `embedded_static_asset`), so its cache lives here.
    let app_html_cache: Arc<OnceLock<(String, Vec<u8>)>> = Arc::new(OnceLock::new());
    let tls_failure_log_state: TlsFailureLogState = Arc::new(Mutex::new(HashMap::new()));

    tokio::spawn(async move {
        let mut listener = listener;
        let bind_addr = listener.local_addr().ok();
        let port = bind_addr.map(|a| a.port()).unwrap_or(0);

        if let Some(p) = tcp_advertised_port {
            eprintln!("[web_gateway] ICE-TCP candidates advertise port {p}");
        }

        let mut fatal_accept_streak: u32 = 0;
        loop {
            let (stream, peer_addr) = match listener.accept().await {
                Ok(conn) => {
                    fatal_accept_streak = 0;
                    conn
                }
                Err(e) => {
                    if should_continue_after_accept_error(&e) {
                        eprintln!("[web_gateway] accept failed on port {port}: {e} (continuing)");
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        continue;
                    }
                    // "Fatal" classifications (EINVAL class) have been
                    // observed spuriously on macOS: accept() failed while
                    // the socket remained LISTEN at the kernel (backlog
                    // still completing handshakes), correlated with
                    // external-agent spawns. Give the same socket a short
                    // streak of retries before declaring it dead.
                    fatal_accept_streak += 1;
                    if fatal_accept_streak < FATAL_ACCEPT_REBIND_THRESHOLD {
                        eprintln!(
                            "[web_gateway] accept failed on port {port}: {e} (retry {fatal_accept_streak}/{FATAL_ACCEPT_REBIND_THRESHOLD} before rebind)"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        continue;
                    }
                    fatal_accept_streak = 0;
                    // Persistent: the listening socket really is dead.
                    // Exiting here would leave the daemon half-alive
                    // (established WebSockets keep the UI looking healthy
                    // while every new request fails) — rebind the original
                    // address instead, backing off up to 30s.
                    let Some(addr) = bind_addr else {
                        eprintln!(
                            "[web_gateway] accept failed on port {port}: {e} (bind address unknown; listener task exiting)"
                        );
                        break;
                    };
                    eprintln!(
                        "[web_gateway] accept failed on port {port}: {e} (rebinding listener)"
                    );
                    // Drop the dead listener before rebinding: it still owns
                    // the port at the kernel level, and SO_REUSEADDR only
                    // bypasses TIME_WAIT — a live (even unusable) LISTEN
                    // socket makes every rebind fail with EADDRINUSE, so
                    // holding it across the loop wedged recovery forever.
                    // Its backlog also keeps completing handshakes for
                    // requests nothing will ever read.
                    drop(listener);
                    let mut delay = std::time::Duration::from_millis(250);
                    listener = loop {
                        tokio::time::sleep(delay).await;
                        match rebind_dead_tcp_listener(addr) {
                            Ok(fresh) => {
                                eprintln!("[web_gateway] listener rebound on port {port}");
                                break fresh;
                            }
                            Err(err) => {
                                delay = (delay * 2).min(std::time::Duration::from_secs(30));
                                eprintln!(
                                    "[web_gateway] listener rebind on port {port} failed: {err} (retrying in {:.1}s)",
                                    delay.as_secs_f32()
                                );
                            }
                        }
                    };
                    continue;
                }
            };

            let bus = bus.clone();
            let broadcast_tx = broadcast_tx.clone();
            let config_json = config_json.clone();
            let agent_card_json = agent_card_json.clone();
            let agent_card_value_for_targets = agent_card_value_for_targets.clone();
            let peer_access_request_config = peer_access_request_config.clone();
            let peer_registry = peer_registry.clone();
            let dashboard_control = Arc::clone(&dashboard_control);
            let peer_file_transfer_registry = Arc::clone(&peer_file_transfer_registry);
            let ice_config = ice_config.clone();
            let tcp_peer_registry = Arc::clone(&tcp_peer_registry);
            let tcp_relay_registry = Arc::clone(&tcp_relay_registry);
            let tcp_advertised_port = tcp_advertised_port;
            let shared_session = shared_session.clone();
            let voice_debug = voice_debug.clone();
            let session_provider = session_provider.clone();
            let session_model = session_model.clone();
            let app_html = app_html.clone();
            let app_html_cache = app_html_cache.clone();
            let app_html_override = app_html_override.clone();
            let transcriber = transcriber.clone();
            let active_presence = active_presence.clone();
            let display_input_authority = display_input_authority.clone();
            let authority_change_tx = authority_change_tx.clone();
            let federated_authority_subscribers = federated_authority_subscribers.clone();
            let last_usage_json = last_usage_json.clone();
            let last_live_usage_json = last_live_usage_json.clone();
            let last_status_json = last_status_json.clone();
            let last_autonomy_json = last_autonomy_json.clone();
            let last_external_agent_json = last_external_agent_json.clone();
            let attached_external_sessions = attached_external_sessions.clone();
            let last_user_display_json = last_user_display_json.clone();
            let display_ready_cache = display_ready_cache.clone();
            let web_tui_tx = web_tui_tx.clone();
            let task_tx = task_tx.clone();
            let project_root = project_root.clone();
            let mcp_server = mcp_server.clone();
            let terminal_registry = terminal_registry.clone();
            let inbound_bearer_token = inbound_bearer_token.clone();
            let worktree_inventory_cache = worktree_inventory_cache.clone();
            let tls_client_cert_required = tls_client_cert_required;
            let source_hint = peer_addr.ip().to_string();
            let tls_failure_log_state = Arc::clone(&tls_failure_log_state);
            // `TlsAcceptor` wraps an `Arc<ServerConfig>`, so cloning is cheap
            // (one Arc bump). `None` when TLS is disabled.
            let tls_acceptor = tls_acceptor.clone();

            tokio::spawn(async move {
                // Snapshot session state at connection time
                let session_snap = shared_session.read().await;
                let daemon_session_id = session_snap.daemon_session_id.clone();
                let query_ctx = session_snap.query_ctx.clone();
                let frame_registry = session_snap.frame_registry.clone();
                let session_log = session_snap.session_log.clone();
                let recording_registry = session_snap.recording_registry.clone();
                let session_registry = session_snap.session_registry.clone();
                let snapshot_dir = session_snap.snapshot_dir.clone();
                let project_root_for_changes = session_snap.project_root_for_changes.clone();
                let file_watcher = session_snap.file_watcher.clone();
                let runtime_settings = session_snap.runtime_settings.clone();
                drop(session_snap);
                // Peek at the first bytes to detect (in order):
                //  1. ICE-TCP STUN-framed traffic (RFC 4571 length prefix
                //     followed by a STUN message whose magic cookie
                //     0x2112A442 sits at payload offset 4 = peek offset 6).
                //     First byte (length MSB) is 0x00 for STUN-sized frames.
                //  2. TLS ClientHello (handshake record: first byte 0x16,
                //     then version major 0x03) — only when a TLS acceptor
                //     is configured. Wrapped in the rustls acceptor; the
                //     decrypted stream then flows through the WS/HTTP paths
                //     below exactly as a plain connection would.
                //  3. WebSocket upgrade (HTTP header containing
                //     "Upgrade: websocket")
                //  4. Plain HTTP (everything else)
                //
                // Cases 3 and 4 are cleartext. When a TLS acceptor is
                // configured the dashboard is HTTPS/WSS-only, so such
                // cleartext connections are refused (see the strict-TLS
                // rejection below) — only the TLS-wrapped path serves them.
                // Case 1 (raw ICE-TCP for the WebRTC media tunnel) stays
                // cleartext regardless: it returns above before that check.
                //
                // The three first-byte classes are mutually exclusive:
                // STUN length-prefix MSB 0x00, TLS handshake 0x16, HTTP
                // method ASCII letters (>= 0x41). So one peeked byte
                // disambiguates raw ICE-TCP from TLS from cleartext HTTP.
                //
                // `peek()` does not consume the data, so the ICE-TCP, TLS,
                // WebSocket, and HTTP branches all still get the full
                // first segment. The ICE-TCP branch reads (and consumes)
                // the first RFC 4571 frame and hands the rest to the
                // WebRTC peer reader; the TLS branch lets the handshake
                // consume the peeked ClientHello and re-reads the
                // decrypted request head before dispatching.
                let mut buf = [0u8; 2048];
                let mut raw_stream = stream;
                let peeked = match raw_stream.peek(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };

                // ICE-TCP detection: look for a STUN binding request
                // wrapped in an RFC 4571 2-byte BE length prefix. STUN
                // binding request type is 0x0001 (first payload byte < 2),
                // magic cookie is 0x2112A442 at payload offset 4, which
                // lives at peek offset 6..10 once we account for the
                // length prefix. A valid HTTP request never starts with
                // these bytes (method chars are ASCII >= 0x41).
                let looks_like_stun_tcp =
                    peeked >= 22 && buf[2] < 2 && buf[6..10] == [0x21, 0x12, 0xA4, 0x42];
                if looks_like_stun_tcp {
                    // Consume the first RFC 4571 frame from the stream
                    // (peek leaves it in the kernel buffer; we have to
                    // read it through to hand a clean stream to the peer
                    // reader task).
                    let first_frame =
                        match crate::display::webrtc::read_rfc4571_frame_pub(&mut raw_stream).await
                        {
                            Ok(f) => f,
                            Err(e) => {
                                eprintln!("[web_gateway] ICE-TCP first-frame read failed: {e}");
                                return;
                            }
                        };
                    let remote_addr = match raw_stream.peer_addr() {
                        Ok(a) => a,
                        Err(_) => return,
                    };

                    // Slice 3b dispatch: parse the frame's ufrag once,
                    // then check the local `TcpPeerRegistry` first (for
                    // local WebRtcPeers the daemon owns) and fall
                    // through to the `TcpRelayRegistry` (federated
                    // peers the primary relays to). Unknown ufrag =
                    // close with a diagnostic log.
                    //
                    // Local first keeps the existing behavior
                    // unchanged for non-federated topologies;
                    // relay-as-fallback adds the federation relay
                    // path without touching the local fast path.
                    match crate::display::webrtc::parse_first_frame_ufrag(&first_frame) {
                        Some(ufrag) if tcp_peer_registry.contains_ufrag(&ufrag) => {
                            if let Err(e) = tcp_peer_registry
                                .route_accepted(raw_stream, first_frame, remote_addr)
                                .await
                            {
                                eprintln!(
                                    "[web_gateway] ICE-TCP local routing for {remote_addr} failed: {e}"
                                );
                            }
                        }
                        Some(ufrag) if tcp_relay_registry.contains_ufrag(&ufrag) => {
                            if let Err(e) = tcp_relay_registry
                                .route_accepted(raw_stream, first_frame)
                                .await
                            {
                                eprintln!(
                                    "[web_gateway] ICE-TCP relay routing for ufrag={ufrag} from {remote_addr} failed: {e}"
                                );
                            }
                        }
                        Some(ufrag) => {
                            eprintln!(
                                "[web_gateway] ICE-TCP: no route for ufrag {ufrag:?} from {remote_addr} \
                                 (neither local peer nor registered relay)"
                            );
                        }
                        None => {
                            eprintln!(
                                "[web_gateway] ICE-TCP: first frame from {remote_addr} isn't a \
                                 STUN binding request with a parseable USERNAME"
                            );
                        }
                    }
                    return;
                }

                // Connection is not raw ICE-TCP. It is one of: TLS
                // (HTTPS/WSS), plain WebSocket, or plain HTTP. Convert the
                // raw `TcpStream` into a unified, boxed `DemuxStream` that
                // the WS/HTTP handling below operates through. The plain
                // path boxes the TcpStream verbatim (the peeked bytes stay
                // in the kernel buffer, unconsumed). The TLS path runs the
                // rustls handshake — which consumes the peeked ClientHello
                // — then re-reads the decrypted request head so the rest of
                // the handler sees cleartext HTTP.
                let is_tls = tls_acceptor.is_some()
                    && crate::web_tls::looks_like_tls_client_hello(&buf[..peeked]);

                let cleartext_header_text = if is_tls {
                    String::new()
                } else {
                    String::from_utf8_lossy(&buf[..peeked]).to_string()
                };
                let allow_loopback_cleartext_mcp =
                    is_loopback_cleartext_mcp_request(peer_addr, is_tls, &cleartext_header_text);

                // Strict TLS: when a TLS acceptor is configured the dashboard
                // is HTTPS/WSS-only. A connection that reaches this point is
                // neither raw ICE-TCP (handled and returned above — that path
                // stays cleartext for the WebRTC media tunnel and must keep
                // working) nor a TLS ClientHello, so it's a cleartext HTTP or
                // WebSocket client dialing the secure port in the clear.
                // Opportunistic TLS — quietly serving such a client over plain
                // HTTP — would undercut the project's "no unencrypted traffic"
                // guarantee, so we refuse it. The one exception is the local
                // loopback `/mcp` endpoint used by managed child CLIs: those
                // clients cannot present the dashboard mTLS certificate, and
                // their transport never leaves the host. Browser-originated
                // requests do not qualify for that exception.
                if tls_acceptor.is_some() && !is_tls && !allow_loopback_cleartext_mcp {
                    use tokio::io::AsyncWriteExt;
                    log_tls_failure_rate_limited(
                        &tls_failure_log_state,
                        &source_hint,
                        "strict TLS cleartext reject",
                        "dashboard is HTTPS/WSS-only; use https:// or wss://",
                    );
                    let body = "This endpoint requires TLS. Use https:// (or wss://) instead of \
                                http:// / ws://.\n";
                    let response = format!(
                        "HTTP/1.1 426 Upgrade Required\r\n\
                         Content-Type: text/plain\r\n\
                         Content-Length: {}\r\n\
                         Upgrade: TLS/1.2\r\n\
                         Connection: close\r\n\
                         \r\n\
                         {body}",
                        body.len(),
                    );
                    let _ = raw_stream.write_all(response.as_bytes()).await;
                    let _ = raw_stream.shutdown().await;
                    return;
                }

                let buf_owned: Vec<u8>;
                let n: usize;
                let mut stream: DemuxStream;
                let tls_client_cert_present: bool;
                let tls_client_cert_fingerprint: Option<String>;
                if is_tls {
                    let acceptor = tls_acceptor
                        .as_ref()
                        .expect("is_tls implies acceptor present")
                        .clone();
                    let mut tls_stream = match acceptor.accept(raw_stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            log_tls_failure_rate_limited(
                                &tls_failure_log_state,
                                &source_hint,
                                "TLS handshake failed",
                                &e.to_string(),
                            );
                            return;
                        }
                    };
                    let peer_certs = tls_stream
                        .get_ref()
                        .1
                        .peer_certificates()
                        .map(|certs| certs.to_vec())
                        .unwrap_or_default();
                    tls_client_cert_present = !peer_certs.is_empty();
                    tls_client_cert_fingerprint = peer_certs
                        .first()
                        .map(|cert| crate::peer::access_policy::fingerprint_der(cert.as_ref()));
                    // Read the first segment of the *decrypted* request so
                    // we can route on the real HTTP request line/headers.
                    // This is the TLS analogue of the plain-path peek.
                    use tokio::io::AsyncReadExt;
                    let mut decrypted = vec![0u8; 8192];
                    let read_n = match tls_stream.read(&mut decrypted).await {
                        Ok(0) => return, // client closed right after handshake
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("[web_gateway] TLS first decrypted read failed: {e}");
                            return;
                        }
                    };
                    decrypted.truncate(read_n);
                    n = read_n;
                    buf_owned = decrypted.clone();
                    // Replay the decrypted request head in front of the TLS
                    // stream so the WS upgrade / HTTP body reads downstream
                    // see the request from byte zero.
                    stream = Box::pin(crate::web_tls::PrefixedStream::new(decrypted, tls_stream));
                } else {
                    // Plain HTTP/WS: the peeked bytes are still in the
                    // kernel buffer. Box the raw stream with an empty
                    // replay prefix — a zero-overhead pass-through that
                    // reads the request straight from the socket.
                    n = peeked;
                    buf_owned = buf[..peeked].to_vec();
                    tls_client_cert_present = false;
                    tls_client_cert_fingerprint = None;
                    stream = Box::pin(crate::web_tls::PrefixedStream::new(Vec::new(), raw_stream));
                }
                // Downstream code reads `buf[..n]`; point `buf` at the
                // (decrypted, for TLS) request head we just captured.
                let buf = buf_owned.as_slice();

                let header_text = String::from_utf8_lossy(&buf[..n]);
                let request_line = header_text.lines().next().unwrap_or("");
                let peer_connection_identity = match resolve_peer_connection_identity(
                    &header_text,
                    tls_client_cert_fingerprint.as_deref(),
                ) {
                    Ok(identity) => identity,
                    Err((status, body)) => {
                        use tokio::io::AsyncWriteExt;
                        let reason = match status {
                            401 => "Unauthorized",
                            403 => "Forbidden",
                            _ => "Error",
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {body}",
                            body.len(),
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        finalize_http_stream(&mut stream).await;
                        return;
                    }
                };
                let is_websocket = header_text
                    .lines()
                    .any(|l| l.to_lowercase().contains("upgrade: websocket"));

                // Parse the `Host:` header to learn what address the
                // browser thinks reaches us. We use this later as the IP
                // for ICE-TCP host candidates: Firefox refuses to pair
                // remote loopback candidates, so we need a non-loopback
                // address the browser can actually connect to. The only
                // one we know for sure the browser can reach is whatever
                // it just used to reach us for HTTP — which is exactly
                // what the Host header contains. If the user accessed
                // via a hostname (`localhost`, `myserver.local`) rather
                // than a literal IP, we get None here and skip the TCP
                // candidate entirely; those users can still use UDP if
                // their topology allows it.
                let browser_host_ip: Option<std::net::IpAddr> =
                    extract_host_header_ip(&header_text);

                if is_websocket {
                    if tls_client_cert_required && !tls_client_cert_present {
                        use tokio::io::AsyncWriteExt;
                        let body = serde_json::json!({
                            "error": "mTLS client certificate required"
                        })
                        .to_string();
                        let response = format!(
                            "HTTP/1.1 401 Unauthorized\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {body}",
                            body.len(),
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        finalize_http_stream(&mut stream).await;
                        return;
                    }
                    // Bearer enforcement on /ws — dual-mode (Authorization
                    // header from daemons, ?token= query param from
                    // browsers). Reject with a plain HTTP 401 *before*
                    // the WebSocket handshake so the rejected client
                    // never sees a successful upgrade.
                    if let Err((status, body)) =
                        verify_bearer_for_ws(&header_text, inbound_bearer_token.as_deref())
                    {
                        use tokio::io::AsyncWriteExt;
                        let reason = match status {
                            401 => "Unauthorized",
                            _ => "Error",
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             WWW-Authenticate: Bearer\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {body}",
                            body.len(),
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        // Flush + cleanly shut down before the task returns and
                        // drops the stream. On the TLS path rustls buffers the
                        // ciphertext for this 401 inside the session; dropping
                        // without flushing discards it and the rejected client
                        // sees an *empty* response instead of the 401 (audit
                        // F2). A no-op pass-through on plain TCP.
                        finalize_http_stream(&mut stream).await;
                        return;
                    }
                    let cert_dir = crate::access::backend::select_backend().cert_dir();
                    let dashboard_control_grant_for_ws = match dashboard_control_grant_for_client(
                        &cert_dir,
                        peer_connection_identity.as_ref(),
                        tls_client_cert_fingerprint.as_deref(),
                    ) {
                        Ok(grant) => grant,
                        Err(message) => {
                            use tokio::io::AsyncWriteExt;
                            let response = json_error("500 Internal Server Error", message);
                            let _ = stream.write_all(response.as_bytes()).await;
                            finalize_http_stream(&mut stream).await;
                            return;
                        }
                    };
                    let peer_identity_for_ws = peer_connection_identity.clone();
                    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };

                    let (mut ws_tx, mut ws_rx) = ws_stream.split();
                    let mut outbound_rx = broadcast_tx.subscribe();

                    // Per-connection identity for active/passive tracking
                    let connection_id = uuid::Uuid::new_v4().to_string();

                    // Direct response channel: tool_response and state_snapshot
                    // messages for this specific connection (not broadcast).
                    let (direct_tx, mut direct_rx) = mpsc::unbounded_channel::<String>();

                    // Register connection with WebTui for per-connection rendering
                    if let Some(ref tx) = web_tui_tx {
                        let _ = tx.send(crate::tui::web::WebTuiCommand::AddConnection {
                            id: connection_id.clone(),
                            direct_tx: direct_tx.clone(),
                            cols: 120,
                            rows: 40,
                        });
                    }

                    // Send bootstrap state snapshot on connect (with connection_id).
                    // Include config (provider/model) since AgentStateSnapshot
                    // doesn't carry those. The top-level `session_id` is the
                    // stable daemon/process session, not the active worker log.
                    let state = query_ctx
                        .as_ref()
                        .map(|ctx| {
                            ctx.agent_state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone()
                        })
                        .unwrap_or_default();
                    let bootstrap_session_id = daemon_session_id
                        .clone()
                        .or_else(|| {
                            query_ctx
                                .as_ref()
                                .and_then(|ctx| replay_session_id_from_dir(&ctx.log_dir))
                        })
                        .or_else(|| session_log.as_ref().and_then(session_log_id));
                    if query_ctx.is_some() || bootstrap_session_id.is_some() {
                        let config: serde_json::Value =
                            serde_json::from_str(&config_json).unwrap_or_default();
                        let bootstrap = serde_json::json!({
                            "t": "state_snapshot",
                            "state": state,
                            "connection_id": connection_id,
                            "config": config,
                            "session_id": bootstrap_session_id.unwrap_or_default(),
                        });
                        let _ = direct_tx.send(bootstrap.to_string());
                    }

                    // Send cached usage data so late-connecting browsers
                    // populate the Usage tab without sending ControlMsg.
                    if let Ok(guard) = last_usage_json.lock() {
                        if let Some(ref usage_json) = *guard {
                            let _ = direct_tx.send(usage_json.clone());
                        }
                    }

                    // Send cached live usage data.
                    if let Ok(guard) = last_live_usage_json.lock() {
                        if let Some(ref live_json) = *guard {
                            let _ = direct_tx.send(live_json.clone());
                        }
                    }

                    // Send cached status (autonomy, session_id, task).
                    if let Ok(guard) = last_status_json.lock() {
                        if let Some(ref status_json) = *guard {
                            let _ = direct_tx.send(status_json.clone());
                        }
                    }

                    // Send cached autonomy after cached status so it wins
                    // when the latest status event is older than the user's
                    // most recent autonomy switch.
                    if let Ok(guard) = last_autonomy_json.lock() {
                        if let Some(ref autonomy_json) = *guard {
                            let _ = direct_tx.send(autonomy_json.clone());
                        }
                    }

                    // Send cached external_agent_changed so the dropdown
                    // and status badge reflect the current value on a
                    // fresh browser connection.
                    if let Ok(guard) = last_external_agent_json.lock() {
                        if let Some(ref ea_json) = *guard {
                            let _ = direct_tx.send(ea_json.clone());
                        }
                    }

                    // Send cached user_display_granted so the "your display"
                    // status bar toggle reflects the current grant state on
                    // a refreshed browser. Cache is cleared on revoke so
                    // a revoked state simply results in nothing being sent
                    // (the dashboard's HTML default is "off").
                    if let Ok(guard) = last_user_display_json.lock() {
                        if let Some(ref ud_json) = *guard {
                            let _ = direct_tx.send(ud_json.clone());
                        }
                    }

                    let browser_workspaces = crate::browser_workspace::list_workspaces().await;
                    let browser_snapshot = serde_json::json!({
                        "t": "browser_workspace_snapshot",
                        "workspaces": browser_workspaces,
                    });
                    let _ = direct_tx.send(browser_snapshot.to_string());

                    // Replay display_ready for every active display session so
                    // late-connecting browsers (including refreshes) recreate
                    // their DisplaySlots and initiate WebRTC.  Prefer the
                    // live session registry over the broadcast cache — it is
                    // authoritative and handles multiple concurrent displays.
                    //
                    // Phase 5a.1: alongside each display_ready, send a
                    // personalized `display_input_authority_state` so the
                    // browser starts at the authoritative state instead of
                    // `unknown`.  Without this snapshot the chip would only
                    // resolve on the next authority transition, which may
                    // be never if no one ever takes control.
                    //
                    // Frame ordering: `display_ready` goes out now (so the
                    // slot exists before any log replay happens); the
                    // per-display `display_input_authority_state` frame is
                    // deferred until *after* `log_replay` below. **#59**:
                    // browser-side `addDisplaySlot` is now idempotent for
                    // an existing live slot, so a replayed historical
                    // `display_ready` no longer destroys the bootstrap
                    // slot. The deferral here is therefore defense-in-
                    // depth against message ordering and late-replay
                    // state — for example a grant→revoke→grant cycle in
                    // session.jsonl whose intermediate `user_display_revoked`
                    // does tear the bootstrap slot down, after which the
                    // replayed re-grant `display_ready` creates a fresh
                    // slot that needs the authority frame to land on it
                    // rather than on the destroyed predecessor. Sending
                    // the authority frame after replay guarantees it lands
                    // on the *final* slot in every replay shape.
                    let bootstrap_authority_snapshots: Vec<(u32, &'static str)> =
                        if let Some(ref sr) = session_registry {
                            let reg = sr.read().await;
                            let active_ids: Vec<u32> = reg.display_ids();
                            // Snapshot resolutions + auth states under the
                            // std lock, then drop the guard before any
                            // direct_tx.send calls.
                            let resolutions: Vec<(u32, u32, u32)> = active_ids
                                .iter()
                                .filter_map(|&did| {
                                    reg.get(did).map(|session| {
                                        let (w, h) = session.resolution();
                                        (did, w, h)
                                    })
                                })
                                .collect();
                            let auth_snapshots = {
                                let auth = display_input_authority
                                    .read()
                                    .unwrap_or_else(|e| e.into_inner());
                                compute_bootstrap_authority_snapshots(
                                    resolutions.iter().map(|(did, _, _)| *did),
                                    &auth,
                                    &connection_id,
                                )
                            };
                            // Send the display_ready frames now; defer the
                            // authority frames until after log_replay.
                            for (did, w, h) in resolutions {
                                let ready = serde_json::json!({
                                    "event": "display_ready",
                                    "display_id": did,
                                    "width": w,
                                    "height": h,
                                });
                                let _ = direct_tx.send(ready.to_string());
                            }
                            auth_snapshots
                        } else {
                            // Fallback: use the broadcast-derived cache when
                            // no session registry is available (shouldn't
                            // happen in practice, but keeps the old
                            // behaviour as safety net).  No authority frame
                            // to send in this branch — the cache only holds
                            // display_ready JSON, no holder state.
                            if let Ok(guard) = display_ready_cache.lock() {
                                for display_json in guard.values() {
                                    let _ = direct_tx.send(display_json.clone());
                                }
                            }
                            Vec::new()
                        };

                    // Replay session log so late-connecting browsers see
                    // historical events (not just real-time from now on).
                    // Each JSONL entry is converted to an OutboundEvent via
                    // session_log_entry_to_app_event → app_event_to_outbound
                    // so replay drives the same rendering path as live.
                    let replay_log_dir =
                        query_ctx
                            .as_ref()
                            .map(|ctx| ctx.log_dir.clone())
                            .or_else(|| {
                                session_log.as_ref().and_then(|sl| {
                                    sl.lock().ok().map(|log| log.dir().to_path_buf())
                                })
                            });
                    let mut replayed_external_session_ids: HashSet<String> = HashSet::new();
                    if let Some(ref log_dir) = replay_log_dir {
                        if let Some((replay, external_session_id)) =
                            session_log_replay_payload_from_dir_with_limit(
                                log_dir,
                                Some(WEBSOCKET_BOOTSTRAP_REPLAY_ENTRY_LIMIT),
                            )
                        {
                            if let Some(external_session_id) = external_session_id {
                                replayed_external_session_ids.insert(external_session_id);
                            }
                            let _ = direct_tx.send(replay);
                        }
                    }

                    let mut active_external_sessions: Vec<(String, String)> =
                        attached_external_sessions
                            .lock()
                            .ok()
                            .map(|guard| {
                                guard
                                    .iter()
                                    .map(|(session_id, source)| {
                                        (session_id.clone(), source.clone())
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                    active_external_sessions.sort_by(|a, b| a.0.cmp(&b.0));
                    let home_dir = crate::platform::home_dir();
                    for (session_id, source) in active_external_sessions {
                        let wrapper_replay_is_current = replayed_external_session_ids
                            .contains(&session_id)
                            && replay_log_dir.as_ref().is_some_and(|log_dir| {
                                !external_session_newer_than_wrapper(
                                    &home_dir,
                                    log_dir,
                                    &source,
                                    &session_id,
                                )
                            });
                        if wrapper_replay_is_current {
                            continue;
                        }
                        if let Some(replay) =
                            external_session_activity_replay_for_websocket(&source, &session_id)
                        {
                            let _ = direct_tx.send(replay);
                        }
                    }

                    // Phase 5a.1: now that log_replay has finished
                    // recreating display slots from historical events,
                    // send the personalized `display_input_authority_state`
                    // for each currently-active display.  Sending these
                    // before log_replay would land the chip on a slot that
                    // log_replay then destroys (see the slot lifecycle
                    // bookkeeping in `addDisplaySlot` / `removeDisplaySlot`
                    // on the browser side).
                    for (did, state) in bootstrap_authority_snapshots {
                        let auth_msg = serde_json::json!({
                            "t": "display_input_authority_state",
                            "display_id": did,
                            "state": state,
                        });
                        let _ = direct_tx.send(auth_msg.to_string());
                    }

                    // Inbound: WebSocket → EventBus
                    // Handles message types:
                    //   {"t":"key", "key":"Enter", ...}  → AppEvent::Key
                    //   {"t":"resize", "cols":N, "rows":N} → AppEvent::Resize
                    //   {"t":"presence_connect",...}     → AppEvent::PresenceConnected
                    //   {"t":"presence_disconnect"}      → AppEvent::PresenceDisconnected
                    //   {"t":"voice_log",...}             → AppEvent::VoiceLog
                    //   {"t":"presence_checkpoint",...}   → AppEvent::PresenceCheckpointReceived
                    //   {"t":"voice_diagnostic",...}      → AppEvent::VoiceDiagnostic
                    //   {"t":"tool_request", "id":"...", "tool":"...", "args":{}} → tool_response
                    //   {"action":"status", ...}         → AppEvent::ControlCommand
                    // Assign a unique peer ID for WebRTC signaling
                    let peer_id = NEXT_PEER_ID.fetch_add(1, Ordering::Relaxed);

                    let bus_inbound = bus.clone();
                    let query_ctx_inbound = query_ctx.clone();
                    let direct_tx_inbound = direct_tx.clone();
                    let voice_debug_inbound = voice_debug.clone();
                    let live_provider = session_provider.clone();
                    let live_model = session_model.clone();
                    let transcriber_inbound = transcriber.clone();
                    let active_presence_inbound = active_presence.clone();
                    let display_input_authority_inbound = display_input_authority.clone();
                    let authority_change_tx_inbound = authority_change_tx.clone();
                    let federated_authority_subscribers_inbound =
                        federated_authority_subscribers.clone();
                    let connection_id_inbound = connection_id.clone();
                    let web_tui_tx_inbound = web_tui_tx.clone();
                    let frame_registry_inbound = frame_registry.clone();
                    let recording_registry_inbound = recording_registry.clone();
                    let session_log_inbound = session_log.clone();
                    let session_registry_inbound = session_registry.clone();
                    let task_tx_inbound = task_tx.clone();
                    let terminal_registry_inbound = terminal_registry.clone();
                    let dashboard_control_inbound = Arc::clone(&dashboard_control);
                    let dashboard_control_grant_inbound = dashboard_control_grant_for_ws.clone();
                    let peer_file_transfer_registry_inbound =
                        Arc::clone(&peer_file_transfer_registry);
                    let peer_identity_inbound = peer_identity_for_ws.clone();
                    let inbound = tokio::spawn(async move {
                        // Track whether this connection has an active presence model,
                        // so we can auto-send PresenceDisconnected if the WebSocket drops
                        // without a clean presence_disconnect message (e.g. tab close
                        // before beforeunload fires, network failure).
                        let mut is_presence_connected = false;
                        // Whether this connection is the active voice owner
                        let mut is_active = false;

                        // Per-connection clip accumulators for batched clip_frame messages
                        struct ClipAccumulator {
                            stream: String,
                            note: String,
                            inject: bool,
                            in_secs: f64,
                            out_secs: f64,
                            fps: u32,
                            #[allow(dead_code)]
                            expected: usize,
                            frames: Vec<(String, String)>, // (frame_id, base64_data)
                        }
                        let mut clip_accumulators: std::collections::HashMap<
                            String,
                            ClipAccumulator,
                        > = std::collections::HashMap::new();

                        // Display IDs this peer has WebRTC connections to,
                        // used for cleanup when the WebSocket disconnects.
                        let mut peer_display_ids: Vec<u32> = Vec::new();
                        let mut dashboard_control_session_ids: Vec<String> = Vec::new();

                        // Frame types already denied+logged once on this
                        // connection — dedupes the warn log only; the denial
                        // frame itself is sent for every rejected frame.
                        let mut ws_denied_logged: std::collections::HashSet<String> =
                            std::collections::HashSet::new();

                        // Shell-session lane for this connection: root sees
                        // every session, scoped principals see owned/shared.
                        let ws_terminal_actor = dashboard_control_grant_inbound.terminal_actor();

                        // Per-connection audio transcription buffer.
                        // PCM16 bytes are accumulated and drained every ~3s.
                        let mut audio_buf: Vec<u8> = Vec::new();
                        let mut audio_seq: u64 = 0;
                        // Input sample rate (known from config, default 16kHz)
                        let audio_sample_rate: u32 = 16000;

                        while let Some(Ok(msg)) = ws_rx.next().await {
                            if let Message::Text(text) = msg {
                                let trimmed = text.trim();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                // Try to parse as JSON for type-tagged messages
                                if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed)
                                {
                                    // Per-frame IAM enforcement on the direct
                                    // /ws path — the same frame→operation
                                    // table the dashboard-control tunnel
                                    // enforces, so a scoped grant means the
                                    // same thing on every transport.
                                    if deny_ws_frame_if_unauthorized(
                                        &dashboard_control_grant_inbound,
                                        &json,
                                        &direct_tx_inbound,
                                        &bus_inbound,
                                        &mut ws_denied_logged,
                                    ) {
                                        continue;
                                    }
                                    match json.get("t").and_then(|v| v.as_str()) {
                                        Some("key") => {
                                            // Route key events to this connection's
                                            // ViewState via WebTuiCommand (not EventBus).
                                            if let Some(key_event) =
                                                crate::tui::web::parse_web_key(&json)
                                            {
                                                if let Some(ref tx) = web_tui_tx {
                                                    let _ = tx.send(
                                                        crate::tui::web::WebTuiCommand::Key {
                                                            id: connection_id_inbound.clone(),
                                                            key: key_event,
                                                        },
                                                    );
                                                } else if is_active {
                                                    // Fallback: no WebTui (headless web mode)
                                                    bus_inbound.send(AppEvent::Key(key_event));
                                                }
                                            }
                                        }
                                        Some("resize") => {
                                            // Route resize to this connection's terminal
                                            let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                                            let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                                            if let Some(ref tx) = web_tui_tx {
                                                let _ = tx.send(
                                                    crate::tui::web::WebTuiCommand::Resize {
                                                        id: connection_id_inbound.clone(),
                                                        cols,
                                                        rows,
                                                    },
                                                );
                                            } else if is_active {
                                                bus_inbound.send(AppEvent::Resize(cols, rows));
                                            }
                                        }
                                        Some("term_subscribe") => {
                                            // Dashboard entered the Terminal tab. Start
                                            // emitting ratatui frames to this connection.
                                            // Every non-Terminal tab (Activity, Stats,
                                            // Video, Sessions, Network, Settings, Debug)
                                            // leaves us unsubscribed, which means WebTui
                                            // stays idle instead of flooding the socket
                                            // with frames nobody is watching.
                                            if let Some(ref tx) = web_tui_tx {
                                                let _ = tx.send(
                                                    crate::tui::web::WebTuiCommand::Subscribe {
                                                        id: connection_id_inbound.clone(),
                                                    },
                                                );
                                            }
                                        }
                                        Some("term_unsubscribe") => {
                                            // Dashboard left the Terminal tab. Stop
                                            // emitting ratatui frames to this connection
                                            // until the next term_subscribe.
                                            if let Some(ref tx) = web_tui_tx {
                                                let _ = tx.send(
                                                    crate::tui::web::WebTuiCommand::Unsubscribe {
                                                        id: connection_id_inbound.clone(),
                                                    },
                                                );
                                            }
                                        }
                                        Some("presence_connect") => {
                                            is_presence_connected = true;
                                            voice_debug_inbound
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .connected = true;
                                            let server_session_id = json
                                                .get("server_session_id")
                                                .and_then(|v| v.as_str())
                                                .map(String::from);
                                            let last_event_seq = json
                                                .get("last_event_seq")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0);
                                            // Use provider/model from the browser if sent,
                                            // fall back to config defaults.
                                            let msg_provider = json
                                                .get("provider")
                                                .and_then(|v| v.as_str())
                                                .filter(|s| !s.is_empty())
                                                .map(String::from)
                                                .unwrap_or_else(|| live_provider.clone());
                                            let msg_model = json
                                                .get("model")
                                                .and_then(|v| v.as_str())
                                                .filter(|s| !s.is_empty())
                                                .map(String::from)
                                                .unwrap_or_else(|| live_model.clone());

                                            // Determine if this connection becomes active or passive.
                                            // Browsers can request always-passive mode (observer/follow-along).
                                            let force_passive = json
                                                .get("passive")
                                                .and_then(|v| v.as_bool())
                                                .unwrap_or(false);
                                            let becomes_active = if force_passive {
                                                false
                                            } else {
                                                let slot = active_presence_inbound
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                // Empty slot → first connect wins.
                                                // Slot occupied by THIS connection → already active
                                                // (happens when active browser reconnects voice after handover).
                                                slot.is_none()
                                                    || slot
                                                        .as_ref()
                                                        .map(|a| {
                                                            a.connection_id == connection_id_inbound
                                                        })
                                                        .unwrap_or(false)
                                            };

                                            let was_already_active = is_active;
                                            if becomes_active {
                                                // First-connect wins (or re-confirm already-active)
                                                *active_presence_inbound
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner()) =
                                                    Some(ActivePresence {
                                                        connection_id: connection_id_inbound
                                                            .clone(),
                                                        direct_tx: direct_tx_inbound.clone(),
                                                    });
                                                is_active = true;
                                            }

                                            // Send welcome with replay window if presence session is available
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                // Build conversation context from recent voice transcripts
                                                let conversation_ctx =
                                                    presence::build_conversation_context(
                                                        &ctx.log_dir,
                                                        20,
                                                    );

                                                if let Some(ref ps) = ctx.presence_session {
                                                    let mut session = ps
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner());
                                                    if becomes_active {
                                                        session.set_connected(true);
                                                    }
                                                    let state = ctx
                                                        .agent_state
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .clone();
                                                    let welcome = session
                                                        .build_welcome(last_event_seq, &state);
                                                    let welcome_msg = serde_json::json!({
                                                        "t": "presence_welcome",
                                                        "session_id": welcome.session_id,
                                                        "state": welcome.state,
                                                        "events": welcome.events,
                                                        "last_checkpoint_summary": welcome.last_checkpoint_summary,
                                                        "current_seq": welcome.current_seq,
                                                        "is_active": becomes_active,
                                                        "conversation_context": conversation_ctx,
                                                    });
                                                    let _ = direct_tx_inbound
                                                        .send(welcome_msg.to_string());
                                                } else {
                                                    let welcome_msg = serde_json::json!({
                                                        "t": "presence_welcome",
                                                        "is_active": becomes_active,
                                                        "conversation_context": conversation_ctx,
                                                    });
                                                    let _ = direct_tx_inbound
                                                        .send(welcome_msg.to_string());
                                                }
                                            } else {
                                                // No presence session — still send a minimal welcome with is_active
                                                let welcome_msg = serde_json::json!({
                                                    "t": "presence_welcome",
                                                    "is_active": becomes_active,
                                                });
                                                let _ =
                                                    direct_tx_inbound.send(welcome_msg.to_string());
                                            }

                                            // Only emit PresenceConnected for the active browser
                                            // (passive browsers don't pause server-side presence).
                                            // Skip if already active (e.g. voice reconnect after make_active
                                            // handover — PresenceConnected was already emitted by make_active).
                                            if becomes_active && !was_already_active {
                                                if let Some(ref sl) = session_log_inbound {
                                                    if let Ok(mut l) = sl.lock() {
                                                        l.presence_connected(
                                                            Some(&msg_provider),
                                                            Some(&msg_model),
                                                        );
                                                    }
                                                }
                                                bus_inbound.send(AppEvent::PresenceConnected {
                                                    server_session_id,
                                                    last_event_seq,
                                                    live_provider: Some(msg_provider),
                                                    live_model: Some(msg_model),
                                                });
                                            }
                                        }
                                        Some("presence_disconnect") => {
                                            is_presence_connected = false;
                                            voice_debug_inbound
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .connected = false;
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                if let Some(ref ps) = ctx.presence_session {
                                                    ps.lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .set_connected(false);
                                                }
                                            }
                                            // Only emit PresenceDisconnected if this was the active browser
                                            if is_active {
                                                // Clear the active slot
                                                let mut slot = active_presence_inbound
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                if slot
                                                    .as_ref()
                                                    .map(|a| {
                                                        a.connection_id == connection_id_inbound
                                                    })
                                                    .unwrap_or(false)
                                                {
                                                    *slot = None;
                                                }
                                                is_active = false;
                                                if let Some(ref sl) = session_log_inbound {
                                                    if let Ok(mut l) = sl.lock() {
                                                        l.presence_disconnected();
                                                    }
                                                }
                                                bus_inbound.send(AppEvent::PresenceDisconnected);
                                            }
                                        }
                                        Some("make_active") => {
                                            // Request to become the active voice owner
                                            let mut slot = active_presence_inbound
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner());
                                            let previous_active = slot
                                                .as_ref()
                                                .map(|active| active.connection_id.clone());
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_diagnostic(
                                                        "make_active_received_gateway",
                                                        &format!(
                                                            "request from connection={} previous_active={}",
                                                            connection_id_inbound,
                                                            previous_active.as_deref().unwrap_or("none"),
                                                        ),
                                                    );
                                                }
                                            }

                                            // Tell old active to disconnect voice
                                            if let Some(ref old) = *slot {
                                                if old.connection_id != connection_id_inbound {
                                                    let force_msg = serde_json::json!({
                                                        "t": "force_disconnect_voice",
                                                        "reason": "handover",
                                                    });
                                                    let _ =
                                                        old.direct_tx.send(force_msg.to_string());
                                                    if let Some(ref sl) = session_log_inbound {
                                                        if let Ok(mut l) = sl.lock() {
                                                            l.voice_diagnostic(
                                                                "make_active_force_disconnect_gateway",
                                                                &format!(
                                                                    "old_active={} new_active={}",
                                                                    old.connection_id, connection_id_inbound,
                                                                ),
                                                            );
                                                        }
                                                    }
                                                } else if let Some(ref sl) = session_log_inbound {
                                                    if let Ok(mut l) = sl.lock() {
                                                        l.voice_diagnostic(
                                                            "make_active_noop_gateway",
                                                            &format!(
                                                                "request from already-active connection={}",
                                                                connection_id_inbound,
                                                            ),
                                                        );
                                                    }
                                                }
                                            }

                                            // Install this connection as new active
                                            *slot = Some(ActivePresence {
                                                connection_id: connection_id_inbound.clone(),
                                                direct_tx: direct_tx_inbound.clone(),
                                            });
                                            drop(slot);

                                            is_active = true;
                                            is_presence_connected = true;
                                            voice_debug_inbound
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .connected = true;

                                            // Build handover context from latest checkpoint
                                            let handover_context = query_ctx_inbound
                                                .as_ref()
                                                .and_then(|ctx| ctx.presence_session.as_ref())
                                                .and_then(|ps| {
                                                    let session = ps
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner());
                                                    session.last_checkpoint_summary()
                                                })
                                                .unwrap_or_default();

                                            // Build conversation context from recent voice transcripts
                                            let conversation_ctx =
                                                query_ctx_inbound.as_ref().and_then(|ctx| {
                                                    presence::build_conversation_context(
                                                        &ctx.log_dir,
                                                        20,
                                                    )
                                                });
                                            let has_handover_context = !handover_context.is_empty();
                                            let has_conversation_context = conversation_ctx
                                                .as_deref()
                                                .map(|s| !s.is_empty())
                                                .unwrap_or(false);

                                            // Send active_granted to this connection
                                            let granted_msg = serde_json::json!({
                                                "t": "active_granted",
                                                "is_active": true,
                                                "handover_context": handover_context,
                                                "conversation_context": conversation_ctx,
                                            });
                                            let _ = direct_tx_inbound.send(granted_msg.to_string());
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_diagnostic(
                                                        "make_active_granted_gateway",
                                                        &format!(
                                                            "connection={} handover_context={} conversation_context={}",
                                                            connection_id_inbound,
                                                            if has_handover_context { "yes" } else { "no" },
                                                            if has_conversation_context { "yes" } else { "no" },
                                                        ),
                                                    );
                                                }
                                            }

                                            // Emit PresenceConnected for the new active browser
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.presence_connected(
                                                        Some(&live_provider),
                                                        Some(&live_model),
                                                    );
                                                }
                                            }
                                            bus_inbound.send(AppEvent::PresenceConnected {
                                                server_session_id: None,
                                                last_event_seq: 0,
                                                live_provider: Some(live_provider.clone()),
                                                live_model: Some(live_model.clone()),
                                            });
                                        }
                                        Some("voice_log") => {
                                            let text =
                                                json["text"].as_str().unwrap_or("").to_string();
                                            let seq = json["seq"].as_u64().unwrap_or(0);
                                            let tool_context = json
                                                .get("tool_context")
                                                .and_then(|v| v.as_str())
                                                .map(String::from);
                                            {
                                                let mut vd = voice_debug_inbound
                                                    .lock()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                vd.voice_log_count += 1;
                                                vd.last_voice_log = text.clone();
                                            }
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_log(
                                                        &text,
                                                        seq,
                                                        tool_context.as_deref(),
                                                    );
                                                }
                                            }
                                            bus_inbound.send(AppEvent::VoiceLog {
                                                text,
                                                seq,
                                                tool_context,
                                            });
                                        }
                                        Some("live_usage_update") => {
                                            bus_inbound.send(AppEvent::LiveUsageUpdate {
                                                provider: json["provider"]
                                                    .as_str()
                                                    .unwrap_or("")
                                                    .to_string(),
                                                model: json["model"]
                                                    .as_str()
                                                    .unwrap_or("")
                                                    .to_string(),
                                                input_tokens: json["input_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                output_tokens: json["output_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                cached_tokens: json["cached_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                total_tokens: json["total_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                thinking_tokens: json["thinking_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                input_text_tokens: json["input_text_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                input_audio_tokens: json["input_audio_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                input_image_tokens: json["input_image_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                cached_text_tokens: json["cached_text_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                cached_audio_tokens: json["cached_audio_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                cached_image_tokens: json["cached_image_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                output_text_tokens: json["output_text_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                                output_audio_tokens: json["output_audio_tokens"]
                                                    .as_u64()
                                                    .unwrap_or(0),
                                            });
                                        }
                                        Some("presence_checkpoint") => {
                                            let summary =
                                                json["summary"].as_str().unwrap_or("").to_string();
                                            let last_event_seq = json
                                                .get("last_event_seq")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0);

                                            // Record checkpoint and send ack
                                            if let Some(ref ctx) = query_ctx_inbound {
                                                if let Some(ref ps) = ctx.presence_session {
                                                    let checkpoint =
                                                        presence_core::PresenceCheckpoint {
                                                            summary: summary.clone(),
                                                            last_event_seq,
                                                        };
                                                    let ack = ps
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .record_checkpoint(checkpoint);
                                                    let ack_msg = serde_json::json!({
                                                        "t": "presence_checkpoint_ack",
                                                        "seq": ack.seq,
                                                    });
                                                    let _ =
                                                        direct_tx_inbound.send(ack_msg.to_string());
                                                }
                                            }

                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.presence_checkpoint(&summary, last_event_seq);
                                                }
                                            }
                                            bus_inbound.send(
                                                AppEvent::PresenceCheckpointReceived {
                                                    summary,
                                                    last_event_seq,
                                                },
                                            );
                                        }
                                        Some("voice_diagnostic") => {
                                            let kind = json["kind"]
                                                .as_str()
                                                .unwrap_or("unknown")
                                                .to_string();
                                            let detail =
                                                json["detail"].as_str().unwrap_or("").to_string();
                                            if let Some(ref sl) = session_log_inbound {
                                                if let Ok(mut l) = sl.lock() {
                                                    l.voice_diagnostic(&kind, &detail);
                                                }
                                            }
                                            bus_inbound
                                                .send(AppEvent::VoiceDiagnostic { kind, detail });
                                        }
                                        Some("user_audio") => {
                                            // Browser sends base64-encoded PCM16 audio for server-side transcription.
                                            if let Some(ref transcriber) = transcriber_inbound {
                                                if let Some(data_b64) = json["data"].as_str() {
                                                    use base64::Engine;
                                                    if let Ok(pcm_bytes) =
                                                        base64::engine::general_purpose::STANDARD
                                                            .decode(data_b64)
                                                    {
                                                        audio_buf.extend_from_slice(&pcm_bytes);
                                                        // Drain at ~3s of audio (16kHz * 2 bytes/sample * 1 channel * 3s = 96000)
                                                        let threshold =
                                                            (audio_sample_rate as usize) * 2 * 3;
                                                        if audio_buf.len() >= threshold {
                                                            // Skip silent buffers — compute RMS energy of PCM16 samples.
                                                            // Whisper hallucinates on silence (outputs "you", ".", etc).
                                                            let rms = {
                                                                let samples = audio_buf
                                                                    .chunks_exact(2)
                                                                    .map(|c| {
                                                                        i16::from_le_bytes([
                                                                            c[0], c[1],
                                                                        ])
                                                                            as f64
                                                                    });
                                                                let sum_sq: f64 =
                                                                    samples.map(|s| s * s).sum();
                                                                let n = audio_buf.len() / 2;
                                                                if n > 0 {
                                                                    (sum_sq / n as f64).sqrt()
                                                                } else {
                                                                    0.0
                                                                }
                                                            };
                                                            if rms < 1000.0 {
                                                                // Below speech threshold — skip transcription.
                                                                // Whisper hallucinates aggressively on low-energy
                                                                // audio ("Thank you", "Bye bye", etc).
                                                                audio_buf.clear();
                                                                continue;
                                                            }
                                                            let wav =
                                                                crate::transcription::encode_wav(
                                                                    &audio_buf,
                                                                    audio_sample_rate,
                                                                    1,
                                                                );
                                                            audio_buf.clear();
                                                            audio_seq += 1;
                                                            let seq = audio_seq;
                                                            let t = transcriber.clone();
                                                            let bus_tx = bus_inbound.clone();
                                                            let session_log_tx =
                                                                session_log_inbound.clone();
                                                            tokio::spawn(async move {
                                                                match t.transcribe(&wav).await {
                                                                    Ok(segment) => {
                                                                        let text = segment
                                                                            .text
                                                                            .trim()
                                                                            .to_string();
                                                                        if !text.is_empty() {
                                                                            if let Some(ref sl) =
                                                                                session_log_tx
                                                                            {
                                                                                if let Ok(mut l) =
                                                                                    sl.lock()
                                                                                {
                                                                                    l.user_transcript(&text, seq);
                                                                                }
                                                                            }
                                                                            bus_tx.send(AppEvent::UserTranscript { text, seq });
                                                                        }
                                                                    }
                                                                    Err(e) => {
                                                                        eprintln!("transcription failed: {}", e);
                                                                    }
                                                                }
                                                            });
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Some("video_frame") => {
                                            // Browser sends a video frame for HQ archival in the frame registry.
                                            let frame_id =
                                                json["frame_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"]
                                                .as_str()
                                                .unwrap_or("cam0")
                                                .to_string();
                                            if let Some(data_b64) = json["data"].as_str() {
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) =
                                                    base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                {
                                                    // Register in frame registry
                                                    if let Some(ref registry) =
                                                        frame_registry_inbound
                                                    {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: stream.clone(),
                                                            timestamp: chrono::Utc::now()
                                                                .to_rfc3339(),
                                                            sent_to_live: true,
                                                            live_resolution: Some(
                                                                "768x768".to_string(),
                                                            ),
                                                            hq_resolution: None,
                                                            note: None,
                                                        };
                                                        let mut reg = registry.write().await;
                                                        if let Err(e) =
                                                            reg.register(meta, &jpeg_bytes)
                                                        {
                                                            eprintln!(
                                                                "frame registry write failed: {}",
                                                                e
                                                            );
                                                        }
                                                    }
                                                    // Feed into recording pipeline (auto-starts on first frame)
                                                    if let Some(ref rec_reg) =
                                                        recording_registry_inbound
                                                    {
                                                        let mut rreg = rec_reg.write().await;
                                                        if rreg.is_enabled() {
                                                            if !rreg.is_recording(&stream)
                                                                && crate::recording::is_ffmpeg_available() {
                                                                    if let Err(e) = rreg.start_stream(&stream).await {
                                                                        eprintln!("camera recording start failed: {}", e);
                                                                    } else {
                                                                        bus_inbound.send(AppEvent::RecordingStarted {
                                                                            stream_name: stream.clone(),
                                                                        });
                                                                    }
                                                                }
                                                            let _ = rreg
                                                                .feed_frame(&stream, &jpeg_bytes)
                                                                .await;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Some("annotation_attach") => {
                                            // User clicked "Attach" on an annotation/frame: register
                                            // the JPEG in the frame registry but DO NOT inject into
                                            // the agent context. The browser tracks this frame ID as
                                            // a pending attachment and submits it with the next task.
                                            //
                                            // Works regardless of presence/agent state — attachments
                                            // are independent of any running task.
                                            let frame_id =
                                                json["frame_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"]
                                                .as_str()
                                                .unwrap_or("annotation")
                                                .to_string();
                                            let note =
                                                json["note"].as_str().unwrap_or("").to_string();
                                            if let Some(data_b64) = json["data"].as_str() {
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) =
                                                    base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                {
                                                    let mut saved_path = String::new();
                                                    let mut registered = false;
                                                    if let Some(ref registry) =
                                                        frame_registry_inbound
                                                    {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: stream.clone(),
                                                            timestamp: chrono::Utc::now()
                                                                .to_rfc3339(),
                                                            sent_to_live: false,
                                                            live_resolution: None,
                                                            hq_resolution: None,
                                                            note: if note.is_empty() {
                                                                None
                                                            } else {
                                                                Some(note.clone())
                                                            },
                                                        };
                                                        let mut reg = registry.write().await;
                                                        match reg.register(meta, &jpeg_bytes) {
                                                            Ok(path) => {
                                                                saved_path = path.display().to_string();
                                                                registered = true;
                                                            }
                                                            Err(e) => eprintln!("annotation_attach frame registry write failed: {}", e),
                                                        }
                                                    }
                                                    let _ = direct_tx_inbound.send(
                                                        serde_json::json!({
                                                            "t": "annotation_attached",
                                                            "frame_id": frame_id,
                                                            "stream": stream,
                                                            "path": saved_path,
                                                            "note": note,
                                                            "ok": registered,
                                                        })
                                                        .to_string(),
                                                    );
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[annotation] {} attached (pending)",
                                                            frame_id
                                                        ),
                                                        level: Some(LogLevel::Info),
                                                        turn: None,
                                                    });
                                                }
                                            }
                                        }
                                        Some("annotation_submit") => {
                                            // User drew annotations on a frame and submitted it with a note.
                                            let frame_id =
                                                json["frame_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"]
                                                .as_str()
                                                .unwrap_or("annotation")
                                                .to_string();
                                            let note =
                                                json["note"].as_str().unwrap_or("").to_string();
                                            let inject = json["inject"].as_bool().unwrap_or(false);
                                            if let Some(data_b64) = json["data"].as_str() {
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) =
                                                    base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                {
                                                    // Register in frame registry
                                                    let mut saved_path = String::new();
                                                    if let Some(ref registry) =
                                                        frame_registry_inbound
                                                    {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: stream.clone(),
                                                            timestamp: chrono::Utc::now()
                                                                .to_rfc3339(),
                                                            sent_to_live: false,
                                                            live_resolution: None,
                                                            hq_resolution: None,
                                                            note: if note.is_empty() {
                                                                None
                                                            } else {
                                                                Some(note.clone())
                                                            },
                                                        };
                                                        let mut reg = registry.write().await;
                                                        match reg.register(meta, &jpeg_bytes) {
                                                            Ok(path) => saved_path = path.display().to_string(),
                                                            Err(e) => eprintln!("annotation frame registry write failed: {}", e),
                                                        }
                                                    }
                                                    // Optionally inject into agent conversation
                                                    let mut injected_to_queue = false;
                                                    if inject {
                                                        if let Some(ref ctx) = query_ctx_inbound {
                                                            if let Some(ref ciq) =
                                                                ctx.context_injection
                                                            {
                                                                if let Ok(mut q) = ciq.lock() {
                                                                    let label = if note.is_empty() {
                                                                        "[User Annotation] User highlighted something on the screen.".to_string()
                                                                    } else {
                                                                        format!(
                                                                            "[User Annotation] {}",
                                                                            note
                                                                        )
                                                                    };
                                                                    q.push(crate::event::ContextInjection {
                                                                        text: label,
                                                                        images: vec![crate::conversation::ImageData {
                                                                            media_type: "image/jpeg".to_string(),
                                                                            data: data_b64.to_string(),
                                                                        }],
                                                                        source: crate::event::InjectionSource::User,
                                                                        target_session_id: None,
                                                                        steer_id: None,
                                                                    });
                                                                    injected_to_queue = true;
                                                                }
                                                            }
                                                        }
                                                    }
                                                    // Send path back to browser. Report whether the injection
                                                    // actually landed in the queue (not just whether the user
                                                    // pressed Send), so the UI doesn't lie when no presence is
                                                    // running.
                                                    let _ = direct_tx_inbound.send(
                                                        serde_json::json!({
                                                            "t": "annotation_saved",
                                                            "frame_id": frame_id,
                                                            "path": saved_path,
                                                            "injected": injected_to_queue,
                                                        })
                                                        .to_string(),
                                                    );
                                                    let status_label = if inject {
                                                        if injected_to_queue {
                                                            " (sent to agent)"
                                                        } else {
                                                            " (saved — no agent connected)"
                                                        }
                                                    } else {
                                                        ""
                                                    };
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[annotation] {} on {}{}",
                                                            frame_id, stream, status_label
                                                        ),
                                                        level: Some(LogLevel::Info),
                                                        turn: None,
                                                    });
                                                }
                                            }
                                        }
                                        Some("clip_start") => {
                                            let clip_id =
                                                json["clip_id"].as_str().unwrap_or("").to_string();
                                            let stream = json["stream"]
                                                .as_str()
                                                .unwrap_or("recording")
                                                .to_string();
                                            let note =
                                                json["note"].as_str().unwrap_or("").to_string();
                                            let inject = json["inject"].as_bool().unwrap_or(false);
                                            let in_secs = json["in_secs"].as_f64().unwrap_or(0.0);
                                            let out_secs = json["out_secs"].as_f64().unwrap_or(0.0);
                                            let fps = json["fps"].as_u64().unwrap_or(2) as u32;
                                            let total =
                                                json["total_frames"].as_u64().unwrap_or(0) as usize;
                                            clip_accumulators.insert(
                                                clip_id.clone(),
                                                ClipAccumulator {
                                                    stream,
                                                    note,
                                                    inject,
                                                    in_secs,
                                                    out_secs,
                                                    fps,
                                                    expected: total,
                                                    frames: Vec::with_capacity(total),
                                                },
                                            );
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!(
                                                    "[clip] started {} ({} frames, {}fps)",
                                                    clip_id, total, fps
                                                ),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });
                                        }
                                        Some("clip_frame") => {
                                            let clip_id =
                                                json["clip_id"].as_str().unwrap_or("").to_string();
                                            let frame_id =
                                                json["frame_id"].as_str().unwrap_or("").to_string();
                                            if let Some(data_b64) = json["data"].as_str() {
                                                // Register frame in frame registry
                                                use base64::Engine;
                                                if let Ok(jpeg_bytes) =
                                                    base64::engine::general_purpose::STANDARD
                                                        .decode(data_b64)
                                                {
                                                    if let Some(ref registry) =
                                                        frame_registry_inbound
                                                    {
                                                        let meta = presence_core::FrameMeta {
                                                            frame_id: frame_id.clone(),
                                                            stream: format!("clip:{}", clip_id),
                                                            timestamp: chrono::Utc::now()
                                                                .to_rfc3339(),
                                                            sent_to_live: false,
                                                            live_resolution: None,
                                                            hq_resolution: None,
                                                            note: None,
                                                        };
                                                        let mut reg = registry.write().await;
                                                        if let Err(e) =
                                                            reg.register(meta, &jpeg_bytes)
                                                        {
                                                            eprintln!("clip frame registry write failed: {}", e);
                                                        }
                                                    }
                                                }
                                                // Accumulate for context injection
                                                if let Some(acc) =
                                                    clip_accumulators.get_mut(&clip_id)
                                                {
                                                    acc.frames
                                                        .push((frame_id, data_b64.to_string()));
                                                }
                                            }
                                        }
                                        Some("clip_end") => {
                                            let clip_id =
                                                json["clip_id"].as_str().unwrap_or("").to_string();
                                            let mut injected = false;

                                            if let Some(acc) = clip_accumulators.remove(&clip_id) {
                                                let frames_registered = acc.frames.len();
                                                if acc.inject {
                                                    if let Some(ref ctx) = query_ctx_inbound {
                                                        if let Some(ref ciq) = ctx.context_injection
                                                        {
                                                            if let Ok(mut q) = ciq.lock() {
                                                                let label = if acc.note.is_empty() {
                                                                    format!(
                                                                        "[Video Clip] {} {:.1}s-{:.1}s ({} frames, {}fps)",
                                                                        acc.stream,
                                                                        acc.in_secs,
                                                                        acc.out_secs,
                                                                        frames_registered, acc.fps,
                                                                    )
                                                                } else {
                                                                    format!(
                                                                        "[Video Clip] {} {:.1}s-{:.1}s ({} frames, {}fps). {}",
                                                                        acc.stream,
                                                                        acc.in_secs,
                                                                        acc.out_secs,
                                                                        frames_registered, acc.fps, acc.note,
                                                                    )
                                                                };
                                                                let images: Vec<crate::conversation::ImageData> = acc.frames.iter().map(|(_, b64)| {
                                                                    crate::conversation::ImageData {
                                                                        media_type: "image/jpeg".to_string(),
                                                                        data: b64.clone(),
                                                                    }
                                                                }).collect();
                                                                q.push(crate::event::ContextInjection {
                                                                    text: label,
                                                                    images,
                                                                    source: crate::event::InjectionSource::User,
                                                                    target_session_id: None,
                                                                    steer_id: None,
                                                                });
                                                                injected = true;
                                                            }
                                                        }
                                                    }
                                                }

                                                let _ = direct_tx_inbound.send(
                                                    serde_json::json!({
                                                        "t": "clip_saved",
                                                        "clip_id": clip_id,
                                                        "frames_registered": frames_registered,
                                                        "injected": injected,
                                                    })
                                                    .to_string(),
                                                );

                                                bus_inbound.send(AppEvent::PresenceLog {
                                                    message: format!(
                                                        "[clip] {} — {} frames{}",
                                                        clip_id,
                                                        frames_registered,
                                                        if injected {
                                                            " (sent to agent)"
                                                        } else {
                                                            " (saved)"
                                                        }
                                                    ),
                                                    level: Some(LogLevel::Info),
                                                    turn: None,
                                                });
                                            }
                                        }
                                        Some("tool_request") => {
                                            let req_id =
                                                json["id"].as_str().unwrap_or("").to_string();
                                            let tool =
                                                json["tool"].as_str().unwrap_or("").to_string();
                                            let args = json.get("args").cloned().unwrap_or(
                                                serde_json::Value::Object(Default::default()),
                                            );

                                            // Log the incoming tool request at Debug level
                                            let args_preview = {
                                                let s = serde_json::to_string(&args)
                                                    .unwrap_or_default();
                                                preview_text(&s, 200)
                                            };
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!(
                                                    "[tool_request] {}({})",
                                                    tool, args_preview
                                                ),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            // Dispatch through presence-core (single canonical layer)
                                            let state = query_ctx_inbound
                                                .as_ref()
                                                .map(|ctx| {
                                                    ctx.agent_state
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner())
                                                        .clone()
                                                })
                                                .unwrap_or_default();
                                            let action =
                                                presence::dispatch_tool_call(&tool, &args, &state);

                                            // SubmitTask: send directly to task_tx (bypasses TUI)
                                            let query_result =
                                                if let presence::PresenceAction::SubmitTask(
                                                    envelope,
                                                ) = action
                                                {
                                                    let msg = format!(
                                                        "Task submitted: {}",
                                                        envelope.task
                                                    );
                                                    if let Some(ref tx) = task_tx_inbound {
                                                        let _ = tx.send(envelope).await;
                                                    } else {
                                                        // Fallback: dispatch via EventBus if no task_tx
                                                        let ctrl_action =
                                                            presence::PresenceAction::SubmitTask(
                                                                envelope,
                                                            );
                                                        if let Some((ctrl, _)) =
                                                            presence::action_to_control_msg(
                                                                &ctrl_action,
                                                            )
                                                        {
                                                            bus_inbound.send(
                                                                AppEvent::ControlCommand(ctrl),
                                                            );
                                                        }
                                                    }
                                                    presence::ToolQueryResult::text(msg)
                                                } else if let Some((ctrl, msg)) =
                                                    presence::action_to_control_msg(&action)
                                                {
                                                    // Other action tools: dispatch via EventBus
                                                    bus_inbound
                                                        .send(AppEvent::ControlCommand(ctrl));
                                                    presence::ToolQueryResult::text(msg)
                                                } else {
                                                    match action {
                                                        presence::PresenceAction::TextResult(
                                                            text,
                                                        ) => presence::ToolQueryResult::text(text),
                                                        presence::PresenceAction::NeedsIO {
                                                            tool_name,
                                                            args: io_args,
                                                        } => {
                                                            if let Some(ref ctx) = query_ctx_inbound
                                                            {
                                                                if let Some(result) =
                                                                    presence::handle_tool_query(
                                                                        &ctx.agent_state,
                                                                        &ctx.project_root,
                                                                        &ctx.log_dir,
                                                                        &ctx.knowledge_path,
                                                                        &tool_name,
                                                                        &io_args,
                                                                        frame_registry_inbound
                                                                            .as_ref(),
                                                                        ctx.context_injection
                                                                            .as_ref(),
                                                                    )
                                                                    .await
                                                                {
                                                                    result
                                                                } else {
                                                                    presence::ToolQueryResult::text(
                                                                        format!(
                                                                            "Unknown tool: {}",
                                                                            tool
                                                                        ),
                                                                    )
                                                                }
                                                            } else {
                                                                presence::ToolQueryResult::text("Presence query context not available".to_string())
                                                            }
                                                        }
                                                        _ => unreachable!(),
                                                    }
                                                };

                                            // Log the tool response at Debug level
                                            let result_preview =
                                                preview_text(&query_result.text, 200);
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!(
                                                    "[tool_response] {} → {}",
                                                    tool, result_preview
                                                ),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let mut response = serde_json::json!({
                                                "t": "tool_response",
                                                "id": req_id,
                                                "result": query_result.text,
                                            });
                                            if !query_result.images.is_empty() {
                                                let img_array: Vec<serde_json::Value> =
                                                    query_result
                                                        .images
                                                        .iter()
                                                        .map(|img| {
                                                            serde_json::json!({
                                                                "mime_type": img.media_type,
                                                                "data": img.data,
                                                            })
                                                        })
                                                        .collect();
                                                response["images"] =
                                                    serde_json::Value::Array(img_array);
                                            }
                                            let _ = direct_tx_inbound.send(response.to_string());
                                        }
                                        Some("async_query") => {
                                            // Async query from browser — same dispatch as tool_request
                                            // but result goes back as async_query_result (injected into
                                            // voice session as text, not as a tool response).
                                            let req_id =
                                                json["id"].as_str().unwrap_or("").to_string();
                                            let tool =
                                                json["tool"].as_str().unwrap_or("").to_string();
                                            let args = json.get("args").cloned().unwrap_or(
                                                serde_json::Value::Object(Default::default()),
                                            );

                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!("[async_query] {}", tool),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let query_result = if let Some(ref ctx) =
                                                query_ctx_inbound
                                            {
                                                if let Some(result) = presence::handle_tool_query(
                                                    &ctx.agent_state,
                                                    &ctx.project_root,
                                                    &ctx.log_dir,
                                                    &ctx.knowledge_path,
                                                    &tool,
                                                    &args,
                                                    frame_registry_inbound.as_ref(),
                                                    ctx.context_injection.as_ref(),
                                                )
                                                .await
                                                {
                                                    result
                                                } else {
                                                    presence::ToolQueryResult::text(format!(
                                                        "Unknown query tool: {}",
                                                        tool
                                                    ))
                                                }
                                            } else {
                                                presence::ToolQueryResult::text(
                                                    "Presence query context not available"
                                                        .to_string(),
                                                )
                                            };

                                            let result_preview =
                                                preview_text(&query_result.text, 200);
                                            bus_inbound.send(AppEvent::PresenceLog {
                                                message: format!(
                                                    "[async_query_result] {} → {}",
                                                    tool, result_preview
                                                ),
                                                level: Some(LogLevel::Debug),
                                                turn: None,
                                            });

                                            let mut response = serde_json::json!({
                                                "t": "async_query_result",
                                                "id": req_id,
                                                "tool": tool,
                                                "result": query_result.text,
                                            });
                                            if !query_result.images.is_empty() {
                                                let img_array: Vec<serde_json::Value> =
                                                    query_result
                                                        .images
                                                        .iter()
                                                        .map(|img| {
                                                            serde_json::json!({
                                                                "mime_type": img.media_type,
                                                                "data": img.data,
                                                            })
                                                        })
                                                        .collect();
                                                response["images"] =
                                                    serde_json::Value::Array(img_array);
                                            }
                                            let _ = direct_tx_inbound.send(response.to_string());
                                        }
                                        Some("display_offer") => {
                                            // WebRTC SDP offer from browser for a display session
                                            let display_id =
                                                json["display_id"].as_u64().unwrap_or(0) as u32;
                                            let sdp =
                                                json["sdp"].as_str().unwrap_or("").to_string();

                                            // Clone the Arc<DisplaySession> out of the read
                                            // lock before calling handle_offer. Holding the
                                            // guard across the await chokes any writer
                                            // (notably deactivate_user_display's
                                            // registry.write()) for as long as this block
                                            // runs. The Arc keeps the session alive
                                            // independently of the lock.
                                            let session: Option<
                                                Arc<crate::display::DisplaySession>,
                                            > = match session_registry_inbound.as_ref() {
                                                Some(sr) => sr.read().await.get(display_id),
                                                None => None,
                                            };
                                            if let Some(session) = session {
                                                let (ice_tx, mut ice_rx) = mpsc::channel::<(
                                                    crate::display::PeerId,
                                                    String,
                                                )>(
                                                    64
                                                );
                                                // Combine the Host-header IP with the
                                                // port we want to advertise (HTTP port
                                                // for Phase 3 multiplex, or standalone
                                                // Phase 2 port) to form the single TCP
                                                // candidate the peer will emit. None
                                                // if either piece is missing (typically
                                                // because the browser connected via
                                                // hostname).
                                                let tcp_advertised_addr: Option<
                                                    std::net::SocketAddr,
                                                > = match (browser_host_ip, tcp_advertised_port) {
                                                    (Some(ip), Some(port)) => {
                                                        Some(std::net::SocketAddr::new(ip, port))
                                                    }
                                                    _ => None,
                                                };
                                                // Phase 5a.1 input authority gate.  The closure
                                                // returns true when this connection is the
                                                // authority holder OR when the display has no
                                                // holder (unclaimed = pre-phase-5 default).
                                                // `display/mod.rs` only sees this boolean; it
                                                // never learns about DisplayInputHolder, the
                                                // map, or connection IDs.  See
                                                // [`build_local_ws_input_authorizer`] for the
                                                // closure semantics + tests.
                                                let input_authorized =
                                                    build_local_ws_input_authorizer(
                                                        display_id,
                                                        connection_id_inbound.clone(),
                                                        Arc::clone(
                                                            &display_input_authority_inbound,
                                                        ),
                                                    );
                                                // F-1.3b2 transport plumbing: local DisplaySlot's
                                                // browser doesn't create the
                                                // `display_input_authority` data channel
                                                // (5a/5c uses the WS path), so the handler is
                                                // never invoked here. The no-op keeps the
                                                // transport-layer signature uniform across
                                                // both peer kinds; the real federated handler
                                                // is wired by the federated path's caller in
                                                // a later slice.
                                                let authority_handler =
                                                    crate::display::webrtc::noop_authority_handler(
                                                    );
                                                match session
                                                    .handle_offer(
                                                        peer_id,
                                                        &sdp,
                                                        &ice_config,
                                                        Some(Arc::clone(&tcp_peer_registry)),
                                                        tcp_advertised_addr,
                                                        ice_tx,
                                                        input_authorized,
                                                        authority_handler,
                                                    )
                                                    .await
                                                {
                                                    Ok(answer_sdp) => {
                                                        peer_display_ids.push(display_id);
                                                        let answer = serde_json::json!({
                                                            "t": "display_answer",
                                                            "display_id": display_id,
                                                            "sdp": answer_sdp,
                                                        });
                                                        let _ = direct_tx_inbound
                                                            .send(answer.to_string());

                                                        // Forward server ICE candidates to browser
                                                        let ice_direct_tx =
                                                            direct_tx_inbound.clone();
                                                        tokio::spawn(async move {
                                                            while let Some((_pid, candidate_json)) =
                                                                ice_rx.recv().await
                                                            {
                                                                let msg = serde_json::json!({
                                                                    "t": "display_ice",
                                                                    "display_id": display_id,
                                                                    "candidate": serde_json::from_str::<serde_json::Value>(&candidate_json).unwrap_or_default(),
                                                                });
                                                                if ice_direct_tx
                                                                    .send(msg.to_string())
                                                                    .is_err()
                                                                {
                                                                    break;
                                                                }
                                                            }
                                                        });
                                                    }
                                                    Err(e) => {
                                                        eprintln!("[web_gateway] WebRTC offer failed for display {}: {}", display_id, e);
                                                    }
                                                }
                                            }
                                        }
                                        Some("display_ice") => {
                                            // Trickle ICE candidate from browser. Spawn the
                                            // handling off the ws reader loop because
                                            // `add_ice_candidate` resolves mDNS hostnames
                                            // (browsers obfuscate host candidates as
                                            // `<uuid>.local`). On hosts without an mDNS
                                            // responder — every headless VM without Avahi,
                                            // which is the common deployment — each lookup
                                            // blocks on the system resolver's full timeout
                                            // (5-20s). With multiple candidates and ICE
                                            // retries, that piles 20-30s of blocking inside
                                            // this reader, stalling every other ws frame
                                            // behind it including grant/revoke — the root
                                            // cause of the "second ON takes 20+s" bug.
                                            //
                                            // Spawning decouples candidate processing from
                                            // frame intake. Failed lookups still log the
                                            // same "mdns resolve failed" diagnostic; losing
                                            // a candidate is survivable (ICE has others),
                                            // whereas blocking the reader is not.
                                            let display_id =
                                                json["display_id"].as_u64().unwrap_or(0) as u32;
                                            let candidate = json["candidate"].to_string();
                                            let sr_clone = session_registry_inbound.clone();
                                            let pid = peer_id;
                                            tokio::spawn(async move {
                                                // Clone the session Arc out of the read
                                                // lock first. The previous spread-across-
                                                // `if let` form held the guard across
                                                // add_ice_candidate's mDNS resolution,
                                                // which on hosts without Avahi blocks for
                                                // 5-20s per candidate — starving any
                                                // concurrent writer (notably
                                                // deactivate_user_display's
                                                // registry.write()). Dropping the guard
                                                // first lets deactivate proceed
                                                // immediately; the session Arc keeps the
                                                // target alive while mDNS resolves.
                                                let session: Option<
                                                    Arc<crate::display::DisplaySession>,
                                                > = match sr_clone.as_ref() {
                                                    Some(sr) => sr.read().await.get(display_id),
                                                    None => None,
                                                };
                                                if let Some(session) = session {
                                                    if let Err(e) = session
                                                        .add_ice_candidate(pid, &candidate)
                                                        .await
                                                    {
                                                        eprintln!("[web_gateway] ICE candidate failed for display {}: {}", display_id, e);
                                                    }
                                                }
                                            });
                                        }
                                        Some("dashboard_control_offer") => {
                                            let sdp =
                                                json["sdp"].as_str().unwrap_or("").to_string();
                                            let client_nonce = json["client_nonce"]
                                                .as_str()
                                                .map(str::trim)
                                                .filter(|nonce| !nonce.is_empty())
                                                .map(str::to_string);
                                            if sdp.is_empty() {
                                                let msg = serde_json::json!({
                                                    "t": "dashboard_control_error",
                                                    "error": "missing sdp",
                                                });
                                                let _ = direct_tx_inbound.send(msg.to_string());
                                                continue;
                                            }
                                            match dashboard_control_inbound
                                                .answer_offer_with_grant(
                                                    sdp,
                                                    None,
                                                    client_nonce,
                                                    dashboard_control_grant_inbound.clone(),
                                                )
                                                .await
                                            {
                                                Ok(answer) => {
                                                    dashboard_control_session_ids
                                                        .push(answer.session_id.clone());
                                                    let msg = serde_json::json!({
                                                        "t": "dashboard_control_answer",
                                                        "session_id": answer.session_id,
                                                        "sdp": answer.sdp,
                                                        "binding": answer.binding,
                                                    });
                                                    let _ = direct_tx_inbound.send(msg.to_string());
                                                }
                                                Err(e) => {
                                                    let msg = serde_json::json!({
                                                        "t": "dashboard_control_error",
                                                        "error": e,
                                                    });
                                                    let _ = direct_tx_inbound.send(msg.to_string());
                                                }
                                            }
                                        }
                                        Some("dashboard_control_ice") => {
                                            let session_id = json["session_id"]
                                                .as_str()
                                                .unwrap_or("")
                                                .to_string();
                                            let candidate = json
                                                .get("candidate")
                                                .cloned()
                                                .unwrap_or_else(|| serde_json::json!({}));
                                            if session_id.is_empty() {
                                                continue;
                                            }
                                            let registry = Arc::clone(&dashboard_control_inbound);
                                            tokio::spawn(async move {
                                                if let Err(e) = registry
                                                    .add_ice_candidate(&session_id, &candidate)
                                                    .await
                                                {
                                                    eprintln!(
                                                        "[dashboard/control] add ICE failed: {e}"
                                                    );
                                                }
                                            });
                                        }
                                        Some("dashboard_control_close") => {
                                            let session_id = json["session_id"]
                                                .as_str()
                                                .unwrap_or("")
                                                .to_string();
                                            if !session_id.is_empty() {
                                                dashboard_control_inbound.close(&session_id).await;
                                                dashboard_control_session_ids
                                                    .retain(|s| s != &session_id);
                                            }
                                        }
                                        Some("terminal_open") => {
                                            // {"t":"terminal_open","host_id":"local","terminal_id":"shell-0","cols":80,"rows":24}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                                            let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                                            let key = crate::terminal::TerminalKey {
                                                host_id: host_id.clone(),
                                                terminal_id: terminal_id.clone(),
                                            };

                                            // Attach needs only the terminal.view
                                            // floor already enforced; creating a
                                            // shell needs shell.spawn, decided at
                                            // frame time so expiry mid-connection
                                            // is honored. A grant-level fs scope
                                            // makes the new shell a sandboxed one.
                                            let spawn_policy = crate::terminal::ShellSpawnPolicy {
                                                may_spawn: dashboard_control_grant_inbound
                                                    .access_decision(
                                                        crate::peer::access_policy::PeerOperation::ShellSpawn,
                                                    )
                                                    .allowed,
                                                shared: json["shared"]
                                                    .as_bool()
                                                    .unwrap_or(false),
                                                scope: dashboard_control_grant_inbound
                                                    .filesystem()
                                                    .cloned(),
                                            };
                                            match terminal_registry_inbound
                                                .open_or_attach(
                                                    key.clone(),
                                                    cols,
                                                    rows,
                                                    &ws_terminal_actor,
                                                    spawn_policy,
                                                )
                                                .await
                                            {
                                                Ok((session, _created)) => {
                                                    // Spawn a forwarder task that drains the session's
                                                    // per-listener channel and sends base64-encoded
                                                    // output to this WS connection.
                                                    let (tx, mut rx) =
                                                        tokio::sync::mpsc::unbounded_channel();
                                                    session.attach(tx);

                                                    let forwarder_tx = direct_tx_inbound.clone();
                                                    let fwd_host = host_id.clone();
                                                    let fwd_term = terminal_id.clone();
                                                    tokio::spawn(async move {
                                                        use base64::Engine as _;
                                                        while let Some(event) = rx.recv().await {
                                                            let msg = match event {
                                                                crate::terminal::TerminalEvent::Output(bytes) => {
                                                                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                                                    serde_json::json!({
                                                                        "t": "terminal_output",
                                                                        "host_id": fwd_host,
                                                                        "terminal_id": fwd_term,
                                                                        "data": b64,
                                                                    })
                                                                }
                                                                crate::terminal::TerminalEvent::Exited { status } => {
                                                                    serde_json::json!({
                                                                        "t": "terminal_exited",
                                                                        "host_id": fwd_host,
                                                                        "terminal_id": fwd_term,
                                                                        "status": status,
                                                                    })
                                                                }
                                                            };
                                                            if forwarder_tx
                                                                .send(msg.to_string())
                                                                .is_err()
                                                            {
                                                                break;
                                                            }
                                                        }
                                                    });

                                                    let ack = serde_json::json!({
                                                        "t": "terminal_opened",
                                                        "host_id": host_id,
                                                        "terminal_id": terminal_id,
                                                        "shared": session.shared(),
                                                        "can_share": session
                                                            .managed_by(&ws_terminal_actor),
                                                    });
                                                    let _ = direct_tx_inbound.send(ack.to_string());
                                                }
                                                Err(e) => {
                                                    let err = serde_json::json!({
                                                        "t": "terminal_error",
                                                        "host_id": host_id,
                                                        "terminal_id": terminal_id,
                                                        "error": e.to_string(),
                                                    });
                                                    let _ = direct_tx_inbound.send(err.to_string());
                                                }
                                            }
                                        }
                                        Some("terminal_input") => {
                                            // {"t":"terminal_input","host_id":"local","terminal_id":"shell-0","data":"<base64>"}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let data_b64 = json["data"].as_str().unwrap_or("");
                                            use base64::Engine as _;
                                            if let Ok(data) =
                                                base64::engine::general_purpose::STANDARD
                                                    .decode(data_b64)
                                            {
                                                let key = crate::terminal::TerminalKey {
                                                    host_id,
                                                    terminal_id,
                                                };
                                                if let Some(session) = terminal_registry_inbound
                                                    .get_visible(&key, &ws_terminal_actor)
                                                    .await
                                                {
                                                    session.write_input(&data);
                                                }
                                            }
                                        }
                                        Some("terminal_resize") => {
                                            // {"t":"terminal_resize","host_id":"local","terminal_id":"shell-0","cols":N,"rows":N}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let cols = json["cols"].as_u64().unwrap_or(80) as u16;
                                            let rows = json["rows"].as_u64().unwrap_or(24) as u16;
                                            let key = crate::terminal::TerminalKey {
                                                host_id,
                                                terminal_id,
                                            };
                                            if let Some(session) = terminal_registry_inbound
                                                .get_visible(&key, &ws_terminal_actor)
                                                .await
                                            {
                                                session.resize(cols, rows);
                                            }
                                        }
                                        Some("terminal_close") => {
                                            // {"t":"terminal_close","host_id":"local","terminal_id":"shell-0"}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let key = crate::terminal::TerminalKey {
                                                host_id,
                                                terminal_id,
                                            };
                                            terminal_registry_inbound
                                                .close_visible(&key, &ws_terminal_actor)
                                                .await;
                                        }
                                        Some("terminal_share") => {
                                            // {"t":"terminal_share","host_id":"local","terminal_id":"shell-0","shared":true}
                                            let host_id = json["host_id"]
                                                .as_str()
                                                .unwrap_or("local")
                                                .to_string();
                                            let terminal_id = json["terminal_id"]
                                                .as_str()
                                                .unwrap_or("shell-0")
                                                .to_string();
                                            let shared = json["shared"].as_bool().unwrap_or(true);
                                            let key = crate::terminal::TerminalKey {
                                                host_id: host_id.clone(),
                                                terminal_id: terminal_id.clone(),
                                            };
                                            let msg = match terminal_registry_inbound
                                                .set_shared(&key, &ws_terminal_actor, shared)
                                                .await
                                            {
                                                Some(state) => serde_json::json!({
                                                    "t": "terminal_shared",
                                                    "host_id": host_id,
                                                    "terminal_id": terminal_id,
                                                    "shared": state,
                                                }),
                                                None => serde_json::json!({
                                                    "t": "terminal_error",
                                                    "host_id": host_id,
                                                    "terminal_id": terminal_id,
                                                    "error": "not allowed: only the session owner or root can change sharing",
                                                }),
                                            };
                                            let _ = direct_tx_inbound.send(msg.to_string());
                                        }
                                        Some("display_input") => {
                                            // Input event (keyboard/mouse) for a display session.
                                            // Drop the registry read lock before the inject
                                            // (which runs xdotool/cliclick subprocesses) so a
                                            // concurrent deactivate can take the write lock
                                            // without waiting on subprocess exits.
                                            let display_id =
                                                json["display_id"].as_u64().unwrap_or(0) as u32;

                                            // Phase 5 authority gate: if someone has claimed
                                            // input authority for this display, only that
                                            // connection's input flows through. Unclaimed
                                            // (no entry in the map) = pre-phase-5 default,
                                            // every connection can input. See the
                                            // `DisplayInputHolder` doc for the full
                                            // contract.
                                            let allowed = {
                                                let authority = display_input_authority_inbound
                                                    .read()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                match authority.get(&display_id) {
                                                    Some(entry) => entry
                                                        .matches_local_ws(&connection_id_inbound),
                                                    None => true,
                                                }
                                            };
                                            if !allowed {
                                                // Silent drop — matches the "force_disconnect_voice"
                                                // convention where demoted connections don't get
                                                // per-message denial feedback; the browser already
                                                // knows it's passive from the authority_revoked
                                                // notification it received when it was demoted.
                                                continue;
                                            }

                                            if let Some(evt) = json.get("event") {
                                                if let Ok(input_event) = serde_json::from_value::<
                                                    crate::display::InputEvent,
                                                >(
                                                    evt.clone()
                                                ) {
                                                    let session: Option<
                                                        Arc<crate::display::DisplaySession>,
                                                    > = match session_registry_inbound.as_ref() {
                                                        Some(sr) => sr.read().await.get(display_id),
                                                        None => None,
                                                    };
                                                    if let Some(session) = session {
                                                        if let Err(e) =
                                                            session.inject_input(input_event).await
                                                        {
                                                            eprintln!("[web_gateway] display input injection failed: {}", e);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Some("set_diagnostics_visual_marker") => {
                                            // **Phase 0 visual-freshness diagnostic toggle**
                                            // (task #83). Inline rather than going through
                                            // the ControlMsg dispatch path because the
                                            // effect is a single atomic store on the
                                            // matching DisplaySession — no shared autonomy
                                            // state, no event-bus side effects, no listener
                                            // chain to wait on. Symmetric with the
                                            // `display_input` arm above for the same reason
                                            // (direct session access, no bus round-trip).
                                            //
                                            // No authority gate: diagnostics is operator-
                                            // initiated and the marker affects every viewer
                                            // of this display when on (it's stamped pre-
                                            // encoder, lands in every encoded layer). An
                                            // operator running a smoke run sets it, all
                                            // viewers see the marker until they unset it.
                                            // No covert-stamp scenario worth gating against.
                                            let display_id =
                                                json["display_id"].as_u64().unwrap_or(0) as u32;
                                            let enabled =
                                                json["enabled"].as_bool().unwrap_or(false);
                                            match session_registry_inbound.as_ref() {
                                                Some(sr) => {
                                                    let applied = sr
                                                        .write()
                                                        .await
                                                        .set_diagnostics_visual_marker(
                                                            display_id, enabled,
                                                        );
                                                    eprintln!(
                                                        "[web_gateway] phase-0 visual marker for display {} = {}{}",
                                                        display_id,
                                                        enabled,
                                                        if applied { "" } else { " (pending)" },
                                                    );
                                                }
                                                None => {
                                                    eprintln!(
                                                        "[web_gateway] phase-0 visual marker request for display {} ({}) ignored; no session registry",
                                                        display_id, enabled,
                                                    );
                                                }
                                            }
                                        }
                                        _ => {
                                            // Fall through to ControlMsg parsing.
                                            // WebRtcSignal needs special handling because
                                            // it requires session_registry / direct_tx
                                            // access for the response leg; everything else
                                            // gets re-broadcast as AppEvent::ControlCommand
                                            // for the agent loop / TUI / MCP consumers.
                                            match serde_json::from_value::<ControlMsg>(json) {
                                                Ok(ctrl)
                                                    if !peer_identity_allows_ws_control(
                                                        peer_identity_inbound.as_ref(),
                                                        &ctrl,
                                                        &bus_inbound,
                                                    ) => {}
                                                Ok(ctrl)
                                                    if !ws_grant_allows_control(
                                                        &dashboard_control_grant_inbound,
                                                        peer_identity_inbound.as_ref(),
                                                        &ctrl,
                                                        &bus_inbound,
                                                    ) => {}
                                                Ok(ControlMsg::WebRtcSignal {
                                                    display_id,
                                                    session_id,
                                                    signal,
                                                }) => {
                                                    let federated_display_input_allowed =
                                                                peer_identity_allows_operation(
                                                                    peer_identity_inbound.as_ref(),
                                                                    crate::peer::access_policy::PeerOperation::DisplayInput,
                                                                    "peer-webrtc-display",
                                                                );
                                                    handle_federated_webrtc_signal(
                                                                display_id,
                                                                session_id,
                                                                signal,
                                                        session_registry_inbound.as_ref(),
                                                        &ice_config,
                                                        Arc::clone(&tcp_peer_registry),
                                                        direct_tx_inbound.clone(),
                                                        &bus_inbound,
                                                        // F-1.3b3 federated authority context.
                                                        // `connection_id_inbound` is this WS's
                                                        // id, which doubles as the federation
                                                        // transport's `federation_connection_id`
                                                        // when this connection is acting as a
                                                        // federation transport.
                                                        connection_id_inbound.clone(),
                                                        Arc::clone(&display_input_authority_inbound),
                                                        authority_change_tx_inbound.clone(),
                                                        Arc::clone(&federated_authority_subscribers_inbound),
                                                        federated_display_input_allowed,
                                                    )
                                                    .await;
                                                }
                                                Ok(ControlMsg::PeerFileTransferSignal {
                                                    session_id,
                                                    signal,
                                                }) => {
                                                    handle_peer_file_transfer_signal(
                                                        session_id,
                                                        signal,
                                                        Arc::clone(
                                                            &peer_file_transfer_registry_inbound,
                                                        ),
                                                        peer_identity_inbound.clone(),
                                                        direct_tx_inbound.clone(),
                                                        &bus_inbound,
                                                    )
                                                    .await;
                                                }
                                                Ok(ControlMsg::PeerDashboardControlSignal {
                                                    session_id,
                                                    signal,
                                                }) => {
                                                    handle_peer_dashboard_control_signal(
                                                        session_id,
                                                        signal,
                                                        Arc::clone(&dashboard_control_inbound),
                                                        peer_identity_inbound.clone(),
                                                        direct_tx_inbound.clone(),
                                                        &bus_inbound,
                                                    )
                                                    .await;
                                                }
                                                Ok(ControlMsg::RequestDisplayInputAuthority {
                                                    display_id,
                                                }) => {
                                                    // Phase 5a.1: handler body lives in
                                                    // `apply_grant_input_authority` so the
                                                    // authority-change emission is unit-testable
                                                    // without standing up a WS lifecycle.  This
                                                    // arm keeps the bus log + the per-connection
                                                    // confirm message at the call site to avoid
                                                    // baking logging dependencies into the helper.
                                                    apply_grant_input_authority(
                                                        display_id,
                                                        connection_id_inbound.clone(),
                                                        direct_tx_inbound.clone(),
                                                        &display_input_authority_inbound,
                                                        &authority_change_tx_inbound,
                                                    );
                                                    // Confirm to the new holder (kept here so the
                                                    // helper has no dependency on the call site's
                                                    // direct_tx — and so the failure-to-send case
                                                    // doesn't bubble past the gate).
                                                    let granted = serde_json::json!({
                                                        "t": "display_input_authority_granted",
                                                        "display_id": display_id,
                                                    })
                                                    .to_string();
                                                    let _ = direct_tx_inbound.send(granted);
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[ws] display_input_authority granted display={} holder={}",
                                                            display_id, connection_id_inbound,
                                                        ),
                                                        level: Some(LogLevel::Debug),
                                                        turn: None,
                                                    });
                                                }
                                                Ok(ControlMsg::ReleaseDisplayInputAuthority {
                                                    display_id,
                                                }) => {
                                                    let removed = apply_release_input_authority(
                                                        display_id,
                                                        connection_id_inbound.as_str(),
                                                        &display_input_authority_inbound,
                                                        &authority_change_tx_inbound,
                                                    );
                                                    if removed {
                                                        bus_inbound.send(AppEvent::PresenceLog {
                                                            message: format!(
                                                                "[ws] display_input_authority released display={} holder={}",
                                                                display_id, connection_id_inbound,
                                                            ),
                                                            level: Some(LogLevel::Debug),
                                                            turn: None,
                                                        });
                                                    }
                                                }
                                                Ok(ControlMsg::SetDiagnosticsVisualMarker {
                                                    display_id,
                                                    enabled,
                                                }) => {
                                                    // Accept the documented ControlMsg wire form
                                                    // (`{"action":"set_diagnostics_visual_marker", ...}`)
                                                    // in addition to the low-level `t` form
                                                    // handled above. The smoke script uses
                                                    // ControlMsg JSON so the toggle must be
                                                    // applied here instead of falling through to
                                                    // the generic bus path, where this variant is
                                                    // intentionally a no-op for TUI/MCP parity.
                                                    let display_id = display_id.unwrap_or(0);
                                                    match session_registry_inbound.as_ref() {
                                                        Some(sr) => {
                                                            let applied = sr
                                                                .write()
                                                                .await
                                                                .set_diagnostics_visual_marker(
                                                                    display_id, enabled,
                                                                );
                                                            eprintln!(
                                                                "[web_gateway] phase-0 visual marker for display {} = {}{}",
                                                                display_id,
                                                                enabled,
                                                                if applied { "" } else { " (pending)" },
                                                            );
                                                        }
                                                        None => {
                                                            eprintln!(
                                                                "[web_gateway] phase-0 visual marker request for display {} ({}) ignored; no session registry",
                                                                display_id, enabled,
                                                            );
                                                        }
                                                    }
                                                }
                                                Ok(ctrl @ ControlMsg::ResumeSession { .. }) => {
                                                    let ControlMsg::ResumeSession {
                                                        source,
                                                        session_id,
                                                        resume_id,
                                                        task,
                                                        ..
                                                    } = &ctrl
                                                    else {
                                                        unreachable!();
                                                    };
                                                    let source = source.clone();
                                                    let session_id = session_id.clone();
                                                    let resume_id = resume_id.clone();
                                                    let task = task.clone();
                                                    let direct_tx_resume =
                                                        direct_tx_inbound.clone();
                                                    let bus_resume = bus_inbound.clone();
                                                    tokio::spawn(async move {
                                                        let replay = tokio::task::spawn_blocking(
                                                            move || {
                                                                resume_session_activity_replay(
                                                                    &source,
                                                                    &session_id,
                                                                    resume_id.as_deref(),
                                                                    task.as_deref(),
                                                                    EXTERNAL_ACTIVITY_REPLAY_LIMIT,
                                                                )
                                                            },
                                                        )
                                                        .await
                                                        .ok()
                                                        .flatten();
                                                        if let Some(replay) = replay {
                                                            let _ = direct_tx_resume.send(replay);
                                                        }
                                                        bus_resume.send(AppEvent::PresenceLog {
                                                            message: format!(
                                                                "[ws] ControlMsg: {:?}",
                                                                ctrl
                                                            ),
                                                            level: Some(LogLevel::Debug),
                                                            turn: None,
                                                        });
                                                        bus_resume
                                                            .send(AppEvent::ControlCommand(ctrl));
                                                    });
                                                }
                                                Ok(ctrl) => {
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[ws] ControlMsg: {:?}",
                                                            match &ctrl {
                                                                ControlMsg::StartTask {
                                                                    task,
                                                                    ..
                                                                } => format!(
                                                                    "StartTask({})",
                                                                    preview_text(task, 60)
                                                                ),
                                                                other => format!("{:?}", other),
                                                            }
                                                        ),
                                                        level: Some(LogLevel::Debug),
                                                        turn: None,
                                                    });
                                                    bus_inbound
                                                        .send(AppEvent::ControlCommand(ctrl));
                                                }
                                                Err(e) => {
                                                    bus_inbound.send(AppEvent::PresenceLog {
                                                        message: format!(
                                                            "[ws] ControlMsg parse failed: {}",
                                                            e
                                                        ),
                                                        level: Some(LogLevel::Warn),
                                                        turn: None,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // WebSocket closed — clean up active slot and auto-resume
                        // server presence if this was the active browser (covers tab
                        // close without beforeunload, network drops, etc.)
                        if is_active {
                            let mut slot = active_presence_inbound
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if slot
                                .as_ref()
                                .map(|a| a.connection_id == connection_id_inbound)
                                .unwrap_or(false)
                            {
                                *slot = None;
                            }
                        }
                        // Also release any display input authority this
                        // connection held (phase 5).  Without this, a
                        // dangling entry would block other browsers from
                        // claiming the display until someone explicitly
                        // sent RequestDisplayInputAuthority to force-take
                        // it — the `retain` below is the normal-drop
                        // cleanup that keeps the map consistent with
                        // live connections.
                        //
                        // Phase 5a.1: helper handles map mutation + per-
                        // display None-holder change emit so other
                        // browsers don't stay stuck on `other` after the
                        // holder's WS drops.  See
                        // `apply_ws_close_input_authority` for the
                        // semantics + tests.
                        apply_ws_close_input_authority(
                            connection_id_inbound.as_str(),
                            &display_input_authority_inbound,
                            &authority_change_tx_inbound,
                        );
                        // F-1.3b3: federation-transport WS-close
                        // cleanup. Two disjoint registry entries can
                        // belong to one connection_id — `LocalWs` from
                        // direct-browser use or `FederatedWebRtc` from
                        // federation-transport use — so both apply_*
                        // helpers fire from the same WS-close hook.
                        // The single WS in practice acts in only one
                        // role at a time, so the second helper is a
                        // no-op in the typical case; the cost of
                        // running both is the bookkeeping above.
                        //
                        // Order: unregister subscribers first (stops
                        // new fanout sends) → release authority (so
                        // observers see `unclaimed`) → close
                        // WebRtcPeers (so the data channels stop
                        // accepting incoming `display_input_authority_request`
                        // frames under the now-defunct federation
                        // identity). Without the peer-teardown step,
                        // the authority handler closure on each
                        // surviving peer would keep mutating the
                        // registry under an identity whose WS is
                        // gone — the structural bug F-1.3b3 fix #2
                        // closes.
                        let released_federated_subs =
                            unregister_all_federated_subscribers_for_connection(
                                connection_id_inbound.as_str(),
                                &federated_authority_subscribers_inbound,
                            );
                        apply_federated_ws_close_input_authority(
                            connection_id_inbound.as_str(),
                            &display_input_authority_inbound,
                            &authority_change_tx_inbound,
                        );
                        close_federated_peers_for_sessions(
                            &released_federated_subs,
                            session_registry_inbound.as_ref(),
                        )
                        .await;
                        if is_presence_connected && is_active {
                            bus_inbound.send(AppEvent::PresenceDisconnected);
                        }
                        // Remove this peer from display sessions it connected to
                        if !peer_display_ids.is_empty() {
                            if let Some(ref sr) = session_registry_inbound {
                                let reg = sr.read().await;
                                for did in &peer_display_ids {
                                    if let Some(session) = reg.get(*did) {
                                        session.remove_peer(peer_id).await;
                                    }
                                }
                            }
                        }
                        for session_id in dashboard_control_session_ids {
                            dashboard_control_inbound.close(&session_id).await;
                        }
                        // Unregister from WebTui
                        if let Some(ref tx) = web_tui_tx_inbound {
                            let _ = tx.send(crate::tui::web::WebTuiCommand::RemoveConnection {
                                id: connection_id_inbound.clone(),
                            });
                        }
                    });

                    // Phase 5a.1 outbound personalization plumbing.  The
                    // authority change channel carries the holder's
                    // server-internal connection_id; this connection's
                    // outbound task converts each incoming change into a
                    // personalized `display_input_authority_state` wire
                    // message.  Connection IDs never leave the daemon —
                    // only the resolved `you|other|unclaimed` state does.
                    let mut authority_change_rx = authority_change_tx.subscribe();
                    let connection_id_outbound = connection_id.clone();
                    let display_input_authority_outbound = display_input_authority.clone();
                    let session_registry_outbound = session_registry.clone();

                    // Outbound: broadcast + direct responses → WebSocket
                    let outbound = tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                msg = outbound_rx.recv() => {
                                    match msg {
                                        Ok(line) => {
                                            if ws_tx
                                                .send(Message::Text(line.into()))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        Err(broadcast::error::RecvError::Closed) => break,
                                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                    }
                                }
                                msg = direct_rx.recv() => {
                                    match msg {
                                        Some(line) => {
                                            if ws_tx
                                                .send(Message::Text(line.into()))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        None => break,
                                    }
                                }
                                msg = authority_change_rx.recv() => {
                                    match msg {
                                        Ok(change) => {
                                            // Personalize: never ship the holder's identity.
                                            let state = match &change.holder {
                                                Some(h) if h.matches_local_ws(&connection_id_outbound) => "you",
                                                Some(_) => "other",
                                                None => "unclaimed",
                                            };
                                            let frame = serde_json::json!({
                                                "t": "display_input_authority_state",
                                                "display_id": change.display_id,
                                                "state": state,
                                            }).to_string();
                                            if ws_tx
                                                .send(Message::Text(frame.into()))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        Err(broadcast::error::RecvError::Closed) => break,
                                        Err(broadcast::error::RecvError::Lagged(_)) => {
                                            // Phase 5a.1: a lagged subscriber missed at least one
                                            // authority transition.  Send a fresh personalized
                                            // snapshot for every currently-active display so the
                                            // browser's chip cannot be left stuck on stale state.
                                            // Snapshot is computed under the std lock (held briefly,
                                            // released before any send) plus the session registry's
                                            // tokio lock for the active-display list — order
                                            // matters: take the std lock LAST and drop it before
                                            // awaiting the send to avoid awaiting under a sync guard.
                                                            let display_ids: Vec<u32> = match session_registry_outbound.as_ref() {
                                                Some(sr) => sr.read().await.display_ids(),
                                                None => Vec::new(),
                                            };
                                            let snapshots: Vec<(u32, &'static str)> = {
                                                let auth = display_input_authority_outbound
                                                    .read()
                                                    .unwrap_or_else(|e| e.into_inner());
                                                display_ids.into_iter().map(|did| {
                                                    let state = match auth.get(&did) {
                                                        Some(entry) if entry.matches_local_ws(&connection_id_outbound) => "you",
                                                        Some(_) => "other",
                                                        None => "unclaimed",
                                                    };
                                                    (did, state)
                                                }).collect()
                                            };
                                            let mut send_failed = false;
                                            for (did, state) in snapshots {
                                                let frame = serde_json::json!({
                                                    "t": "display_input_authority_state",
                                                    "display_id": did,
                                                    "state": state,
                                                }).to_string();
                                                if ws_tx
                                                    .send(Message::Text(frame.into()))
                                                    .await
                                                    .is_err()
                                                {
                                                    send_failed = true;
                                                    break;
                                                }
                                            }
                                            if send_failed { break; }
                                        }
                                    }
                                }
                            }
                        }
                    });

                    let _ = tokio::join!(inbound, outbound);
                } else {
                    // Plain HTTP: consume the peeked request bytes, then send response.
                    let mut discard = vec![0u8; n];
                    use tokio::io::AsyncReadExt;
                    let _ = stream.read_exact(&mut discard).await;

                    // Parse the request target once: the static-asset arms
                    // below match on the *exact* path (query stripped), so
                    // an API request that merely mentions an asset path in
                    // a query parameter can no longer be shadowed by them.
                    let (req_method, req_path, req_query) = parse_request_target(request_line);

                    // CORS preflight: respond to OPTIONS with permissive headers.
                    // Needed when the page is served from a custom scheme (intendant://)
                    // in the macOS app bundle — API fetches become cross-origin.
                    //
                    // Exception: the fleet Access APIs never get the wildcard.
                    // Their preflight only succeeds for allowlisted fleet
                    // origins (and the write-side gate below enforces the same
                    // list on the actual requests, so a non-preflighted
                    // cross-site POST is refused too).
                    if req_method == "OPTIONS" {
                        use tokio::io::AsyncWriteExt;
                        let opt_path = req_path;
                        // Table-declared paths take their posture and method
                        // union from the same route declarations that drive
                        // dispatch and IAM — a preflight can no longer be
                        // looser (or tighter) than its endpoint. Undeclared
                        // paths keep the legacy prefix rules: /api/* look-
                        // alikes stay own-origin-scoped, everything else gets
                        // the permissive wildcard (needed when the page is
                        // served from the macOS app's intendant:// scheme).
                        let table_posture = crate::gateway_routes::preflight_posture(opt_path);
                        let table_methods =
                            crate::gateway_routes::allowed_methods_for_path(opt_path);
                        let own_origin_scoped = match table_posture {
                            Some(crate::gateway_routes::CorsPosture::OwnOrigin) => true,
                            Some(_) => false,
                            None => {
                                (opt_path.starts_with("/api/")
                                    && !is_fleet_cors_access_path(opt_path)
                                    && !is_public_peer_access_request_path(request_line))
                                    || opt_path == "/mcp"
                            }
                        };
                        let fleet_scoped = matches!(
                            table_posture,
                            Some(crate::gateway_routes::CorsPosture::FleetAllowlist)
                        ) || (table_posture.is_none()
                            && is_fleet_cors_access_path(opt_path));
                        let response = if own_origin_scoped {
                            // Own-origin APIs (and /mcp) are same-origin (or
                            // app-scheme) only; a cross-origin preflight gets
                            // no ACAO and the browser stops there.
                            let methods = table_methods
                                .as_deref()
                                .unwrap_or("GET, POST, DELETE, OPTIONS");
                            let allowed = extract_origin_header(&header_text).filter(|origin| {
                                is_own_or_app_origin(origin, is_tls, &header_text)
                            });
                            match allowed {
                                Some(origin) => format!(
                                    "HTTP/1.1 204 No Content\r\n\
                                     Access-Control-Allow-Origin: {origin}\r\n\
                                     Access-Control-Allow-Methods: {methods}\r\n\
                                     Access-Control-Allow-Headers: Content-Type, Authorization\r\n\
                                     Access-Control-Max-Age: 86400\r\n\
                                     Vary: Origin\r\n\
                                     Connection: close\r\n\
                                     \r\n"
                                ),
                                None => "HTTP/1.1 204 No Content\r\n\
                                    Vary: Origin\r\n\
                                    Connection: close\r\n\
                                    \r\n"
                                    .to_string(),
                            }
                        } else if fleet_scoped {
                            let methods =
                                table_methods.as_deref().unwrap_or("GET, POST, OPTIONS");
                            let cert_dir = crate::access::backend::select_backend().cert_dir();
                            let allowed = extract_origin_header(&header_text).filter(|origin| {
                                fleet_access_origin_allowed(
                                    origin,
                                    is_tls,
                                    &header_text,
                                    peer_registry.as_ref(),
                                    &cert_dir,
                                )
                            });
                            match allowed {
                                Some(origin) => format!(
                                    "HTTP/1.1 204 No Content\r\n\
                                     Access-Control-Allow-Origin: {origin}\r\n\
                                     Access-Control-Allow-Methods: {methods}\r\n\
                                     Access-Control-Allow-Headers: Content-Type, Authorization\r\n\
                                     Access-Control-Max-Age: 86400\r\n\
                                     Vary: Origin\r\n\
                                     Connection: close\r\n\
                                     \r\n"
                                ),
                                None => "HTTP/1.1 204 No Content\r\n\
                                    Vary: Origin\r\n\
                                    Connection: close\r\n\
                                    \r\n"
                                    .to_string(),
                            }
                        } else {
                            let methods = table_methods
                                .as_deref()
                                .unwrap_or("GET, POST, DELETE, OPTIONS");
                            format!(
                                "HTTP/1.1 204 No Content\r\n\
                                 Access-Control-Allow-Origin: *\r\n\
                                 Access-Control-Allow-Methods: {methods}\r\n\
                                 Access-Control-Allow-Headers: Content-Type, Authorization\r\n\
                                 Access-Control-Max-Age: 86400\r\n\
                                 Connection: close\r\n\
                                 \r\n"
                            )
                        };
                        let _ = stream.write_all(response.as_bytes()).await;
                        finalize_http_stream(&mut stream).await;
                        return;
                    }

                    if tls_client_cert_required
                        && !tls_client_cert_present
                        && !is_loopback_cleartext_mcp_request(peer_addr, is_tls, &header_text)
                        && !is_public_peer_access_request_path(request_line)
                        && !is_public_org_grant_path(request_line)
                        && !is_public_connect_bootstrap_path(request_line)
                    {
                        use tokio::io::AsyncWriteExt;
                        let body = serde_json::json!({
                            "error": "mTLS client certificate required"
                        })
                        .to_string();
                        let response = format!(
                            "HTTP/1.1 401 Unauthorized\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {body}",
                            body.len(),
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        finalize_http_stream(&mut stream).await;
                        return;
                    }

                    let cert_dir = crate::access::backend::select_backend().cert_dir();
                    let http_access_context = match http_access_context(
                        &cert_dir,
                        peer_connection_identity.as_ref(),
                        tls_client_cert_fingerprint.as_deref(),
                        tls_client_cert_present,
                        is_tls,
                    ) {
                        Ok(context) => context,
                        Err(message) => {
                            use tokio::io::AsyncWriteExt;
                            let response = json_error("500 Internal Server Error", message);
                            let _ = stream.write_all(response.as_bytes()).await;
                            finalize_http_stream(&mut stream).await;
                            return;
                        }
                    };

                    if let Some((op, kind)) = peer_filesystem_query_request(req_method, req_path) {
                        let path = query_param(request_line, "path").unwrap_or_default();
                        if let Err(message) = authorize_http_filesystem_access(
                            &http_access_context,
                            peer_connection_identity.as_ref(),
                            op,
                            kind,
                            &path,
                            &bus,
                        ) {
                            use tokio::io::AsyncWriteExt;
                            let response = json_error("403 Forbidden", message);
                            let _ = stream.write_all(response.as_bytes()).await;
                            finalize_http_stream(&mut stream).await;
                            return;
                        }
                    }

                    // Federation auth enforcement. Applied before any
                    // federation API branch in the dispatch chain
                    // below; non-federation paths (WASM, frames,
                    // dashboard HTML, /config, /.well-known, /ws,
                    // /static/*) sail through unauthenticated. See
                    // `is_federation_path` for the exact set and the
                    // `inbound_bearer_token` docs on `spawn_web_gateway`
                    // for the design rationale.
                    if is_federation_path(request_line) {
                        if let Some(op) = crate::peer::access_policy::federation_http_operation(
                            req_method, req_path,
                        ) {
                            let decision = http_access_context.decision(op);
                            if !decision.allowed {
                                use tokio::io::AsyncWriteExt;
                                let body = serde_json::json!({
                                    "error": "principal does not allow this operation",
                                    "principal": http_access_context.principal.as_value(),
                                    "permission": decision.permission,
                                    "reason": decision.reason,
                                })
                                .to_string();
                                let response = format!(
                                    "HTTP/1.1 403 Forbidden\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {body}",
                                    body.len(),
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                                finalize_http_stream(&mut stream).await;
                                return;
                            }
                        }
                        if let Err((status, body)) =
                            verify_bearer_token(&header_text, inbound_bearer_token.as_deref())
                        {
                            use tokio::io::AsyncWriteExt;
                            let reason = match status {
                                401 => "Unauthorized",
                                _ => "Error",
                            };
                            let response = format!(
                                "HTTP/1.1 {status} {reason}\r\n\
                                 Content-Type: application/json\r\n\
                                 Content-Length: {}\r\n\
                                 Cache-Control: no-cache\r\n\
                                 WWW-Authenticate: Bearer\r\n\
                                 Connection: close\r\n\
                                 \r\n\
                                 {body}",
                                body.len(),
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                            finalize_http_stream(&mut stream).await;
                            return;
                        }
                    }

                    if let Some(op) = dashboard_http_operation(req_method, req_path) {
                        let decision = http_access_context.decision(op);
                        if !decision.allowed {
                            use tokio::io::AsyncWriteExt;
                            let response =
                                http_access_forbidden_response(&http_access_context, decision);
                            let _ = stream.write_all(response.as_bytes()).await;
                            finalize_http_stream(&mut stream).await;
                            return;
                        }
                    }

                    // API origin gate + CORS echo. A browser sends an Origin
                    // header on every cross-origin request (and on
                    // same-origin POSTs); the browser-attached mTLS
                    // certificate must not let an arbitrary website drive or
                    // read these APIs cross-site. Policy:
                    //   - no Origin header (same-origin GETs, curl, native
                    //     code, the macOS app's URLSession proxy): untouched;
                    //   - own origin or the intendant:// app scheme: allowed;
                    //   - fleet-allowlisted origins: allowed on the six fleet
                    //     Access APIs, which also echo the origin so the
                    //     anchor page can read the responses;
                    //   - anything else on any /api/ path: 403, except the
                    //     public doorbell, which is designed to be knocked on.
                    let request_origin = extract_origin_header(&header_text);
                    let mut fleet_cors_origin: Option<String> = None;
                    if let Some(origin) = request_origin.as_deref().filter(|_| {
                        req_path.starts_with("/api/")
                            && !is_public_peer_access_request_path(request_line)
                            && !is_public_org_grant_path(request_line)
                    }) {
                        let own = is_own_or_app_origin(origin, is_tls, &header_text);
                        let fleet_allowed = !own
                            && is_fleet_cors_access_path(req_path)
                            && fleet_access_origin_allowed(
                                origin,
                                is_tls,
                                &header_text,
                                peer_registry.as_ref(),
                                &cert_dir,
                            );
                        if fleet_allowed {
                            fleet_cors_origin = Some(origin.to_string());
                        } else if !own {
                            use tokio::io::AsyncWriteExt;
                            let body = serde_json::json!({
                                "error": "cross-origin caller is not allowed on this API",
                                "origin": origin,
                            })
                            .to_string();
                            let response = json_response("403 Forbidden", body);
                            let _ = stream.write_all(response.as_bytes()).await;
                            finalize_http_stream(&mut stream).await;
                            return;
                        }
                    }

                    if let Some((route, _route_captures)) =
                        crate::gateway_routes::match_route(req_method, req_path)
                    {
                        // Table-dispatched routes: every /api/* and /mcp route is
                        // declared once in gateway_routes::ROUTES (which the IAM
                        // gate above already consulted through
                        // dashboard_http_operation). The if/else chain below serves
                        // only the non-API surface (connect bootstrap, recordings,
                        // frames, debug, config, static assets, SPA fallback) — a
                        // route is served by the table or the chain, never both.
                        // Handler bodies moved here verbatim from their chain arms;
                        // never add an API route to the chain, declare it in the
                        // table instead.
                        use crate::gateway_routes::RouteHandlerId;
                        match route.handler {
                            RouteHandlerId::FsWrite => {
                                return handle_fs_write(
                                    stream,
                                    &header_text,
                                    http_access_context,
                                    peer_connection_identity,
                                    bus,
                                )
                                .await;
                            }
                            RouteHandlerId::SessionCurrentChanges => {
                                return handle_session_current_changes(
                                    stream,
                                    request_line,
                                    project_root_for_changes,
                                    snapshot_dir,
                                )
                                .await;
                            }
                            RouteHandlerId::WorktreesInspect => {
                                return handle_worktrees_inspect(stream, &header_text).await;
                            }
                            RouteHandlerId::WorktreesRemove => {
                                return handle_worktrees_remove(
                                    stream,
                                    &header_text,
                                    worktree_inventory_cache,
                                )
                                .await;
                            }
                            RouteHandlerId::WorktreesScan => {
                                return handle_worktrees_scan(
                                    stream,
                                    project_root,
                                    worktree_inventory_cache,
                                )
                                .await;
                            }
                            RouteHandlerId::WorktreesList => {
                                return handle_worktrees_list(
                                    stream,
                                    worktree_inventory_cache,
                                )
                                .await;
                            }
                            RouteHandlerId::SessionsList => {
                                return handle_sessions_list(stream, request_line).await;
                            }
                            RouteHandlerId::FsStat => {
                                return handle_fs_stat(stream, request_line).await;
                            }
                            RouteHandlerId::FsList => {
                                return handle_fs_list(stream, request_line).await;
                            }
                            RouteHandlerId::FsRead => {
                                return handle_fs_read(stream, &header_text, request_line).await;
                            }
                            RouteHandlerId::FsMkdir => {
                                return handle_fs_mkdir(
                                    stream,
                                    &header_text,
                                    http_access_context,
                                    peer_connection_identity,
                                    bus,
                                )
                                .await;
                            }
                            RouteHandlerId::FsRename => {
                                return handle_fs_rename(
                                    stream,
                                    &header_text,
                                    http_access_context,
                                    peer_connection_identity,
                                    bus,
                                )
                                .await;
                            }
                            RouteHandlerId::FsDelete => {
                                return handle_fs_delete(
                                    stream,
                                    &header_text,
                                    http_access_context,
                                    peer_connection_identity,
                                    bus,
                                )
                                .await;
                            }
                            RouteHandlerId::CurrentHistory => {
                                return handle_current_history(stream, file_watcher).await;
                            }
                            RouteHandlerId::CurrentRollback => {
                                return handle_current_rollback(
                                    stream,
                                    &header_text,
                                    bus,
                                    query_ctx,
                                    file_watcher,
                                )
                                .await;
                            }
                            RouteHandlerId::CurrentRedo => {
                                return handle_current_redo(
                                    stream,
                                    &header_text,
                                    query_ctx,
                                    file_watcher,
                                )
                                .await;
                            }
                            RouteHandlerId::CurrentPrune => {
                                return handle_current_prune(
                                    stream,
                                    &header_text,
                                    file_watcher,
                                )
                                .await;
                            }
                            RouteHandlerId::CurrentAgentOutput => {
                                return handle_current_agent_output(
                                    stream,
                                    &header_text,
                                    query_ctx,
                                    session_log,
                                )
                                .await;
                            }
                            RouteHandlerId::CurrentUploadsPost => {
                                return handle_current_uploads_post(
                                    stream,
                                    &header_text,
                                    request_line,
                                    discard,
                                    bus,
                                    project_root_for_changes,
                                    session_log,
                                    daemon_session_id,
                                )
                                .await;
                            }
                            RouteHandlerId::CurrentUploadsGet => {
                                return handle_current_uploads_get(
                                    stream,
                                    request_line,
                                    project_root_for_changes,
                                    session_log,
                                )
                                .await;
                            }
                            RouteHandlerId::CurrentUploadDelete => {
                                return handle_current_upload_delete(
                                    stream,
                                    request_line,
                                    bus,
                                    project_root_for_changes,
                                    session_log,
                                )
                                .await;
                            }
                            RouteHandlerId::SessionDelete => {
                                return handle_session_delete(stream, request_line).await;
                            }
                            RouteHandlerId::SessionAgentOutput => {
                                return handle_session_agent_output(
                                    stream,
                                    &header_text,
                                    request_line,
                                )
                                .await;
                            }
                            RouteHandlerId::SessionSubRouter => {
                                return handle_session_sub_router(
                                    stream,
                                    request_line,
                                    session_log,
                                    query_ctx,
                                )
                                .await;
                            }
                            RouteHandlerId::McAnchors => {
                                return handle_mc_anchors(stream, request_line, session_log).await;
                            }
                            RouteHandlerId::McRecords => {
                                return handle_mc_records(stream, request_line, session_log).await;
                            }
                            RouteHandlerId::McFission => {
                                return handle_mc_fission(stream, request_line, session_log).await;
                            }
                            RouteHandlerId::SessionsStream => {
                                return handle_sessions_stream(stream, request_line).await;
                            }
                            RouteHandlerId::SessionsSearch => {
                                return handle_sessions_search(stream, request_line).await;
                            }
                            RouteHandlerId::ProjectRoot => {
                                return handle_project_root(stream, project_root).await;
                            }
                            RouteHandlerId::SettingsPost => {
                                return handle_settings_post(
                                    stream,
                                    &header_text,
                                    bus,
                                    project_root,
                                )
                                .await;
                            }
                            RouteHandlerId::SettingsGet => {
                                return handle_settings_get(
                                    stream,
                                    project_root,
                                    runtime_settings,
                                )
                                .await;
                            }
                            RouteHandlerId::ApiKeysPost => {
                                return handle_api_keys_post(stream, &header_text).await;
                            }
                            RouteHandlerId::ApiKeyStatus => {
                                return handle_api_key_status(stream).await;
                            }
                            RouteHandlerId::ExternalAgents => {
                                return handle_external_agents(stream, project_root).await;
                            }
                            RouteHandlerId::DiagnosticsVisualFreshness => {
                                return handle_diagnostics_visual_freshness(
                                    stream,
                                    &header_text,
                                    request_line,
                                )
                                .await;
                            }
                            RouteHandlerId::Displays => {
                                return handle_displays(stream, session_registry).await;
                            }
                            RouteHandlerId::Doorbell => {
                                return handle_doorbell(
                                    stream,
                                    &header_text,
                                    request_line,
                                    req_method,
                                    peer_access_request_config,
                                    source_hint,
                                    is_tls,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessOrgGrantPresent => {
                                return handle_access_org_grant_present(
                                    stream,
                                    &header_text,
                                    req_method,
                                    agent_card_value_for_targets,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessOrgRevocations => {
                                return handle_access_org_revocations(stream, req_path).await;
                            }
                            RouteHandlerId::AccessOrgApplyRenew => {
                                return handle_access_org_apply_renew(
                                    stream,
                                    &header_text,
                                    req_method,
                                    req_path,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessIamGrants => {
                                return handle_access_iam_grants(
                                    stream,
                                    &header_text,
                                    req_method,
                                    req_path,
                                    http_access_context,
                                    fleet_cors_origin,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessOrgManage => {
                                return handle_access_org_manage(
                                    stream,
                                    &header_text,
                                    req_method,
                                    req_path,
                                    http_access_context,
                                    fleet_cors_origin,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessEnrollmentDecide => {
                                return handle_access_enrollment_decide(
                                    stream,
                                    &header_text,
                                    req_method,
                                    http_access_context,
                                    fleet_cors_origin,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessEnrollmentRequests => {
                                return handle_access_enrollment_requests(
                                    stream,
                                    fleet_cors_origin,
                                )
                                .await;
                            }
                            RouteHandlerId::AccessIamState => {
                                return handle_access_iam_state(stream, fleet_cors_origin).await;
                            }
                            RouteHandlerId::AccessOverview => {
                                return handle_access_overview(
                                    stream,
                                    http_access_context,
                                    fleet_cors_origin,
                                    peer_registry,
                                    agent_card_value_for_targets,
                                )
                                .await;
                            }
                            RouteHandlerId::DashboardTargets => {
                                return handle_dashboard_targets(
                                    stream,
                                    peer_registry,
                                    agent_card_value_for_targets,
                                )
                                .await;
                            }
                            RouteHandlerId::PeersSubRouter => {
                                return handle_peers_sub_router(
                                    stream,
                                    &header_text,
                                    request_line,
                                    req_method,
                                    bus,
                                    project_root,
                                    peer_registry,
                                )
                                .await;
                            }
                            RouteHandlerId::CoordinatorRoute => {
                                return handle_coordinator_route(
                                    stream,
                                    &header_text,
                                    req_method,
                                    peer_registry,
                                )
                                .await;
                            }
                            RouteHandlerId::McpPost => {
                                return handle_mcp_post(
                                    stream,
                                    &header_text,
                                    request_line,
                                    peer_connection_identity,
                                    mcp_server,
                                    is_tls,
                                    tls_client_cert_present,
                                    tls_client_cert_fingerprint,
                                    peer_addr,
                                )
                                .await;
                            }
                            RouteHandlerId::McpStream => {
                                return handle_mcp_stream(stream, &header_text, is_tls).await;
                            }
                        }
                    } else if let Some(allow) =
                        crate::gateway_routes::allowed_methods_for_path(req_path)
                    {
                        // Declared API path, undeclared method: uniform 405
                        // with an Allow header derived from the route table.
                        // Before the method tightening these requests either
                        // reached a method-blind read handler or fell all the
                        // way through to the SPA-shell fallback. CORS posture
                        // mirrors the path's declared posture so cross-origin
                        // fleet/public callers can read the error.
                        use tokio::io::AsyncWriteExt;
                        let body = serde_json::json!({
                            "error": "method not allowed",
                            "allow": allow,
                        })
                        .to_string();
                        let base = HttpResponse::json("405 Method Not Allowed", body)
                            .header("Allow", &allow);
                        let response = match crate::gateway_routes::preflight_posture(req_path) {
                            Some(crate::gateway_routes::CorsPosture::Public) => base.public_cors(),
                            Some(crate::gateway_routes::CorsPosture::FleetAllowlist) => {
                                base.fleet_cors(fleet_cors_origin.as_deref())
                            }
                            _ => base,
                        };
                        let _ = stream.write_all(&response.into_bytes()).await;
                    } else if req_method == "GET" && req_path == "/connect/bootstrap" {
                        use tokio::io::AsyncWriteExt;
                        let body = connect_bootstrap_html();
                        let response = html_response("200 OK", body);
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if req_method == "GET" && req_path == "/connect/status" {
                        use tokio::io::AsyncWriteExt;
                        let body = serde_json::json!({
                            "ok": true,
                            "transport": "webrtc-dashboard-control",
                            "signaling": "connect-bootstrap-local",
                            "mtls_required_for_dashboard": tls_client_cert_required,
                        });
                        let _ = stream
                            .write_all(with_public_cors(json_ok(body)).as_bytes())
                            .await;
                    } else if req_method == "POST" && req_path == "/connect/dashboard/offer" {
                        use tokio::io::AsyncWriteExt;
                        let body_text = read_post_body(&header_text, &mut stream).await;
                        let response = with_public_cors(
                            connect_dashboard_offer_response(
                                &dashboard_control,
                                &body_text,
                                &agent_card_value_for_targets,
                            )
                            .await,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if req_method == "POST" && req_path == "/connect/dashboard/ice" {
                        use tokio::io::AsyncWriteExt;
                        let body_text = read_post_body(&header_text, &mut stream).await;
                        let response = with_public_cors(
                            connect_dashboard_ice_response(&dashboard_control, &body_text).await,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if req_method == "POST" && req_path == "/connect/dashboard/close" {
                        use tokio::io::AsyncWriteExt;
                        let body_text = read_post_body(&header_text, &mut stream).await;
                        let response = with_public_cors(
                            connect_dashboard_close_response(&dashboard_control, &body_text).await,
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    // Route WASM binaries (need async write_all for large payloads)
                    } else if let Some(asset) = static_asset_arm(
                        req_method,
                        req_path,
                        &[
                            "/wasm-web/presence_web_bg.wasm",
                            "/wasm-station/station_web_bg.wasm",
                        ],
                    ) {
                        let response = build_static_asset_response(
                            req_method,
                            &header_text,
                            req_query,
                            asset_version(),
                            asset.view(),
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(&response).await;
                    } else if let Some(asset) = static_asset_arm(
                        req_method,
                        req_path,
                        &[
                            "/icon-128.png",
                            "/favicon.ico",
                            "/icon-512.png",
                            "/icon-512-maskable.png",
                            "/apple-touch-icon.png",
                            "/manifest.webmanifest",
                        ],
                    ) {
                        let response = build_static_asset_response(
                            req_method,
                            &header_text,
                            req_query,
                            asset_version(),
                            asset.view(),
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(&response).await;
                    } else if req_path.starts_with("/frames/") {
                        // Serve HQ frame images from the frame registry.
                        // URL format: /frames/<frame_id> (not /api/session/*/frames/*)
                        use tokio::io::AsyncWriteExt;
                        let frame_id = request_line
                            .split("/frames/")
                            .nth(1)
                            .and_then(|s| s.split_whitespace().next())
                            .unwrap_or("");
                        let data = if let Some(ref reg) = frame_registry {
                            let reg = reg.read().await;
                            reg.read_hq(frame_id).ok()
                        } else {
                            None
                        };
                        if let Some(jpeg_data) = data {
                            let header = format!(
                                "HTTP/1.1 200 OK\r\n\
                                 Content-Type: image/jpeg\r\n\
                                 Content-Length: {}\r\n\
                                 Cache-Control: public, max-age=31536000, immutable\r\n\
                                 Connection: close\r\n\
                                 \r\n",
                                jpeg_data.len()
                            );
                            let _ = stream.write_all(header.as_bytes()).await;
                            let _ = stream.write_all(&jpeg_data).await;
                        } else {
                            let body = "Frame not found";
                            let response = format!(
                                "HTTP/1.1 404 Not Found\r\n\
                                 Content-Type: text/plain\r\n\
                                 Content-Length: {}\r\n\
                                 Connection: close\r\n\
                                 \r\n\
                                 {}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    } else if req_method == "POST" && req_path == "/session" {
                        let result = mint_session_token(&session_provider, &session_model).await;
                        let (status, body) = match result {
                            Ok(json) => ("200 OK", json),
                            Err(msg) => (
                                "502 Bad Gateway",
                                serde_json::json!({"error": msg}).to_string(),
                            ),
                        };
                        let response = format!(
                            "HTTP/1.1 {}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            status,
                            body.len(),
                            body
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if req_path.starts_with("/recordings/") {
                        // Serve recording data: segment files and metadata.
                        use tokio::io::AsyncWriteExt;
                        let path_part = request_line
                            .split("/recordings/")
                            .nth(1)
                            .and_then(|s| s.split_whitespace().next())
                            .unwrap_or("");
                        let parts: Vec<&str> = path_part.split('/').collect();

                        if let Some(ref rec_reg) = recording_registry {
                            let reg = rec_reg.read().await;

                            if parts.len() == 2 && parts[1] == "segments" {
                                // GET /recordings/{stream}/segments — check session then daemon dir
                                let stream_name = parts[0];
                                let mut segments = reg.segments(stream_name);
                                if segments.is_empty() {
                                    // Fallback to daemon recordings dir
                                    let daemon_dir = crate::debug::daemon_recordings_dir();
                                    let stream_dir = daemon_dir.join(stream_name);
                                    segments = crate::recording::parse_segment_csv_pub(
                                        &stream_dir.join("segments.csv"),
                                        &stream_dir,
                                    );
                                }
                                let json: Vec<serde_json::Value> = segments
                                    .iter()
                                    .map(|s| {
                                        serde_json::json!({
                                            "filename": s.filename,
                                            "start_secs": s.start_secs,
                                            "end_secs": s.end_secs,
                                        })
                                    })
                                    .collect();
                                let body = serde_json::to_string(&json).unwrap_or("[]".to_string());
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(),
                                    body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            } else if parts.len() == 2 && parts[1] == "playlist.m3u8" {
                                // GET /recordings/{stream}/playlist.m3u8 — HLS playlist
                                let stream_name = parts[0];
                                let mut segments = reg.segments(stream_name);
                                if segments.is_empty() {
                                    let daemon_dir = crate::debug::daemon_recordings_dir();
                                    let stream_dir = daemon_dir.join(stream_name);
                                    segments = crate::recording::parse_segment_csv_pub(
                                        &stream_dir.join("segments.csv"),
                                        &stream_dir,
                                    );
                                }
                                let m3u8 = recording_playlist_m3u8(&segments);
                                let response = format!(
                                    "HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/vnd.apple.mpegurl\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    m3u8.len(),
                                    m3u8
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            } else if parts.len() == 2 {
                                // GET /recordings/{stream}/{filename} — serve segment file
                                let stream_name = parts[0];
                                let filename = parts[1];
                                // Validate filename to prevent path traversal
                                let valid = filename.starts_with("seg_")
                                    && (filename.ends_with(".mp4") || filename.ends_with(".ts"))
                                    && filename.len() < 30
                                    && !filename.contains("..");
                                if valid {
                                    // Check session dir first, then daemon dir
                                    let session_path = reg
                                        .session_dir()
                                        .join("recordings")
                                        .join(stream_name)
                                        .join(filename);
                                    let daemon_path = crate::debug::daemon_recordings_dir()
                                        .join(stream_name)
                                        .join(filename);
                                    let seg_path = if session_path.exists() {
                                        session_path
                                    } else {
                                        daemon_path
                                    };
                                    let content_type = if filename.ends_with(".ts") {
                                        "video/mp2t"
                                    } else {
                                        "video/mp4"
                                    };
                                    match tokio::fs::read(&seg_path).await {
                                        Ok(data) => {
                                            let header = format!(
                                                "HTTP/1.1 200 OK\r\n\
                                                 Content-Type: {}\r\n\
                                                 Content-Length: {}\r\n\
                                                 Cache-Control: public, max-age=3600\r\n\
                                                 Connection: close\r\n\
                                                 \r\n",
                                                content_type,
                                                data.len()
                                            );
                                            let _ = stream.write_all(header.as_bytes()).await;
                                            let _ = stream.write_all(&data).await;
                                        }
                                        Err(_) => {
                                            let body = "Segment not found";
                                            let response = format!(
                                                "HTTP/1.1 404 Not Found\r\n\
                                                 Content-Type: text/plain\r\n\
                                                 Content-Length: {}\r\n\
                                                 Connection: close\r\n\
                                                 \r\n\
                                                 {}",
                                                body.len(),
                                                body
                                            );
                                            let _ = stream.write_all(response.as_bytes()).await;
                                        }
                                    }
                                } else {
                                    let body = "Invalid filename";
                                    let response = format!(
                                        "HTTP/1.1 400 Bad Request\r\n\
                                         Content-Type: text/plain\r\n\
                                         Content-Length: {}\r\n\
                                         Connection: close\r\n\
                                         \r\n\
                                         {}",
                                        body.len(),
                                        body
                                    );
                                    let _ = stream.write_all(response.as_bytes()).await;
                                }
                            } else {
                                let body = "Not found";
                                let response = format!(
                                    "HTTP/1.1 404 Not Found\r\n\
                                     Content-Type: text/plain\r\n\
                                     Content-Length: {}\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(),
                                    body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            }
                        } else {
                            let body = "Recording not available";
                            let response = format!(
                                "HTTP/1.1 404 Not Found\r\n\
                                 Content-Type: text/plain\r\n\
                                 Content-Length: {}\r\n\
                                 Connection: close\r\n\
                                 \r\n\
                                 {}",
                                body.len(),
                                body
                            );
                            use tokio::io::AsyncWriteExt;
                            let _ = stream.write_all(response.as_bytes()).await;
                        }
                    } else if req_path == "/recordings" {
                        // GET /recordings — list all streams (session + daemon-scoped)
                        use tokio::io::AsyncWriteExt;

                        let body = recordings_list_response_body(recording_registry.clone()).await;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if req_path == "/debug" {
                        // Debug endpoint: returns agent state + voice connection info
                        let state = query_ctx.as_ref().map(|ctx| {
                            ctx.agent_state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone()
                        });
                        let vd = voice_debug
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        let active_id = active_presence
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_ref()
                            .map(|a| a.connection_id.clone());
                        let debug_json = serde_json::json!({
                            "agent_state": state,
                            "voice": vd,
                            "active_connection_id": active_id,
                        })
                        .to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            debug_json.len(),
                            debug_json
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if let Some(response) = dashboard_local_file_response(request_line) {
                        use tokio::io::AsyncWriteExt;
                        match response {
                            DashboardLocalFileResponse::Html { status, body } => {
                                let response = format!(
                                    "HTTP/1.1 {status}\r\n\
                                     Content-Type: text/html; charset=utf-8\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     Connection: close\r\n\
                                     \r\n\
                                     {}",
                                    body.len(),
                                    body
                                );
                                let _ = stream.write_all(response.as_bytes()).await;
                            }
                            DashboardLocalFileResponse::Bytes {
                                status,
                                content_type,
                                bytes,
                            } => {
                                let header = format!(
                                    "HTTP/1.1 {status}\r\n\
                                     Content-Type: {content_type}\r\n\
                                     Content-Length: {}\r\n\
                                     Cache-Control: no-cache\r\n\
                                     X-Content-Type-Options: nosniff\r\n\
                                     Connection: close\r\n\
                                     \r\n",
                                    bytes.len()
                                );
                                let _ = stream.write_all(header.as_bytes()).await;
                                let _ = stream.write_all(&bytes).await;
                            }
                        }
                    } else if let Some(asset) = static_asset_arm(
                        req_method,
                        req_path,
                        &[
                            "/wasm-web/presence_web.js",
                            "/wasm-station/station_web.js",
                            "/three.module.min.js",
                            "/codemirror-bundle.js",
                            "/codemirror-bundle.css",
                            "/audio-processor.js",
                            "/icon-128.png",
                            "/favicon.ico",
                            "/icon-512.png",
                            "/icon-512-maskable.png",
                            "/apple-touch-icon.png",
                            "/manifest.webmanifest",
                        ],
                    ) {
                        let response = build_static_asset_response(
                            req_method,
                            &header_text,
                            req_query,
                            asset_version(),
                            asset.view(),
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(&response).await;
                    } else if req_path == "/.well-known/agent-card.json" || req_path == "/config" {
                        let body = if req_path == "/.well-known/agent-card.json" {
                            // Canonical peer identity + capability surface.
                            // Served alongside /config so the browser and
                            // federated peers can discover who this daemon
                            // is without parsing the voice-runtime config.
                            agent_card_json.clone()
                        } else {
                            config_json.clone()
                        };
                        // CORS: allow the multi-host dashboard to
                        // `fetch()` /config and /.well-known/agent-card.json
                        // on this daemon from a page served by a sibling
                        // daemon (cross-origin). `*` works because our
                        // fetches don't send credentials.
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    } else if req_method == "GET" || req_method == "HEAD" {
                        // Default: serve app.html (also matches /app for
                        // backward compat). The entry point stays no-cache —
                        // it carries the rewritten `?v=` busters — but gets
                        // an ETag (cheap 304 revalidation) and gzip. ETag +
                        // gzip are computed once per gateway spawn, on first
                        // page load: the rewritten HTML is gateway-scoped,
                        // unlike the constants behind `embedded_static_asset`.
                        // Under INTENDANT_APP_HTML_PATH the disk copy is
                        // re-read (and re-tagged) on every request instead.
                        let response = if let Some(path) = app_html_override.as_deref() {
                            app_html_override_response(req_method, &header_text, req_query, path)
                        } else {
                            let (etag, gzip) = app_html_cache.get_or_init(|| {
                                (
                                    asset_etag(app_html.as_bytes()),
                                    gzip_compress(app_html.as_bytes()),
                                )
                            });
                            build_static_asset_response(
                                req_method,
                                &header_text,
                                req_query,
                                asset_version(),
                                StaticAssetView {
                                    content_type: "text/html; charset=utf-8",
                                    body: app_html.as_bytes(),
                                    etag,
                                    gzip: Some(gzip),
                                    cache_control: Some("no-cache"),
                                },
                            )
                        };
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(&response).await;
                    } else {
                        // Non-GET/HEAD fallback: plain app.html, as before.
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: text/html; charset=utf-8\r\n\
                             Content-Length: {}\r\n\
                             Cache-Control: no-cache\r\n\
                             Access-Control-Allow-Origin: *\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            app_html.len(),
                            app_html
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                    }

                    // Flush + cleanly shut down the stream before this task
                    // returns and drops it. Mandatory for the TLS path so the
                    // final ciphertext records reach the socket (rustls buffers
                    // them; dropping mid-buffer truncates large bodies); a
                    // harmless pass-through on plain TCP. Covers every
                    // fall-through chain arm above in one place; the early
                    // `return`s (OPTIONS / failed federation auth) finalize
                    // inline before returning, and every table-dispatched
                    // handler owns its stream and finalizes it itself.
                    finalize_http_stream(&mut stream).await;
                }
            });
        }
    })
}














// Parameter count rides until the GatewayRequestCtx bundle planned for the
// serializer step of phase 4 collapses the shared request context.
















async fn handle_project_root(mut stream: DemuxStream, project_root: Option<PathBuf>) {
    use tokio::io::AsyncWriteExt;
    let body = project_root_response_body(project_root.as_deref());
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

async fn handle_settings_post(
    mut stream: DemuxStream,
    header_text: &str,
    bus: EventBus,
    project_root: Option<PathBuf>,
) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
    // Read POST body — may be partially or fully outside the peek buffer
    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
    let body_owned;
    let body_text = if peeked_body.len() >= content_length {
        crate::types::truncate_str(peeked_body, content_length)
    } else {
        let remaining = content_length.saturating_sub(peeked_body.len());
        let mut full = peeked_body.to_string();
        if remaining > 0 {
            let mut rest = vec![0u8; remaining];
            if stream.read_exact(&mut rest).await.is_ok() {
                full.push_str(&String::from_utf8_lossy(&rest));
            }
        }
        body_owned = full;
        &body_owned
    };
    let (status, result) =
        settings_post_result(body_text, project_root.as_deref(), &bus);
    let response = json_response(status, result);
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

async fn handle_settings_get(
    mut stream: DemuxStream,
    project_root: Option<PathBuf>,
    runtime_settings: RuntimeSettingsState,
) {
    use tokio::io::AsyncWriteExt;
    let body =
        settings_get_response_body(project_root.as_deref(), &runtime_settings)
            .await;
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

async fn handle_api_keys_post(mut stream: DemuxStream, header_text: &str) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
    let body_owned;
    let body_text = if peeked_body.len() >= content_length {
        crate::types::truncate_str(peeked_body, content_length)
    } else {
        let remaining = content_length.saturating_sub(peeked_body.len());
        let mut full = peeked_body.to_string();
        if remaining > 0 {
            let mut rest = vec![0u8; remaining];
            if stream.read_exact(&mut rest).await.is_ok() {
                full.push_str(&String::from_utf8_lossy(&rest));
            }
        }
        body_owned = full;
        &body_owned
    };
    let result = handle_set_api_keys(body_text);
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        result.len(),
        result
    );
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

async fn handle_api_key_status(mut stream: DemuxStream) {
    use tokio::io::AsyncWriteExt;
    let body = api_key_status_response_body();
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

async fn handle_external_agents(mut stream: DemuxStream, project_root: Option<PathBuf>) {
    use tokio::io::AsyncWriteExt;
    let body = external_agents_response_body(project_root.as_deref());
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

async fn handle_diagnostics_visual_freshness(
    mut stream: DemuxStream,
    header_text: &str,
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
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt};
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
    let content_length: usize = header_text
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let peeked_body = header_text.split("\r\n\r\n").nth(1).unwrap_or("");
    let body_owned;
    let body_bytes: &[u8] = if peeked_body.len() >= content_length {
        &peeked_body.as_bytes()[..content_length]
    } else {
        let remaining = content_length.saturating_sub(peeked_body.len());
        let mut full: Vec<u8> = peeked_body.as_bytes().to_vec();
        if remaining > 0 {
            let mut rest = vec![0u8; remaining];
            if stream.read_exact(&mut rest).await.is_ok() {
                full.extend_from_slice(&rest);
            }
        }
        body_owned = full;
        &body_owned
    };
    let (status_line, body) =
        match crate::diagnostics::append_visual_freshness_record(
            &session_id_raw,
            body_bytes,
        ) {
            Ok(written) => (
                "HTTP/1.1 200 OK",
                serde_json::json!({"ok": true, "written": written}).to_string(),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => (
                "HTTP/1.1 400 Bad Request",
                serde_json::json!({"error": e.to_string()}).to_string(),
            ),
            Err(e) => (
                "HTTP/1.1 500 Internal Server Error",
                serde_json::json!({"error": e.to_string()}).to_string(),
            ),
        };
    let response = format!(
        "{status_line}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}















// Parameter count rides until the GatewayRequestCtx bundle planned for the
// serializer step of phase 4 collapses the shared request context.


/// Build a `WebGatewayConfig` from the presence config's live fields,
/// falling back to environment variable detection.
///
/// Returns voice/runtime fields only. Daemon identity (host label,
/// version, git sha) lives on the Agent Card at
/// `/.well-known/agent-card.json` and is assembled at gateway spawn
/// time via [`build_local_agent_card`].
pub fn build_config(
    live_provider: Option<&str>,
    live_model: Option<&str>,
    transcription_enabled: bool,
    ice_config: crate::display::IceConfig,
    federation_allow_h264: bool,
) -> WebGatewayConfig {
    build_config_inner(
        live_provider,
        live_model,
        transcription_enabled,
        ice_config.ice_servers,
        federation_allow_h264,
    )
}

// ---------------------------------------------------------------------------
// /api/peers helpers
// ---------------------------------------------------------------------------


#[derive(Deserialize)]
struct AddPeerRequest {
    card_url: String,
    /// Optional display label override for this peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// Persist this manual add into `intendant.toml` after the live
    /// registration succeeds. Unchecked manual adds remain runtime-only.
    #[serde(default)]
    persist: bool,
    /// Optional connecting-side override for the peer's transport
    /// URLs. When non-empty, the card's `transports` field is
    /// replaced with one `IntendantWs` entry per URL. Lets the
    /// operator route around topologies the advertising peer's card
    /// doesn't know about (port-forwards, proxies, named tunnels).
    /// `#[serde(default)]` so older clients without this field
    /// continue to work.
    #[serde(default)]
    via_urls: Vec<String>,
    /// Optional outbound bearer token sent to this peer (the
    /// `[[peer]] bearer_token` equivalent for dashboard-added
    /// peers). When set, sent on the agent-card fetch and the
    /// WebSocket upgrade. Required when the peer's card declares
    /// `auth.application = Some(Bearer)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bearer_token: Option<String>,
    /// Optional operator-supplied pinned cert fingerprints. When
    /// non-empty, REPLACES whatever the peer's card declares for
    /// `auth.transport` — eliminates the TOFU window when the
    /// operator got the fingerprint out-of-band. Same wire format
    /// as the card's: lowercase hex with optional `:` separators.
    #[serde(default)]
    pinned_fingerprints: Vec<String>,
    /// Explicit URL the **browser** uses to reach this peer's HTTP
    /// port for WebRTC ICE-TCP. When set, the dashboard uses this
    /// (not `d.ws_url`) as the `advertise_tcp_via_url` hint in the
    /// federated WebRTC offer. Decouples the browser-side URL from
    /// the via URL the primary uses for federation, which matters
    /// when the two network positions differ (primary-side localhost
    /// tunnel, browser on a different machine, etc.). `None` falls
    /// back to the slice 3a.2 behavior of using the primary's via URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    browser_tcp_via_url: Option<String>,
}







#[derive(Deserialize)]
struct RemovePeerRequest {
    peer_id: String,
}















































fn trimmed_nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn persist_manual_peer(
    project_root: &Path,
    req: &AddPeerRequest,
    label: Option<String>,
) -> Result<PathBuf, crate::error::CallerError> {
    let mut project = crate::project::Project::from_root(project_root.to_path_buf())?;
    let existing = project
        .config
        .peers
        .iter_mut()
        .find(|peer| peer.card_url == req.card_url);
    match existing {
        Some(peer) => {
            if label.is_some() {
                peer.label = label;
            }
            peer.bearer_token = req.bearer_token.clone();
            peer.via_urls = req.via_urls.clone();
            peer.pinned_fingerprints = req.pinned_fingerprints.clone();
            peer.browser_tcp_via_url = req.browser_tcp_via_url.clone();
        }
        None => {
            project.config.peers.push(crate::project::PeerConfig {
                card_url: req.card_url.clone(),
                label,
                bearer_token: req.bearer_token.clone(),
                via_urls: req.via_urls.clone(),
                client_cert: None,
                client_key: None,
                pinned_fingerprints: req.pinned_fingerprints.clone(),
                browser_tcp_via_url: req.browser_tcp_via_url.clone(),
            });
        }
    }
    project.save_config()?;
    Ok(project.root.join("intendant.toml"))
}





fn target_card_url_from_request(header_text: &str, is_tls: bool) -> Option<String> {
    let host = header_text
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("host"))
        })
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())?;
    let scheme = if is_tls { "https" } else { "http" };
    Some(format!(
        "{scheme}://{}{}",
        host.trim_end_matches('/'),
        crate::peer::pairing::AGENT_CARD_PATH
    ))
}










fn identity_summary_json(
    record: crate::peer::access_policy::PeerIdentityRecord,
) -> serde_json::Value {
    // `active` mirrors the gateway auth gate (approved AND unexpired), so
    // an org-materialized identity past its expiry reads as inert here —
    // the raw status/expiry/provenance fields let the UI say why.
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    serde_json::json!({
        "fingerprint": record.fingerprint,
        "label": record.label,
        "profile": record.profile,
        "status": record.status,
        "active": record.is_active(now_unix),
        "card_url": record.card_url,
        "request_id": record.request_id,
        "created_at_unix": record.created_at_unix,
        "revoked_at_unix": record.revoked_at_unix,
        "expires_at_unix": record.expires_at_unix,
        "source": record.source,
        "org_grant_id": record.org_grant_id,
        "issued_via": record.issued_via,
    })
}






// ---------------------------------------------------------------------------
// Per-peer outbound op handlers
// ---------------------------------------------------------------------------
//
// These three endpoints let the dashboard drive the read-write peer
// transport directly. Each maps a JSON body to the matching
// [`crate::peer::PeerHandle`] method:
//
//   POST /api/peers/{id}/message  →  PeerHandle::send_message
//   POST /api/peers/{id}/task     →  PeerHandle::delegate_task
//   POST /api/peers/{id}/approval →  PeerHandle::resolve_approval
//
// Error model (uniform across the three):
//
//   400  bad JSON / missing required field
//   404  peer not in registry
//   405  peer's transport doesn't support this op (UnsupportedCapability)
//   502  transport-level failure (NotConnected, Transport, Auth, …)
//   500  catch-all for unexpected errors
//
// Status codes pick a meaningful HTTP semantic per [`PeerError`] variant
// rather than collapsing everything to 502 — the dashboard renders a
// different message for "wrong peer kind" vs "peer is offline".

/// Shared body for `POST /api/peers/{id}/message`.
///
/// Two equivalent shapes accepted:
///
/// 1. Shorthand: `{"text": "hello"}` — implicit user role + Text content.
/// 2. Full:     `{"role": "user", "content": {"type": "text", "text": "hello"}, "session": null}`.
///
/// The `content` field, when present, wins over `text`. Either `text`
/// or `content` is required; everything else is optional.
#[derive(Deserialize)]
struct SendMessageRequest {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    role: Option<crate::peer::MessageRole>,
    #[serde(default)]
    content: Option<crate::peer::MessageContent>,
    #[serde(default)]
    session: Option<String>,
}

impl SendMessageRequest {
    fn into_message(self) -> Result<crate::peer::PeerMessage, String> {
        let role = self.role.unwrap_or(crate::peer::MessageRole::User);
        let content = match (self.content, self.text) {
            (Some(c), _) => c,
            (None, Some(t)) => crate::peer::MessageContent::Text { text: t },
            (None, None) => {
                return Err("either 'text' or 'content' is required".to_string());
            }
        };
        Ok(crate::peer::PeerMessage {
            session: self.session,
            role,
            content,
        })
    }
}

#[derive(Deserialize)]
struct DelegateTaskRequest {
    instructions: String,
    #[serde(default)]
    context: serde_json::Value,
    #[serde(default)]
    client_correlation_id: Option<String>,
}

#[derive(Deserialize)]
struct ResolveApprovalRequest {
    request_id: String,
    decision: crate::peer::ApprovalDecision,
}









/// Slice 3b: rewrite an outgoing federated `WebRtcSignal::Answer` to
/// (a) register the peer's ICE ufrag in the relay registry and
/// (b) inject a TCP candidate pointing at the primary's own address
/// alongside the peer's direct candidate.
///
/// After the rewrite, a browser receiving this Answer has two TCP
/// candidates: the peer's direct TCP candidate (if the peer provided
/// one via `advertise_tcp_via_url`) and the primary's relay
/// candidate. Browser ICE tries both and uses whichever forms first.
/// Because the relay candidate is emitted with a lower priority
/// (see `inject_relay_tcp_candidate`), direct wins on reachable
/// topologies and relay is the fallback.
///
/// Non-Answer events pass through verbatim. Events with malformed
/// SDPs, missing ufrags, or a peer URL that can't be resolved fall
/// through without rewriting — the browser still sees the original
/// Answer, just without the relay candidate.
async fn maybe_rewrite_federated_answer(
    peer: &crate::peer::PeerId,
    event: crate::peer::PeerEvent,
    registry: &crate::peer::PeerRegistry,
    relay_registry: &Arc<crate::display::webrtc::TcpRelayRegistry>,
    relay_advertise_url: Option<&str>,
    bus: &EventBus,
) -> crate::peer::PeerEvent {
    const LOG_SOURCE: &str = "webrtc-peer";

    // Match only the specific variant that carries an Answer SDP; all
    // other event variants (Log, Usage, ActivityStarted, IceCandidate,
    // etc.) pass through unchanged.
    let (display_id, session_id, sdp) = match &event {
        crate::peer::PeerEvent::WebRtcSignal {
            display_id,
            session_id,
            signal: crate::peer::WebRtcSignal::Answer { sdp, .. },
        } => (*display_id, session_id.clone(), sdp.clone()),
        _ => return event,
    };

    // Extract the peer's ICE ufrag from the Answer SDP. Without it we
    // can't key the relay registry, so we skip rewriting and let the
    // browser try whatever direct candidate the peer advertised.
    let ufrag = match crate::display::webrtc::parse_sdp_ice_ufrag(&sdp) {
        Some(u) => u,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     Answer SDP missing a=ice-ufrag attribute"
                ),
                turn: None,
            });
            return event;
        }
    };

    // Resolve the peer's outbound TCP address — where the primary
    // will dial when it sees a relay-destined TCP connection. Prefer
    // `browser_tcp_via_url` (operator's split-browser-side URL) then
    // fall back to `ws_url` (primary-side via URL). In the typical
    // co-located case the two are the same; in split topologies the
    // operator uses browser_tcp_via_url to point at where they'd
    // like the BROWSER to reach the peer. Here we're dialing FROM
    // the primary, but the primary typically shares the LAN position
    // of the operator's browser-reachable URL when one is set.
    let outbound_url = registry.get(peer).and_then(|h| {
        let snap = h.snapshot();
        snap.browser_tcp_via_url.or(snap.ws_url)
    });
    let outbound_url = match outbound_url {
        Some(u) => u,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     no outbound URL on the peer's snapshot (peer removed mid-Answer?)"
                ),
                turn: None,
            });
            return event;
        }
    };
    let outbound_addr = match resolve_url_to_socket_addr(&outbound_url).await {
        Some(addr) => addr,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     outbound URL {outbound_url:?} didn't resolve to a SocketAddr"
                ),
                turn: None,
            });
            return event;
        }
    };
    relay_registry.register(ufrag.clone(), outbound_addr);

    // Resolve the primary's own relay URL into a SocketAddr we can
    // put in an SDP candidate line. When the primary has no
    // advertised URL we can work with (local_addr() was None at
    // spawn, headless mode, etc), skip injection and just forward
    // the Answer unchanged — the browser still has the peer's
    // direct candidate to try.
    let primary_relay_addr = match relay_advertise_url {
        Some(url) => match resolve_url_to_socket_addr(url).await {
            Some(addr) => addr,
            None => {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "registered ufrag={ufrag} outbound={outbound_addr} for peer={peer} session={session_id} \
                         but can't inject relay candidate — primary's own URL {url:?} doesn't resolve"
                    ),
                    turn: None,
                });
                return event;
            }
        },
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "registered ufrag={ufrag} outbound={outbound_addr} for peer={peer} session={session_id} \
                     but no primary relay URL configured — skipping candidate injection"
                ),
                turn: None,
            });
            return event;
        }
    };

    let rewritten_sdp =
        crate::display::webrtc::inject_relay_tcp_candidate(&sdp, primary_relay_addr);
    bus.send(AppEvent::LogEntry {
        session_id: None,
        level: "info".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!(
            "relay registered ufrag={ufrag} peer={peer} session={session_id} \
             primary_relay={primary_relay_addr} outbound={outbound_addr}"
        ),
        turn: None,
    });

    crate::peer::PeerEvent::WebRtcSignal {
        display_id,
        session_id,
        signal: crate::peer::WebRtcSignal::Answer {
            sdp: rewritten_sdp,
            binding: None,
        },
    }
}

/// Parse a WebSocket / HTTP URL and resolve it to a [`SocketAddr`].
///
/// Used to convert the browser's view of a peer's HTTP port (the
/// `advertise_tcp_via_url` hint in a federated
/// [`crate::peer::WebRtcSignal::Offer`]) into the concrete address
/// the peer advertises in its ICE-TCP candidate.
///
/// Accepts `ws://` / `wss://` / `http://` / `https://` schemes (all
/// produce the same authority shape). The host can be an IPv4
/// literal, a bracketed IPv6 literal, or a hostname — hostnames are
/// resolved via [`tokio::net::lookup_host`] and the first returned
/// address is used. The port must be explicit; there's no default-
/// port fallback, because we can't know what the peer's HTTP
/// listener bound to without being told.
///
/// Returns `None` on any parse or resolution failure. Callers treat
/// that as "no TCP candidate, UDP-only path" — the same behavior as
/// slice 3a's pre-3a.2 baseline.
async fn resolve_url_to_socket_addr(url: &str) -> Option<std::net::SocketAddr> {
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("http://"))?;
    // Strip any path / query that follows the authority. Authority
    // for an IPv6 literal is `[::1]:8766`, which contains neither
    // `/` nor `?` inside the brackets, so split-on-first is safe.
    let authority = rest.split(['/', '?']).next()?;
    // Fast path for `ipv4:port` or `[ipv6]:port`: parse directly.
    if let Ok(addr) = authority.parse::<std::net::SocketAddr>() {
        return Some(addr);
    }
    // Hostname:port — needs DNS. `lookup_host` accepts `host:port`
    // and returns the resolved SocketAddrs in OS-chosen order; first
    // is the winner (matches what the kernel would pick for a
    // regular connect()).
    tokio::net::lookup_host(authority).await.ok()?.next()
}







// ---------------------------------------------------------------------------
// Coordinator endpoints — capability-based discovery + delegation
// ---------------------------------------------------------------------------

/// Parse `?capability=display&capability=custom:foo` into a typed
/// `Vec<Capability>` plus a list of unknown strings (for diagnostics).
/// Empty input returns `(vec![], vec![])` — empty-required-capabilities
/// matches every peer, which the handler rejects upstream.
fn parse_capability_query(query: &str) -> (Vec<crate::peer::Capability>, Vec<String>) {
    let mut caps = Vec::new();
    let mut unknown = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k != "capability" {
            continue;
        }
        match crate::peer::Capability::from_query_string(v) {
            Some(cap) => caps.push(cap),
            None => unknown.push(v.to_string()),
        }
    }
    (caps, unknown)
}





/// True for HTTP requests that hit the federation REST surface:
/// `/api/peers*`, `/api/coordinator/*`, `/api/sessions`, and
/// `/api/worktrees`. These
/// are the endpoints the bearer-token enforcement layer protects
/// when `[server.auth] bearer_token` is set. Discovery
/// (`/.well-known/agent-card.json`), browser bootstrap (`/config`,
/// `/`, `/static/*`), and `/ws` are exempt — see
/// `spawn_web_gateway::inbound_bearer_token` docs for why.
fn is_federation_path(request_line: &str) -> bool {
    let (_, path, _) = parse_request_target(request_line);
    path_is_or_under(path, "/api/peers")
        || path.starts_with("/api/coordinator/")
        || path_is_or_under(path, "/api/sessions")
        || path_is_or_under(path, "/api/worktrees")
}


fn dashboard_http_operation(
    req_method: &str,
    req_path: &str,
) -> Option<crate::peer::access_policy::PeerOperation> {
    // Pure table lookup: every dispatched route is declared in
    // gateway_routes::ROUTES with its IAM operation, and an undeclared
    // (method, path) is not a route — nothing to gate. (The hand-written
    // match this function used to be lived and died with the route-table
    // migration; the invariants in gateway_routes.rs hold in its place.)
    match crate::gateway_routes::classify(req_method, req_path) {
        crate::gateway_routes::TableClassification::Matched(op) => op,
        crate::gateway_routes::TableClassification::NoMatch => None,
    }
}

fn http_access_forbidden_response(
    access: &HttpAccessContext,
    decision: crate::access::iam::AccessDecision,
) -> String {
    json_response(
        "403 Forbidden",
        serde_json::json!({
            "error": "principal does not allow this operation",
            "principal": access.principal.as_value(),
            "permission": decision.permission,
            "reason": decision.reason,
        })
        .to_string(),
    )
}














fn is_public_connect_bootstrap_path(request_line: &str) -> bool {
    let Some(path) = request_line.split_whitespace().nth(1) else {
        return false;
    };
    let path = path.split('?').next().unwrap_or(path);
    matches!(
        path,
        "/connect/bootstrap"
            | "/connect/status"
            | "/connect/dashboard/offer"
            | "/connect/dashboard/ice"
            | "/connect/dashboard/close"
    )
}




fn peer_identity_allows_ws_control(
    identity: Option<&PeerConnectionIdentity>,
    ctrl: &ControlMsg,
    bus: &EventBus,
) -> bool {
    let Some(identity) = identity else {
        return true;
    };
    // The dashboard-control tunnel is multi-capability; its signaling relay
    // opens for any profile that can use something inside it, and every
    // method/frame is then individually authorized on this same identity.
    if matches!(ctrl, ControlMsg::PeerDashboardControlSignal { .. }) {
        if crate::peer::access_policy::profile_allows_dashboard_control_tunnel(&identity.profile) {
            return true;
        }
        bus.send(AppEvent::PresenceLog {
            message: format!(
                "[ws] denied peer dashboard-control signaling from {}: profile={} allows no tunnel capability",
                identity.label, identity.profile,
            ),
            level: Some(LogLevel::Warn),
            turn: None,
        });
        return false;
    }
    let op = crate::peer::access_policy::control_msg_operation(ctrl);
    let decision = crate::access::iam::evaluate_principal_operation(
        &peer_identity_access_principal(identity, "peer-ws"),
        op,
    );
    if decision.allowed {
        return true;
    }
    bus.send(AppEvent::PresenceLog {
        message: format!(
            "[ws] denied peer control frame from {}: profile={} permission={} reason={}",
            identity.label, identity.profile, decision.permission, decision.reason,
        ),
        level: Some(LogLevel::Warn),
        turn: None,
    });
    false
}

/// Map a typed `/ws` frame to the `PeerOperation` it exercises — the
/// direct-WebSocket mirror of `dashboard_control_frame_operation` and
/// the `CONTROL_METHODS` table (dashboard_control.rs), so the same
/// IAM grant answers the same way whichever transport a client speaks.
/// `None` means the frame carries no authority of its own: replies, pings,
/// and the `dashboard_control_*` signaling frames (the tunnel they establish
/// enforces this very grant per-frame itself, and scoped clients must be
/// able to reach their allowed surface through it).
fn ws_frame_operation(frame_type: &str) -> Option<crate::peer::access_policy::PeerOperation> {
    use crate::peer::access_policy::PeerOperation;
    match frame_type {
        // Same frame names as the dashboard-control tunnel table. Floor
        // operations: terminal_open may additionally require shell.spawn
        // (when the session doesn't exist yet) and every terminal frame is
        // scoped to sessions the actor can see — both enforced statefully
        // in the frame handlers.
        "terminal_open" => Some(PeerOperation::TerminalView),
        "terminal_input" | "terminal_resize" | "terminal_close" | "terminal_share" => {
            Some(PeerOperation::TerminalWrite)
        }
        "display_input" => Some(PeerOperation::DisplayInput),
        // Parity: api_diagnostics_visual_freshness → DisplayInput. The
        // marker is stamped pre-encoder and lands in every viewer's stream,
        // so it is display mutation, not viewing.
        "set_diagnostics_visual_marker" => Some(PeerOperation::DisplayInput),
        // Parity: api_display_bootstrap / api_display_webrtc_signal.
        "display_offer" | "display_ice" => Some(PeerOperation::DisplayView),
        // The embedded web TUI drives the daemon's own runtime — the direct
        // twins of the tunnel's tui_* frames.
        "key" | "resize" | "term_subscribe" | "term_unsubscribe" => {
            Some(PeerOperation::RuntimeControl)
        }
        // Live voice/media session machinery. Parity: api_voice_session,
        // api_presence_video_frame, api_media_annotation_*, api_media_clip_*.
        "presence_connect"
        | "presence_disconnect"
        | "make_active"
        | "user_audio"
        | "video_frame"
        | "voice_log"
        | "voice_diagnostic"
        | "presence_checkpoint"
        | "live_usage_update"
        | "annotation_attach"
        | "annotation_submit"
        | "clip_start"
        | "clip_frame"
        | "clip_end" => Some(PeerOperation::RuntimeControl),
        // Presence tool dispatch. Parity: api_mcp_tool_call → Message.
        "tool_request" | "async_query" => Some(PeerOperation::Message),
        _ => None,
    }
}

/// Per-frame IAM gate for the direct `/ws` path. Returns `true` when the
/// frame was denied and fully handled — a denial frame has been sent (plus
/// the pane-visible `terminal_error` shape for terminal frames) and a
/// once-per-frame-type warning logged — so the caller drops the frame.
/// Root-equivalent grants (plain local dashboards, unbound mTLS root
/// certificates) short-circuit to allow inside the evaluator; the check is
/// pure in-memory, safe at keystroke/audio-frame rates.
fn deny_ws_frame_if_unauthorized(
    grant: &crate::dashboard_control::DashboardControlGrant,
    json: &serde_json::Value,
    direct_tx: &mpsc::UnboundedSender<String>,
    bus: &EventBus,
    logged_denials: &mut std::collections::HashSet<String>,
) -> bool {
    let Some(frame_type) = json.get("t").and_then(|v| v.as_str()) else {
        return false;
    };
    let Some(op) = ws_frame_operation(frame_type) else {
        return false;
    };
    let decision = grant.access_decision(op);
    if decision.allowed {
        return false;
    }
    if frame_type.starts_with("terminal_") {
        let err = serde_json::json!({
            "t": "terminal_error",
            "host_id": json.get("host_id").and_then(|v| v.as_str()).unwrap_or("local"),
            "terminal_id": json.get("terminal_id").and_then(|v| v.as_str()).unwrap_or(""),
            "error": format!("not allowed: {}", decision.reason),
        });
        let _ = direct_tx.send(err.to_string());
    }
    let denied = serde_json::json!({
        "t": "ws_denied",
        "frame": frame_type,
        "permission": decision.permission,
        "reason": decision.reason,
    });
    let _ = direct_tx.send(denied.to_string());
    if logged_denials.insert(frame_type.to_string()) {
        bus.send(AppEvent::PresenceLog {
            message: format!(
                "[ws] denied {frame_type} frame for {}: permission={} reason={}",
                grant.wire_kind(),
                decision.permission,
                decision.reason,
            ),
            level: Some(LogLevel::Warn),
            turn: None,
        });
    }
    true
}

/// Grant-lane twin of `peer_identity_allows_ws_control` for the ControlMsg
/// fall-through on the direct `/ws` path: peer connections keep their
/// identity-based gate (which already ran in the preceding match guard),
/// every other connection answers to its dashboard-control grant through
/// the same ControlMsg→operation table the peer lane uses.
fn ws_grant_allows_control(
    grant: &crate::dashboard_control::DashboardControlGrant,
    peer_identity: Option<&PeerConnectionIdentity>,
    ctrl: &ControlMsg,
    bus: &EventBus,
) -> bool {
    if peer_identity.is_some() {
        return true;
    }
    // Relaying signaling to a connected peer delegates THIS daemon's peer
    // identity — the receiving peer authorizes the tunnel against its
    // grants for this daemon, not against the human grant that asked for
    // the relay. That delegation is its own named permission (peer.use),
    // never inferred from local capabilities.
    if matches!(
        ctrl,
        ControlMsg::PeerDashboardControlSignal { .. } | ControlMsg::PeerFileTransferSignal { .. }
    ) {
        let decision = grant.access_decision(crate::peer::access_policy::PeerOperation::PeerUse);
        if decision.allowed {
            return true;
        }
        bus.send(AppEvent::PresenceLog {
            message: format!(
                "[ws] denied {} peer signaling relay: permission={} reason={}",
                grant.wire_kind(),
                decision.permission,
                decision.reason,
            ),
            level: Some(LogLevel::Warn),
            turn: None,
        });
        return false;
    }
    let op = crate::peer::access_policy::control_msg_operation(ctrl);
    let decision = grant.access_decision(op);
    if decision.allowed {
        return true;
    }
    bus.send(AppEvent::PresenceLog {
        message: format!(
            "[ws] denied {} control frame: permission={} reason={}",
            grant.wire_kind(),
            decision.permission,
            decision.reason,
        ),
        level: Some(LogLevel::Warn),
        turn: None,
    });
    false
}


/// Verify a WebSocket upgrade request carries the expected bearer
/// token. Browser WebSocket clients cannot natively set custom
/// headers on `WebSocket` opens, so this accepts the token in EITHER
/// an `Authorization: Bearer <token>` header (sent by
/// `IntendantWsTransport` from the daemon side) OR a `?token=...`
/// URL query parameter (sent by the browser dashboard). The dual
/// path is the standard pragmatic workaround for the browser
/// limitation.
///
/// Returns `Ok(())` when no token is required (no
/// `inbound_bearer_token` configured) or when the request presents
/// the matching token via either method. Returns `Err((401, body))`
/// otherwise — the caller writes a plain HTTP 401 response *before*
/// the WebSocket handshake and returns, so the rejected client never
/// sees a successful upgrade.
pub(crate) fn verify_bearer_for_ws(
    header_text: &str,
    expected_token: Option<&str>,
) -> Result<(), (u16, String)> {
    let Some(expected) = expected_token else {
        return Ok(());
    };

    // Try the Authorization header first (cheaper and the daemon-to-
    // daemon path uses it). On miss, fall back to the URL query.
    if verify_bearer_token(header_text, Some(expected)).is_ok() {
        return Ok(());
    }

    let request_line = header_text.lines().next().unwrap_or("");
    if extract_token_query_param(request_line).as_deref() == Some(expected) {
        return Ok(());
    }

    Err((
        401,
        serde_json::json!({
            "error": "missing or invalid bearer token (Authorization header or ?token=)"
        })
        .to_string(),
    ))
}

/// Verify a federation HTTP request carries the expected bearer
/// token in the `Authorization` header. Header name lookup is
/// case-insensitive per the HTTP spec; the `Bearer` scheme prefix
/// match accepts either case.
///
/// Returns `Ok(())` when no token is required (no
/// `inbound_bearer_token` configured) or when the request presents
/// the matching token. Returns `Err((401, body_json))` otherwise —
/// the caller writes that response and returns.
pub(crate) fn verify_bearer_token(
    header_text: &str,
    expected_token: Option<&str>,
) -> Result<(), (u16, String)> {
    let Some(expected) = expected_token else {
        return Ok(());
    };
    let auth_header = header_text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("authorization") {
            Some(value.trim().to_string())
        } else {
            None
        }
    });
    let auth = match auth_header {
        Some(v) => v,
        None => {
            return Err((
                401,
                serde_json::json!({"error": "missing Authorization header"}).to_string(),
            ));
        }
    };
    let token = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "));
    let token = match token {
        Some(t) => t.trim(),
        None => {
            return Err((
                401,
                serde_json::json!({
                    "error": "Authorization header must use Bearer scheme"
                })
                .to_string(),
            ));
        }
    };
    if token == expected {
        Ok(())
    } else {
        Err((
            401,
            serde_json::json!({"error": "invalid bearer token"}).to_string(),
        ))
    }
}

/// Resolve the list of WebSocket URLs to advertise in the Agent
/// Card for this daemon, in preference order.
///
/// **Additive auto-detection.** Mirrors WebRTC's host-candidate
/// gathering pattern: the daemon enumerates its own routable
/// interfaces via [`crate::access::routable_local_addrs`] and emits one
/// URL per address by default, so the operator doesn't need to type
/// their own LAN IP into `--advertise-url`. The operator's overrides
/// (CLI `--advertise-url` or `[server.advertise]` in intendant.toml)
/// are *prepended* — they win on preference order, but the auto-
/// detected entries still ride along as fallbacks. The connecting
/// peer's `MultiTransport::connect` walks the merged list top-down
/// and picks the first that succeeds.
///
/// ## Bind-address rules
///
/// - **Specific bind** (e.g. `192.168.1.42:8765`): only that one IP
///   is auto-detected. The operator narrowed the listener for a
///   reason; we don't second-guess by also enumerating other
///   interfaces.
/// - **Wildcard bind** (`0.0.0.0` / `::`): every routable interface
///   becomes one URL. Loopback is excluded — advertising loopback to
///   remote peers is useless. If the operator wants to expose
///   loopback (e.g. for self-peering tests), they can pass it via
///   `--advertise-url`.
///
/// ## Fallbacks (in order, when auto-detection finds nothing)
///
/// 1. Resolved host label ([`crate::access::resolve_host_label`]) —
///    works on a trusted LAN with mDNS, fragile elsewhere. Last-
///    ditch best-effort.
/// 2. `ws://localhost:0/ws` if there's no listener at all
///    (shouldn't happen in practice; the listener is always bound by
///    the time spawn is called). Card stays valid; URL won't work.
///
/// Dedup: exact-string match. If the operator's override happens to
/// match an auto-detected URL, only the operator's copy is kept.
///
/// ## Scheme
///
/// `tls_enabled` selects the auto-detected URL scheme: `wss://` when the
/// dashboard is served over TLS (`--tls` / `[server.tls]`), `ws://`
/// otherwise. This keeps advertised peer URLs honest — a TLS daemon is
/// HTTPS/WSS-only (see the strict-TLS demux in `spawn_web_gateway`), so a
/// peer handed a `ws://` URL would be refused. Operator overrides are
/// taken verbatim (the operator owns their scheme) and the final
/// no-listener fallback tracks the flag too.
pub(crate) fn resolve_advertise_urls(
    local_addr: Option<std::net::SocketAddr>,
    overrides: &[String],
    tls_enabled: bool,
) -> Vec<String> {
    let port = local_addr.map(|a| a.port()).unwrap_or(0);

    // Auto-detect. Operator overrides come first; auto entries append.
    let auto = auto_detect_advertise_urls(local_addr, port, tls_enabled);

    let mut out: Vec<String> = Vec::with_capacity(overrides.len() + auto.len());
    for url in overrides {
        if !out.contains(url) {
            out.push(url.clone());
        }
    }
    for url in auto {
        if !out.contains(&url) {
            out.push(url);
        }
    }

    if out.is_empty() {
        // No bind, no overrides, no interfaces. Card stays valid;
        // URL just won't work until the next daemon restart. Match the
        // TLS scheme so even this degenerate fallback is scheme-honest.
        out.push(format_ws_url("localhost", 0, tls_enabled));
    }
    out
}

/// Build the auto-detected URL list from the listener bind address.
/// See [`resolve_advertise_urls`] for the full resolution rules.
/// `tls_enabled` selects `wss://` vs `ws://` (see that fn's docstring).
fn auto_detect_advertise_urls(
    local_addr: Option<std::net::SocketAddr>,
    port: u16,
    tls_enabled: bool,
) -> Vec<String> {
    use std::net::IpAddr;
    let Some(addr) = local_addr else {
        return Vec::new();
    };

    // Specific bind: that one IP wins, no enumeration.
    match addr.ip() {
        IpAddr::V4(v4) if !v4.is_unspecified() => {
            return vec![format_ws_url(&v4.to_string(), port, tls_enabled)];
        }
        IpAddr::V6(v6) if !v6.is_unspecified() => {
            return vec![format_ws_url(&format!("[{v6}]"), port, tls_enabled)];
        }
        _ => {}
    }

    // Wildcard bind: enumerate every non-loopback routable interface.
    // IPv4 entries sort before IPv6 — WebRTC ICE-TCP in WebKit/WKWebView
    // silently drops IPv6 ULA candidates (seen empirically against
    // fdc2::/8 addresses on macOS 15), so the *first* URL in the list
    // — which slice 3b's `maybe_rewrite_federated_answer` takes as the
    // relay candidate verbatim — needs to be the one browsers actually
    // dial. Within each address family we preserve `getifaddrs` order
    // (`stable_sort_by`), so a multi-NIC host that already had a
    // preferred primary interface keeps it.
    let mut ips = crate::access::routable_local_addrs(false);
    ips.sort_by(|a, b| match (a, b) {
        (IpAddr::V4(_), IpAddr::V6(_)) => std::cmp::Ordering::Less,
        (IpAddr::V6(_), IpAddr::V4(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    let mut urls: Vec<String> = ips
        .into_iter()
        .map(|ip| match ip {
            IpAddr::V6(v6) => format_ws_url(&format!("[{v6}]"), port, tls_enabled),
            ip => format_ws_url(&ip.to_string(), port, tls_enabled),
        })
        .collect();

    // No interfaces found (unusual — host with no networking?). Fall
    // back to the resolved host label so the card carries *something*
    // dialable on a trusted LAN with mDNS.
    if urls.is_empty() {
        urls.push(format_ws_url(
            &crate::access::resolve_host_label(),
            port,
            tls_enabled,
        ));
    }
    urls
}

/// Format one advertised WebSocket URL. `tls_enabled` picks the secure
/// scheme (`wss://`) so a TLS daemon never advertises a `ws://` URL a peer
/// would be refused on.
fn format_ws_url(host: &str, port: u16, tls_enabled: bool) -> String {
    let scheme = if tls_enabled { "wss" } else { "ws" };
    format!("{scheme}://{host}:{port}/ws")
}

/// Assemble the [`crate::peer::AgentCard`] for this daemon from live
/// runtime state.
///
/// Called once per `spawn_web_gateway` invocation, right after the
/// config is serialized — the result is cached as `agent_card_json`
/// and cloned into each per-connection handler, matching the pattern
/// used for `/config`.
///
/// Capabilities:
/// - `ComputerUse`, `Knowledge`, `Display` are always-on subsystems
///   compiled into every build and always able to service a federation
///   request (for `Display`, that's `DisplaySession::handle_offer`
///   against whatever the local dashboard has activated — returns
///   "no such display" if nothing is active, which is the correct
///   semantics for a peer trying to view a display the operator
///   hasn't opened yet).
/// - `Voice` / `Phone` / `Recording` are gated on runtime configuration
///   that isn't plumbed through here yet. Those become additive as
///   each subsystem teaches itself to advertise, likely via dynamic
///   `PeerEvent::CapabilityEngaged` once slice 3a.2 lands.
///
/// `advertise_urls` is the preference-ordered list of WebSocket URLs
/// peers should try when dialing this daemon. Each becomes a
/// [`crate::peer::TransportSpec::IntendantWs`] entry in the card.
/// Built by [`resolve_advertise_urls`], which merges operator
/// overrides (`--advertise-url`, `[server.advertise]`) with auto-
/// detected fallback. The list is non-empty by construction.
///
/// `auth` is the [`crate::peer::AuthRequirements`] to advertise —
/// what connecting peers should send. Built by
/// `crate::main::build_local_advertised_auth` from
/// `[server.auth]` (advertised_transport + bearer_token) and the
/// access cert dir (for `pin-self-cert` fingerprint). Phase 1 of slice
/// 2c always passed `AuthRequirements::none()`; this signature
/// change lets the operator advertise mTLS / pinned-mTLS / bearer
/// in the card so connecting peers know what to send.
pub fn build_local_agent_card(
    advertise_urls: Vec<String>,
    auth: crate::peer::AuthRequirements,
) -> crate::peer::AgentCard {
    use crate::peer::{Capability, TransportSpec};
    let transports: Vec<TransportSpec> = advertise_urls
        .into_iter()
        .map(|url| TransportSpec::IntendantWs { url })
        .collect();
    crate::peer::AgentCard::local_intendant(
        crate::access::resolve_host_label(),
        env!("CARGO_PKG_VERSION").to_string(),
        Some(env!("INTENDANT_GIT_SHA").to_string()),
        transports,
        vec![
            Capability::ComputerUse,
            Capability::Knowledge,
            Capability::Display,
        ],
        auth,
    )
}

fn build_config_inner(
    live_provider: Option<&str>,
    live_model: Option<&str>,
    transcription_enabled: bool,
    ice_servers: Vec<crate::display::IceServer>,
    federation_allow_h264: bool,
) -> WebGatewayConfig {
    // If an explicit provider is given, use it directly.
    if let Some(provider) = live_provider {
        let model = live_model.unwrap_or(match provider {
            "openai" => "gpt-4o-realtime-preview",
            _ => "gemini-2.5-flash-native-audio-preview-12-2025",
        });
        let (input_rate, output_rate) = if provider == "openai" {
            (24000, 24000)
        } else {
            (16000, 24000)
        };
        return WebGatewayConfig {
            provider: provider.to_string(),
            model: model.to_string(),
            input_sample_rate: input_rate,
            output_sample_rate: output_rate,
            transcription_enabled,
            ice_servers,
            federation_allow_h264,
            ..Default::default()
        };
    }

    // If an explicit live model is given, detect provider from the model name.
    if let Some(model) = live_model {
        if model.starts_with("gpt")
            || model.starts_with("o1")
            || model.starts_with("o3")
            || model.starts_with("o4")
        {
            return WebGatewayConfig {
                provider: "openai".to_string(),
                model: model.to_string(),
                input_sample_rate: 24000,
                output_sample_rate: 24000,
                transcription_enabled,
                ice_servers,
                federation_allow_h264,
                ..Default::default()
            };
        }
        return WebGatewayConfig {
            provider: "gemini".to_string(),
            model: model.to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
            transcription_enabled,
            ice_servers,
            federation_allow_h264,
            ..Default::default()
        };
    }

    // Fall back to usable-key detection (leases shadow env vars).
    if crate::credential_leases::provider_api_key("OPENAI_API_KEY").is_some()
        && crate::credential_leases::provider_api_key("GEMINI_API_KEY").is_none()
    {
        WebGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
            transcription_enabled,
            ice_servers,
            federation_allow_h264,
            ..Default::default()
        }
    } else {
        WebGatewayConfig {
            transcription_enabled,
            ice_servers,
            federation_allow_h264,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OutboundEvent;
    use tokio::io::AsyncWriteExt;

    static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());


    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
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


    async fn next_ws_json_matching<S, F>(ws_rx: &mut S, mut matches: F) -> serde_json::Value
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

    async fn next_ws_json_type<S>(ws_rx: &mut S, ty: &str) -> serde_json::Value
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        next_ws_json_matching(ws_rx, |json| json["t"] == ty).await
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
    fn settings_payload_accepts_settings_tab_save_without_agent_runtime_fields() {
        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex"
        })
        .to_string();

        let payload: SettingsPayload = serde_json::from_str(&body).unwrap();
        assert_eq!(payload.external_agent.as_deref(), Some("codex"));
        assert_eq!(payload.codex_sandbox, None);
        assert_eq!(payload.codex_approval_policy, None);
        assert_eq!(payload.codex_managed_context, None);

        let mut config = crate::project::ProjectConfig::default();
        config.agent.codex.command = "/opt/codex/bin/codex".to_string();
        config.agent.codex.sandbox = "danger-full-access".to_string();
        config.agent.codex.approval_policy = "never".to_string();
        config.agent.codex.managed_context = "managed".to_string();
        config.agent.codex.service_tier = Some("priority".to_string());
        apply_settings_payload(&mut config, &payload);

        assert_eq!(config.agent.default_backend.as_deref(), Some("codex"));
        assert_eq!(config.agent.codex.command, "/opt/codex/bin/codex");
        assert_eq!(config.agent.codex.sandbox, "danger-full-access");
        assert_eq!(config.agent.codex.approval_policy, "never");
        assert_eq!(config.agent.codex.managed_context, "managed");
        assert_eq!(config.agent.codex.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn settings_payload_round_trips_codex_command() {
        let mut config = crate::project::ProjectConfig::default();
        config.agent.codex.command = "/usr/local/bin/codex".to_string();
        config.agent.codex.managed_context = "managed".to_string();
        config.agent.codex.service_tier = Some("priority".to_string());
        config.agent.claude_code.command = "/usr/local/bin/claude".to_string();

        let payload = settings_payload_from_config(&config);
        assert_eq!(
            payload.codex_command.as_deref(),
            Some("/usr/local/bin/codex")
        );
        assert_eq!(payload.codex_sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(payload.codex_approval_policy.as_deref(), Some("on-request"));
        assert_eq!(payload.codex_managed_context.as_deref(), Some("managed"));
        assert_eq!(payload.codex_service_tier.as_deref(), Some("priority"));
        assert_eq!(
            payload.claude_command.as_deref(),
            Some("/usr/local/bin/claude")
        );

        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex",
            "codex_command": "  /opt/homebrew/bin/codex  ",
            "codex_sandbox": "danger-full-access",
            "codex_approval_policy": "never",
            "codex_service_tier": "normal",
            "codex_managed_context": "true",
            "claude_command": "  /opt/claude/bin/claude  "
        })
        .to_string();

        let payload: SettingsPayload = serde_json::from_str(&body).unwrap();
        apply_settings_payload(&mut config, &payload);

        assert_eq!(config.agent.codex.command, "/opt/homebrew/bin/codex");
        assert_eq!(config.agent.codex.sandbox, "danger-full-access");
        assert_eq!(config.agent.codex.approval_policy, "never");
        assert_eq!(config.agent.codex.service_tier.as_deref(), Some("standard"));
        assert_eq!(config.agent.codex.managed_context, "managed");
        assert_eq!(config.agent.claude_code.command, "/opt/claude/bin/claude");
    }

    #[test]
    fn settings_post_result_rejects_invalid_json_with_bad_request() {
        let (status, body) = settings_post_result(
            "{\"external_agent\":",
            Some(Path::new(".")),
            &EventBus::new(),
        );

        assert_eq!(status, "400 Bad Request");
        assert!(body.contains("Invalid settings"));
    }

    #[test]
    fn settings_post_result_rejects_missing_project_root_with_bad_request() {
        let (status, body) = settings_post_result("{}", None, &EventBus::new());

        assert_eq!(status, "400 Bad Request");
        assert!(body.contains("No project root"));
    }

    /// POST /api/settings must keep the LIVE codex runtime config coherent,
    /// not just the TOML: launches read the shared `CodexRuntimeConfig`,
    /// which overrides the file. The gateway does that by re-dispatching
    /// the codex fields as control-plane intents after a successful save.
    #[test]
    fn settings_post_dispatches_codex_control_msgs_for_live_state() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex",
            "codex_command": "/opt/codex/bin/codex",
            "codex_sandbox": "danger-full-access",
            "codex_approval_policy": "never",
            "codex_service_tier": "priority",
            "codex_managed_context": "managed",
            "codex_context_archive": "exact"
        })
        .to_string();

        let (status, _) = settings_post_result(&body, Some(dir.path()), &bus);
        assert_eq!(status, "200 OK");

        let mut saw_command = false;
        let mut saw_sandbox = false;
        let mut saw_approval = false;
        let mut saw_service_tier = false;
        let mut saw_managed = false;
        let mut saw_archive = false;
        while let Ok(event) = rx.try_recv() {
            let AppEvent::ControlCommand(msg) = event else {
                continue;
            };
            match msg {
                ControlMsg::SetCodexCommand { command } => {
                    assert_eq!(command.as_deref(), Some("/opt/codex/bin/codex"));
                    saw_command = true;
                }
                ControlMsg::SetCodexSandbox { mode } => {
                    assert_eq!(mode, "danger-full-access");
                    saw_sandbox = true;
                }
                ControlMsg::SetCodexApprovalPolicy { policy } => {
                    assert_eq!(policy, "never");
                    saw_approval = true;
                }
                ControlMsg::SetCodexServiceTier { service_tier } => {
                    assert_eq!(service_tier.as_deref(), Some("priority"));
                    saw_service_tier = true;
                }
                ControlMsg::SetCodexManagedContext { mode } => {
                    assert_eq!(mode, "managed");
                    saw_managed = true;
                }
                ControlMsg::SetCodexContextArchive { mode } => {
                    assert_eq!(mode, "exact");
                    saw_archive = true;
                }
                _ => {}
            }
        }
        assert!(saw_command, "SetCodexCommand was not dispatched");
        assert!(saw_sandbox, "SetCodexSandbox was not dispatched");
        assert!(saw_approval, "SetCodexApprovalPolicy was not dispatched");
        assert!(saw_service_tier, "SetCodexServiceTier was not dispatched");
        assert!(saw_managed, "SetCodexManagedContext was not dispatched");
        assert!(saw_archive, "SetCodexContextArchive was not dispatched");

        // The synchronous TOML write still happened (read-after-write
        // consistency for an immediate GET /api/settings).
        let saved = std::fs::read_to_string(dir.path().join("intendant.toml")).unwrap();
        assert!(saved.contains("managed_context = \"managed\""));
    }

    /// Codex fields absent from the payload must not be re-dispatched —
    /// a partial settings save must not clobber live state with defaults.
    #[test]
    fn settings_post_skips_codex_control_msgs_for_absent_fields() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex"
        })
        .to_string();

        let (status, _) = settings_post_result(&body, Some(dir.path()), &bus);
        assert_eq!(status, "200 OK");

        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(msg) = event {
                assert!(
                    !matches!(
                        msg,
                        ControlMsg::SetCodexCommand { .. }
                            | ControlMsg::SetCodexSandbox { .. }
                            | ControlMsg::SetCodexApprovalPolicy { .. }
                            | ControlMsg::SetCodexServiceTier { .. }
                            | ControlMsg::SetCodexManagedContext { .. }
                            | ControlMsg::SetCodexContextArchive { .. }
                    ),
                    "unexpected codex control msg for absent payload field: {msg:?}"
                );
            }
        }
    }











    /// A specific bind address is preserved verbatim in the
    /// advertised URL. The operator chose it; we trust them.
    #[test]
    fn advertise_url_preserves_specific_bind_address() {
        use std::net::{Ipv4Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(specific), &[], false),
            vec!["ws://127.0.0.1:8765/ws".to_string()]
        );
        let lan_ip = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(lan_ip), &[], false),
            vec!["ws://192.168.1.42:8765/ws".to_string()]
        );
    }

    /// With TLS enabled the auto-detected scheme is `wss://`, not `ws://`
    /// — a TLS daemon is HTTPS/WSS-only, so advertising `ws://` would hand
    /// peers a URL they'd be refused on. Operator overrides are still
    /// taken verbatim (they own their scheme).
    #[test]
    fn advertise_url_uses_wss_when_tls_enabled() {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(specific), &[], true),
            vec!["wss://192.168.1.42:8765/ws".to_string()]
        );
        let v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 8765);
        let urls = resolve_advertise_urls(Some(v6), &[], true);
        assert_eq!(urls, vec!["wss://[::1]:8765/ws".to_string()]);
        // Wildcard bind with TLS: every auto-detected URL is wss://.
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        for url in resolve_advertise_urls(Some(wildcard), &[], true) {
            assert!(url.starts_with("wss://"), "tls scheme on every URL: {url}");
        }
        // Operator override is verbatim — its scheme is not rewritten.
        let overrides = vec!["ws://operator.example:9000/ws".to_string()];
        let urls = resolve_advertise_urls(Some(specific), &overrides, true);
        assert_eq!(urls[0], "ws://operator.example:9000/ws");
    }

    /// Wildcard bind (0.0.0.0) gets replaced with one URL per routable
    /// interface (auto-detection), never the literal wildcard. This
    /// is the guard against the production case where main.rs binds
    /// to 0.0.0.0:8765 and an earlier implementation was handing out
    /// `ws://0.0.0.0:8765/ws` in the Agent Card — an unusable URL
    /// that the transport-url-is-the-listener-addr assumption let
    /// slip through localhost-only tests.
    ///
    /// The exact set of interfaces is environment-dependent so we
    /// can't pin specific addresses; we only assert that no entry is
    /// the wildcard literal and the port is preserved everywhere.
    #[test]
    fn advertise_url_replaces_ipv4_wildcard_with_interface_urls() {
        use std::net::{Ipv4Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[], false);
        assert!(
            !urls.is_empty(),
            "auto-detect should produce at least one URL"
        );
        for url in &urls {
            assert!(
                !url.contains("0.0.0.0"),
                "wildcard must not appear in any auto-detected URL: {url}"
            );
            assert!(url.starts_with("ws://"), "scheme preserved: {url}");
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
            let host = url
                .strip_prefix("ws://")
                .and_then(|rest| rest.strip_suffix(":8765/ws"))
                .expect("url has expected prefix/suffix");
            assert!(
                !host.is_empty(),
                "host must resolve to something non-empty: {url}"
            );
        }
    }

    /// Same guard for IPv6 wildcards (::), which have the same
    /// unreachability problem as 0.0.0.0. Auto-detected v6 entries
    /// are bracketed per RFC 3986; we don't pin which interfaces are
    /// found because that's environment-dependent.
    #[test]
    fn advertise_url_replaces_ipv6_wildcard_with_interface_urls() {
        use std::net::{Ipv6Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[], false);
        assert!(
            !urls.is_empty(),
            "wildcard v6 bind should still produce some auto-detected URLs"
        );
        for url in &urls {
            assert!(
                !url.contains("[::]"),
                "ipv6 wildcard must not appear in any auto-detected URL: {url}"
            );
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
        }
    }

    /// IPv6 specific addresses are bracketed in the URL per RFC 3986
    /// so a literal address like `::1` doesn't collide with the
    /// `:port` separator.
    #[test]
    fn advertise_url_brackets_specific_ipv6_address() {
        use std::net::{Ipv6Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 8765);
        let urls = resolve_advertise_urls(Some(specific), &[], false);
        assert_eq!(urls.len(), 1);
        assert!(
            urls[0].contains("[::1]"),
            "ipv6 literal must be bracketed: {}",
            urls[0]
        );
    }

    // -----------------------------------------------------------------
    // resolve_url_to_socket_addr (slice 3a.2 — URL hint parsing)
    // -----------------------------------------------------------------

    /// Directly-parseable `ipv4:port` authorities are returned
    /// without any DNS round-trip.
    #[tokio::test]
    async fn resolve_url_parses_ipv4_literal_url() {
        let addr = resolve_url_to_socket_addr("ws://127.0.0.1:8766/ws")
            .await
            .expect("parses");
        assert_eq!(addr.to_string(), "127.0.0.1:8766");
    }

    /// Bracketed IPv6 literals round-trip through the parser; the
    /// `/ws` path suffix is stripped before the SocketAddr parse.
    #[tokio::test]
    async fn resolve_url_parses_ipv6_literal_url() {
        let addr = resolve_url_to_socket_addr("wss://[::1]:8443/ws")
            .await
            .expect("parses");
        assert_eq!(addr.port(), 8443);
        assert!(addr.is_ipv6(), "expected IPv6, got {addr}");
    }

    /// `http://` and `https://` are accepted alongside the WebSocket
    /// schemes so the same URL form works whether the operator types
    /// the dashboard URL or the /ws URL.
    #[tokio::test]
    async fn resolve_url_accepts_http_and_https_schemes() {
        let a = resolve_url_to_socket_addr("http://127.0.0.1:8000/")
            .await
            .expect("http parses");
        assert_eq!(a.port(), 8000);
        let b = resolve_url_to_socket_addr("https://127.0.0.1:8443")
            .await
            .expect("https parses");
        assert_eq!(b.port(), 8443);
    }

    /// Hostnames route through `tokio::net::lookup_host`. `localhost`
    /// is the one name we can rely on across every test environment.
    #[tokio::test]
    async fn resolve_url_resolves_localhost_via_dns() {
        let addr = resolve_url_to_socket_addr("ws://localhost:8766/ws")
            .await
            .expect("resolves");
        assert_eq!(addr.port(), 8766);
        assert!(
            addr.ip().is_loopback(),
            "localhost must resolve to a loopback address: {addr}"
        );
    }

    /// URLs with a path + query string strip cleanly: the authority
    /// is everything up to the first `/` or `?`.
    #[tokio::test]
    async fn resolve_url_strips_path_and_query() {
        let a = resolve_url_to_socket_addr("ws://127.0.0.1:8766/ws/path?foo=bar")
            .await
            .expect("parses");
        assert_eq!(a.to_string(), "127.0.0.1:8766");
    }

    /// Unknown schemes, missing ports, and unresolvable hostnames
    /// all return `None` — caller falls back to UDP-only path.
    #[tokio::test]
    async fn resolve_url_returns_none_on_malformed_inputs() {
        // Unknown scheme
        assert!(resolve_url_to_socket_addr("foo://127.0.0.1:8766")
            .await
            .is_none());
        // Empty authority
        assert!(resolve_url_to_socket_addr("ws:///path").await.is_none());
        // No port (authority parses as IP but not SocketAddr; lookup_host
        // rejects a bare host with no port).
        assert!(resolve_url_to_socket_addr("ws://127.0.0.1/ws")
            .await
            .is_none());
    }

    /// Operator overrides come first in the merged list (preference
    /// order), but auto-detected entries are appended as fallbacks.
    /// The connecting peer's `MultiTransport::connect` walks the list
    /// top-down and uses the first that succeeds, so overrides win on
    /// preference while auto entries provide redundancy.
    #[test]
    fn advertise_overrides_prepend_to_auto_detected() {
        use std::net::{Ipv4Addr, SocketAddr};
        // Specific bind so we can assert exactly one auto-detected entry
        // (wildcard bind would enumerate every host interface — non-
        // deterministic in CI). Specific-bind also covers the
        // intentionally-narrowed-listener case.
        let bind = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8765);
        let overrides = vec![
            "ws://192.168.1.42:8765/ws".to_string(),
            "wss://laptop.tail-abcd.ts.net:8443/ws".to_string(),
        ];
        let urls = resolve_advertise_urls(Some(bind), &overrides, false);
        // Overrides come first, auto-detected entry appended.
        assert_eq!(urls.len(), 3, "got: {urls:?}");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
        assert_eq!(urls[1], "wss://laptop.tail-abcd.ts.net:8443/ws");
        assert_eq!(urls[2], "ws://127.0.0.1:8765/ws");
    }

    /// An empty overrides list relies entirely on auto-detection.
    /// With a specific bind the result is exactly that one URL.
    #[test]
    fn empty_overrides_use_only_auto_detected_url() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let urls = resolve_advertise_urls(Some(lan), &[], false);
        assert_eq!(urls, vec!["ws://192.168.1.42:8765/ws".to_string()]);
    }

    /// Dedup: an operator URL that happens to match an auto-detected
    /// entry is kept exactly once (in operator position, since
    /// overrides are processed first). Avoids advertising the same
    /// URL twice when the operator types out their LAN IP that the
    /// daemon would have auto-detected anyway.
    #[test]
    fn advertise_dedupes_overrides_matching_auto_detected() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let overrides = vec!["ws://192.168.1.42:8765/ws".to_string()];
        let urls = resolve_advertise_urls(Some(lan), &overrides, false);
        assert_eq!(urls.len(), 1, "duplicate suppressed: {urls:?}");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
    }

    /// A wildcard bind enumerates every routable non-loopback
    /// interface. We can't pin exact addresses (CI hosts vary) but
    /// can assert: (a) at least one URL is produced, (b) loopback is
    /// excluded (advertising loopback to remote peers is useless),
    /// (c) the port matches the bind port.
    #[test]
    fn advertise_wildcard_bind_enumerates_interfaces_excluding_loopback() {
        use std::net::{Ipv4Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[], false);
        assert!(
            !urls.is_empty(),
            "expected at least one auto-detected URL, got: {urls:?}"
        );
        for url in &urls {
            assert!(
                !url.contains("127.0.0.1"),
                "loopback must not appear in auto-detected federation URLs: {url}"
            );
            assert!(
                !url.contains("0.0.0.0"),
                "wildcard must not appear in auto-detected URLs: {url}"
            );
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
        }
    }

    /// When operator wants to override completely (e.g. for security
    /// reasons — only advertise the Tailscale URL even though the
    /// daemon binds wildcard), they bind to a specific interface
    /// instead of wildcard. Specific bind narrows auto-detection to
    /// just that interface, so combined with operator override the
    /// effective list is `[override..., that_one_interface]`.
    #[test]
    fn specific_bind_narrows_auto_detection_to_one_interface() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan_only = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let urls = resolve_advertise_urls(Some(lan_only), &[], false);
        assert_eq!(urls.len(), 1, "specific bind = exactly one auto entry");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
    }





    #[test]
    fn is_federation_path_uses_parsed_routes() {
        assert!(is_federation_path("GET /api/peers HTTP/1.1"));
        assert!(is_federation_path("POST /api/peers/p-1/task HTTP/1.1"));
        assert!(is_federation_path("GET /api/sessions?limit=5 HTTP/1.1"));
        // Look-alike paths and query mentions are not federation routes.
        assert!(!is_federation_path("GET /api/peersonal HTTP/1.1"));
        assert!(!is_federation_path(
            "GET /api/fs/stat?path=/api/sessions HTTP/1.1"
        ));
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
    fn test_build_config_gemini_model() {
        let config = build_config(
            None,
            Some("gemini-2.5-flash-native-audio-preview-12-2025"),
            false,
            crate::display::IceConfig::default(),
            false,
        );
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
    }

    #[test]
    fn test_build_config_openai_model() {
        let config = build_config(
            None,
            Some("gpt-4o-realtime-preview"),
            false,
            crate::display::IceConfig::default(),
            false,
        );
        assert_eq!(config.provider, "openai");
        assert_eq!(config.input_sample_rate, 24000);
    }

    #[test]
    fn test_build_config_explicit_provider() {
        let config = build_config(
            Some("openai"),
            None,
            false,
            crate::display::IceConfig::default(),
            false,
        );
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "gpt-4o-realtime-preview");
    }

    #[test]
    fn test_build_config_no_model() {
        // With no model and no env vars set in a predictable way,
        // this should default to gemini
        let config = build_config(
            None,
            None,
            false,
            crate::display::IceConfig::default(),
            false,
        );
        // Either gemini or openai depending on env, but it shouldn't panic
        assert!(!config.provider.is_empty());
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


    #[test]
    fn accept_error_classifier_keeps_listener_alive_for_transient_errors() {
        assert!(should_continue_after_accept_error(&std::io::Error::from(
            std::io::ErrorKind::ConnectionAborted
        )));
        assert!(should_continue_after_accept_error(
            &std::io::Error::from_raw_os_error(24)
        ));
        assert!(!should_continue_after_accept_error(
            &std::io::Error::from_raw_os_error(9)
        ));
    }

    #[tokio::test]
    async fn test_spawn_web_gateway_lifecycle() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );

        // Give it a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        handle.abort();
    }

    #[tokio::test]
    async fn test_websocket_echo() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        // Bind to port 0 for a random free port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect as WebSocket client
        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a Status control message
        ws.send(Message::Text(r#"{"action":"status"}"#.into()))
            .await
            .unwrap();

        // Verify the EventBus receives the ControlCommand
        // (may be preceded by a PresenceLog debug event from the diagnostic logging)
        let mut found = false;
        for _ in 0..5 {
            let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout")
                .expect("channel closed");

            if matches!(event, AppEvent::ControlCommand(ControlMsg::Status { .. })) {
                found = true;
                break;
            }
        }
        assert!(found, "expected ControlCommand(Status)");

        handle.abort();
    }

    #[tokio::test]
    async fn test_broadcast_to_websocket() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx.clone(),
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Connect as WebSocket client
        let url = format!("ws://127.0.0.1:{}", port);
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_ws_tx, mut ws_rx) = ws.split();

        // Give the subscription a moment to register
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Broadcast an event
        let event = OutboundEvent::Status {
            turn: 1,
            phase: "thinking".to_string(),
            autonomy: "medium".to_string(),
            session_id: "test-session".to_string(),
            task: "test task".to_string(),
            external_agent: None,
        };
        crate::control::broadcast_event(&broadcast_tx, &event);

        // Verify the WebSocket client receives it. Other bootstrap snapshots may
        // be sent first.
        let json = next_ws_json_matching(&mut ws_rx, |json| json["event"] == "status").await;
        assert_eq!(json["turn"], 1);

        handle.abort();
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
    async fn http_request(port: u16, request: &str) -> String {
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
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tls.read_to_end(&mut response),
        )
        .await;
        String::from_utf8_lossy(&response).into_owned()
    }

    // -----------------------------------------------------------------
    // verify_bearer_token + is_federation_path unit tests
    // -----------------------------------------------------------------

    #[test]
    fn verify_bearer_token_passes_when_no_token_configured() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(verify_bearer_token(header, None).is_ok());
    }

    #[test]
    fn verify_bearer_token_rejects_missing_header_when_required() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_token(header, Some("expected-token")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("missing Authorization"));
    }

    #[test]
    fn verify_bearer_token_rejects_wrong_token() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n";
        let err = verify_bearer_token(header, Some("expected-token")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("invalid bearer"));
    }

    #[test]
    fn verify_bearer_token_accepts_correct_token() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_header_name_case_insensitive() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nauthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_scheme_case_insensitive() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_rejects_non_bearer_scheme() {
        let header =
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Basic Zm9vOmJhcg==\r\n\r\n";
        let err = verify_bearer_token(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("Bearer scheme"));
    }

    #[test]
    fn is_federation_path_recognizes_federation_endpoints() {
        assert!(is_federation_path("GET /api/peers HTTP/1.1"));
        assert!(is_federation_path("POST /api/peers HTTP/1.1"));
        assert!(is_federation_path("DELETE /api/peers HTTP/1.1"));
        assert!(is_federation_path("GET /api/peers/eligible HTTP/1.1"));
        assert!(is_federation_path(
            "POST /api/peers/intendant:foo/message HTTP/1.1"
        ));
        assert!(is_federation_path("POST /api/coordinator/route HTTP/1.1"));
        assert!(is_federation_path("GET /api/sessions HTTP/1.1"));
    }

    #[test]
    fn is_federation_path_excludes_unauthenticated_endpoints() {
        // Discovery, dashboard bootstrap, and `/ws` must NOT be
        // mistaken for federation paths — they're intentionally
        // exempt from bearer enforcement.
        assert!(!is_federation_path(
            "GET /.well-known/agent-card.json HTTP/1.1"
        ));
        assert!(!is_federation_path("GET /config HTTP/1.1"));
        assert!(!is_federation_path("GET / HTTP/1.1"));
        assert!(!is_federation_path("GET /static/app.js HTTP/1.1"));
        assert!(!is_federation_path(
            "GET /ws HTTP/1.1\r\nUpgrade: websocket"
        ));
        assert!(!is_federation_path("GET /api/settings HTTP/1.1"));
        assert!(!is_federation_path("POST /api/api-keys HTTP/1.1"));
    }


    #[test]
    fn public_connect_bootstrap_path_is_narrow() {
        assert!(is_public_connect_bootstrap_path(
            "GET /connect/bootstrap HTTP/1.1"
        ));
        assert!(is_public_connect_bootstrap_path(
            "GET /connect/status?poll=1 HTTP/1.1"
        ));
        assert!(is_public_connect_bootstrap_path(
            "POST /connect/dashboard/offer HTTP/1.1"
        ));
        assert!(is_public_connect_bootstrap_path(
            "POST /connect/dashboard/ice HTTP/1.1"
        ));
        assert!(is_public_connect_bootstrap_path(
            "POST /connect/dashboard/close HTTP/1.1"
        ));

        assert!(!is_public_connect_bootstrap_path("GET / HTTP/1.1"));
        assert!(!is_public_connect_bootstrap_path("GET /config HTTP/1.1"));
        assert!(!is_public_connect_bootstrap_path(
            "GET /connect/dashboard HTTP/1.1"
        ));
        assert!(!is_public_connect_bootstrap_path(
            "GET /connect/dashboard/offers HTTP/1.1"
        ));
        assert!(!is_public_connect_bootstrap_path(
            "POST /api/peers HTTP/1.1"
        ));
    }

    #[test]
    fn connect_bootstrap_html_exposes_debug_api() {
        let html = connect_bootstrap_html();
        assert!(html.contains("Intendant Connect Bootstrap"));
        assert!(html.contains("window.intendantConnectDashboard"));
        assert!(html.contains("/connect/dashboard/offer"));
        assert!(html.contains("intendant-dashboard-control-v1"));
    }





    #[test]
    fn dashboard_http_operation_maps_access_and_dashboard_routes() {
        use crate::peer::access_policy::PeerOperation;

        assert_eq!(
            dashboard_http_operation("GET", "/api/access/overview"),
            Some(PeerOperation::AccessInspect)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/access/iam/grants/update"),
            Some(PeerOperation::AccessManage)
        );
        assert_eq!(
            dashboard_http_operation("GET", "/api/dashboard/targets"),
            Some(PeerOperation::AccessInspect)
        );
        assert_eq!(
            dashboard_http_operation("GET", "/api/session/current/uploads"),
            Some(PeerOperation::SessionManage)
        );
        assert_eq!(
            dashboard_http_operation("GET", "/api/fs/read"),
            Some(PeerOperation::FilesystemRead)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/fs/write"),
            Some(PeerOperation::FilesystemWrite)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/fs/rename"),
            Some(PeerOperation::FilesystemWrite)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/fs/delete"),
            Some(PeerOperation::FilesystemWrite)
        );
        // GET must not inherit the write classification, and look-alike
        // paths must not classify at all.
        assert_eq!(dashboard_http_operation("GET", "/api/fs/write"), None);
        assert_eq!(dashboard_http_operation("GET", "/api/fs/rename"), None);
        assert_eq!(dashboard_http_operation("GET", "/api/fs/delete"), None);
        assert_eq!(dashboard_http_operation("POST", "/api/fs/writeable"), None);
        assert_eq!(dashboard_http_operation("POST", "/api/fs/deleted"), None);
        // Historically unclassified (browsers ungated); the table row
        // delegates to federation_http_operation, closing the gap the
        // federation bearer gate already closed for peers.
        assert_eq!(
            dashboard_http_operation("POST", "/api/coordinator/route"),
            Some(PeerOperation::Task)
        );
        assert_eq!(dashboard_http_operation("GET", "/config"), None);
        // The prefix families use the same boundary rule as dispatch:
        // exact or a real `/` segment — dispatch's look-alike non-routes
        // must be non-routes for the classifier too.
        assert_eq!(
            dashboard_http_operation("GET", "/api/sessions"),
            Some(PeerOperation::SessionInspect)
        );
        assert_eq!(dashboard_http_operation("GET", "/api/sessionsfoo"), None);
        assert_eq!(
            dashboard_http_operation("POST", "/api/worktrees/inspect"),
            Some(PeerOperation::SessionInspect)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/worktrees/inspect-old"),
            None
        );
        assert_eq!(dashboard_http_operation("GET", "/api/peersfoo"), None);
        assert_eq!(
            dashboard_http_operation("GET", "/api/session/current/changes/src/main.rs"),
            Some(PeerOperation::SessionManage)
        );
        // Methods a route does not declare are not routes and carry no
        // operation (the retired hand classifier used to gate some of
        // these method-blind; dispatch never served them).
        assert_eq!(dashboard_http_operation("GET", "/api/worktrees/inspect"), None);
        assert_eq!(
            dashboard_http_operation("PUT", "/api/session/current/history"),
            None
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/managed-context/anchors"),
            None
        );
        // Deliberately public routes classify as no operation: the
        // payload's own signature/shape is the authority.
        assert_eq!(
            dashboard_http_operation("POST", "/api/peer-pairing/requests"),
            None
        );
        assert_eq!(
            dashboard_http_operation("GET", "/api/peer-pairing/requests/req1"),
            None
        );
        assert_eq!(dashboard_http_operation("POST", "/api/access/org-grants"), None);
        assert_eq!(
            dashboard_http_operation("POST", "/api/access/orgs/revocations/apply"),
            None
        );
        // The federation surface delegates to federation_http_operation —
        // the same ladder the federation bearer gate enforces.
        assert_eq!(
            dashboard_http_operation("GET", "/api/peers"),
            Some(PeerOperation::PeerInspect)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/peers/p1/message"),
            Some(PeerOperation::PeerUse)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/peers/pairing/invite"),
            Some(PeerOperation::AccessManage)
        );
        // /mcp is token-bound inside the handler, not operation-gated.
        assert_eq!(dashboard_http_operation("POST", "/mcp"), None);
        // Method tightening (phase 4d) superseded the Any-era gate on
        // DELETE /api/settings: the method matches no row, so it never
        // classifies — and never reaches a handler; dispatch answers the
        // miss with 405 + the Allow union derived from the table.
        assert_eq!(dashboard_http_operation("DELETE", "/api/settings"), None);
        assert_eq!(
            crate::gateway_routes::allowed_methods_for_path("/api/settings").as_deref(),
            Some("GET, POST, OPTIONS")
        );
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

    // -----------------------------------------------------------------
    // /ws bearer enforcement (slice 2d)
    // -----------------------------------------------------------------





    #[test]
    fn ws_frame_operation_mirrors_dashboard_control_tables() {
        use crate::peer::access_policy::PeerOperation;
        assert_eq!(
            ws_frame_operation("terminal_open"),
            Some(PeerOperation::TerminalView)
        );
        assert_eq!(
            ws_frame_operation("terminal_input"),
            Some(PeerOperation::TerminalWrite)
        );
        assert_eq!(
            ws_frame_operation("terminal_share"),
            Some(PeerOperation::TerminalWrite)
        );
        assert_eq!(
            ws_frame_operation("display_input"),
            Some(PeerOperation::DisplayInput)
        );
        assert_eq!(
            ws_frame_operation("set_diagnostics_visual_marker"),
            Some(PeerOperation::DisplayInput)
        );
        assert_eq!(
            ws_frame_operation("display_offer"),
            Some(PeerOperation::DisplayView)
        );
        assert_eq!(
            ws_frame_operation("key"),
            Some(PeerOperation::RuntimeControl)
        );
        assert_eq!(
            ws_frame_operation("term_subscribe"),
            Some(PeerOperation::RuntimeControl)
        );
        assert_eq!(
            ws_frame_operation("presence_connect"),
            Some(PeerOperation::RuntimeControl)
        );
        assert_eq!(
            ws_frame_operation("user_audio"),
            Some(PeerOperation::RuntimeControl)
        );
        assert_eq!(
            ws_frame_operation("tool_request"),
            Some(PeerOperation::Message)
        );
        assert_eq!(
            ws_frame_operation("async_query"),
            Some(PeerOperation::Message)
        );
        // Tunnel signaling stays open: the tunnel enforces the same grant
        // per-frame itself, and scoped clients must be able to establish it.
        assert_eq!(ws_frame_operation("dashboard_control_offer"), None);
        assert_eq!(ws_frame_operation("dashboard_control_ice"), None);
        assert_eq!(ws_frame_operation("dashboard_control_close"), None);
        assert_eq!(ws_frame_operation("ping"), None);
    }


    #[test]
    fn verify_bearer_for_ws_passes_when_no_token_configured() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n";
        assert!(verify_bearer_for_ws(header, None).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_accepts_authorization_header() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_accepts_token_query_param() {
        // The dashboard browser path: no Authorization header (browsers
        // can't easily set headers on WebSocket opens), token rides on
        // the URL.
        let header = "GET /ws?token=right HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_rejects_when_neither_present() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_for_ws(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
    }

    #[test]
    fn verify_bearer_for_ws_rejects_wrong_query_token() {
        let header = "GET /ws?token=wrong HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_for_ws(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
    }

    /// Header AND query both present — header wins (matches first).
    /// Mismatched header with matching query: header check fails, query
    /// check passes, overall accepted. Documents the fallback behavior.
    #[test]
    fn verify_bearer_for_ws_header_wrong_falls_back_to_query() {
        let header = "GET /ws?token=right HTTP/1.1\r\n\
                      Host: x\r\n\
                      Authorization: Bearer wrong\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
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



    #[test]
    fn test_persist_manual_peer_writes_outbound_peer_config() {
        let root = tempfile::TempDir::new().unwrap();
        let req = AddPeerRequest {
            card_url: "https://peer.example:8765/.well-known/agent-card.json".into(),
            label: Some("Ignored Raw Label".into()),
            persist: true,
            via_urls: vec!["wss://tailnet-peer.example:8765/ws".into()],
            bearer_token: Some("legacy-token".into()),
            pinned_fingerprints: vec![
                "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899".into(),
            ],
            browser_tcp_via_url: Some("wss://browser-peer.example:8765/ws".into()),
        };

        let path = persist_manual_peer(root.path(), &req, Some("Peer Display".into())).unwrap();

        assert_eq!(path, root.path().join("intendant.toml"));
        let project = crate::project::Project::from_root(root.path().to_path_buf()).unwrap();
        assert_eq!(project.config.peers.len(), 1);
        let peer = &project.config.peers[0];
        assert_eq!(peer.card_url, req.card_url);
        assert_eq!(peer.label.as_deref(), Some("Peer Display"));
        assert_eq!(peer.via_urls, req.via_urls);
        assert_eq!(peer.bearer_token, req.bearer_token);
        assert_eq!(peer.pinned_fingerprints, req.pinned_fingerprints);
        assert_eq!(peer.browser_tcp_via_url, req.browser_tcp_via_url);
        assert!(peer.client_cert.is_none());
        assert!(peer.client_key.is_none());
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
    async fn setup_peer_op_test() -> (
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

    /// `POST /api/peers/{id}/message` with a full `{role, content,
    /// session}` body works the same. Verifies the full-control shape
    /// path through `SendMessageRequest::into_message` (where `content`
    /// wins over `text` when both are present).
    #[tokio::test]
    async fn test_api_peers_send_message_full_shape_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "role": "user",
            "content": {"type": "text", "text": "hello"},
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "send_message failed: {resp}");

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

    /// With one connected peer that advertises both ComputerUse and
    /// Knowledge (the test fixture's defaults), `?capability=computer-use`
    /// returns the peer; `?capability=display` returns an empty list
    /// (the fixture doesn't advertise Display).
    #[tokio::test]
    async fn test_api_peers_eligible_returns_matching_peers() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        // Hits: the test peer's card advertises ComputerUse.
        let resp = http_request(
            dash_port,
            "GET /api/peers/eligible?capability=computer-use HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        let peers = parsed["peers"].as_array().expect("peers array");
        assert_eq!(peers.len(), 1, "expected one matching peer");
        assert_eq!(peers[0]["id"].as_str().unwrap(), peer_id);

        // Misses: the fixture doesn't advertise Voice (build_local_agent_card
        // advertises ComputerUse + Knowledge + Display; Voice / Phone /
        // Recording are gated on runtime configuration that isn't plumbed
        // through yet).
        let resp = http_request(
            dash_port,
            "GET /api/peers/eligible?capability=voice HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["peers"].as_array().unwrap().len(), 0);

        target_handle.abort();
        dash_handle.abort();
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

    /// Routing a capability no connected peer satisfies returns 404
    /// with the considered peer ids surfaced for diagnostics.
    #[tokio::test]
    async fn test_api_coordinator_route_no_match_returns_404() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        // Voice is the "gated, not-advertised-by-default" capability
        // that the stock build_local_agent_card fixture doesn't claim
        // — so routing by it hits no-route and surfaces the considered
        // list. Display moved to always-on in the 3a.1 fix, so it can
        // no longer serve as the deliberately-unsatisfied capability.
        let body = serde_json::json!({
            "required_capabilities": ["voice"],
            "task": {"instructions": "needs voice"},
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert_eq!(parsed["error"].as_str().unwrap(), "no route");
        let considered = parsed["considered"].as_array().expect("considered array");
        assert!(
            considered.iter().any(|v| v.as_str() == Some(&peer_id)),
            "considered list should include the peer that didn't match"
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

    #[tokio::test]
    async fn test_http_serves_html() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Plain HTTP GET
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        // Read with timeout
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"));
        assert!(response_str.contains("<!DOCTYPE html>"));

        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_config() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
            transcription_enabled: false,
            ice_servers: Vec::new(),
            ..Default::default()
        };
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // GET /config
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /config HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"));
        assert!(response_str.contains("application/json"));
        assert!(response_str.contains("\"provider\":\"openai\""));

        handle.abort();
    }

    /// `/config` is scoped to voice/runtime config only after the
    /// AgentCard split. Identity fields (host_label, version, git_sha)
    /// moved to /.well-known/agent-card.json. This test enforces the
    /// boundary so a future code change can't reintroduce drift
    /// between the two by sneaking identity fields back into
    /// WebGatewayConfig.
    #[tokio::test]
    async fn test_config_endpoint_has_no_identity_fields() {
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
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /config HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"));

        // Extract the JSON body (after the header terminator).
        let body = response_str
            .split("\r\n\r\n")
            .nth(1)
            .expect("body after headers");
        let parsed: serde_json::Value = serde_json::from_str(body).expect("body is JSON");
        let obj = parsed.as_object().expect("body is an object");

        assert!(
            obj.contains_key("provider"),
            "should still have runtime fields"
        );
        assert!(obj.contains_key("model"));
        assert!(
            !obj.contains_key("host_label"),
            "host_label must live on the agent card, not /config: {obj:?}"
        );
        assert!(
            !obj.contains_key("version"),
            "version must live on the agent card, not /config: {obj:?}"
        );
        assert!(
            !obj.contains_key("git_sha"),
            "git_sha must live on the agent card, not /config: {obj:?}"
        );

        handle.abort();
    }

    /// `/.well-known/agent-card.json` reflects live daemon state and
    /// deserializes into an [`crate::peer::AgentCard`] with the
    /// expected shape. This is the server-side guardrail the user
    /// asked for — if someone breaks the assembly in
    /// `build_local_agent_card`, the endpoint round-trip fails here
    /// before anyone hits it in the browser.
    #[tokio::test]
    async fn test_agent_card_endpoint_reflects_live_state() {
        use crate::peer::{AgentCard, Capability, TransportAuth, TransportSpec};

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
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /.well-known/agent-card.json HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("200 OK"),
            "agent card endpoint should return 200: {response_str}"
        );
        assert!(response_str.contains("application/json"));

        let body = response_str
            .split("\r\n\r\n")
            .nth(1)
            .expect("body after headers");
        let card: AgentCard = serde_json::from_str(body).expect("body deserializes as AgentCard");

        // Identity fields must be populated from live state.
        assert_eq!(
            card.id.kind(),
            Some(crate::peer::PeerKind::Intendant),
            "local daemon must identify as Intendant kind: id = {:?}",
            card.id
        );
        assert!(
            card.id.as_str().starts_with("intendant:"),
            "PeerId must have intendant prefix: {}",
            card.id.as_str()
        );
        assert!(
            !card.label.is_empty(),
            "label must be resolved from access::resolve_host_label"
        );
        assert_eq!(
            card.version,
            env!("CARGO_PKG_VERSION"),
            "version must come from CARGO_PKG_VERSION"
        );
        assert_eq!(
            card.git_sha.as_deref(),
            Some(env!("INTENDANT_GIT_SHA")),
            "git_sha must come from INTENDANT_GIT_SHA"
        );

        // Transports must advertise at least the native Intendant WS
        // transport, with a URL that points back at this listener.
        assert_eq!(card.transports.len(), 1, "expected one transport");
        let expected_url_prefix = format!("ws://127.0.0.1:{port}");
        match &card.transports[0] {
            TransportSpec::IntendantWs { url } => {
                assert!(
                    url.starts_with(&expected_url_prefix) && url.ends_with("/ws"),
                    "transport URL {url} should start with {expected_url_prefix} and end with /ws"
                );
            }
            other => panic!("expected IntendantWs transport, got {other:?}"),
        }

        // Phase 1 conservative capability set.
        assert!(
            card.capabilities.contains(&Capability::ComputerUse),
            "card should advertise ComputerUse capability: {:?}",
            card.capabilities
        );
        assert!(
            card.capabilities.contains(&Capability::Knowledge),
            "card should advertise Knowledge capability: {:?}",
            card.capabilities
        );

        // Auth defaults to None in phase 1 (trust the network layer).
        assert!(
            matches!(card.auth.transport, TransportAuth::None) && card.auth.application.is_none(),
            "expected AuthRequirements::none() in phase 1, got {:?}",
            card.auth
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_presence_connect_disconnect() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send presence_connect (new protocol)
        ws.send(Message::Text(
            r#"{"t":"presence_connect","server_session_id":"sess-1","last_event_seq":5}"#.into(),
        ))
        .await
        .unwrap();

        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        match event {
            AppEvent::PresenceConnected {
                server_session_id,
                last_event_seq,
                ..
            } => {
                assert_eq!(server_session_id.as_deref(), Some("sess-1"));
                assert_eq!(last_event_seq, 5);
            }
            _ => panic!("expected PresenceConnected, got {:?}", event),
        }

        // Send presence_disconnect
        ws.send(Message::Text(r#"{"t":"presence_disconnect"}"#.into()))
            .await
            .unwrap();

        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        assert!(matches!(event, AppEvent::PresenceDisconnected));

        handle.abort();
    }

    #[tokio::test]
    async fn test_voice_log_forwarding() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        ws.send(Message::Text(
            r#"{"t":"voice_log","text":"hello","seq":3,"tool_context":"check_status"}"#.into(),
        ))
        .await
        .unwrap();

        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        match event {
            AppEvent::VoiceLog {
                text,
                seq,
                tool_context,
            } => {
                assert_eq!(text, "hello");
                assert_eq!(seq, 3);
                assert_eq!(tool_context.as_deref(), Some("check_status"));
            }
            _ => panic!("expected VoiceLog"),
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_tool_request_check_status() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Create a query context with a known agent state
        let agent_state = Arc::new(Mutex::new(AgentStateSnapshot {
            phase: "thinking".to_string(),
            turn: 3,
            budget_pct: 0.15,
            ..Default::default()
        }));
        let query_ctx = Some(WebQueryCtx {
            agent_state,
            project_root: PathBuf::from("/tmp"),
            log_dir: PathBuf::from("/tmp"),
            knowledge_path: PathBuf::from("/tmp/knowledge.json"),
            presence_session: None,
            context_injection: None,
        });

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            ss.write().await.query_ctx = query_ctx;
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
                false,
                None,
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_ws_tx_split, mut ws_rx) = ws.split();

        // First message should be the bootstrap state_snapshot
        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
            .await
            .expect("timeout")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "state_snapshot");
            assert_eq!(json["state"]["phase"], "thinking");
            assert_eq!(json["state"]["turn"], 3);
        } else {
            panic!("expected text message for state_snapshot");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_bootstrap_state_snapshot_uses_daemon_session_without_active_session() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            ss.write().await.daemon_session_id = Some("daemon-session".to_string());
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
                false,
                None,
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (_ws, mut ws_rx) = tokio_tungstenite::connect_async(&url)
            .await
            .unwrap()
            .0
            .split();

        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
            .await
            .expect("timeout")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "state_snapshot");
            assert_eq!(json["session_id"], "daemon-session");
        } else {
            panic!("expected text message for state_snapshot");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_bootstrap_state_snapshot_prefers_daemon_over_active_session_log() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let active_log = Arc::new(Mutex::new(
            crate::session_log::SessionLog::open(dir.path().join("active-worker")).unwrap(),
        ));

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            {
                let mut state = ss.write().await;
                state.daemon_session_id = Some("daemon-session".to_string());
                state.session_log = Some(active_log);
            }
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
                false,
                None,
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (_ws, mut ws_rx) = tokio_tungstenite::connect_async(&url)
            .await
            .unwrap()
            .0
            .split();

        let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
            .await
            .expect("timeout")
            .unwrap()
            .unwrap();

        if let Message::Text(text) = msg {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json["t"], "state_snapshot");
            assert_eq!(json["session_id"], "daemon-session");
        } else {
            panic!("expected text message for state_snapshot");
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_tool_request_response_roundtrip() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // listener passed directly to spawn_web_gateway (no TOCTOU)

        let agent_state = Arc::new(Mutex::new(AgentStateSnapshot {
            phase: "running_agent".to_string(),
            turn: 5,
            budget_pct: 0.42,
            last_command_preview: "cargo test".to_string(),
            ..Default::default()
        }));
        let query_ctx = Some(WebQueryCtx {
            agent_state,
            project_root: PathBuf::from("/tmp"),
            log_dir: PathBuf::from("/tmp"),
            knowledge_path: PathBuf::from("/tmp/knowledge.json"),
            presence_session: None,
            context_injection: None,
        });

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            ss.write().await.query_ctx = query_ctx;
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
                false,
                None,
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a check_status tool request
        ws.send(Message::Text(
            r#"{"t":"tool_request","id":"req_1","tool":"check_status","args":{}}"#.into(),
        ))
        .await
        .unwrap();

        let json = next_ws_json_type(&mut ws, "tool_response").await;
        assert_eq!(json["id"], "req_1");
        let result = json["result"].as_str().unwrap();
        assert!(result.contains("running_agent"), "result: {}", result);
        assert!(result.contains("Turn: 5"), "result: {}", result);

        handle.abort();
    }

    #[tokio::test]
    async fn test_tool_request_action_dispatches_control() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // listener passed directly to spawn_web_gateway (no TOCTOU)

        let agent_state = Arc::new(Mutex::new(AgentStateSnapshot::default()));
        let query_ctx = Some(WebQueryCtx {
            agent_state,
            project_root: PathBuf::from("/tmp"),
            log_dir: PathBuf::from("/tmp"),
            knowledge_path: PathBuf::from("/tmp/knowledge.json"),
            presence_session: None,
            context_injection: None,
        });

        let config = WebGatewayConfig::default();
        let handle = {
            let ss = ActiveSessionState::empty();
            ss.write().await.query_ctx = query_ctx;
            spawn_web_gateway(
                listener,
                bus,
                broadcast_tx,
                config,
                ss,
                None,
                None,
                None,
                None,
                None,
                None,
                Vec::new(),
                None,
                crate::peer::AuthRequirements::none(),
                false,
                None,
            )
        };
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send an approve_action tool request
        ws.send(Message::Text(
            r#"{"t":"tool_request","id":"req_2","tool":"approve_action","args":{"id":42}}"#.into(),
        ))
        .await
        .unwrap();

        // Should emit a ControlCommand(Approve) on the EventBus
        // (may be preceded by PresenceLog debug events)
        let mut found = false;
        for _ in 0..10 {
            let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout")
                .expect("channel closed");
            if let AppEvent::ControlCommand(ControlMsg::Approve { id, .. }) = event {
                assert_eq!(id, 42);
                found = true;
                break;
            }
        }
        assert!(found, "expected ControlCommand(Approve)");

        // Should also get a tool_response back. Other bootstrap snapshots may
        // be sent first.
        let json = next_ws_json_type(&mut ws, "tool_response").await;
        assert_eq!(json["id"], "req_2");
        assert!(json["result"].as_str().unwrap().contains("Approved"));

        handle.abort();
    }

    /// When a WebSocket client that sent `presence_connect` drops without
    /// sending `presence_disconnect`, the server should auto-emit
    /// `PresenceDisconnected` to resume server-side presence.
    #[tokio::test]
    async fn test_ws_drop_auto_sends_presence_disconnected() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send presence_connect
        ws.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drop the WebSocket WITHOUT sending presence_disconnect
        ws.close(None).await.unwrap();

        // Server should auto-send PresenceDisconnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for auto PresenceDisconnected")
            .expect("channel closed");

        assert!(matches!(event, AppEvent::PresenceDisconnected));

        handle.abort();
    }

    /// When a client that never sent `presence_connect` drops, no
    /// `PresenceDisconnected` should be emitted.
    #[tokio::test]
    async fn test_ws_drop_no_auto_disconnect_without_presence() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send a control action (routes through EventBus regardless of active state)
        ws.send(Message::Text(r#"{"action":"status"}"#.into()))
            .await
            .unwrap();

        // Drain events until we see the Status control event
        // (may be preceded by PresenceLog debug events)
        let mut found = false;
        for _ in 0..5 {
            let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout")
                .expect("channel closed");
            if matches!(event, AppEvent::ControlCommand(ControlMsg::Status { .. })) {
                found = true;
                break;
            }
        }
        assert!(found, "expected ControlCommand(Status)");

        // Drop the WebSocket
        ws.close(None).await.unwrap();

        // Should NOT receive PresenceDisconnected — only a timeout
        let result = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv()).await;
        assert!(result.is_err(), "expected timeout, got {:?}", result);

        handle.abort();
    }

    /// POST /session returns 502 when no API key is configured.
    #[tokio::test]
    async fn test_post_session_no_api_key() {
        let _env_lock = TEST_ENV_LOCK.lock().await;
        let _gemini_key = EnvVarGuard::unset("GEMINI_API_KEY");
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // POST /session without any API key env var set
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("502 Bad Gateway"),
            "response: {}",
            response_str
        );
        assert!(
            response_str.contains("not set on server"),
            "response: {}",
            response_str
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_http_serves_audio_processor_js() {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /audio-processor.js HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;

        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("200 OK"),
            "response: {}",
            response_str
        );
        assert!(
            response_str.contains("application/javascript"),
            "response: {}",
            response_str
        );
        assert!(
            response_str.contains("AudioCaptureProcessor"),
            "response: {}",
            response_str
        );

        handle.abort();
    }

    /// First browser to send presence_connect should become active.
    #[tokio::test]
    async fn test_first_browser_becomes_active() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Send presence_connect
        ws.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Should get PresenceConnected on the bus (active browser emits it)
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Should receive a presence_welcome with is_active: true via direct
        // channel. Other bootstrap snapshots may be sent first.
        let (_ws_tx_split, mut ws_rx) = ws.split();
        let json = next_ws_json_type(&mut ws_rx, "presence_welcome").await;
        assert_eq!(json["is_active"], true);

        handle.abort();
    }

    /// Second browser to send presence_connect should be passive (no PresenceConnected emitted).
    #[tokio::test]
    async fn test_second_browser_is_passive() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        // First browser connects — becomes active
        let (mut ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws1.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Drain PresenceConnected from first browser
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Second browser connects — should be passive
        let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws2.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Should NOT receive PresenceConnected on bus (passive)
        let result = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv()).await;
        assert!(
            result.is_err(),
            "passive browser should not emit PresenceConnected"
        );

        // Second browser should receive welcome with is_active: false
        // Drain bootstrap state_snapshot first
        let (_ws2_tx, mut ws2_rx) = ws2.split();
        let mut found_welcome = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws2_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "presence_welcome" {
                            assert_eq!(
                                json["is_active"], false,
                                "second browser should be passive"
                            );
                            found_welcome = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            found_welcome,
            "second browser should receive presence_welcome"
        );

        handle.abort();
    }

    /// When second browser sends make_active, the first should receive force_disconnect_voice.
    #[tokio::test]
    async fn test_make_active_handover() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        // Browser 1 connects and becomes active
        let (ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut ws1_tx, mut ws1_rx) = ws1.split();
        ws1_tx
            .send(Message::Text(
                r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
            ))
            .await
            .unwrap();

        // Drain PresenceConnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drain ws1's bootstrap + welcome messages
        for _ in 0..3 {
            let _ =
                tokio::time::timeout(tokio::time::Duration::from_millis(300), ws1_rx.next()).await;
        }

        // Browser 2 connects (passive — no presence_connect yet, just make_active)
        let (ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut ws2_tx, mut ws2_rx) = ws2.split();

        // Drain ws2's bootstrap state_snapshot
        let _ = tokio::time::timeout(tokio::time::Duration::from_millis(300), ws2_rx.next()).await;

        // Browser 2 sends make_active
        ws2_tx
            .send(Message::Text(r#"{"t":"make_active"}"#.into()))
            .await
            .unwrap();

        // Browser 1 should receive force_disconnect_voice
        let mut found_force_disconnect = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws1_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "force_disconnect_voice" {
                            assert_eq!(json["reason"], "handover");
                            found_force_disconnect = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            found_force_disconnect,
            "browser 1 should receive force_disconnect_voice"
        );

        // Browser 2 should receive active_granted
        let mut found_active_granted = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws2_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "active_granted" {
                            assert_eq!(json["is_active"], true);
                            found_active_granted = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            found_active_granted,
            "browser 2 should receive active_granted"
        );

        // EventBus should have received a new PresenceConnected for browser 2
        let mut found_connected = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(AppEvent::PresenceConnected { .. })) => {
                    found_connected = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(found_connected, "make_active should emit PresenceConnected");

        handle.abort();
    }

    /// When the active browser drops, the next browser to connect should get active.
    #[tokio::test]
    async fn test_active_drop_clears_slot() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        // First browser connects and becomes active
        let (mut ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws1.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Drain PresenceConnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drop the active browser
        ws1.close(None).await.unwrap();

        // Should get PresenceDisconnected
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceDisconnected));

        // Give server a moment to process the drop
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Second browser connects — should now become active
        let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws2.send(Message::Text(
            r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
        ))
        .await
        .unwrap();

        // Should get PresenceConnected (new active)
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Should receive welcome with is_active: true
        let (_ws2_tx, mut ws2_rx) = ws2.split();
        let mut found_welcome = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws2_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "presence_welcome" {
                            assert_eq!(json["is_active"], true);
                            found_welcome = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            found_welcome,
            "new browser should be active after old one dropped"
        );

        handle.abort();
    }

    /// An already-active browser re-sending presence_connect (e.g. after voice reconnect)
    /// should receive is_active: true and NOT emit a duplicate PresenceConnected.
    #[tokio::test]
    async fn test_active_browser_resend_presence_connect() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let config = WebGatewayConfig::default();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            config,
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let url = format!("ws://127.0.0.1:{}", port);

        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut ws_tx, mut ws_rx) = ws.split();

        // First presence_connect — becomes active
        ws_tx
            .send(Message::Text(
                r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
            ))
            .await
            .unwrap();

        // Drain PresenceConnected from first connect
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        assert!(matches!(event, AppEvent::PresenceConnected { .. }));

        // Drain welcome + bootstrap messages
        for _ in 0..5 {
            let _ =
                tokio::time::timeout(tokio::time::Duration::from_millis(200), ws_rx.next()).await;
        }

        // Re-send presence_connect (simulates voice reconnect after handover)
        ws_tx
            .send(Message::Text(
                r#"{"t":"presence_connect","last_event_seq":0}"#.into(),
            ))
            .await
            .unwrap();

        // Should receive welcome with is_active: true (still active)
        let mut found_welcome = false;
        for _ in 0..5 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        if json["t"] == "presence_welcome" {
                            assert_eq!(
                                json["is_active"], true,
                                "already-active browser should still be active on re-connect"
                            );
                            found_welcome = true;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(found_welcome, "should receive presence_welcome");

        // Should NOT get a duplicate PresenceConnected on the bus
        let result = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv()).await;
        assert!(
            result.is_err(),
            "should not emit duplicate PresenceConnected for already-active browser"
        );

        handle.abort();
    }

    // ---------------------------------------------------------------
    // Phase 5a.1: input-authority closure semantics + emission tests
    // ---------------------------------------------------------------

    /// Build an empty `display_input_authority` map of the production
    /// shape, for the helper-shape tests below.
    fn empty_authority_map() -> Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>> {
        Arc::new(StdRwLock::new(HashMap::new()))
    }

    /// Insert a `DisplayInputHolder` directly into the map for tests
    /// that need to seed a holder without going through the full
    /// `apply_grant_input_authority` flow.  The inserted holder owns
    /// a fresh dummy `direct_tx` whose receiver is dropped — sends to
    /// it return `Err`, which the production code already tolerates
    /// (the WS-close path would have cleared this entry in real life).
    fn seed_holder(
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

    /// Closure semantics: unclaimed map → authorized.  Matches the
    /// pre-phase-5 backwards-compat default; without this, the gate
    /// would block input on a fresh display that no one has claimed
    /// yet (regression hazard).
    #[test]
    fn local_ws_authorizer_returns_true_when_unclaimed() {
        let map = empty_authority_map();
        let authz = build_local_ws_input_authorizer(0, "conn-A".to_string(), map);
        assert!(authz(), "unclaimed display should authorize any connection");
    }

    /// Closure semantics: holder asks → authorized.  The on-going
    /// holder's input keeps flowing without re-asking.
    #[test]
    fn local_ws_authorizer_returns_true_for_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A");
        let authz = build_local_ws_input_authorizer(0, "conn-A".to_string(), map);
        assert!(authz(), "holder must remain authorized");
    }

    /// Closure semantics: non-holder asks → denied.  This is the
    /// silent-drop case — the closure returns false; the gate in
    /// `display/mod.rs::gated_input_handler` then drops the event.
    #[test]
    fn local_ws_authorizer_returns_false_for_non_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A");
        let authz = build_local_ws_input_authorizer(0, "conn-B".to_string(), map);
        assert!(
            !authz(),
            "non-holder must be denied even though display is claimed"
        );
    }

    /// Closure re-evaluates on every call — the gate must observe
    /// live grant/release transitions for a long-lived `WebRtcPeer`.
    /// Captured-snapshot semantics would freeze the gate at the value
    /// at construction time, breaking the take-control flow mid-session.
    #[test]
    fn local_ws_authorizer_re_evaluates_on_each_call() {
        let map = empty_authority_map();
        let authz = build_local_ws_input_authorizer(0, "conn-A".to_string(), Arc::clone(&map));
        assert!(authz(), "starts unclaimed → authorized");
        seed_holder(&map, 0, "conn-B");
        assert!(!authz(), "after seeding conn-B as holder → denied");
        // Replace holder with self.
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::LocalWs {
                connection_id: "conn-A".to_string(),
                direct_tx: mpsc::unbounded_channel().0,
            },
        );
        assert!(authz(), "after taking holder → re-authorized");
        // Release.
        map.write().unwrap_or_else(|e| e.into_inner()).remove(&0);
        assert!(authz(), "after release back to unclaimed → authorized");
    }

    /// `apply_grant_input_authority` emits a personalized authority
    /// change carrying `Some(holder)`.  The change flows through the
    /// broadcast channel; per-connection outbound tasks resolve the
    /// holder against their own id (via `matches_local_ws`) to produce
    /// `you|other|unclaimed` for browsers — the authoritative state
    /// the dashboard chip binds against.
    #[test]
    fn apply_grant_emits_authority_change_with_holder() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let (direct_tx, _direct_rx) = mpsc::unbounded_channel::<String>();
        let prior = apply_grant_input_authority(7, "conn-A".to_string(), direct_tx, &map, &auth_tx);
        assert!(prior.is_none(), "no prior holder on first grant");
        let change = auth_rx.try_recv().expect("authority change emitted");
        assert_eq!(change.display_id, 7);
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_local_ws("conn-A"))
                .unwrap_or(false),
            "broadcast holder must identify conn-A as the LocalWs holder"
        );
        // And the map records the new holder.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&7).unwrap().matches_local_ws("conn-A"),
            "registry entry must identify conn-A as LocalWs holder"
        );
    }

    /// A second grant from a different connection must auto-revoke
    /// the prior holder (matches Zoom's "granting auto-revokes prior"
    /// UX).  The prior holder receives a `display_input_authority_revoked`
    /// notification on its own direct_tx; the personalized change
    /// emits with the new holder's id.
    #[test]
    fn apply_grant_auto_revokes_prior_holder() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let (direct_tx_a, mut direct_rx_a) = mpsc::unbounded_channel::<String>();
        let (direct_tx_b, _direct_rx_b) = mpsc::unbounded_channel::<String>();

        // First grant to A.
        apply_grant_input_authority(7, "conn-A".to_string(), direct_tx_a.clone(), &map, &auth_tx);
        // Drain the first authority change.
        let _ = auth_rx.try_recv().expect("first grant emitted");

        // Second grant to B → A is auto-revoked.
        let prior =
            apply_grant_input_authority(7, "conn-B".to_string(), direct_tx_b, &map, &auth_tx);
        let prior_entry = prior.expect("prior holder returned");
        assert!(
            prior_entry.matches_local_ws("conn-A"),
            "prior holder must be conn-A"
        );

        // Authority change shows new holder.
        let change = auth_rx.try_recv().expect("second grant emitted");
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_local_ws("conn-B"))
                .unwrap_or(false),
            "broadcast holder must identify conn-B"
        );

        // A receives a revoked notification on its direct_tx.
        let notify = direct_rx_a
            .try_recv()
            .expect("prior holder gets display_input_authority_revoked");
        assert!(notify.contains("display_input_authority_revoked"));
        assert!(notify.contains("\"display_id\":7"));
    }

    /// `apply_release_input_authority` emits a `None`-holder change
    /// only when the release actually took effect (caller is the
    /// current holder).  No-op release does not emit.
    #[test]
    fn apply_release_emits_authority_change_with_none() {
        let map = empty_authority_map();
        seed_holder(&map, 7, "conn-A");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let removed = apply_release_input_authority(7, "conn-A", &map, &auth_tx);
        assert!(removed, "holder's release should succeed");
        let change = auth_rx.try_recv().expect("authority change emitted");
        assert_eq!(change.display_id, 7);
        assert!(change.holder.is_none(), "release emits None holder");
        assert!(map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&7)
            .is_none());
    }

    #[test]
    fn dashboard_control_authority_grant_release_and_cleanup() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);

        apply_grant_input_authority_dashboard_control(
            11,
            "control-session-A".to_string(),
            &map,
            &auth_tx,
        );
        let grant = auth_rx.try_recv().expect("dashboard grant emitted");
        assert_eq!(grant.display_id, 11);
        assert!(
            grant
                .holder
                .as_ref()
                .map(|holder| holder.matches_dashboard_control("control-session-A"))
                .unwrap_or(false),
            "broadcast holder must identify the dashboard-control session"
        );
        assert!(dashboard_control_input_authorized(
            "control-session-A",
            11,
            &map
        ));
        assert!(!dashboard_control_input_authorized(
            "control-session-B",
            11,
            &map
        ));
        assert_eq!(
            dashboard_control_authority_state_frame("control-session-A", 11, &map)["state"],
            "you"
        );
        assert_eq!(
            dashboard_control_authority_state_frame("control-session-B", 11, &map)["state"],
            "other"
        );

        let released = apply_release_input_authority_dashboard_control(
            11,
            "control-session-B",
            &map,
            &auth_tx,
        );
        assert!(!released, "non-holder release should be a no-op");
        assert!(auth_rx.try_recv().is_err(), "no-op release should not emit");

        apply_grant_input_authority_dashboard_control(
            12,
            "control-session-A".to_string(),
            &map,
            &auth_tx,
        );
        let _ = auth_rx.try_recv().expect("second dashboard grant emitted");
        let mut cleaned =
            apply_dashboard_control_close_input_authority("control-session-A", &map, &auth_tx);
        cleaned.sort_unstable();
        assert_eq!(cleaned, vec![11, 12]);
        let cleanup_a = auth_rx.try_recv().expect("first cleanup emitted");
        let cleanup_b = auth_rx.try_recv().expect("second cleanup emitted");
        assert!(cleanup_a.holder.is_none());
        assert!(cleanup_b.holder.is_none());
        assert!(dashboard_control_input_authorized(
            "control-session-B",
            11,
            &map
        ));
    }

    /// Release attempted by a non-holder is a no-op — prevents A from
    /// unclaiming B's slot.  No authority change is emitted.
    #[test]
    fn apply_release_is_noop_for_non_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 7, "conn-A");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let removed = apply_release_input_authority(7, "conn-B", &map, &auth_tx);
        assert!(!removed, "non-holder cannot unclaim");
        // No change emitted.
        assert!(
            auth_rx.try_recv().is_err(),
            "no authority change for no-op release"
        );
        // Original holder still in map.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&7).unwrap().matches_local_ws("conn-A"),
            "original holder conn-A must still be in registry after no-op release"
        );
    }

    /// WS-close cleanup releases every entry held by the dropping
    /// connection and emits one `None`-holder change per affected
    /// display.  Without this fan-out, browsers in `other` state
    /// after the dropping connection had taken control would stay
    /// stuck on stale UI.
    #[test]
    fn apply_ws_close_emits_authority_change_with_none_for_each_held_display() {
        let map = empty_authority_map();
        seed_holder(&map, 1, "conn-A");
        seed_holder(&map, 2, "conn-A");
        seed_holder(&map, 3, "conn-B");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let released = apply_ws_close_input_authority("conn-A", &map, &auth_tx);
        // A's two holdings released; B untouched.
        let mut released_sorted = released.clone();
        released_sorted.sort();
        assert_eq!(released_sorted, vec![1, 2]);
        assert!(map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&1)
            .is_none());
        assert!(map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&2)
            .is_none());
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&3).unwrap().matches_local_ws("conn-B"),
            "other connections' holdings preserved",
        );
        drop(map_guard);
        // One change emitted per released display, both with None.
        let mut events: Vec<DisplayInputAuthorityChange> = Vec::new();
        while let Ok(change) = auth_rx.try_recv() {
            events.push(change);
        }
        assert_eq!(events.len(), 2);
        for change in &events {
            assert!(change.holder.is_none());
            assert!(change.display_id == 1 || change.display_id == 2);
        }
    }

    /// WS-close for a connection that holds no slots → no events,
    /// empty release list.  Common case (non-controller dropping out).
    #[test]
    fn apply_ws_close_is_noop_when_no_slots_held() {
        let map = empty_authority_map();
        seed_holder(&map, 1, "conn-other");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let released = apply_ws_close_input_authority("conn-A", &map, &auth_tx);
        assert!(released.is_empty(), "no slots held → no releases");
        assert!(auth_rx.try_recv().is_err(), "no authority changes emitted");
        // Other holder untouched.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&1).unwrap().matches_local_ws("conn-other"),
            "other holder untouched after no-op close",
        );
    }

    // ===================================================================
    // F-1.3b: federated authority registry helpers
    // ===================================================================

    /// Seed a `FederatedWebRtc` holder directly into the map for tests
    /// that need to set up cross-provenance scenarios. Mirrors
    /// `seed_holder` for `LocalWs`.
    fn seed_federated_holder(
        map: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
        display_id: u32,
        federation_connection_id: &str,
        session_id: &str,
    ) {
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            display_id,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: federation_connection_id.to_string(),
                session_id: session_id.to_string(),
            },
        );
    }

    /// `matches_federated`: same `(federation_connection_id, session_id)`
    /// pair matches; mismatched connection or mismatched session does
    /// not. Pins the F-1 identity rule that one federation tab can't
    /// pose as another (even from the same primary).
    #[test]
    fn matches_federated_identity_check() {
        let h = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "fed-conn-1".to_string(),
            session_id: "sess-A".to_string(),
        };
        assert!(h.matches_federated("fed-conn-1", "sess-A"));
        assert!(
            !h.matches_federated("fed-conn-1", "sess-B"),
            "same connection + different session must not match"
        );
        assert!(
            !h.matches_federated("fed-conn-2", "sess-A"),
            "different connection + same session must not match"
        );
        assert!(
            !h.matches_federated("fed-conn-2", "sess-B"),
            "fully-different identity must not match"
        );
    }

    /// `matches_federated` returns false for a `LocalWs` holder
    /// regardless of inputs. Cross-provenance equality is impossible.
    #[test]
    fn matches_federated_false_for_local_ws() {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let h = DisplayInputHolder::LocalWs {
            connection_id: "conn-A".to_string(),
            direct_tx: tx,
        };
        assert!(!h.matches_federated("conn-A", "sess-A"));
    }

    /// `matches_local_ws` returns false for a `FederatedWebRtc`
    /// holder regardless of inputs. Symmetric with the test above.
    #[test]
    fn matches_local_ws_false_for_federated() {
        let h = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "fed-conn-1".to_string(),
            session_id: "sess-A".to_string(),
        };
        assert!(!h.matches_local_ws("fed-conn-1"));
    }

    /// `same_identity` distinguishes provenance even when string
    /// values collide. A `LocalWs { connection_id: "x" }` is NOT
    /// `same_identity` as `FederatedWebRtc { federation_connection_id:
    /// "x", session_id: "x" }` even though all the strings happen to
    /// match.
    #[test]
    fn same_identity_does_not_cross_provenance() {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let local = DisplayInputHolder::LocalWs {
            connection_id: "x".to_string(),
            direct_tx: tx,
        };
        let federated = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "x".to_string(),
            session_id: "x".to_string(),
        };
        assert!(!local.same_identity(&federated));
        assert!(!federated.same_identity(&local));
    }

    /// `apply_grant_input_authority_federated` first call inserts a
    /// `FederatedWebRtc` holder, returns no prior, emits the change
    /// with the new holder.
    #[test]
    fn apply_grant_federated_first_grant_no_prior() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let prior = apply_grant_input_authority_federated(
            7,
            "fed-conn-1".to_string(),
            "sess-A".to_string(),
            &map,
            &auth_tx,
        );
        assert!(prior.is_none(), "no prior on first grant");
        let change = auth_rx.try_recv().expect("change emitted");
        assert_eq!(change.display_id, 7);
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_federated("fed-conn-1", "sess-A"))
                .unwrap_or(false),
            "broadcast holder must be FederatedWebRtc(fed-conn-1, sess-A)"
        );
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard
                .get(&7)
                .unwrap()
                .matches_federated("fed-conn-1", "sess-A"),
            "registry must record the federated holder"
        );
    }

    /// **Cross-provenance handover**: a federated grant takes from a
    /// local holder. The local holder's `direct_tx` receives the
    /// `display_input_authority_revoked` notification (legacy local
    /// protocol); the broadcast change carries the new federated
    /// holder so other viewers personalize to "other".
    #[test]
    fn apply_grant_federated_takes_from_local_holder() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let (local_tx, mut local_rx) = mpsc::unbounded_channel::<String>();
        // Local holder.
        apply_grant_input_authority(7, "conn-LOCAL".to_string(), local_tx, &map, &auth_tx);
        let _ = auth_rx.try_recv().expect("local grant change");

        // Federated takes.
        let prior = apply_grant_input_authority_federated(
            7,
            "fed-conn-1".to_string(),
            "sess-A".to_string(),
            &map,
            &auth_tx,
        );
        let prior_entry = prior.expect("prior holder returned");
        assert!(
            prior_entry.matches_local_ws("conn-LOCAL"),
            "prior holder must be the local one"
        );

        // Local holder gets the legacy direct revoke.
        let revoke = local_rx
            .try_recv()
            .expect("local prior holder must receive direct revoke");
        assert!(revoke.contains("display_input_authority_revoked"));
        assert!(revoke.contains("\"display_id\":7"));

        // Broadcast carries the new federated holder.
        let change = auth_rx.try_recv().expect("broadcast change after handover");
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_federated("fed-conn-1", "sess-A"))
                .unwrap_or(false),
            "broadcast holder after handover must be the federated one"
        );
    }

    /// **Cross-provenance handover (other direction)**: a local grant
    /// takes from a federated holder. The federated holder gets NO
    /// direct revoke (federated state always flows through the
    /// personalized broadcast — see `DisplayInputHolder` doc).
    #[test]
    fn apply_grant_local_takes_from_federated_holder_no_direct_revoke() {
        let map = empty_authority_map();
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        // Federated holder.
        apply_grant_input_authority_federated(
            7,
            "fed-conn-1".to_string(),
            "sess-A".to_string(),
            &map,
            &auth_tx,
        );
        let _ = auth_rx.try_recv().expect("federated grant change");

        // Local takes.
        let (local_tx, _local_rx) = mpsc::unbounded_channel::<String>();
        let prior =
            apply_grant_input_authority(7, "conn-LOCAL".to_string(), local_tx, &map, &auth_tx);
        let prior_entry = prior.expect("prior holder returned");
        assert!(
            prior_entry.matches_federated("fed-conn-1", "sess-A"),
            "prior holder must be the federated one"
        );

        // Federated holder is informed via the broadcast (handler
        // would compute "other" for this federated subscriber). The
        // direct-revoke path is not used for federated prior holders.
        let change = auth_rx.try_recv().expect("broadcast change after handover");
        assert!(
            change
                .holder
                .as_ref()
                .map(|h| h.matches_local_ws("conn-LOCAL"))
                .unwrap_or(false),
            "broadcast holder after handover must be the local one"
        );
    }

    /// Federated release succeeds only when the calling
    /// `(federation_connection_id, session_id)` matches the current
    /// holder. A different session on the same federation connection
    /// cannot unclaim.
    #[test]
    fn apply_release_federated_only_on_matching_identity() {
        let map = empty_authority_map();
        seed_federated_holder(&map, 7, "fed-conn-1", "sess-A");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);

        // Wrong session — no-op.
        let removed =
            apply_release_input_authority_federated(7, "fed-conn-1", "sess-B", &map, &auth_tx);
        assert!(!removed, "wrong session must not unclaim");
        assert!(auth_rx.try_recv().is_err(), "no change for no-op release");

        // Wrong connection — no-op.
        let removed =
            apply_release_input_authority_federated(7, "fed-conn-2", "sess-A", &map, &auth_tx);
        assert!(!removed, "wrong connection must not unclaim");
        assert!(auth_rx.try_recv().is_err(), "no change for no-op release");

        // Original holder still in map.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard
                .get(&7)
                .unwrap()
                .matches_federated("fed-conn-1", "sess-A"),
            "original federated holder still in registry"
        );
        drop(map_guard);

        // Correct identity — releases.
        let removed =
            apply_release_input_authority_federated(7, "fed-conn-1", "sess-A", &map, &auth_tx);
        assert!(removed, "matching identity must release");
        let change = auth_rx.try_recv().expect("change emitted on release");
        assert!(change.holder.is_none(), "release emits None");
        assert!(
            map.read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&7)
                .is_none(),
            "registry empty after release"
        );
    }

    /// Federated release is also no-op against a `LocalWs` holder —
    /// federated session can't unclaim a local one even if the IDs
    /// happen to collide.
    #[test]
    fn apply_release_federated_noop_on_local_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 7, "conn-A");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let removed =
            apply_release_input_authority_federated(7, "conn-A", "sess-X", &map, &auth_tx);
        assert!(
            !removed,
            "federated release must not unclaim a LocalWs holder"
        );
        assert!(auth_rx.try_recv().is_err(), "no change emitted");
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&7).unwrap().matches_local_ws("conn-A"),
            "local holder still in registry"
        );
    }

    /// Federated WS-close releases ALL `FederatedWebRtc` entries with
    /// matching `federation_connection_id`, regardless of `session_id`
    /// (the WS drop kills every session multiplexed over that primary's
    /// federation transport). Other federation connections' entries
    /// AND any local entries are untouched.
    #[test]
    fn apply_federated_ws_close_releases_all_sessions_on_dropping_connection() {
        let map = empty_authority_map();
        seed_federated_holder(&map, 1, "fed-conn-1", "sess-A");
        seed_federated_holder(&map, 2, "fed-conn-1", "sess-B");
        seed_federated_holder(&map, 3, "fed-conn-2", "sess-C");
        seed_holder(&map, 4, "conn-LOCAL");

        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(16);
        let released = apply_federated_ws_close_input_authority("fed-conn-1", &map, &auth_tx);
        let mut released_sorted = released.clone();
        released_sorted.sort();
        assert_eq!(
            released_sorted,
            vec![1, 2],
            "both sessions on fed-conn-1 must be released"
        );

        // Other federation connection's entry untouched.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard
                .get(&3)
                .unwrap()
                .matches_federated("fed-conn-2", "sess-C"),
            "other federation connection's entry untouched"
        );
        // Local entry untouched.
        assert!(
            map_guard.get(&4).unwrap().matches_local_ws("conn-LOCAL"),
            "local holder untouched"
        );
        drop(map_guard);

        // One change emitted per affected display, all with None.
        let mut events = Vec::new();
        while let Ok(change) = auth_rx.try_recv() {
            events.push(change);
        }
        assert_eq!(events.len(), 2);
        for change in &events {
            assert!(change.holder.is_none());
            assert!(change.display_id == 1 || change.display_id == 2);
        }
    }

    /// Federated WS-close with no matching entries → empty list, no
    /// events. Local entries with the same `connection_id` value are
    /// not touched (the function is provenance-scoped).
    #[test]
    fn apply_federated_ws_close_is_noop_with_no_matching_entries() {
        let map = empty_authority_map();
        seed_holder(&map, 1, "fed-conn-1");
        let (auth_tx, mut auth_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let released = apply_federated_ws_close_input_authority("fed-conn-1", &map, &auth_tx);
        assert!(
            released.is_empty(),
            "no FederatedWebRtc entries with this connection — no releases"
        );
        assert!(auth_rx.try_recv().is_err(), "no change emitted");
        // Local entry with the same connection_id (rare but possible
        // if a single connection_id value is reused across phases) is
        // untouched by the federated cleanup.
        let map_guard = map.read().unwrap_or_else(|e| e.into_inner());
        assert!(
            map_guard.get(&1).unwrap().matches_local_ws("fed-conn-1"),
            "LocalWs entry with same id value untouched by federated cleanup"
        );
    }

    /// F-2: positive — an authority entry of `FederatedWebRtc` matching
    /// this closure's `(federation_connection_id, session_id)`
    /// authorizes input. Mirrors the local 5c
    /// `local_ws_authorizer_returns_true_for_holder` shape.
    #[test]
    fn federated_input_authorizer_returns_true_for_matching_holder() {
        let map = empty_authority_map();
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-1".to_string(),
                session_id: "sess-A".to_string(),
            },
        );
        let authz = build_federated_input_authorizer(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
        );
        assert!(authz(), "matching identity must authorize input");
    }

    /// F-2: negative — unclaimed (`None`) is strict deny on the
    /// federated path. Different from local 5c (which treats `None`
    /// as "anyone may input" for backwards compat); federated has no
    /// such legacy.
    #[test]
    fn federated_input_authorizer_returns_false_when_no_holder() {
        let map = empty_authority_map();
        let authz =
            build_federated_input_authorizer(0, "fed-1".to_string(), "sess-A".to_string(), map);
        assert!(
            !authz(),
            "unclaimed display must drop federated input — different \
             from local 5c's pre-phase-5 default-allow"
        );
    }

    /// F-2: negative — a `LocalWs` holder denies federated input.
    /// Mixed cross-provenance hold: local browser drives input; the
    /// federated browser's events are dropped at the gate.
    #[test]
    fn federated_input_authorizer_returns_false_when_local_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "local-conn-A");
        let authz =
            build_federated_input_authorizer(0, "fed-1".to_string(), "sess-A".to_string(), map);
        assert!(
            !authz(),
            "LocalWs holder must drop federated input even though the \
             registry is non-empty"
        );
    }

    /// F-2: negative — same `federation_connection_id`, different
    /// `session_id`. Two tabs from the same primary; only one holds.
    /// The non-holding tab's events drop.
    #[test]
    fn federated_input_authorizer_returns_false_when_wrong_session() {
        let map = empty_authority_map();
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-1".to_string(),
                session_id: "sess-OTHER".to_string(),
            },
        );
        let authz =
            build_federated_input_authorizer(0, "fed-1".to_string(), "sess-A".to_string(), map);
        assert!(
            !authz(),
            "same connection + different session must deny — distinct \
             tabs from the same primary don't share input authority"
        );
    }

    /// F-2: negative — different `federation_connection_id` (different
    /// primary). The federated holder belongs to a different primary's
    /// transport; this primary's federated browser must not be able to
    /// drive input on behalf of the other primary's session.
    #[test]
    fn federated_input_authorizer_returns_false_when_wrong_connection() {
        let map = empty_authority_map();
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-OTHER".to_string(),
                session_id: "sess-A".to_string(),
            },
        );
        let authz =
            build_federated_input_authorizer(0, "fed-1".to_string(), "sess-A".to_string(), map);
        assert!(
            !authz(),
            "different federation_connection_id must deny even when \
             session_id matches — distinct primaries are distinct \
             security boundaries"
        );
    }

    // ---------------------------------------------------------------
    // F-1.3b3: federated authority handler + subscriber registry
    // ---------------------------------------------------------------

    /// Test helper: build a stub `WebRtcPeer` via the existing
    /// `new_for_test` constructor. Send-authority-state calls against
    /// the returned peer will fail (its command_rx is dropped) but
    /// the registry-level tests below only inspect the subscriber
    /// map, never await on delivery.
    fn make_test_peer(peer_id: u64) -> Arc<crate::display::webrtc::WebRtcPeer> {
        use crate::display::encode::pool::SimulcastRid;
        use crate::display::webrtc::WebRtcPeer;
        Arc::new(WebRtcPeer::new_for_test(
            peer_id,
            vec![SimulcastRid::full()],
        ))
    }

    /// Build an empty subscriber registry of the production shape.
    fn empty_subscribers() -> FederatedAuthoritySubscribers {
        Arc::new(StdRwLock::new(HashMap::new()))
    }

    /// `personalize_authority_for_federated` returns `You` when the
    /// holder's identity matches this subscriber's
    /// `(federation_connection_id, session_id)`. Mirrors the local
    /// 5c outbound personalization at the per-WS subscriber loop.
    #[test]
    fn personalize_authority_for_federated_returns_you_on_match() {
        let holder = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "fed-1".to_string(),
            session_id: "sess-A".to_string(),
        };
        let state = personalize_authority_for_federated(Some(&holder), "fed-1", "sess-A");
        assert_eq!(
            state,
            crate::display::webrtc::DisplayInputAuthorityState::You
        );
    }

    /// `personalize_authority_for_federated` returns `Other` when
    /// any holder exists that isn't this subscriber's identity. The
    /// "wrong session, same connection" case (two tabs from one
    /// primary) also resolves to `Other` — distinct session IDs
    /// don't collapse.
    #[test]
    fn personalize_authority_for_federated_returns_other_when_someone_else_holds() {
        let other_federated = DisplayInputHolder::FederatedWebRtc {
            federation_connection_id: "fed-1".to_string(),
            session_id: "sess-B".to_string(),
        };
        assert_eq!(
            personalize_authority_for_federated(Some(&other_federated), "fed-1", "sess-A"),
            crate::display::webrtc::DisplayInputAuthorityState::Other,
            "same connection, different session must be 'other'",
        );
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let local = DisplayInputHolder::LocalWs {
            connection_id: "local-conn".to_string(),
            direct_tx: tx,
        };
        assert_eq!(
            personalize_authority_for_federated(Some(&local), "fed-1", "sess-A"),
            crate::display::webrtc::DisplayInputAuthorityState::Other,
            "LocalWs holder must surface as 'other' to a federated subscriber",
        );
    }

    /// `personalize_authority_for_federated` returns `Unclaimed` when
    /// no holder is in the registry. Map absence is the canonical
    /// "no one holds" signal — no `Option` in the value type.
    #[test]
    fn personalize_authority_for_federated_returns_unclaimed_when_no_holder() {
        let state = personalize_authority_for_federated(None, "fed-1", "sess-A");
        assert_eq!(
            state,
            crate::display::webrtc::DisplayInputAuthorityState::Unclaimed
        );
    }

    /// The handler closure built by `build_federated_authority_handler`
    /// dispatches a `Request` to `apply_grant_input_authority_federated`,
    /// resulting in a holder bound to this peer's identity in the
    /// registry. Pins that the handler closure carries the right
    /// identity and that the dispatch shape is correct.
    #[test]
    fn build_federated_authority_handler_dispatches_request_to_grant() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
            true,
        );

        handler(AuthorityChannelMessage::Request { display_id: 0 });

        let guard = map.read().unwrap_or_else(|e| e.into_inner());
        match guard.get(&0) {
            Some(DisplayInputHolder::FederatedWebRtc {
                federation_connection_id,
                session_id,
            }) => {
                assert_eq!(federation_connection_id, "fed-1");
                assert_eq!(session_id, "sess-A");
            }
            other => panic!("expected FederatedWebRtc holder, got {other:?}"),
        }
    }

    /// `Release` against a holder of this same identity removes the
    /// entry from the registry. Pins the wire→registry round-trip.
    #[test]
    fn build_federated_authority_handler_dispatches_release_to_apply_release() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        // Seed a federated holder with the identity the handler was
        // built for.
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-1".to_string(),
                session_id: "sess-A".to_string(),
            },
        );

        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
            true,
        );
        handler(AuthorityChannelMessage::Release { display_id: 0 });

        assert!(
            map.read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&0)
                .is_none(),
            "release with matching identity must remove the holder"
        );
    }

    /// `Release` on a holder of a DIFFERENT identity is a silent
    /// no-op — the F-1.3b1 helper enforces identity matching at the
    /// registry layer, and the handler can't bypass it. Two tabs from
    /// the same primary can't unclaim each other.
    #[test]
    fn build_federated_authority_handler_release_noop_on_wrong_identity() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        // Seed with a holder of a DIFFERENT session id.
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            0,
            DisplayInputHolder::FederatedWebRtc {
                federation_connection_id: "fed-1".to_string(),
                session_id: "sess-OTHER".to_string(),
            },
        );

        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
            true,
        );
        handler(AuthorityChannelMessage::Release { display_id: 0 });

        let guard = map.read().unwrap_or_else(|e| e.into_inner());
        match guard.get(&0) {
            Some(DisplayInputHolder::FederatedWebRtc { session_id, .. }) => {
                assert_eq!(
                    session_id, "sess-OTHER",
                    "wrong-identity release must not remove the slot"
                );
            }
            other => panic!("expected slot to remain held by sess-OTHER, got {other:?}"),
        }
    }

    /// Display-ID mismatches drop silently. The federated peer's
    /// `PeerDisplayConnection` is bound to one display; a `Request`
    /// targeting any other display must not mutate the registry.
    #[test]
    fn build_federated_authority_handler_ignores_display_id_mismatch() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
            true,
        );

        handler(AuthorityChannelMessage::Request { display_id: 99 });
        handler(AuthorityChannelMessage::Release { display_id: 99 });

        assert!(
            map.read().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "display-id mismatch must not mutate the registry"
        );
    }

    #[test]
    fn build_federated_authority_handler_denies_request_when_profile_read_only() {
        use crate::display::webrtc::AuthorityChannelMessage;
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        let handler = build_federated_authority_handler(
            0,
            "fed-1".to_string(),
            "sess-A".to_string(),
            Arc::clone(&map),
            change_tx.clone(),
            false,
        );

        handler(AuthorityChannelMessage::Request { display_id: 0 });

        assert!(
            map.read().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "read-only profile must not grant federated input authority"
        );
    }

    /// `unregister_federated_authority_subscriber` removes the entry
    /// when the identity tuple matches and returns true. Cancellation
    /// of the spawned fanout task is a side effect of the cancel call
    /// on the stored token; not directly observable in this test, but
    /// the broadcast channel close on test exit reaps any orphaned
    /// task cleanly.
    #[tokio::test]
    async fn unregister_federated_authority_subscriber_removes_matching() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx,
            Arc::clone(&subscribers),
        );
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "register must insert one entry"
        );

        let removed = unregister_federated_authority_subscriber("fed-1", "sess-A", 0, &subscribers);

        assert!(removed, "matching unregister returns true");
        assert!(
            subscribers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "registry must be empty after unregister"
        );
    }

    /// `unregister_federated_authority_subscriber` returns false (and
    /// leaves the registry untouched) when no entry matches.
    #[tokio::test]
    async fn unregister_federated_authority_subscriber_noop_on_miss() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx,
            Arc::clone(&subscribers),
        );

        let removed =
            unregister_federated_authority_subscriber("fed-1", "sess-OTHER", 0, &subscribers);
        assert!(!removed, "non-matching unregister returns false");
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "registry must be unchanged after non-matching unregister"
        );
    }

    /// Federation WS-close cleanup releases every subscriber whose
    /// `federation_connection_id` matches the dropping connection,
    /// regardless of `session_id` or `display_id`. Counterpart to
    /// `apply_federated_ws_close_input_authority`.
    #[tokio::test]
    async fn unregister_all_federated_subscribers_for_connection_releases_matching() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        // Three subscribers: two on fed-1 (different sessions, same
        // display), one on fed-2 (the survivor).
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx.clone(),
            Arc::clone(&subscribers),
        );
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-B".to_string(),
            0,
            make_test_peer(2),
            Arc::clone(&map),
            change_tx.clone(),
            Arc::clone(&subscribers),
        );
        register_federated_authority_subscriber(
            "fed-2".to_string(),
            "sess-C".to_string(),
            0,
            make_test_peer(3),
            Arc::clone(&map),
            change_tx.clone(),
            Arc::clone(&subscribers),
        );
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            3
        );

        let released = unregister_all_federated_subscribers_for_connection("fed-1", &subscribers);

        assert_eq!(released.len(), 2, "two fed-1 entries released");
        let remaining: Vec<(String, String, u32)> = subscribers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            remaining,
            vec![("fed-2".to_string(), "sess-C".to_string(), 0)],
            "only fed-2 entry must remain"
        );
    }

    /// `unregister_all_federated_subscribers_for_connection` returns
    /// an empty vec and leaves the registry untouched when no entries
    /// match the dropping connection.
    #[tokio::test]
    async fn unregister_all_federated_subscribers_for_connection_noop_on_no_match() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        register_federated_authority_subscriber(
            "fed-2".to_string(),
            "sess-C".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx,
            Arc::clone(&subscribers),
        );

        let released = unregister_all_federated_subscribers_for_connection("fed-1", &subscribers);
        assert!(
            released.is_empty(),
            "no matching entries → empty release list"
        );
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "registry unchanged"
        );
    }

    /// `register_federated_authority_subscriber` replaces an existing
    /// entry with the same `(fcid, sid, did)` key (renegotiated peer
    /// for the same identity). Map size stays at 1; the prior entry's
    /// shutdown token fires via the in-helper cancel path.
    #[tokio::test]
    async fn register_federated_authority_subscriber_replaces_on_collision() {
        let subscribers = empty_subscribers();
        let map = empty_authority_map();
        let (change_tx, _change_rx) = broadcast::channel::<DisplayInputAuthorityChange>(8);
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(1),
            Arc::clone(&map),
            change_tx.clone(),
            Arc::clone(&subscribers),
        );
        register_federated_authority_subscriber(
            "fed-1".to_string(),
            "sess-A".to_string(),
            0,
            make_test_peer(2),
            Arc::clone(&map),
            change_tx,
            Arc::clone(&subscribers),
        );
        assert_eq!(
            subscribers.read().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "duplicate-key registration must replace, not append"
        );
    }

    // ---------------------------------------------------------------
    // F-1.3b3 fix #2: WS-close peer teardown — peer_id helper
    // determinism + close-helper edge cases. The actual
    // session.remove_peer side effect requires a real
    // DisplaySession (which needs a real backend) and is exercised
    // by the F-3 smoke; these unit tests pin the contract that
    // must hold for the smoke to be meaningful: the same session_id
    // hashes to the same PeerId on both the Offer (insert) and the
    // WS-close (cleanup) sides.
    // ---------------------------------------------------------------


    /// Distinct `session_id`s map to distinct `PeerId`s in
    /// practice. (`u64` hash collisions are theoretically possible
    /// but vanishingly unlikely between any two real session ids
    /// generated by the browser.) Without this property, two
    /// federated tabs from one primary would alias to the same
    /// `WebRtcPeer` slot — cleanup of one tab would tear down the
    /// other.
    #[test]
    fn peer_id_for_federated_session_distinct_for_distinct_sessions() {
        let a = peer_id_for_federated_session("sess-A");
        let b = peer_id_for_federated_session("sess-B");
        assert_ne!(
            a, b,
            "distinct session ids should produce distinct peer ids"
        );
    }

    /// `close_federated_peers_for_sessions` short-circuits to 0 on
    /// empty release input — covers the "WS-close fired but the
    /// connection had no federated subscribers" no-op path
    /// (typical: the connection was a local browser, not a
    /// federation transport).
    #[tokio::test]
    async fn close_federated_peers_for_sessions_noop_on_empty_release() {
        let reg = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let count = close_federated_peers_for_sessions(&[], Some(&reg)).await;
        assert_eq!(count, 0, "empty release must short-circuit");
    }

    /// `close_federated_peers_for_sessions` short-circuits on a
    /// `None` session_registry — the daemon may run without one
    /// (e.g. presence-disabled startup), and the WS-close path
    /// must not panic in that mode.
    #[tokio::test]
    async fn close_federated_peers_for_sessions_noop_on_no_registry() {
        let count = close_federated_peers_for_sessions(&[("sess-A".to_string(), 0)], None).await;
        assert_eq!(count, 0, "missing registry must short-circuit");
    }

    /// `close_federated_peers_for_sessions` returns 0 (and runs no
    /// `remove_peer` calls) when the listed displays aren't in the
    /// registry — covers the race where a display session gets
    /// deactivated between Offer-time and WS-close.
    #[tokio::test]
    async fn close_federated_peers_for_sessions_noop_when_displays_missing() {
        let reg = Arc::new(tokio::sync::RwLock::new(
            crate::display::SessionRegistry::new(),
        ));
        let count = close_federated_peers_for_sessions(
            &[("sess-A".to_string(), 0), ("sess-B".to_string(), 1)],
            Some(&reg),
        )
        .await;
        assert_eq!(
            count, 0,
            "missing displays in the registry must fall through silently",
        );
    }

    // ---------------------------------------------------------------
    // Phase 5c.2: bootstrap snapshot regression — late-second browser
    // joining a daemon that already has an active display must end up
    // with its chip resolved to `you`/`other`/`unclaimed`, never stuck
    // at `unknown`.  The snapshot computation is the per-connection
    // personalization pass (the holder-id never reaches the wire).
    // ---------------------------------------------------------------

    /// Active display, no holder → `unclaimed` for the connecting browser.
    /// Covers the "fresh display granted before browser B connects, no one
    /// has clicked Take Control yet" case.
    #[test]
    fn bootstrap_authority_snapshots_unclaimed_when_no_holder() {
        let map = empty_authority_map();
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([0u32], &auth, "conn-B");
        assert_eq!(snaps, vec![(0, "unclaimed")]);
    }

    /// Active display, browser A holds → connecting browser B sees `other`.
    /// This is the exact regression that left B's chip at `unknown`
    /// before slice 5c.2 — the bootstrap was sent but landed on the
    /// wrong slot, so this test pins the snapshot resolution.
    #[test]
    fn bootstrap_authority_snapshots_other_for_late_second_browser() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A");
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([0u32], &auth, "conn-B");
        assert_eq!(
            snaps,
            vec![(0, "other")],
            "browser B (different connection_id) must see `other` while A holds",
        );
    }

    /// Active display, this connection IS the holder → `you`.
    /// Covers a holder browser refresh: same `connection_id` (or
    /// equivalent) reconnecting must see `you` so the chip stays
    /// consistent with the server-side gate.
    #[test]
    fn bootstrap_authority_snapshots_you_when_self_is_holder() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A");
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([0u32], &auth, "conn-A");
        assert_eq!(snaps, vec![(0, "you")]);
    }

    /// Multiple active displays, mixed holders → per-display
    /// personalization is independent.  The connecting browser sees
    /// `you` for its own holdings and `other`/`unclaimed` for the rest.
    /// Locks in that the snapshot iterates per display, not per holder.
    #[test]
    fn bootstrap_authority_snapshots_resolve_per_display_independently() {
        let map = empty_authority_map();
        seed_holder(&map, 0, "conn-A"); // you
        seed_holder(&map, 1, "conn-B"); // other
                                        // display 2 unclaimed
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([0u32, 1, 2], &auth, "conn-A");
        assert_eq!(
            snaps,
            vec![(0, "you"), (1, "other"), (2, "unclaimed")],
            "each display's state resolves independently against this connection",
        );
    }

    /// Empty session registry → no snapshots, no frames to send.
    /// Matches the "browser connects to a daemon with no granted
    /// display" path; bootstrap loop is a no-op.
    #[test]
    fn bootstrap_authority_snapshots_empty_when_no_active_displays() {
        let map = empty_authority_map();
        let auth = map.read().unwrap_or_else(|e| e.into_inner());
        let snaps = compute_bootstrap_authority_snapshots([] as [u32; 0], &auth, "conn-A");
        assert!(snaps.is_empty());
    }













    /// The gateway must be able to re-establish its listener on the exact
    /// address a dead one occupied (accept() EINVAL/EBADF recovery path),
    /// and the fresh listener must actually accept connections.
    #[tokio::test]
    async fn rebind_dead_tcp_listener_restores_reachability() {
        let original = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = original.local_addr().unwrap();
        drop(original);

        let rebound = rebind_dead_tcp_listener(addr).expect("rebind on the freed address");
        assert_eq!(rebound.local_addr().unwrap(), addr);

        let (client, (server, _peer)) = tokio::join!(
            tokio::net::TcpStream::connect(addr),
            async { rebound.accept().await.unwrap() },
        );
        client.expect("client connects to rebound listener");
        drop(server);
    }

    /// SO_REUSEADDR does not override an actively bound listener on Unix —
    /// the accept-loop recovery MUST drop the dead socket before rebinding,
    /// or every attempt self-inflicts EADDRINUSE (seen live: a daemon whose
    /// accept loop died spun on rebind for over an hour while its own dead
    /// listener still owned the port). Windows semantics differ, so the
    /// still-bound assertion is Unix-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn rebind_fails_while_dead_listener_is_still_bound() {
        let holder = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = holder.local_addr().unwrap();

        let err = rebind_dead_tcp_listener(addr)
            .expect_err("rebinding must fail while the previous listener still holds the address");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

        drop(holder);
        assert!(rebind_dead_tcp_listener(addr).is_ok());
    }
}
