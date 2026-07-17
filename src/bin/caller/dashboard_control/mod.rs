//! Daemon-scoped WebRTC control tunnel for dashboard RPC experiments.
//!
//! The dashboard still uses HTTP plus the main WebSocket by default. This
//! module provides a lower-latency path for clients already admitted through a
//! trusted local, independently verified direct-mTLS, or authenticated peer
//! transport: daemon/peer signaling creates a direct WebRTC data channel, then
//! the channel carries small JSON RPC frames. Hosted Connect has no signaling
//! or control path into this module.

use crate::daemon_identity::{b64u, DaemonIdentity};
use crate::error::CallerError;
use crate::event::{AppEvent, ControlMsg};
use crate::peer::access_policy::PeerOperation;
use crate::types::{truncate_str, LogLevel};
use base64::Engine as _;
use bytes::BytesMut;
use rtc::peer_connection::configuration::setting_engine::SettingEngine;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::event::{RTCDataChannelEvent, RTCPeerConnectionEvent};
use rtc::peer_connection::message::RTCMessage;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, RTCDtlsRole, RTCIceCandidate, RTCIceCandidateInit,
    RTCIceServer,
};
use rtc::peer_connection::{RTCPeerConnection, RTCPeerConnectionBuilder};
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::{Read as _, Seek as _, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

mod wire;
pub(crate) use wire::*;
mod dispatch;
pub(crate) use dispatch::*;
mod api_sessions;
pub(crate) use api_sessions::*;
mod api_media;
pub(crate) use api_media::*;
mod api_transfers_fs;
pub(crate) use api_transfers_fs::*;
mod api_control;
pub(crate) use api_control::*;

const CONTROL_CHANNEL_LABEL: &str = "intendant-dashboard-control";
/// Maximum idle-session delay before a long-lived dashboard transport notices
/// that its opening IAM authority changed and tears itself down. Active
/// inbound/outbound paths re-check synchronously before processing a frame.
pub(crate) const LIVE_AUTHORITY_RECHECK_INTERVAL: Duration = Duration::from_millis(250);
const CONTROL_PROTOCOL_VERSION: u32 = 1;
const CONTROL_SIGNATURE_CONTEXT: &str = "intendant-dashboard-control-v1";
const CONTROL_DEFAULT_SESSION_LIMIT: usize = 600;
const CONTROL_RESPONSE_CHUNK_THRESHOLD_BYTES: usize = 64 * 1024;
const CONTROL_RESPONSE_CHUNK_BYTES: usize = 16 * 1024;
const CONTROL_BYTE_STREAM_CHUNK_BYTES: usize = 16 * 1024;
/// Capacity of the per-connection bounded PTY-output lane (the tunnel twin
/// of `ws_session::TERMINAL_FORWARD_LANE_CAP`). Entries are one JSON frame
/// around a base64 chunk (terminal.rs merges output up to 64 KiB per
/// entry), so the lane holds ~1.4 MiB worst case — the same order as
/// terminal.rs's own 1 MiB per-listener bound that takes over
/// (drop-oldest) once this lane fills and the forwarders park.
const TERMINAL_OUTPUT_LANE_CAP: usize = 16;
/// Stop draining the terminal-output lane once the control channel's SCTP
/// send buffer holds this much; resume at the low watermark. The response
/// lane is client-credit-gated, but PTY output had no end-to-end
/// backpressure — a stalled tab grew the unbounded SCTP pending queue at
/// PTY rate.
const TERMINAL_LANE_BUFFERED_HIGH_WATERMARK_BYTES: usize = 1024 * 1024;
/// Low watermark paired with [`TERMINAL_LANE_BUFFERED_HIGH_WATERMARK_BYTES`].
const TERMINAL_LANE_BUFFERED_LOW_WATERMARK_BYTES: usize = 256 * 1024;
/// Per-connection cap on bytes resident in the credit-gated outbound
/// queue. Sized above the single largest legitimate response (the 100 MB
/// fs-read cap) so one full-size download always fits, while N stalled
/// downloads to a quiet client can no longer pin N full payloads.
const CONTROL_OUTBOUND_QUEUE_MAX_BYTES: usize = 128 * 1024 * 1024;
/// Companion frame-count cap: small immediate frames parked behind a
/// zero-credit chunked head are byte-cheap but were unbounded in number.
const CONTROL_OUTBOUND_QUEUE_MAX_FRAMES: usize = 4096;

/// Clamp a byte watermark into the `u32` the rtc data-channel threshold
/// setters take (shared with the peer file-transfer driver).
pub(crate) fn watermark_to_u32(bytes: usize) -> u32 {
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

/// ERROR-class classification for the immediate-frame budget seam: the
/// plain error envelope (`ok: false` — auth denials, unknown methods,
/// admission refusals) and the injected-status error shape
/// (`result._httpOk: false` — the upload lane's 4xx/5xx envelopes).
/// These are the frames a client can mint cheaply by spamming invalid
/// requests; success frames are never budgeted.
pub(crate) fn is_error_class_frame(frame: &serde_json::Value) -> bool {
    if frame.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        return true;
    }
    frame
        .get("result")
        .and_then(|result| result.get("_httpOk"))
        .and_then(serde_json::Value::as_bool)
        == Some(false)
}

/// Token budget for DIRECT error frames — the admission-refusal and
/// rejection replies that deliberately bypass the queue/backpressure
/// lanes (they must be deliverable exactly when those lanes are the
/// problem). While the wire is below its high watermark the budget stays
/// full (frames drain as fast as they are sent); while congested each
/// direct error spends one token, and at zero the frame is DROPPED and
/// counted (logged once per connection, summarized at teardown) — the
/// protocol contract is best-effort error delivery under abuse, bounded
/// memory always. Shared by the dashboard tunnel and the peer
/// file-transfer driver.
pub(crate) struct DirectErrorBudget {
    label: &'static str,
    remaining: u32,
    dropped: u64,
}

/// Direct error frames spendable while the wire stays congested.
pub(crate) const DIRECT_ERROR_BUDGET_FRAMES: u32 = 64;

impl DirectErrorBudget {
    pub(crate) fn new(label: &'static str) -> Self {
        Self {
            label,
            remaining: DIRECT_ERROR_BUDGET_FRAMES,
            dropped: 0,
        }
    }

    /// Whether one direct error frame may be sent right now.
    pub(crate) fn allow(&mut self, wire_congested: bool) -> bool {
        if !wire_congested {
            // Uncongested wire drains what it is handed; refill.
            self.remaining = DIRECT_ERROR_BUDGET_FRAMES;
            return true;
        }
        if self.remaining > 0 {
            self.remaining -= 1;
            return true;
        }
        self.dropped = self.dropped.saturating_add(1);
        if self.dropped == 1 {
            eprintln!(
                "[{}] direct error-frame budget exhausted while congested; dropping further error frames",
                self.label
            );
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Log the drop tally at connection teardown (silent when zero).
    pub(crate) fn log_teardown(&self) {
        if self.dropped > 0 {
            eprintln!(
                "[{}] dropped {} direct error frame(s) under congestion this session",
                self.label, self.dropped
            );
        }
    }
}
const CONTROL_RESPONSE_INITIAL_CHUNK_CREDIT: usize = 16;
const CONTROL_RESPONSE_MAX_CREDIT_GRANT: usize = 64;
const CONTROL_BINDING_TTL_MS: i64 = 5 * 60 * 1000;
const DASHBOARD_MEDIA_CLIP_MAX_FRAMES: usize = 1000;
/// One dashboard-control method's declared surface. The effective method
/// table (`all_control_methods`) is the single source the method authorizer
/// (`authorize_dashboard_control_method`), the advertised feature list
/// (`control_features`), the per-method `<method>_available` status booleans,
/// and the upload-frame allowlist (`authorize_dashboard_control_upload`) all
/// derive from — a method added or re-gated in one place cannot drift out of
/// sync in the others. It is the union of two declaration sources, resolved
/// rows-first: tunnel columns on `gateway_routes::ROUTES` rows (twinned
/// methods, whose IAM operation derives from the route row — transport-
/// unification design §2.2) and the residue `CONTROL_ONLY_METHODS` below
/// (tunnel-only methods with no HTTP twin). The
/// `tunnel_method_partition_is_pinned` differential test freezes
/// which methods live on which side. Composite rollup booleans the SPA also
/// reads (peer mutations, managed context, …) stay hand-written next to the
/// derived block in `status_response_frame`.
#[derive(Clone, Copy)]
struct ControlMethodSpec {
    name: &'static str,
    /// Operation gating the method; `None` = any bound session (ping).
    op: Option<PeerOperation>,
    /// Listed in the `features` handshake. `subscribe_events` /
    /// `unsubscribe_events` ride the "events" umbrella; upload-only
    /// methods advertise through the "upload_frames" transport feature.
    advertised: bool,
    /// May also (or only) be delivered as an upload frame.
    upload: bool,
}

/// Advertised request method gated by `op`.
const fn method(name: &'static str, op: PeerOperation) -> ControlMethodSpec {
    ControlMethodSpec {
        name,
        op: Some(op),
        advertised: true,
        upload: false,
    }
}

/// Request method the feature list doesn't name (covered by an umbrella).
const fn internal(name: &'static str, op: PeerOperation) -> ControlMethodSpec {
    ControlMethodSpec {
        name,
        op: Some(op),
        advertised: false,
        upload: false,
    }
}

/// Advertised method that may also arrive as an upload frame.
const fn uploadable(name: &'static str, op: PeerOperation) -> ControlMethodSpec {
    ControlMethodSpec {
        name,
        op: Some(op),
        advertised: true,
        upload: true,
    }
}

/// The residue half of the tunnel method table: methods with no HTTP twin
/// — transport/handshake, credential custody (tunnel-scoped by design),
/// connection-scoped signaling/authority RPCs whose HTTP-era twin is the
/// `/ws` socket, `ControlMsg` intent lanes, media/clip upload lanes, and
/// the bootstrap/replay reads (the tunnel-only set the transport-
/// unification program's S11 removal stage left behind; `/session`,
/// `/recordings*`, and custody HTTP rows stay parked future decisions —
/// design §2.7). Do NOT add a method here if its HTTP twin has a route
/// row — declare it on the row (`Route::with_tunnel`) so its IAM
/// operation derives from the shared declaration. Never read directly
/// outside this module's union plumbing and pins — consume
/// `all_control_methods()` / `control_method_spec()`.
const CONTROL_ONLY_METHODS: &[ControlMethodSpec] = &[
    ControlMethodSpec {
        name: "ping",
        op: None,
        advertised: true,
        upload: false,
    },
    method("config", PeerOperation::PresenceRead),
    method("status", PeerOperation::PresenceRead),
    method("api_agent_card", PeerOperation::PresenceRead),
    method("api_cached_bootstrap_events", PeerOperation::SessionInspect),
    internal("subscribe_events", PeerOperation::SessionInspect),
    internal("unsubscribe_events", PeerOperation::SessionInspect),
    // The access inspect reads (api_access_overview, api_access_iam_state,
    // api_access_enrollment_requests, api_dashboard_targets), the connect
    // admin quartet (status, claim-code, config, unclaim), and the
    // trust-tier setter lives as a tunnel column on its route row — its IAM
    // operation derives from that row (S6). Hosted ceilings are immutable
    // `role:none`; the former setter is deliberately absent.
    // api_fleet_cert_request lives as a tunnel column on its ROW-NEW
    // (POST /api/access/fleet-cert/request — S6 closed the family's one
    // missing HTTP twin).
    // Credential custody (vault leases + client egress): granting, renewing,
    // revoking, and even reading lease status all sit behind the dedicated
    // gate — a scoped guest session can neither fuel nor drain a daemon, nor
    // see which providers are fueled. Raw egress_* relay frames are a
    // separate wire family and deliberately not methods here.
    method(
        "api_credential_lease_grant",
        PeerOperation::CredentialsManage,
    ),
    method(
        "api_credential_lease_renew",
        PeerOperation::CredentialsManage,
    ),
    method(
        "api_credential_lease_revoke",
        PeerOperation::CredentialsManage,
    ),
    method(
        "api_credential_lease_status",
        PeerOperation::CredentialsManage,
    ),
    method(
        "api_credential_custody_trail",
        PeerOperation::CredentialsManage,
    ),
    // Daemon-local vault blob storage (the local-vault half of custody):
    // blind E2E ciphertext the daemon can neither read nor forge, so a
    // direct dashboard has a vault home without any Connect service in
    // the loop. Same gate as leases — fetch included: envelope metadata
    // and revision history are custody-sensitive.
    method("api_daemon_vault_fetch", PeerOperation::CredentialsManage),
    method("api_daemon_vault_publish", PeerOperation::CredentialsManage),
    // Write-only vault deposits (vault_deposits.rs): the dashboard
    // publishes the vault's deposit public key here and folds queued
    // deposits into the blob on unlock. All ciphertext/public material —
    // the daemon can neither read deposits nor mint vault entries.
    method(
        "api_daemon_vault_deposit_key_fetch",
        PeerOperation::CredentialsManage,
    ),
    method(
        "api_daemon_vault_deposit_key_publish",
        PeerOperation::CredentialsManage,
    ),
    method(
        "api_daemon_vault_deposits_fetch",
        PeerOperation::CredentialsManage,
    ),
    method(
        "api_daemon_vault_deposits_consume",
        PeerOperation::CredentialsManage,
    ),
    method(
        "api_credential_egress_register",
        PeerOperation::CredentialsManage,
    ),
    method(
        "api_credential_egress_unregister",
        PeerOperation::CredentialsManage,
    ),
    method(
        "api_credential_egress_probe",
        PeerOperation::CredentialsManage,
    ),
    // The IAM grant mutations (upsert/update), enrollment decide, and
    // the seven org-manage methods live as tunnel columns on their route
    // rows — their IAM operations derive from the rows (S6).
    // The signed-org doorbell methods (present, orl, orl_apply, renew)
    // live as tunnel columns on their PUBLIC route rows with documented
    // op overrides — session-gated on the tunnel by design (S6; the
    // override enumeration test in gateway_routes pins the set).
    // The peers/coordinator federation family (registry, eligibility,
    // pairing, quick controls, signaling relays, coordinator routing)
    // lives as tunnel columns on its carved per-leaf route rows (S7):
    // each twin's IAM operation derives from federation_http_operation
    // on its row's canonical leaf — acting through a connected peer
    // (quick controls + signaling relays) classifies as peer use, not
    // peer administration, exactly as the HTTP gate has always ruled.
    // Coordinator routing joined that rule on the 2026-07-11 owner
    // decision: it delegates this daemon's peer identity like the quick
    // controls, so both lanes gate on PeerUse (its historical
    // PeerManage-tunnel / Task-HTTP op override is gone).
    // The sessions read-core methods (api_sessions, api_sessions_search,
    // api_session_detail, api_session_agent_output,
    // api_session_context_snapshot) live as tunnel columns on their
    // route rows — their IAM operations derive from the rows (S4a; the
    // formerly divergent agent-output/context-snapshot twins are now
    // derivation-equal by construction).
    // The session artifact reads (api_session_report,
    // api_session_recordings, api_session_recording_asset,
    // api_session_frame_asset), api_session_delete, and the worktrees
    // quartet live as tunnel columns on their route rows (S4b), and so
    // does the whole current-session family — history/rollback/redo/
    // prune, changes, agent-output, uploads list/raw/delete, the
    // upload-frame-only api_session_current_upload (S4c), and the
    // Stream-lane api_sessions_stream (S10).
    method("api_session_control_msg", PeerOperation::SessionManage),
    // The api_fs_* methods live as tunnel columns on their route rows
    // (gateway_routes::ROUTES, /api/fs/*) — the first family whose tunnel
    // ops derive from the rows instead of entries here — and the
    // api_transfer_* family joined them with its /api/transfers rows
    // (S9, design §4, task #6). Tunnel-side transfer chunks still arrive
    // only as upload frames: their destination was path-scoped when the
    // job was created, so the chunk needs only the write operation
    // (`authorize_dashboard_control_upload`), which now derives from the
    // chunk row like everything else.
    method("api_display_bootstrap", PeerOperation::DisplayView),
    method("api_display_webrtc_signal", PeerOperation::DisplayView),
    // api_displays and api_diagnostics_visual_freshness live as tunnel
    // columns on their route rows (S5); the signaling/authority methods
    // below stay residue (their HTTP-era twin is /ws, not a route).
    method(
        "api_display_input_authority_snapshot",
        PeerOperation::DisplayInput,
    ),
    method(
        "api_display_input_authority_request",
        PeerOperation::DisplayInput,
    ),
    method(
        "api_display_input_authority_release",
        PeerOperation::DisplayInput,
    ),
    method("api_control_msg", PeerOperation::Message),
    method("api_dashboard_action_msg", PeerOperation::Message),
    method("api_mcp_tool_call", PeerOperation::Message),
    // The settings/keys family (api_settings, api_settings_save,
    // api_key_status, api_api_keys_save, api_project_root) lives as
    // tunnel columns on its route rows (S5).
    method("api_voice_session", PeerOperation::RuntimeControl),
    uploadable("api_presence_video_frame", PeerOperation::RuntimeControl),
    uploadable("api_media_annotation_attach", PeerOperation::RuntimeControl),
    uploadable("api_media_annotation_submit", PeerOperation::RuntimeControl),
    method("api_media_clip_start", PeerOperation::RuntimeControl),
    uploadable("api_media_clip_frame", PeerOperation::RuntimeControl),
    method("api_media_clip_end", PeerOperation::RuntimeControl),
    method("api_media_clip_cancel", PeerOperation::RuntimeControl),
    method("api_recordings", PeerOperation::RuntimeControl),
    method("api_recording_asset", PeerOperation::RuntimeControl),
    method(
        "api_browser_workspace_snapshot",
        PeerOperation::SessionInspect,
    ),
    method("api_state_snapshot", PeerOperation::SessionInspect),
    method("api_session_log_replay", PeerOperation::SessionInspect),
    method(
        "api_external_session_activity_replay",
        PeerOperation::SessionInspect,
    ),
    method("api_dashboard_bootstrap", PeerOperation::SessionInspect),
    // The api_managed_context_* trio lives as tunnel columns on the
    // /api/managed-context/* route rows (S4c); api_external_agents on
    // its row (S5).
];

/// The effective method table: route-row tunnel specs first (in ROUTES
/// declaration order, with the IAM operation derived from each row —
/// `Route::tunnel_operation`), then the residue `CONTROL_ONLY_METHODS`.
/// Materialized once; every consumer sees the same union. Resolution is
/// deterministic — rows win — so even the (unlandable, pin-tested) state
/// of a method declared on both sides cannot flap between operations. A
/// tunnel row without a fail-closed operation derivation (non-Operation
/// authz and no override; equally unlandable per the gateway invariant
/// test) is skipped entirely, leaving the authorizer's unknown-method
/// deny as the runtime backstop.
fn all_control_methods() -> &'static [ControlMethodSpec] {
    static METHODS: std::sync::OnceLock<Vec<ControlMethodSpec>> = std::sync::OnceLock::new();
    METHODS.get_or_init(|| {
        let mut methods: Vec<ControlMethodSpec> = crate::gateway_routes::tunnel_specs()
            .filter_map(|(route, spec)| {
                let op = route.tunnel_operation()?;
                Some(ControlMethodSpec {
                    name: spec.name,
                    op: Some(op),
                    advertised: spec.advertised,
                    upload: spec.upload,
                })
            })
            .collect();
        methods.extend_from_slice(CONTROL_ONLY_METHODS);
        methods
    })
}

fn control_method_spec(method: &str) -> Option<&'static ControlMethodSpec> {
    // Indexed once: this lookup runs at least twice per tunnel request
    // (authorizer + dispatch), and the effective table is ~150 entries.
    // `or_insert` preserves the table's first-wins resolution order.
    static INDEX: std::sync::OnceLock<HashMap<&'static str, &'static ControlMethodSpec>> =
        std::sync::OnceLock::new();
    INDEX
        .get_or_init(|| {
            let methods = all_control_methods();
            let mut index = HashMap::with_capacity(methods.len());
            for spec in methods {
                index.entry(spec.name).or_insert(spec);
            }
            index
        })
        .get(method)
        .copied()
}

/// Transport/frame-family features that aren't request methods (chunking,
/// credit, frame families, the events umbrella covering
/// `subscribe_events`/`unsubscribe_events`).
const CONTROL_WIRE_FEATURES: &[&str] = &[
    "events",
    "response_chunks",
    "response_credit",
    "stream_frames",
    "byte_streams",
    "upload_frames",
    "terminal_frames",
    "presence_frames",
    "presence_active_handoff",
    "presence_tool_request",
];

/// The advertised `features` list: every advertised method in the
/// effective table (route-row tunnel specs ∪ the `CONTROL_ONLY_METHODS`
/// residue) plus the wire features. Consumers membership-test — order
/// carries no meaning.
fn control_features() -> &'static [&'static str] {
    static FEATURES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    FEATURES.get_or_init(|| {
        let mut features: Vec<&'static str> = CONTROL_WIRE_FEATURES.to_vec();
        features.extend(
            all_control_methods()
                .iter()
                .filter(|spec| spec.advertised)
                .map(|spec| spec.name),
        );
        features
    })
}

/// Runtime prerequisites for a method beyond its operation grant: the
/// subsystem the method drives must be wired on this daemon (peer registry
/// configured, project root known, display-authority bridge present, MCP
/// server running). `true` for methods with no such dependency.
fn control_method_runtime_ready(runtime: &ControlRuntime, method: &str) -> bool {
    match method {
        "api_peers"
        | "api_peer_eligible"
        | "api_peer_add"
        | "api_peer_remove"
        | "api_peer_message"
        | "api_peer_task"
        | "api_peer_approval"
        | "api_peer_webrtc_signal"
        | "api_peer_file_transfer_signal"
        | "api_peer_dashboard_control_signal"
        | "api_coordinator_route" => runtime.peer_registry.is_some(),
        // Settings have their own durable root even when the daemon is
        // projectless; method execution resolves it from RuntimeSettingsState.
        "api_settings_save" => true,
        "api_access_connect_config" | "api_access_connect_unclaim" => {
            runtime.project_root.is_some()
        }
        "api_mcp_tool_call" => runtime.mcp_server.is_some(),
        // api_transfer_* deliberately has no project_root gate: the store
        // resolves through StoreScope (daemon-global fallback on projectless
        // daemons), and the S9 HTTP rows already serve projectless — the
        // old gate made the tunnel lane lie about the same store.
        method if method.starts_with("api_display_input_authority_") => {
            runtime.display_authority.is_some()
        }
        _ => true,
    }
}

const UDP_BUF_LEN: usize = 2000;
const COMMAND_CHANNEL: usize = 16;
const TCP_OUT_QUEUE: usize = 256;
type TcpFrameSender = mpsc::Sender<Vec<u8>>;

pub struct DashboardControlRegistry {
    config: crate::web_gateway::WebGatewayConfig,
    broadcast_tx: tokio::sync::broadcast::Sender<String>,
    bus: crate::event::EventBus,
    peer_registry: Option<crate::peer::PeerRegistry>,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    shared_session: crate::web_gateway::SharedActiveSession,
    project_root: Option<PathBuf>,
    worktree_inventory_cache: Arc<std::sync::Mutex<Option<String>>>,
    terminal_registry: Arc<crate::terminal::TerminalRegistry>,
    task_tx: Option<mpsc::Sender<presence_core::TaskEnvelope>>,
    agent_card: serde_json::Value,
    bootstrap_caches: DashboardBootstrapCaches,
    display_authority: Option<DashboardDisplayAuthorityBridge>,
    presence: Option<DashboardPresenceBridge>,
    ice_config: crate::display::IceConfig,
    tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
    tabs: crate::web_gateway::DashboardTabsRegistry,
    identity: Mutex<Option<Arc<DaemonIdentity>>>,
    peers: Mutex<HashMap<String, DashboardControlPeer>>,
}

#[derive(Clone, Debug)]
pub enum DashboardControlGrant {
    TrustedLocal,
    UserClient {
        principal: crate::access::iam::AccessPrincipal,
        /// The opening IAM snapshot (shared from the stat-fingerprint
        /// cache — construction must not deep-clone the state per
        /// connection). Immutable for the session lifetime: liveness
        /// checks compare it against freshly loaded state.
        iam_state: std::sync::Arc<crate::access::iam::LocalIamState>,
        /// Production sessions reload this daemon-owned IAM directory through
        /// the stat-fingerprint cache before every authorization decision.
        /// `None` is reserved for hermetic in-memory tests.
        iam_cert_dir: Option<PathBuf>,
        /// Per-session memo for [`Self::opening_authority_is_current`];
        /// construct with `Default::default()`.
        authority_memo: OpeningAuthorityMemo,
    },
    Peer {
        fingerprint: String,
        label: String,
        profile: String,
        filesystem: crate::peer::access_policy::FilesystemAccessPolicy,
        /// Exact active peer-identity record that authenticated the opening,
        /// plus its daemon-owned directory for live revocation checks.
        identity_record: Option<crate::peer::access_policy::PeerIdentityRecord>,
        iam_cert_dir: Option<PathBuf>,
        /// Delegation-lane attribution (docs/src/trust-tiers.md § Two
        /// lanes): the browser identity key that signed the relayed
        /// offer, when one did. Attribution never widens authority —
        /// the peer profile above remains the ceiling — it gives the
        /// audit trail and the UI badge a human identity beside the
        /// daemon principal.
        attributed: Option<PeerAttribution>,
    },
}

/// Stable identity of the transport credential + principal + grant that
/// opened a dashboard-control session. Follow-up HTTP signaling is stateless,
/// so the registry retains this binding and refuses ICE/close from another
/// CA-valid certificate or from a principal whose grant changed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DashboardControlSessionOwner {
    TrustedLocal,
    UserClient {
        principal_id: String,
        grant_id: Option<String>,
        authn_kind: Option<String>,
        authn_binding: Option<String>,
    },
    Peer {
        fingerprint: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DashboardControlSessionMutation {
    Applied,
    NotFound,
    Forbidden,
}

/// Memo for [`DashboardControlGrant::opening_authority_is_current`]: the
/// exact IAM state snapshot (by `Arc` identity) the last **successful** full
/// validation ran against, plus the validated grant's expiry — the only
/// input of that verdict that changes without a state change. Clones of a
/// grant share the cell, which is sound because clones carry the identical
/// opening snapshot.
///
/// Trust invariants (do not weaken):
/// - Only positive verdicts are memoized; any negative outcome re-runs the
///   full validation on the next call.
/// - A hit requires `Arc::ptr_eq` with a snapshot this memo keeps alive, so
///   a match can never be an ABA false positive from a recycled allocation.
///   Every IAM edit — revocation included — reaches sessions as a *new* Arc
///   from the stat-fingerprint cache and therefore forces full revalidation.
/// - The expiry instant is re-checked on every hit (`IamGrant::is_active_at`
///   semantics): a grant expiring between two events is caught even though
///   the state snapshot never changed.
#[derive(Clone, Debug, Default)]
pub struct OpeningAuthorityMemo(Arc<std::sync::Mutex<Option<OpeningAuthorityMemoEntry>>>);

#[derive(Debug)]
struct OpeningAuthorityMemoEntry {
    /// The snapshot the full validation passed against. Holding the Arc
    /// pins the allocation, making the pointer comparison an identity test.
    validated: Arc<crate::access::iam::LocalIamState>,
    /// `expires_at_unix_ms` of the validated current grant.
    expires_at_unix_ms: Option<u64>,
}

impl OpeningAuthorityMemo {
    /// `Some(expires_at_unix_ms)` when `current` is the exact snapshot the
    /// last successful full validation ran against.
    fn validated_expiry(
        &self,
        current: &Arc<crate::access::iam::LocalIamState>,
    ) -> Option<Option<u64>> {
        let memo = self.0.lock().unwrap_or_else(|e| e.into_inner());
        memo.as_ref()
            .filter(|entry| Arc::ptr_eq(&entry.validated, current))
            .map(|entry| entry.expires_at_unix_ms)
    }

    fn store(
        &self,
        validated: Arc<crate::access::iam::LocalIamState>,
        expires_at_unix_ms: Option<u64>,
    ) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(OpeningAuthorityMemoEntry {
            validated,
            expires_at_unix_ms,
        });
    }

    #[cfg(test)]
    fn is_primed(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
}

/// The verified human identity behind a delegation-lane connection.
#[derive(Clone, Debug)]
pub struct PeerAttribution {
    /// base64url(sha256(raw P-256 point)) — the IAM binding value.
    pub fingerprint: String,
    /// The raw public key, retained for display/audit.
    #[allow(dead_code)] // recorded for the delegation-lane audit surface; not read yet
    pub public_key_b64u: String,
    /// The label of the enrolled user/client principal this key matches
    /// in the TARGET's local IAM, when it matches one. `None` = a valid
    /// signature from a key this daemon has never enrolled (attribution
    /// is still recorded; the audit shows the fingerprint).
    pub enrolled_label: Option<String>,
}

impl DashboardControlGrant {
    fn current_user_client_state(&self) -> Result<Arc<crate::access::iam::LocalIamState>, String> {
        let Self::UserClient {
            iam_state,
            iam_cert_dir,
            ..
        } = self
        else {
            return Err("dashboard grant is not a local IAM user/client".to_string());
        };
        match iam_cert_dir {
            Some(cert_dir) => crate::access::iam::load_state_cached_arc(cert_dir)
                .map_err(|error| format!("reload local IAM state: {error}")),
            None => Ok(Arc::clone(iam_state)),
        }
    }

    /// A long-lived browser session must never outlive the exact IAM record
    /// that opened it. Any grant/principal mutation (including downgrade,
    /// revocation, expiry/ORL materialization, scope edit, deletion, or a
    /// reload error) makes the session stale and forces a reconnect under the
    /// new authority. Unrelated IAM edits do not disturb it.
    ///
    /// Called per injected input event and per transfer-pump beat, so the
    /// full record validation is memoized per state snapshot (see
    /// [`OpeningAuthorityMemo`] for the trust argument): the state content
    /// is immutable within one cached `Arc`, revocation liveness arrives as
    /// a *new* `Arc`, and the expiry instant — the one time-dependent input
    /// — is re-checked on every call, memo hit or not.
    pub(crate) fn opening_authority_is_current(&self) -> bool {
        match self {
            Self::UserClient {
                principal,
                iam_state,
                authority_memo,
                ..
            } => {
                let Some(grant_id) = principal.grant_id.as_deref() else {
                    return false;
                };
                let Ok(current) = self.current_user_client_state() else {
                    return false;
                };
                let now_unix_ms = crate::access::client_key::now_unix_ms();
                if let Some(expires_at_unix_ms) = authority_memo.validated_expiry(&current) {
                    // This exact snapshot already passed the full validation
                    // below; only the expiry comparison involves the clock.
                    // Mirrors `IamGrant::is_active_at` (statuses were
                    // enforced at memo time and are immutable in-snapshot).
                    return match expires_at_unix_ms {
                        Some(expires) => (now_unix_ms as u128) < (expires as u128),
                        None => true,
                    };
                }
                let Some(opening_grant) =
                    iam_state.grants.iter().find(|grant| grant.id == grant_id)
                else {
                    return false;
                };
                let Some(current_grant) = current.grants.iter().find(|grant| grant.id == grant_id)
                else {
                    return false;
                };
                let Some(opening_principal) = iam_state
                    .principals
                    .iter()
                    .find(|record| record.id == principal.id)
                else {
                    return false;
                };
                let Some(current_principal) = current
                    .principals
                    .iter()
                    .find(|record| record.id == principal.id)
                else {
                    return false;
                };
                if crate::access::hosted_control::is_hosted_lease_principal(principal) {
                    let records_current = opening_grant == current_grant
                        && opening_principal == current_principal
                        && crate::access::iam::is_enforced_status(&current_grant.status)
                        && crate::access::iam::is_enforced_status(&current_principal.status)
                        && crate::access::hosted_control::hosted_preset_for_principal(
                            &current, principal,
                        )
                        .is_ok();
                    if records_current {
                        authority_memo
                            .store(Arc::clone(&current), current_grant.expires_at_unix_ms);
                    }
                    return records_current
                        && match current_grant.expires_at_unix_ms {
                            Some(expires) => (now_unix_ms as u128) < (expires as u128),
                            None => false,
                        };
                }
                let role_id = if opening_grant.role_id.trim().is_empty() {
                    "role:scoped-human"
                } else {
                    opening_grant.role_id.as_str()
                };
                let Some(opening_role) = iam_state.roles.iter().find(|role| role.id == role_id)
                else {
                    return false;
                };
                let Some(current_role) = current.roles.iter().find(|role| role.id == role_id)
                else {
                    return false;
                };
                let hosted_provenance_unchanged = principal.authn_kind.as_deref()
                    != Some("client_key")
                    || iam_state.hosted_origins == current.hosted_origins;
                let records_current = opening_grant == current_grant
                    && opening_principal == current_principal
                    && opening_role == current_role
                    && hosted_provenance_unchanged
                    && crate::access::iam::is_enforced_status(&current_grant.status)
                    && crate::access::iam::is_enforced_status(&current_principal.status);
                if records_current {
                    // Everything except the expiry instant validated true
                    // for this snapshot; hits re-run only the expiry check.
                    authority_memo.store(Arc::clone(&current), current_grant.expires_at_unix_ms);
                }
                // `records_current` covers the grant-status half of
                // `is_active_at`; the expiry half stays live.
                records_current
                    && match current_grant.expires_at_unix_ms {
                        Some(expires) => (now_unix_ms as u128) < (expires as u128),
                        None => true,
                    }
            }
            Self::Peer {
                fingerprint,
                identity_record,
                iam_cert_dir,
                ..
            } => match (identity_record, iam_cert_dir) {
                (None, None) => true,
                (Some(opening), Some(cert_dir)) => {
                    let now_unix = crate::access::client_key::now_unix_ms() / 1000;
                    matches!(
                        crate::peer::access_policy::lookup_identity_cached_arc(cert_dir, fingerprint),
                        Ok(Some(current)) if current.as_ref() == opening && current.is_active(now_unix)
                    )
                }
                _ => false,
            },
            Self::TrustedLocal => true,
        }
    }

    fn signaling_owner(&self) -> DashboardControlSessionOwner {
        match self {
            Self::TrustedLocal => DashboardControlSessionOwner::TrustedLocal,
            Self::UserClient { principal, .. } => DashboardControlSessionOwner::UserClient {
                principal_id: principal.id.clone(),
                grant_id: principal.grant_id.clone(),
                authn_kind: principal.authn_kind.clone(),
                authn_binding: principal.authn_binding.clone(),
            },
            Self::Peer { fingerprint, .. } => DashboardControlSessionOwner::Peer {
                fingerprint: fingerprint.clone(),
            },
        }
    }

    pub(crate) fn label(&self) -> &str {
        match self {
            Self::TrustedLocal => "trusted-local",
            Self::UserClient { principal, .. } => principal.label.as_str(),
            Self::Peer { label, .. } => label.as_str(),
        }
    }

    /// Coarse provenance bucket for the tabs-presence surface: the
    /// owner's own dashboard, an enrolled client key, or a federated
    /// peer / delegation-lane connection.
    pub(crate) fn connection_kind(&self) -> &'static str {
        match self {
            Self::TrustedLocal => "local",
            Self::UserClient { .. } => "client",
            Self::Peer { .. } => "peer",
        }
    }

    fn profile(&self) -> Option<&str> {
        match self {
            Self::TrustedLocal | Self::UserClient { .. } => None,
            Self::Peer { profile, .. } => Some(profile.as_str()),
        }
    }

    pub(crate) fn filesystem(&self) -> Option<crate::peer::access_policy::FilesystemAccessPolicy> {
        match self {
            // TrustedLocal is the owner's own dashboard. Every enrolled
            // client, including a root-role client, is re-derived from IAM.
            Self::TrustedLocal => None,
            Self::UserClient { principal, .. } => match self.current_user_client_state() {
                Ok(state) => crate::access::iam::fs_scope_for_principal(&state, principal).cloned(),
                // A malformed/unreadable live IAM file must not turn a scoped
                // grant into unrestricted filesystem access.
                Err(_) => Some(crate::peer::access_policy::FilesystemAccessPolicy::default()),
            },
            Self::Peer { filesystem, .. } => Some(if self.opening_authority_is_current() {
                filesystem.clone()
            } else {
                crate::peer::access_policy::FilesystemAccessPolicy::default()
            }),
        }
    }

    fn access_principal(&self) -> crate::access::iam::AccessPrincipal {
        match self {
            Self::TrustedLocal => crate::access::iam::AccessPrincipal::root_dashboard_session(
                "dashboard-control",
                "webrtc-datachannel",
            ),
            Self::UserClient { principal, .. } => {
                let mut current = principal.clone();
                if let Ok(state) = self.current_user_client_state() {
                    if let Some(grant_id) = principal.grant_id.as_deref() {
                        if let Some(grant) = state.grants.iter().find(|grant| grant.id == grant_id)
                        {
                            current.role_id = if grant.role_id.trim().is_empty() {
                                "role:scoped-human".to_string()
                            } else {
                                grant.role_id.clone()
                            };
                        }
                    }
                }
                current
            }
            Self::Peer {
                fingerprint,
                label,
                profile,
                ..
            } => crate::access::iam::AccessPrincipal::peer_daemon(
                fingerprint.clone(),
                label.clone(),
                profile.clone(),
                "peer-dashboard-control",
            ),
        }
    }

    /// Origin class of this session for the custody trail —
    /// `hosted` / `direct` / `local` / `peer`
    /// (`access::iam::session_origin_class`). `UserClient` grants carry
    /// their IAM snapshot's `hosted_origins`.
    pub(crate) fn custody_origin_class(&self) -> &'static str {
        match self {
            Self::TrustedLocal => "local",
            Self::UserClient { principal, .. } => self
                .current_user_client_state()
                .map(|state| {
                    crate::access::iam::session_origin_class(&state.hosted_origins, principal)
                })
                .unwrap_or("direct"),
            Self::Peer { .. } => "peer",
        }
    }

    /// The terminal actor lane for this connection: trusted-local and
    /// explicitly granted root principals own the root lane and see every
    /// shell session; everyone else acts as their principal id and sees only
    /// owned or shared sessions.
    pub(crate) fn terminal_actor(&self) -> crate::terminal::TerminalActor {
        let principal = self.access_principal();
        let is_root = match self {
            Self::TrustedLocal => true,
            Self::UserClient { .. } => {
                principal.role_id == "role:root" && self.opening_authority_is_current()
            }
            Self::Peer { .. } => false,
        };
        if is_root {
            crate::terminal::TerminalActor::Root
        } else {
            crate::terminal::TerminalActor::Principal(principal.id)
        }
    }

    /// Whether this connection is one of the owner's authenticated dashboard
    /// surfaces. Private user-session displays and actions that mint access to
    /// them must never be exposed merely by granting `display.view` or
    /// `display.input` to a delegate.
    pub(crate) fn has_owner_dashboard_authority(&self) -> bool {
        match self {
            Self::TrustedLocal => true,
            Self::UserClient { principal, .. } => {
                principal.role_id == "role:root" && self.opening_authority_is_current()
            }
            Self::Peer { .. } => false,
        }
    }

    /// Resolve a display through this connection's visibility boundary.
    /// Generic display permissions expose agent-visible displays only; the
    /// owner's authenticated dashboard may additionally resolve private user
    /// views.
    pub(crate) fn display_session(
        &self,
        registry: &crate::display::SessionRegistry,
        display_id: u32,
    ) -> Option<Arc<crate::display::DisplaySession>> {
        if self.has_owner_dashboard_authority() {
            registry.get_any(display_id)
        } else {
            registry.get(display_id)
        }
    }

    /// Enumerate displays through the same visibility boundary as
    /// [`Self::display_session`].
    pub(crate) fn display_ids(&self, registry: &crate::display::SessionRegistry) -> Vec<u32> {
        if self.has_owner_dashboard_authority() {
            registry.all_display_ids()
        } else {
            registry.display_ids()
        }
    }

    /// Apply the owner-only boundary to dashboard event streams. This hides
    /// explicit private ready/grant records, display-request prompts, and
    /// portal approval prompts from scoped dashboards. Display lifecycle
    /// failure/teardown metadata does not carry the original visibility bit
    /// and remains audit metadata rather than a secrecy boundary.
    pub(crate) fn allows_dashboard_event_line(&self, line: &str) -> bool {
        if let Self::UserClient { principal, .. } = self {
            if crate::access::hosted_control::is_hosted_lease_principal(principal) {
                return !Self::dashboard_event_line_requires_owner(line)
                    && self
                        .current_user_client_state()
                        .ok()
                        .and_then(|state| {
                            crate::access::hosted_control::hosted_preset_for_principal(
                                &state, principal,
                            )
                            .ok()
                        })
                        .is_some_and(|preset| {
                            crate::access::hosted_control::hosted_outbound_line_allowed(
                                preset, line,
                            )
                        });
            }
        }
        self.has_owner_dashboard_authority() || !Self::dashboard_event_line_requires_owner(line)
    }

    pub(crate) fn dashboard_event_line_requires_owner(line: &str) -> bool {
        if !line.contains("display_request_raised")
            && !line.contains("display_request_resolved")
            && !line.contains("display_approval_pending")
            && !line.contains("\"agent_visible\"")
        {
            return false;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return false;
        };
        match value.get("event").and_then(serde_json::Value::as_str) {
            Some(
                "display_request_raised" | "display_request_resolved" | "display_approval_pending",
            ) => true,
            Some("display_ready" | "user_display_granted") => {
                value
                    .get("agent_visible")
                    .and_then(serde_json::Value::as_bool)
                    == Some(false)
            }
            _ => false,
        }
    }

    /// Remove owner-only display events from a browser bootstrap replay.
    ///
    /// Session logs remain the daemon's audit record; this filters only the
    /// live dashboard projection so a historical private `display_ready`
    /// cannot recreate a denied display slot for a scoped client.
    pub(crate) fn filter_dashboard_replay_payload(&self, replay: &mut serde_json::Value) {
        if self.has_owner_dashboard_authority() && !self.is_hosted_lease() {
            return;
        }
        let Some(entries) = replay
            .get_mut("entries")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return;
        };
        let hosted = self.is_hosted_lease();
        let mut hidden_hosted_displays = HashSet::new();
        entries.retain(|entry| {
            let display_id = entry.get("display_id").and_then(serde_json::Value::as_u64);
            if hosted {
                match (
                    display_id,
                    entry
                        .get("agent_visible")
                        .and_then(serde_json::Value::as_bool),
                ) {
                    (Some(display_id), Some(true)) => {
                        hidden_hosted_displays.remove(&display_id);
                    }
                    (Some(display_id), Some(false)) => {
                        hidden_hosted_displays.insert(display_id);
                    }
                    _ => {}
                }
            }
            let targets_hidden_hosted_display =
                display_id.is_some_and(|display_id| hidden_hosted_displays.contains(&display_id));
            !targets_hidden_hosted_display
                && serde_json::to_string(entry).ok().is_some_and(|line| {
                    !Self::dashboard_event_line_requires_owner(&line)
                        && (!hosted || self.allows_dashboard_event_line(&line))
                })
        });
    }

    /// Display target carried by a serialized dashboard event, when present.
    /// Live event loops use it to suppress every event for an active private
    /// session, including variants such as resize that do not repeat the
    /// `agent_visible` bit.
    pub(crate) fn dashboard_event_display_id(line: &str) -> Option<u32> {
        if !line.contains("\"display_id\"") {
            return None;
        }
        serde_json::from_str::<serde_json::Value>(line)
            .ok()?
            .get("display_id")?
            .as_u64()
            .and_then(|display_id| u32::try_from(display_id).ok())
    }

    pub(crate) fn dashboard_event_targets_hidden_display(
        &self,
        line: &str,
        registry: &crate::display::SessionRegistry,
    ) -> bool {
        let Some(display_id) = Self::dashboard_event_display_id(line) else {
            return false;
        };
        registry.get_any(display_id).is_some()
            && self.display_session(registry, display_id).is_none()
    }

    pub(crate) fn access_decision(
        &self,
        op: crate::peer::access_policy::PeerOperation,
    ) -> crate::access::iam::AccessDecision {
        match self {
            Self::UserClient { principal, .. } => match self.current_user_client_state() {
                Ok(state) => crate::access::iam::evaluate_principal_operation_with_state(
                    &state, principal, op,
                ),
                Err(error) => crate::access::iam::AccessDecision::denied(
                    principal,
                    op,
                    format!("local IAM state is unavailable: {error}"),
                ),
            },
            Self::Peer { .. } if !self.opening_authority_is_current() => {
                let principal = self.access_principal();
                crate::access::iam::AccessDecision::denied(
                    &principal,
                    op,
                    "peer identity changed, expired, was revoked, or could not be reloaded",
                )
            }
            _ => crate::access::iam::evaluate_principal_operation(&self.access_principal(), op),
        }
    }

    /// Authorize the concrete action carried inside a multiplexed control RPC.
    /// The outer method's permission is only a conservative admission floor;
    /// it must not become a confused-deputy grant for every action in the
    /// method's allowlist.
    pub(crate) fn control_msg_access_decision(
        &self,
        ctrl: &ControlMsg,
    ) -> crate::access::iam::AccessDecision {
        let operation = crate::access::access_policy::control_msg_operation(ctrl);
        if let Self::UserClient { principal, .. } = self {
            if crate::access::hosted_control::is_hosted_lease_principal(principal) {
                let allowed = self
                    .current_user_client_state()
                    .ok()
                    .and_then(|state| {
                        crate::access::hosted_control::hosted_preset_for_principal(
                            &state, principal,
                        )
                        .ok()
                        .map(|preset| {
                            crate::access::hosted_control::hosted_control_msg_allowed(
                                &state, preset, ctrl,
                            )
                        })
                    })
                    .unwrap_or(false);
                if !allowed {
                    return crate::access::iam::AccessDecision::denied(
                        principal,
                        operation,
                        "concrete action or target is outside the hosted lease action wall",
                    );
                }
            }
        }
        if crate::access::access_policy::control_msg_requires_owner_dashboard(ctrl)
            && !self.has_owner_dashboard_authority()
        {
            let principal = self.access_principal();
            return crate::access::iam::AccessDecision::denied(
                &principal,
                operation,
                "owner dashboard authority is required for this action",
            );
        }
        self.access_decision(operation)
    }

    /// Pre-session fail-closed gate for transports that push snapshots or
    /// transcripts immediately after opening. A principal with no effective
    /// operation must not reach the WebSocket/DataChannel stage, because
    /// inbound frame authorization cannot retract already-sent outbound data.
    pub(crate) fn has_any_effective_operation(&self) -> bool {
        crate::access::access_policy::ALL_OPERATIONS
            .iter()
            .copied()
            .any(|operation| self.access_decision(operation).allowed)
    }

    /// Whether this grant may enter the legacy `/ws` event lane. That lane
    /// sends a whole-dashboard bootstrap and then an unfiltered broadcast
    /// stream, so method-level inbound authorization cannot make a narrower
    /// role safe. Until outbound events are permission-filtered, require the
    /// complete built-in observer read set. Root/operator and an equivalent
    /// custom role pass; scoped-human and single-purpose roles do not.
    pub(crate) fn allows_unfiltered_websocket_stream(&self) -> bool {
        use crate::access::access_policy::PeerOperation;

        if self.is_hosted_lease() {
            // Hosted sockets are not unfiltered: the outbound writer applies
            // `allows_dashboard_event_line` to bootstrap, direct, replay, and
            // live frames. This predicate is the admission hook retained by
            // the legacy `/ws` setup.
            return self.opening_authority_is_current();
        }
        [
            PeerOperation::PresenceRead,
            PeerOperation::StatsRead,
            PeerOperation::DisplayView,
            PeerOperation::AccessInspect,
            PeerOperation::PeerInspect,
            PeerOperation::SessionInspect,
        ]
        .into_iter()
        .all(|operation| self.access_decision(operation).allowed)
    }

    pub(crate) fn wire_kind(&self) -> &'static str {
        match self {
            Self::TrustedLocal => "trusted-local",
            Self::UserClient { .. } => "user-client",
            Self::Peer { .. } => "peer",
        }
    }

    pub(crate) fn is_hosted_lease(&self) -> bool {
        matches!(
            self,
            Self::UserClient { principal, .. }
                if crate::access::hosted_control::is_hosted_lease_principal(principal)
        )
    }

    /// Return the exact active hosted lease that opened this connection.
    /// This is transport-derived provenance for internal session creation,
    /// never a client-supplied identifier.
    pub(crate) fn hosted_lease_id(&self) -> Option<String> {
        let Self::UserClient { principal, .. } = self else {
            return None;
        };
        if !crate::access::hosted_control::is_hosted_lease_principal(principal) {
            return None;
        }
        let state = self.current_user_client_state().ok()?;
        crate::access::hosted_control::hosted_preset_for_principal(&state, principal).ok()?;
        let grant_id = principal.grant_id.as_deref()?;
        state
            .hosted_control
            .leases
            .iter()
            .find(|lease| {
                lease.status == crate::access::hosted_control::HostedLeaseStatus::Active
                    && lease.document.grant_id == grant_id
                    && lease.document.principal_id == principal.id
            })
            .map(|lease| lease.document.lease_id.clone())
    }

    fn hosted_preset(&self) -> Option<crate::access::hosted_control::HostedPreset> {
        let Self::UserClient { principal, .. } = self else {
            return None;
        };
        if !crate::access::hosted_control::is_hosted_lease_principal(principal) {
            return None;
        }
        let state = self.current_user_client_state().ok()?;
        crate::access::hosted_control::hosted_preset_for_principal(&state, principal).ok()
    }

    pub(crate) fn hosted_dashboard_method_allowed(&self, method: &str) -> bool {
        !self.is_hosted_lease()
            || self.hosted_preset().is_some_and(|preset| {
                crate::access::hosted_control::hosted_dashboard_method_allowed(preset, method)
            })
    }

    pub(crate) fn hosted_tunnel_frame_allowed(&self, frame_type: &str) -> bool {
        !self.is_hosted_lease()
            || self.hosted_preset().is_some_and(|preset| {
                crate::access::hosted_control::hosted_tunnel_frame_classification(
                    preset, frame_type,
                ) == Some(true)
            })
    }

    pub(crate) fn stamp_hosted_session_provenance(
        &self,
        ctrl: &mut ControlMsg,
    ) -> Result<(), String> {
        if !self.is_hosted_lease() {
            return Ok(());
        }
        if let ControlMsg::CreateSession {
            hosted_lease_id, ..
        } = ctrl
        {
            *hosted_lease_id = Some(
                self.hosted_lease_id()
                    .ok_or_else(|| "hosted lease is no longer active".to_string())?,
            );
        }
        Ok(())
    }

    pub(crate) fn sanitize_state_snapshot(&self, state: &mut presence_core::AgentStateSnapshot) {
        if !self.is_hosted_lease() {
            return;
        }
        state.pending_approval = None;
        state.pending_question = None;
        state.last_command_preview.clear();
        state.last_task_result = None;
        // Display inventory is projected from SessionRegistry through the
        // agent-visible boundary; the generic snapshot may name private
        // owner displays and must not become a second inventory lane.
        state.available_displays.clear();
    }

    pub(crate) fn project_runtime_config(&self, config: &serde_json::Value) -> serde_json::Value {
        if !self.is_hosted_lease() {
            return config.clone();
        }
        crate::access::hosted_control::project_hosted_runtime_config(config)
    }

    pub(crate) fn hosted_ws_frame_allowed(&self, value: &serde_json::Value) -> bool {
        let Self::UserClient { principal, .. } = self else {
            return true;
        };
        if !crate::access::hosted_control::is_hosted_lease_principal(principal) {
            return true;
        }
        self.current_user_client_state()
            .ok()
            .and_then(|state| {
                crate::access::hosted_control::hosted_preset_for_principal(&state, principal)
                    .ok()
                    .map(|preset| {
                        crate::access::hosted_control::hosted_ws_frame_allowed(
                            &state, preset, value,
                        )
                    })
            })
            .unwrap_or(false)
    }
}

#[derive(Clone, Default)]
pub struct DashboardBootstrapCaches {
    pub(crate) last_usage_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_live_usage_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_status_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_autonomy_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_external_agent_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) last_user_display_json: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) attached_external_sessions: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Latest change-detected per-session state lines (`session_vitals`,
    /// `session_goal`), replayed to every new connection. These
    /// emissions fire on change only — an idle session never repeats
    /// them — so a late joiner (browser refresh on an idle daemon, a
    /// peer transport attaching) would otherwise never learn state that
    /// last changed before it connected. Keyed session id → event kind
    /// → serialized wire line; pruned on `session_ended`.
    pub(crate) session_state_lines:
        Arc<std::sync::Mutex<BTreeMap<String, BTreeMap<&'static str, String>>>>,
}

type DashboardPresenceFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Clone)]
pub struct DashboardPresenceBridge {
    connect: Arc<dyn Fn(DashboardPresenceConnectRequest) -> DashboardPresenceFuture + Send + Sync>,
    disconnect:
        Arc<dyn Fn(DashboardPresenceDisconnectRequest) -> DashboardPresenceFuture + Send + Sync>,
    make_active:
        Arc<dyn Fn(DashboardPresenceMakeActiveRequest) -> DashboardPresenceFuture + Send + Sync>,
    cleanup: Arc<dyn Fn(String) -> DashboardPresenceFuture + Send + Sync>,
    record_voice_log: Arc<dyn Fn(String) + Send + Sync>,
}

#[derive(Clone)]
pub struct DashboardPresenceConnectRequest {
    pub session_id: String,
    pub control_tx: mpsc::UnboundedSender<serde_json::Value>,
    pub server_session_id: Option<String>,
    pub last_event_seq: u64,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub passive: bool,
}

#[derive(Clone)]
pub struct DashboardPresenceDisconnectRequest {
    pub session_id: String,
}

#[derive(Clone)]
pub struct DashboardPresenceMakeActiveRequest {
    pub session_id: String,
    pub control_tx: mpsc::UnboundedSender<serde_json::Value>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl DashboardPresenceBridge {
    pub fn new(
        connect: impl Fn(DashboardPresenceConnectRequest) -> DashboardPresenceFuture
            + Send
            + Sync
            + 'static,
        disconnect: impl Fn(DashboardPresenceDisconnectRequest) -> DashboardPresenceFuture
            + Send
            + Sync
            + 'static,
        make_active: impl Fn(DashboardPresenceMakeActiveRequest) -> DashboardPresenceFuture
            + Send
            + Sync
            + 'static,
        cleanup: impl Fn(String) -> DashboardPresenceFuture + Send + Sync + 'static,
        record_voice_log: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        Self {
            connect: Arc::new(connect),
            disconnect: Arc::new(disconnect),
            make_active: Arc::new(make_active),
            cleanup: Arc::new(cleanup),
            record_voice_log: Arc::new(record_voice_log),
        }
    }

    async fn connect(&self, request: DashboardPresenceConnectRequest) {
        (self.connect)(request).await
    }

    async fn disconnect(&self, request: DashboardPresenceDisconnectRequest) {
        (self.disconnect)(request).await
    }

    async fn make_active(&self, request: DashboardPresenceMakeActiveRequest) {
        (self.make_active)(request).await
    }

    async fn cleanup(&self, session_id: String) {
        (self.cleanup)(session_id).await
    }

    fn record_voice_log(&self, text: String) {
        (self.record_voice_log)(text)
    }
}

/// Callback signatures for [`DashboardDisplayAuthorityBridge`]: each takes the
/// dashboard client id (and a display id / id list) and returns event JSON.
type AuthoritySnapshotFn = dyn Fn(&str, &[u32]) -> Vec<serde_json::Value> + Send + Sync;
type AuthorityStateFrameFn = dyn Fn(&str, u32) -> Option<serde_json::Value> + Send + Sync;
type AuthorityRequestFn = dyn Fn(&str, u32, bool) -> Vec<serde_json::Value> + Send + Sync;
type AuthorityEventsFn = dyn Fn(&str, u32) -> Vec<serde_json::Value> + Send + Sync;
type AuthorityInputAuthorizedFn = dyn Fn(&str, u32) -> bool + Send + Sync;
type AuthorityInputRevisionFn = dyn Fn(u32) -> Arc<AtomicU64> + Send + Sync;

#[derive(Clone)]
pub struct DashboardDisplayAuthorityBridge {
    snapshot: Arc<AuthoritySnapshotFn>,
    state_frame: Arc<AuthorityStateFrameFn>,
    request: Arc<AuthorityRequestFn>,
    release: Arc<AuthorityEventsFn>,
    input_authorized: Arc<AuthorityInputAuthorizedFn>,
    input_revision: Arc<AuthorityInputRevisionFn>,
    cleanup: Arc<dyn Fn(&str) + Send + Sync>,
    subscribe: Arc<dyn Fn() -> tokio::sync::broadcast::Receiver<u32> + Send + Sync>,
}

impl DashboardDisplayAuthorityBridge {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        snapshot: impl Fn(&str, &[u32]) -> Vec<serde_json::Value> + Send + Sync + 'static,
        state_frame: impl Fn(&str, u32) -> Option<serde_json::Value> + Send + Sync + 'static,
        request: impl Fn(&str, u32, bool) -> Vec<serde_json::Value> + Send + Sync + 'static,
        release: impl Fn(&str, u32) -> Vec<serde_json::Value> + Send + Sync + 'static,
        input_authorized: impl Fn(&str, u32) -> bool + Send + Sync + 'static,
        input_revision: impl Fn(u32) -> Arc<AtomicU64> + Send + Sync + 'static,
        cleanup: impl Fn(&str) + Send + Sync + 'static,
        subscribe: impl Fn() -> tokio::sync::broadcast::Receiver<u32> + Send + Sync + 'static,
    ) -> Self {
        Self {
            snapshot: Arc::new(snapshot),
            state_frame: Arc::new(state_frame),
            request: Arc::new(request),
            release: Arc::new(release),
            input_authorized: Arc::new(input_authorized),
            input_revision: Arc::new(input_revision),
            cleanup: Arc::new(cleanup),
            subscribe: Arc::new(subscribe),
        }
    }

    fn snapshot(&self, session_id: &str, display_ids: &[u32]) -> Vec<serde_json::Value> {
        (self.snapshot)(session_id, display_ids)
    }

    fn state_frame(&self, session_id: &str, display_id: u32) -> Option<serde_json::Value> {
        (self.state_frame)(session_id, display_id)
    }

    fn request(
        &self,
        session_id: &str,
        display_id: u32,
        include_private: bool,
    ) -> Vec<serde_json::Value> {
        (self.request)(session_id, display_id, include_private)
    }

    fn release(&self, session_id: &str, display_id: u32) -> Vec<serde_json::Value> {
        (self.release)(session_id, display_id)
    }

    fn input_authorized(&self, session_id: &str, display_id: u32) -> bool {
        (self.input_authorized)(session_id, display_id)
    }

    fn input_revision(&self, display_id: u32) -> Arc<AtomicU64> {
        (self.input_revision)(display_id)
    }

    fn cleanup(&self, session_id: &str) {
        (self.cleanup)(session_id)
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<u32> {
        (self.subscribe)()
    }
}

impl DashboardControlRegistry {
    /// This daemon's own agent-card id — the target-id expectation for
    /// delegation-lane attribution (the browser signs the id it dialed;
    /// we verify it meant us).
    pub fn local_card_id(&self) -> String {
        self.agent_card
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    pub fn new(
        config: crate::web_gateway::WebGatewayConfig,
        broadcast_tx: tokio::sync::broadcast::Sender<String>,
        bus: crate::event::EventBus,
        peer_registry: Option<crate::peer::PeerRegistry>,
        mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
        shared_session: crate::web_gateway::SharedActiveSession,
        project_root: Option<PathBuf>,
        worktree_inventory_cache: Arc<std::sync::Mutex<Option<String>>>,
        terminal_registry: Arc<crate::terminal::TerminalRegistry>,
        task_tx: Option<mpsc::Sender<presence_core::TaskEnvelope>>,
        agent_card: serde_json::Value,
        bootstrap_caches: DashboardBootstrapCaches,
        display_authority: Option<DashboardDisplayAuthorityBridge>,
        presence: Option<DashboardPresenceBridge>,
        ice_config: crate::display::IceConfig,
        tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
        tabs: crate::web_gateway::DashboardTabsRegistry,
    ) -> Self {
        Self {
            config,
            broadcast_tx,
            bus,
            peer_registry,
            mcp_server,
            shared_session,
            project_root,
            worktree_inventory_cache,
            terminal_registry,
            task_tx,
            agent_card,
            bootstrap_caches,
            display_authority,
            presence,
            ice_config,
            tcp_peer_registry,
            tabs,
            identity: Mutex::new(None),
            peers: Mutex::new(HashMap::new()),
        }
    }

    /// The live tabs-presence registry (shared with the `/ws` lane); the
    /// HTTP `GET /api/dashboard/tabs` handler reads it through here.
    pub(crate) fn tabs(&self) -> &crate::web_gateway::DashboardTabsRegistry {
        &self.tabs
    }

    /// Annotate a control session with the client-declared tab id from
    /// its offer body (the offer paths learn the id after `answer_offer`
    /// has registered the session).
    pub(crate) fn note_tab_id(&self, session_id: &str, tab_id: &str) {
        self.tabs.note_tab_id(session_id, tab_id);
    }

    pub async fn answer_offer_with_grant(
        &self,
        offer_sdp: String,
        session_grant: Option<String>,
        client_nonce: Option<String>,
        grant: DashboardControlGrant,
    ) -> Result<DashboardControlAnswer, String> {
        let session_id = uuid::Uuid::new_v4().to_string();
        self.answer_offer_with_session_id_and_grant(
            session_id,
            offer_sdp,
            session_grant,
            client_nonce,
            grant,
        )
        .await
    }

    pub async fn answer_offer_with_session_id_and_grant(
        &self,
        session_id: String,
        offer_sdp: String,
        session_grant: Option<String>,
        client_nonce: Option<String>,
        grant: DashboardControlGrant,
    ) -> Result<DashboardControlAnswer, String> {
        self.answer_offer_with_session_id_grant_and_tcp(
            session_id,
            offer_sdp,
            session_grant,
            client_nonce,
            grant,
            None,
        )
        .await
    }

    pub async fn answer_offer_with_session_id_grant_and_tcp(
        &self,
        session_id: String,
        offer_sdp: String,
        session_grant: Option<String>,
        client_nonce: Option<String>,
        grant: DashboardControlGrant,
        tcp_advertised_addr: Option<SocketAddr>,
    ) -> Result<DashboardControlAnswer, String> {
        let identity = self.identity().await?;
        // Captured before `grant` moves into the session task; the tab id
        // (when the offer carried one) is annotated post-answer via
        // `note_tab_id`.
        let tab_entry = crate::web_gateway::DashboardTabConnection {
            lane: crate::web_gateway::DashboardTabLane::ControlTunnel,
            kind: grant.connection_kind(),
            label: grant.label().to_string(),
            tab_id: None,
            remote: None,
            user_agent: None,
            connected_at_unix_ms: crate::web_gateway::now_unix_ms(),
        };
        let (peer, answer_sdp, binding) = DashboardControlPeer::answer_offer(
            session_id.clone(),
            offer_sdp,
            session_grant,
            client_nonce,
            &self.config,
            self.broadcast_tx.clone(),
            self.bus.clone(),
            self.peer_registry.clone(),
            self.mcp_server.clone(),
            self.shared_session.clone(),
            self.project_root.clone(),
            self.worktree_inventory_cache.clone(),
            self.terminal_registry.clone(),
            self.task_tx.clone(),
            self.agent_card.clone(),
            self.bootstrap_caches.clone(),
            self.display_authority.clone(),
            self.presence.clone(),
            self.ice_config.clone(),
            Arc::clone(&self.tcp_peer_registry),
            tcp_advertised_addr,
            identity,
            grant,
            self.tabs.clone(),
        )
        .await
        .map_err(|e| e.to_string())?;
        let rejected = {
            let mut peers = self.peers.lock().await;
            insert_dashboard_control_peer_if_vacant(&mut peers, session_id.clone(), peer).err()
        };
        if let Some(rejected) = rejected {
            rejected.close().await;
            return Err(
                "dashboard-control session id is already occupied; refusing to replace it"
                    .to_string(),
            );
        }
        self.tabs.register(&session_id, tab_entry);
        Ok(DashboardControlAnswer {
            session_id,
            sdp: answer_sdp,
            binding,
        })
    }

    pub async fn add_ice_candidate(
        &self,
        session_id: &str,
        candidate_json: &serde_json::Value,
    ) -> Result<bool, String> {
        let peers = self.peers.lock().await;
        let Some(peer) = peers.get(session_id) else {
            return Ok(false);
        };
        peer.add_ice_candidate(candidate_json).await?;
        Ok(true)
    }

    pub(crate) async fn add_ice_candidate_for_grant(
        &self,
        session_id: &str,
        candidate_json: &serde_json::Value,
        caller: &DashboardControlGrant,
    ) -> Result<DashboardControlSessionMutation, String> {
        let peers = self.peers.lock().await;
        let Some(peer) = peers.get(session_id) else {
            return Ok(DashboardControlSessionMutation::NotFound);
        };
        if !peer.belongs_to(caller) {
            return Ok(DashboardControlSessionMutation::Forbidden);
        }
        peer.add_ice_candidate(candidate_json).await?;
        Ok(DashboardControlSessionMutation::Applied)
    }

    pub(crate) async fn close_for_grant(
        &self,
        session_id: &str,
        caller: &DashboardControlGrant,
    ) -> DashboardControlSessionMutation {
        let peer = {
            let mut peers = self.peers.lock().await;
            let Some(peer) = peers.get(session_id) else {
                return DashboardControlSessionMutation::NotFound;
            };
            if !peer.belongs_to(caller) {
                return DashboardControlSessionMutation::Forbidden;
            }
            peers
                .remove(session_id)
                .expect("dashboard-control peer existed under the same lock")
        };
        // Kill the live transport guard before any cleanup callback can yield.
        // Once an explicit close is accepted, no buffered control frame may
        // retain the opening grant while presence/authority teardown runs.
        peer.close().await;
        self.tabs.unregister(session_id);
        if let Some(bridge) = &self.display_authority {
            bridge.cleanup(session_id);
        }
        if let Some(bridge) = &self.presence {
            bridge.cleanup(session_id.to_string()).await;
        }
        DashboardControlSessionMutation::Applied
    }

    pub async fn close(&self, session_id: &str) {
        let peer = self.peers.lock().await.remove(session_id);
        if let Some(peer) = peer {
            peer.close().await;
        }
        self.tabs.unregister(session_id);
        if let Some(bridge) = &self.display_authority {
            bridge.cleanup(session_id);
        }
        if let Some(bridge) = &self.presence {
            bridge.cleanup(session_id.to_string()).await;
        }
    }

    async fn identity(&self) -> Result<Arc<DaemonIdentity>, String> {
        let mut guard = self.identity.lock().await;
        if let Some(identity) = guard.as_ref() {
            return Ok(Arc::clone(identity));
        }
        let identity = Arc::new(DaemonIdentity::load_or_create_default()?);
        *guard = Some(Arc::clone(&identity));
        Ok(identity)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DashboardControlAnswer {
    pub session_id: String,
    pub sdp: String,
    pub binding: DashboardControlBinding,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DashboardControlBinding {
    pub protocol: &'static str,
    pub session_id: String,
    pub daemon_public_key: String,
    pub created_unix_ms: i64,
    pub expires_unix_ms: i64,
    pub offer_sha256: String,
    pub answer_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_grant_sha256: Option<String>,
    pub signature: String,
}

impl DashboardControlBinding {
    pub fn new(
        identity: &DaemonIdentity,
        session_id: String,
        offer_sdp: &str,
        answer_sdp: &str,
        session_grant: Option<&str>,
        client_nonce: Option<&str>,
    ) -> Self {
        let daemon_public_key = identity.public_key_b64u();
        let created_unix_ms = chrono::Utc::now().timestamp_millis();
        let expires_unix_ms = created_unix_ms + CONTROL_BINDING_TTL_MS;
        let offer_sha256 = sha256_b64u(offer_sdp.as_bytes());
        let answer_sha256 = sha256_b64u(answer_sdp.as_bytes());
        let client_nonce = client_nonce
            .map(str::trim)
            .filter(|nonce| !nonce.is_empty())
            .map(str::to_string);
        let session_grant_sha256 = session_grant
            .map(str::trim)
            .filter(|grant| !grant.is_empty())
            .map(|grant| sha256_b64u(grant.as_bytes()));
        let mut binding = Self {
            protocol: CONTROL_SIGNATURE_CONTEXT,
            session_id,
            daemon_public_key,
            created_unix_ms,
            expires_unix_ms,
            offer_sha256,
            answer_sha256,
            client_nonce,
            session_grant_sha256,
            signature: String::new(),
        };
        let payload = binding.signing_payload();
        binding.signature = identity.sign_b64u(payload.as_bytes());
        binding
    }

    pub fn signing_payload(&self) -> String {
        let mut payload = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.protocol,
            self.session_id,
            self.daemon_public_key,
            self.created_unix_ms,
            self.expires_unix_ms,
            self.offer_sha256,
            self.answer_sha256,
        );
        if let Some(client_nonce) = &self.client_nonce {
            payload.push('\n');
            payload.push_str(client_nonce);
        }
        if let Some(session_grant_sha256) = &self.session_grant_sha256 {
            payload.push('\n');
            payload.push_str(session_grant_sha256);
        }
        payload
    }
}

pub struct DashboardControlPeer {
    command_tx: mpsc::Sender<ControlCommand>,
    shutdown: CancellationToken,
    owner: DashboardControlSessionOwner,
}

fn insert_dashboard_control_peer_if_vacant(
    peers: &mut HashMap<String, DashboardControlPeer>,
    session_id: String,
    peer: DashboardControlPeer,
) -> Result<(), DashboardControlPeer> {
    match peers.entry(session_id) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(peer);
            Ok(())
        }
        std::collections::hash_map::Entry::Occupied(_) => Err(peer),
    }
}

impl DashboardControlPeer {
    fn belongs_to(&self, caller: &DashboardControlGrant) -> bool {
        self.owner == caller.signaling_owner()
    }

    #[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
    async fn answer_offer(
        session_id: String,
        offer_sdp: String,
        session_grant: Option<String>,
        client_nonce: Option<String>,
        config: &crate::web_gateway::WebGatewayConfig,
        broadcast_tx: tokio::sync::broadcast::Sender<String>,
        bus: crate::event::EventBus,
        peer_registry: Option<crate::peer::PeerRegistry>,
        mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
        shared_session: crate::web_gateway::SharedActiveSession,
        project_root: Option<PathBuf>,
        worktree_inventory_cache: Arc<std::sync::Mutex<Option<String>>>,
        terminal_registry: Arc<crate::terminal::TerminalRegistry>,
        task_tx: Option<mpsc::Sender<presence_core::TaskEnvelope>>,
        agent_card: serde_json::Value,
        bootstrap_caches: DashboardBootstrapCaches,
        display_authority: Option<DashboardDisplayAuthorityBridge>,
        presence: Option<DashboardPresenceBridge>,
        ice_config: crate::display::IceConfig,
        tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
        tcp_advertised_addr: Option<SocketAddr>,
        identity: Arc<DaemonIdentity>,
        grant: DashboardControlGrant,
        tabs: crate::web_gateway::DashboardTabsRegistry,
    ) -> Result<(Self, String, DashboardControlBinding), CallerError> {
        let owner = grant.signaling_owner();
        let local_ufrag = new_control_ice_fragment();
        let local_pwd = new_control_ice_password();
        let mut setting_engine = SettingEngine::default();
        setting_engine.set_ice_credentials(local_ufrag.clone(), local_pwd);
        setting_engine
            .set_answering_dtls_role(RTCDtlsRole::Server)
            .map_err(|e| CallerError::WebRtc(format!("set answering DTLS role: {e}")))?;

        let rtc_config = RTCConfigurationBuilder::new()
            .with_ice_servers(to_rtc_ice_servers(&config.ice_servers))
            .build();
        let mut rtc = RTCPeerConnectionBuilder::new()
            .with_configuration(rtc_config)
            .with_setting_engine(setting_engine)
            .build()
            .map_err(|e| CallerError::WebRtc(format!("build control rtc peer: {e}")))?;

        let mut sockets = Vec::new();
        for ip in crate::access::routable_local_addrs(true) {
            let socket = match UdpSocket::bind(SocketAddr::new(ip, 0)).await {
                Ok(socket) => socket,
                Err(e) => {
                    eprintln!("[dashboard/control] skipping UDP bind on {ip}: {e}");
                    continue;
                }
            };
            let local = match socket.local_addr() {
                Ok(local) => local,
                Err(e) => {
                    eprintln!("[dashboard/control] skipping UDP socket on {ip}: {e}");
                    continue;
                }
            };
            let candidate = udp_host_candidate_init(local)?;
            match rtc.add_local_candidate(candidate) {
                Ok(()) => sockets.push(Arc::new(socket)),
                Err(e) => eprintln!("[dashboard/control] skipping UDP host candidate {local}: {e}"),
            }
        }
        if sockets.is_empty() {
            return Err(CallerError::WebRtc(
                "no usable local UDP sockets bound for dashboard control".into(),
            ));
        }

        let mut peer_registration = None;
        let mut tcp_conn_rx = None;
        let mut tcp_advertised = None;
        if let Some(advertised) =
            tcp_advertised_addr.filter(|a| !a.ip().is_loopback() && !a.ip().is_unspecified())
        {
            let (registration, rx) = tcp_peer_registry.register(local_ufrag.clone());
            peer_registration = Some(registration);
            tcp_conn_rx = Some(rx);
            tcp_advertised = Some(advertised);
            let candidate = tcp_host_candidate_init(advertised);
            if let Err(e) = rtc.add_local_candidate(candidate) {
                eprintln!("[dashboard/control] failed to add TCP host candidate {advertised}: {e}");
            } else {
                eprintln!(
                    "[dashboard/control] ICE-TCP enabled on {advertised} for ufrag {local_ufrag}"
                );
            }
        }

        let offer = RTCSessionDescription::offer(offer_sdp.clone())
            .map_err(|e| CallerError::WebRtc(format!("parse control offer: {e}")))?;
        rtc.set_remote_description(offer)
            .map_err(|e| CallerError::WebRtc(format!("set control remote offer: {e}")))?;
        let answer = rtc
            .create_answer(None)
            .map_err(|e| CallerError::WebRtc(format!("create control answer: {e}")))?;
        rtc.set_local_description(answer.clone())
            .map_err(|e| CallerError::WebRtc(format!("set control local answer: {e}")))?;

        let answer_sdp = answer.sdp;
        let binding = DashboardControlBinding::new(
            &identity,
            session_id.clone(),
            &offer_sdp,
            &answer_sdp,
            session_grant.as_deref(),
            client_nonce.as_deref(),
        );
        let display_peer_id = crate::display_peer_ids::allocate_dashboard_control_display_peer_id()
            .ok_or_else(|| {
                CallerError::WebRtc("dashboard display peer-id namespace exhausted".to_string())
            })?;
        let shutdown = CancellationToken::new();
        let runtime = ControlRuntime {
            session_id,
            daemon_public_key: identity.public_key_b64u(),
            created_unix_ms: binding.created_unix_ms,
            events_subscribed: false,
            events_sent: 0,
            response_credit_enabled: false,
            config: Arc::new(
                serde_json::to_value(config).unwrap_or_else(|_| serde_json::json!({})),
            ),
            bus,
            peer_registry,
            mcp_server,
            shared_session,
            project_root,
            worktree_inventory_cache,
            terminal_registry,
            task_tx,
            agent_card: Arc::new(agent_card),
            bootstrap_caches,
            display_authority,
            presence,
            ice_config,
            tcp_peer_registry,
            tcp_advertised,
            media_clip_ops: Arc::new(Mutex::new(HashMap::new())),
            control_frames_tx: None,
            display_peer_id,
            display_peer_sessions: Arc::new(Mutex::new(Vec::new())),
            grant,
            shutdown: shutdown.clone(),
            tabs,
            state_root: crate::platform::intendant_home(),
        };
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL);
        tokio::spawn(control_driver(
            rtc,
            sockets,
            tcp_conn_rx,
            tcp_advertised,
            peer_registration,
            runtime,
            broadcast_tx.subscribe(),
            command_rx,
            shutdown.clone(),
        ));
        Ok((
            Self {
                command_tx,
                shutdown,
                owner,
            },
            answer_sdp,
            binding,
        ))
    }

    async fn add_ice_candidate(&self, candidate_json: &serde_json::Value) -> Result<(), String> {
        let candidate_str = candidate_json
            .get("candidate")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if candidate_str.is_empty() {
            return Ok(());
        }
        let resolved = match crate::display::webrtc::resolve_mdns_in_candidate(candidate_str).await
        {
            Ok(candidate) => candidate,
            Err(e) => {
                eprintln!("[dashboard/control] mDNS resolve failed: {e}, dropping candidate");
                return Ok(());
            }
        };
        self.command_tx
            .send(ControlCommand::AddIceCandidate(resolved))
            .await
            .map_err(|_| "dashboard control driver gone".to_string())
    }

    async fn close(self) {
        self.shutdown.cancel();
    }
}

#[derive(Clone)]
pub(crate) struct ControlRuntime {
    session_id: String,
    daemon_public_key: String,
    created_unix_ms: i64,
    events_subscribed: bool,
    events_sent: u64,
    response_credit_enabled: bool,
    /// Shared, not owned: `ControlRuntime` is cloned per spawned request,
    /// and an owned tree deep-copied this multi-KB JSON every time.
    config: Arc<serde_json::Value>,
    bus: crate::event::EventBus,
    peer_registry: Option<crate::peer::PeerRegistry>,
    mcp_server: Option<Arc<crate::mcp::IntendantServer>>,
    shared_session: crate::web_gateway::SharedActiveSession,
    project_root: Option<PathBuf>,
    worktree_inventory_cache: Arc<std::sync::Mutex<Option<String>>>,
    terminal_registry: Arc<crate::terminal::TerminalRegistry>,
    task_tx: Option<mpsc::Sender<presence_core::TaskEnvelope>>,
    /// Shared like `config` (multi-KB tree; `ControlRuntime` is cloned
    /// per spawned request).
    agent_card: Arc<serde_json::Value>,
    bootstrap_caches: DashboardBootstrapCaches,
    display_authority: Option<DashboardDisplayAuthorityBridge>,
    presence: Option<DashboardPresenceBridge>,
    ice_config: crate::display::IceConfig,
    tcp_peer_registry: Arc<crate::display::webrtc::TcpPeerRegistry>,
    /// The ICE-TCP tuple supplied by an authenticated peer signaling path.
    /// Trusted local and independently verified direct-mTLS sessions use
    /// `None` and signal displays over the gateway WebSocket instead. Display
    /// offers arriving on a peer control channel advertise the same tuple —
    /// that peer reached us through it, so display traffic can too.
    tcp_advertised: Option<SocketAddr>,
    media_clip_ops: Arc<Mutex<HashMap<String, DashboardMediaClipOperation>>>,
    control_frames_tx: Option<mpsc::UnboundedSender<serde_json::Value>>,
    display_peer_id: crate::display::PeerId,
    /// Display sessions on which this control transport has attempted to
    /// register `display_peer_id`. A display's media WebRTC transport is
    /// separate from this control peer, so the control driver tracks sessions
    /// so teardown can close a still-live media peer on disconnect or IAM
    /// revocation without retaining a completed display session. The weak
    /// vector is pointer-deduplicated and pruned when offers arrive.
    display_peer_sessions: Arc<Mutex<Vec<std::sync::Weak<crate::display::DisplaySession>>>>,
    grant: DashboardControlGrant,
    /// Lifetime of the authenticated control transport. Interactive display
    /// channels created through it retain this token so queued input and
    /// clipboard access die with the session even if the separate display
    /// WebRTC transport has not reaped yet.
    shutdown: CancellationToken,
    /// The live tabs-presence registry (shared with the `/ws` lane) —
    /// serves the `api_dashboard_tabs` twin.
    tabs: crate::web_gateway::DashboardTabsRegistry,
    /// The daemon state root (`intendant_home()`), resolved once at the
    /// control-channel edge. Adapters that fall back to the daemon-global
    /// store (uploads, transfers) resolve their scope against this instead
    /// of ambient state, so the test runtime's scratch root keeps
    /// projectless fixtures out of the machine's real `~/.intendant`.
    state_root: PathBuf,
}

// The clip-operation type moved to web_gateway::media_store
// (transport-unification S8): the /ws lane accumulates with the same
// type the tunnel's media_clip_ops map stores.
pub(crate) use crate::web_gateway::DashboardMediaClipOperation;

pub(crate) enum ControlCommand {
    AddIceCandidate(String),
}

#[derive(Debug)]
struct InboundPacket {
    proto: TransportProtocol,
    source: SocketAddr,
    destination: SocketAddr,
    bytes: Vec<u8>,
    received_at: Instant,
}

/// Outbound transmits the drain dropped, by reason. Individual drops stay
/// silent (cross-family pairs are routine noise while ICE probes candidate
/// combinations), but a connection that dies with a nonzero tally logs the
/// summary — a misrouted-transmit bug then names itself instead of
/// presenting as a bare DTLS timeout (which once cost a full debugging
/// round on the hosted-Connect path).
#[derive(Debug, Default)]
pub(crate) struct TransmitDropStats {
    cross_family: u64,
    loopback_mismatch: u64,
    unknown_udp_source: u64,
    tcp_without_stream: u64,
}

impl TransmitDropStats {
    fn any(&self) -> bool {
        self.cross_family != 0
            || self.loopback_mismatch != 0
            || self.unknown_udp_source != 0
            || self.tcp_without_stream != 0
    }
}

pub(crate) struct ControlTaskResponse {
    id: String,
    frame: serde_json::Value,
    byte_stream: Option<ControlByteStream>,
    done: bool,
}

/// One spawned-lane response paired with the reservation generation that
/// produced it (see [`PendingControlRequests`]): the driver forwards a
/// response only while its generation still owns the id's reservation,
/// so a superseded task can neither reach the wire under a reused id nor
/// free its replacement's reservation.
pub(crate) type SequencedTaskResponse = (u64, ControlTaskResponse);

/// The spawned-request reservations for one tunnel connection, keyed by
/// request id with a per-admission GENERATION (the peer-transfer read
/// registry's pattern): same-id replacement cancels its predecessor and
/// mints a new generation, completion frees only its own generation, and
/// stale-generation responses are dropped.
///
/// Admission is bounded by LIVE WORK, not addressable entries: every
/// admit hands out an RAII [`LiveWorkSlot`] that the spawned task owns
/// until its work ACTUALLY ends (the handler future runs to completion —
/// awaited `spawn_blocking` segments included). A cancelled predecessor
/// therefore keeps holding its slot while it drains, so rapid same-id
/// cycling saturates [`MAX_PENDING_CONTROL_REQUESTS`] and gets refused
/// instead of stacking untracked work onto the blocking pool.
pub(crate) struct PendingControlRequests {
    entries: HashMap<String, PendingControlRequest>,
    /// Live-work ledger: strong count − 1 = slots currently held.
    live_work: Arc<()>,
    /// Committing-upload ledger: commits in flight with their byte
    /// weights (they left `inbound_uploads` but their work — and spool —
    /// lives until the commit finishes; the upload 8-cap counts them and
    /// the 256 MiB aggregate counts their bytes).
    committing_uploads: Arc<UploadCommitLedger>,
    next_generation: u64,
}

#[derive(Default)]
struct UploadCommitLedger {
    count: std::sync::atomic::AtomicUsize,
    bytes: std::sync::atomic::AtomicUsize,
}

struct PendingControlRequest {
    cancel: CancellationToken,
    generation: u64,
}

/// RAII live-work slot: dropping it is the ONLY thing that frees
/// admission capacity, so a slot must be owned by the spawned task (or
/// the upload state that becomes one) and live until the work ends.
pub(crate) struct LiveWorkSlot(#[allow(dead_code)] Arc<()>);

/// RAII committing-upload slot (see `committing_uploads`): carries the
/// commit's byte weight, debited from the ledger only when the commit
/// actually finishes.
pub(crate) struct UploadCommitSlot {
    ledger: Arc<UploadCommitLedger>,
    bytes: usize,
}

impl Drop for UploadCommitSlot {
    fn drop(&mut self) {
        self.ledger
            .count
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        self.ledger
            .bytes
            .fetch_sub(self.bytes, std::sync::atomic::Ordering::Relaxed);
    }
}

impl PendingControlRequests {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            live_work: Arc::new(()),
            committing_uploads: Arc::new(UploadCommitLedger::default()),
            next_generation: 0,
        }
    }

    /// Reserve a live-work slot for `id`, cancelling and replacing any
    /// predecessor ENTRY. The predecessor's SLOT stays with its task —
    /// capacity frees when that work exits, never at replacement.
    pub(crate) fn admit(&mut self, id: &str) -> (CancellationToken, u64, LiveWorkSlot) {
        if let Some(previous) = self.entries.remove(id) {
            previous.cancel.cancel();
        }
        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        let cancel = CancellationToken::new();
        self.entries.insert(
            id.to_string(),
            PendingControlRequest {
                cancel: cancel.clone(),
                generation,
            },
        );
        (
            cancel,
            generation,
            LiveWorkSlot(Arc::clone(&self.live_work)),
        )
    }

    /// Slots currently held by live work (draining predecessors included).
    pub(crate) fn live_work(&self) -> usize {
        Arc::strong_count(&self.live_work).saturating_sub(1)
    }

    /// One committing upload's cap + byte presence (held until the
    /// commit ends; `bytes` is the upload's actual received size).
    pub(crate) fn upload_commit_slot(&self, bytes: usize) -> UploadCommitSlot {
        self.committing_uploads
            .count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.committing_uploads
            .bytes
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        UploadCommitSlot {
            ledger: Arc::clone(&self.committing_uploads),
            bytes,
        }
    }

    /// Commits currently in flight.
    pub(crate) fn committing_uploads(&self) -> usize {
        self.committing_uploads
            .count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Bytes held by commits in flight (counted against the aggregate
    /// declared-bytes budget until each commit completes).
    pub(crate) fn committing_upload_bytes(&self) -> usize {
        self.committing_uploads
            .bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The client's `cancel` frame: cancel and free the reservation.
    pub(crate) fn cancel_remove(&mut self, id: &str) -> bool {
        match self.entries.remove(id) {
            Some(entry) => {
                entry.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Whether `generation` still owns `id`'s reservation.
    pub(crate) fn matches(&self, id: &str, generation: u64) -> bool {
        self.entries
            .get(id)
            .is_some_and(|entry| entry.generation == generation)
    }

    /// Free the reservation on completion — only for its own generation.
    pub(crate) fn complete(&mut self, id: &str, generation: u64) -> bool {
        if self.matches(id, generation) {
            self.entries.remove(id);
            true
        } else {
            false
        }
    }

    /// Test-only observers (production reads go through `matches` /
    /// `live_work` / `at_capacity`).
    #[cfg(test)]
    pub(crate) fn contains_key(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Spawn-time admission against LIVE WORK. Deliberately no same-id
    /// exemption: a replacement cannot make its predecessor's in-flight
    /// work disappear, so it must fit under the same bound.
    pub(crate) fn at_capacity(&self) -> bool {
        self.live_work() >= MAX_PENDING_CONTROL_REQUESTS
    }

    /// Teardown: cancel every reservation.
    pub(crate) fn cancel_all(&mut self) {
        for (_, entry) in self.entries.drain() {
            entry.cancel.cancel();
        }
    }
}

pub(crate) struct ControlByteStream {
    id: String,
    stream_id: String,
    content_type: String,
    filename: Option<String>,
    bytes: Vec<u8>,
    result: serde_json::Value,
}

/// Where an in-flight tunnel upload accumulates.
///
/// Payloads whose declared size fits [`UPLOAD_MEMORY_SPOOL_MAX_BYTES`]
/// accumulate in memory: every payload — including each recurring
/// presence webcam frame (~30–100 KB) — used to pay a tempfile
/// create/write/seek/read/unlink round-trip, with the per-chunk blocking
/// `write_all` running on the tunnel's wire-driver task (latency jitter
/// for everything multiplexed on the connection). Larger uploads spool
/// to a tempfile as before, with chunk writes batched through a small
/// buffer instead of one blocking write per 16 KiB frame.
pub(crate) enum UploadSpool {
    Memory(Vec<u8>),
    Disk {
        tmp: tempfile::NamedTempFile,
        /// Chunk bytes not yet written to `tmp`; flushed at
        /// [`UPLOAD_DISK_SPOOL_BUFFER_BYTES`] and at upload end.
        buf: Vec<u8>,
    },
}

/// Uploads at or under this declared size accumulate in memory. The
/// declared size is enforced by the chunk-lane byte checks, so the spool
/// can never grow past it.
const UPLOAD_MEMORY_SPOOL_MAX_BYTES: usize = 1024 * 1024;
/// Batch size for disk-spool writes on the wire-driver task.
const UPLOAD_DISK_SPOOL_BUFFER_BYTES: usize = 256 * 1024;
/// Concurrent in-flight upload transfers per tunnel connection.
const MAX_INBOUND_UPLOADS_PER_CONNECTION: usize = 8;
/// Aggregate DECLARED bytes across a connection's in-flight uploads —
/// admission control at upload_start, so a burst of starts cannot
/// reserve unbounded spool space regardless of the per-upload cap.
const MAX_INBOUND_UPLOAD_TOTAL_BYTES: usize = 256 * 1024 * 1024;
/// LIVE spawned-work slots per tunnel connection (request tasks, stream
/// framers + their line producers, upload states and their commit
/// tasks). Each spawned handler may construct a response as large as the
/// fs-read cap before queue admission runs, so the reservation happens
/// at spawn time and — via the RAII [`LiveWorkSlot`] — frees only when
/// the work actually ends, covering cancelled-but-draining predecessors.
///
/// Deliberate fail-closed occupancy: a WEDGED operation (an fs read
/// stuck on a dead mount, a producer that never finishes) legitimately
/// HOLDS its slot — slots measure live work, so 64 wedged operations
/// mean a fully occupied connection until teardown. That is the design:
/// the bound guarantees bounded memory, not bounded latency, and a
/// connection that has genuinely wedged 64 operations has no business
/// admitting more work it also cannot finish.
const MAX_PENDING_CONTROL_REQUESTS: usize = 64;

impl UploadSpool {
    fn for_declared_size(total_bytes: usize) -> std::io::Result<Self> {
        if total_bytes <= UPLOAD_MEMORY_SPOOL_MAX_BYTES {
            // Allocate as chunks arrive — reserving the declared size up
            // front let a burst of no-data upload_starts reserve
            // gigabytes; the received-bytes checks bound actual growth.
            Ok(Self::Memory(Vec::new()))
        } else {
            Ok(Self::Disk {
                tmp: tempfile::NamedTempFile::new()?,
                buf: Vec::with_capacity(UPLOAD_DISK_SPOOL_BUFFER_BYTES),
            })
        }
    }

    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Memory(data) => {
                data.extend_from_slice(bytes);
                Ok(())
            }
            Self::Disk { tmp, buf } => {
                buf.extend_from_slice(bytes);
                if buf.len() >= UPLOAD_DISK_SPOOL_BUFFER_BYTES {
                    tmp.as_file_mut().write_all(buf)?;
                    buf.clear();
                }
                Ok(())
            }
        }
    }

    /// Settle the spool at upload end: drain the write buffer and flush.
    fn finish(&mut self) -> std::io::Result<()> {
        if let Self::Disk { tmp, buf } = self {
            if !buf.is_empty() {
                tmp.as_file_mut().write_all(buf)?;
                buf.clear();
            }
            tmp.as_file_mut().flush()?;
        }
        Ok(())
    }

    /// The whole payload as bytes (the media handlers' shape): a memory
    /// spool moves out with zero I/O; a disk spool settles and reads
    /// back, exactly as the tempfile path always did.
    pub(crate) fn take_bytes(&mut self, expected_len: usize) -> Result<Vec<u8>, String> {
        self.finish()
            .map_err(|e| format!("flush upload spool: {e}"))?;
        let bytes = match self {
            Self::Memory(data) => std::mem::take(data),
            Self::Disk { tmp, .. } => {
                tmp.as_file_mut()
                    .seek(std::io::SeekFrom::Start(0))
                    .map_err(|e| format!("seek upload tempfile: {e}"))?;
                let mut bytes = Vec::with_capacity(expected_len);
                tmp.as_file_mut()
                    .read_to_end(&mut bytes)
                    .map_err(|e| format!("read upload tempfile: {e}"))?;
                bytes
            }
        };
        if bytes.len() != expected_len {
            return Err(format!(
                "upload byte count changed while committing: expected {}, got {}",
                expected_len,
                bytes.len()
            ));
        }
        Ok(bytes)
    }

    /// The whole payload as a [`crate::web_gateway::SpooledBody`]
    /// tempfile (the staged-upload / transfer-chunk / fs-write commit
    /// shape): a disk spool settles and hands over its tempfile; a
    /// memory spool writes once.
    fn into_spooled_tempfile(mut self) -> std::io::Result<tempfile::NamedTempFile> {
        self.finish()?;
        match self {
            Self::Memory(data) => {
                let mut tmp = tempfile::NamedTempFile::new()?;
                tmp.as_file_mut().write_all(&data)?;
                tmp.as_file_mut().flush()?;
                Ok(tmp)
            }
            Self::Disk { tmp, .. } => Ok(tmp),
        }
    }
}

pub(crate) struct InboundUploadState {
    method: String,
    params: serde_json::Value,
    spool: UploadSpool,
    total_bytes: usize,
    expected_chunks: usize,
    next_seq: usize,
    received_bytes: usize,
    /// The pending-request reservation generation minted at
    /// `upload_start` — the terminal task's response carries it so the
    /// driver can match it against the live reservation.
    generation: u64,
    /// The live-work slot minted at `upload_start`, carried through the
    /// receiving state and taken by the commit task at `upload_end` —
    /// one continuous slot from first frame to commit completion.
    /// `None` only in hermetic fixtures.
    slot: Option<LiveWorkSlot>,
}

impl InboundUploadState {
    /// The upload-frame spool as the Streaming lane's common handle
    /// (transport-unification S8): the frame params plus the spooled
    /// bytes, for handlers that commit the spool wholesale — the staged
    /// upload today, the S9 transfer-chunk appends next. The media
    /// handlers read bytes out instead ([`UploadSpool::take_bytes`]).
    pub(crate) fn into_spooled_body(
        self,
    ) -> std::io::Result<(serde_json::Value, crate::web_gateway::SpooledBody)> {
        let len = self.received_bytes;
        let tmp = self.spool.into_spooled_tempfile()?;
        Ok((self.params, crate::web_gateway::SpooledBody { tmp, len }))
    }
}

pub(crate) struct OutboundControlQueue {
    frames: VecDeque<QueuedControlFrame>,
    /// Bytes resident in `frames` (immediate texts + chunked payloads with
    /// their start/end envelopes), maintained on every enqueue/removal.
    /// Chunked enqueues are refused above
    /// [`CONTROL_OUTBOUND_QUEUE_MAX_BYTES`] — before this cap, N stalled
    /// credit-gated downloads pinned N full payload materializations for
    /// as long as the client stayed quiet.
    queued_bytes: usize,
}

enum QueuedControlFrame {
    Immediate { request_id: String, text: String },
    Chunked(QueuedChunkedFrame),
}

impl QueuedControlFrame {
    /// Byte accounting for [`OutboundControlQueue::queued_bytes`], captured
    /// at enqueue and debited verbatim at removal (chunked frames memoize
    /// theirs, so draining `start`/`end` out of the plan cannot skew it).
    fn queued_bytes(&self) -> usize {
        match self {
            Self::Immediate { text, .. } => text.len(),
            Self::Chunked(queued) => queued.accounted_bytes,
        }
    }
}

struct QueuedChunkedFrame {
    plan: ChunkedFramePlan,
    /// `plan` size at enqueue (see [`QueuedControlFrame::queued_bytes`]).
    accounted_bytes: usize,
    next_chunk: usize,
    credit: usize,
    started: bool,
}

/// A chunked wire frame with its payload held raw and every
/// `*_chunk` frame rendered lazily at send time. The old shape
/// materialized each base64+JSON chunk String up front — ~1.37× the
/// payload held simultaneously with the payload itself — and the drain
/// cloned each chunk String again at send: ~5 copies end to end and
/// ~2.4× payload peak RSS per queued download.
pub(crate) struct ChunkedFramePlan {
    pub(crate) request_id: String,
    pub(crate) chunk_id: String,
    /// Small header/footer frames, rendered eagerly (they carry counts and
    /// metadata, not payload).
    pub(crate) start: String,
    pub(crate) end: String,
    envelope: ChunkEnvelope,
    payload: Vec<u8>,
    chunk_bytes: usize,
}

/// Which chunk-frame envelope [`ChunkedFramePlan::render_chunk`] emits.
enum ChunkEnvelope {
    /// `response_chunk` frames around chunked JSON response text.
    Response,
    /// `byte_stream_chunk` frames around raw byte payloads.
    ByteStream,
}

impl ChunkedFramePlan {
    /// Plan for chunked JSON response text (`response_chunk` envelopes).
    pub(crate) fn response(
        request_id: String,
        chunk_id: String,
        start: String,
        end: String,
        payload: Vec<u8>,
        chunk_bytes: usize,
    ) -> Self {
        Self {
            request_id,
            chunk_id,
            start,
            end,
            envelope: ChunkEnvelope::Response,
            payload,
            chunk_bytes,
        }
    }

    /// Plan for a raw byte download (`byte_stream_chunk` envelopes).
    pub(crate) fn byte_stream(
        request_id: String,
        chunk_id: String,
        start: String,
        end: String,
        payload: Vec<u8>,
        chunk_bytes: usize,
    ) -> Self {
        Self {
            request_id,
            chunk_id,
            start,
            end,
            envelope: ChunkEnvelope::ByteStream,
            payload,
            chunk_bytes,
        }
    }

    pub(crate) fn chunk_count(&self) -> usize {
        // Zero chunk size renders zero chunks (`render_chunk` returns
        // `None` for it); reporting payload-length chunks here would
        // wedge the credit queue on a frame that can never complete.
        if self.chunk_bytes == 0 {
            return 0;
        }
        self.payload.len().div_ceil(self.chunk_bytes)
    }

    /// Render the `seq`-th chunk frame (base64 slice + envelope), `None`
    /// past the end. Byte-identical to the frames the eager path built.
    pub(crate) fn render_chunk(&self, seq: usize) -> Option<String> {
        let offset = seq.checked_mul(self.chunk_bytes)?;
        if offset >= self.payload.len() || self.chunk_bytes == 0 {
            return None;
        }
        let end = offset
            .saturating_add(self.chunk_bytes)
            .min(self.payload.len());
        let data = base64::engine::general_purpose::STANDARD.encode(&self.payload[offset..end]);
        let frame = match self.envelope {
            ChunkEnvelope::Response => serde_json::json!({
                "t": "response_chunk",
                "id": self.request_id,
                "chunk_id": self.chunk_id,
                "seq": seq,
                "data": data,
            }),
            ChunkEnvelope::ByteStream => serde_json::json!({
                "t": "byte_stream_chunk",
                "id": self.request_id,
                "stream_id": self.chunk_id,
                "seq": seq,
                "data": data,
            }),
        };
        Some(frame.to_string())
    }

    fn resident_bytes(&self) -> usize {
        self.payload
            .len()
            .saturating_add(self.start.len())
            .saturating_add(self.end.len())
    }

    /// All frames in order (start, chunks, end) — test helpers and the
    /// no-credit immediate send path.
    pub(crate) fn render_all(&self) -> Vec<String> {
        let mut frames = Vec::with_capacity(self.chunk_count() + 2);
        frames.push(self.start.clone());
        for seq in 0..self.chunk_count() {
            if let Some(text) = self.render_chunk(seq) {
                frames.push(text);
            }
        }
        frames.push(self.end.clone());
        frames
    }
}

pub(crate) enum ControlFrameTexts {
    Immediate(Vec<String>),
    Chunked(ChunkedFramePlan),
}

impl OutboundControlQueue {
    fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            queued_bytes: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Queue an immediate frame; `false` when the per-connection byte or
    /// frame-count cap is exhausted (the caller answers with an error
    /// instead — immediate frames parked behind a zero-credit chunked
    /// head used to accumulate without bound).
    #[must_use]
    fn enqueue_immediate(&mut self, request_id: String, text: String) -> bool {
        if self.frames.len() >= CONTROL_OUTBOUND_QUEUE_MAX_FRAMES
            || self.queued_bytes.saturating_add(text.len()) > CONTROL_OUTBOUND_QUEUE_MAX_BYTES
        {
            return false;
        }
        let frame = QueuedControlFrame::Immediate { request_id, text };
        self.queued_bytes = self.queued_bytes.saturating_add(frame.queued_bytes());
        self.frames.push_back(frame);
        true
    }

    /// Queue a chunked frame; `false` when the per-connection byte cap is
    /// exhausted (the caller answers the request with an error instead —
    /// admission control, never a silent drop).
    #[must_use]
    fn enqueue_chunked(&mut self, plan: ChunkedFramePlan) -> bool {
        self.cancel_chunk(&plan.chunk_id.clone());
        let accounted_bytes = plan.resident_bytes();
        if self.frames.len() >= CONTROL_OUTBOUND_QUEUE_MAX_FRAMES
            || self.queued_bytes.saturating_add(accounted_bytes) > CONTROL_OUTBOUND_QUEUE_MAX_BYTES
        {
            return false;
        }
        self.queued_bytes = self.queued_bytes.saturating_add(accounted_bytes);
        self.frames
            .push_back(QueuedControlFrame::Chunked(QueuedChunkedFrame {
                plan,
                accounted_bytes,
                next_chunk: 0,
                credit: CONTROL_RESPONSE_INITIAL_CHUNK_CREDIT,
                started: false,
            }));
        true
    }

    /// Remove and return the front frame, keeping the byte accounting.
    fn pop_front(&mut self) -> Option<QueuedControlFrame> {
        let frame = self.frames.pop_front()?;
        self.queued_bytes = self.queued_bytes.saturating_sub(frame.queued_bytes());
        Some(frame)
    }

    fn grant_credit(&mut self, request_id: &str, chunk_id: Option<&str>, chunks: usize) {
        if chunks == 0 {
            return;
        }
        let granted = chunks.min(CONTROL_RESPONSE_MAX_CREDIT_GRANT);
        for frame in &mut self.frames {
            let QueuedControlFrame::Chunked(queued) = frame else {
                continue;
            };
            let matches_chunk = chunk_id
                .map(|id| queued.plan.chunk_id == id)
                .unwrap_or(false);
            if matches_chunk || (chunk_id.is_none() && queued.plan.request_id == request_id) {
                queued.credit = queued.credit.saturating_add(granted);
            }
        }
    }

    fn cancel(&mut self, request_id: &str) -> bool {
        self.retain_accounted(|frame| match frame {
            QueuedControlFrame::Immediate {
                request_id: queued_id,
                ..
            } => queued_id != request_id,
            QueuedControlFrame::Chunked(queued) => {
                queued.plan.request_id != request_id && queued.plan.chunk_id != request_id
            }
        })
    }

    fn cancel_chunk(&mut self, chunk_id: &str) -> bool {
        self.retain_accounted(|frame| match frame {
            QueuedControlFrame::Immediate { .. } => true,
            QueuedControlFrame::Chunked(queued) => queued.plan.chunk_id != chunk_id,
        })
    }

    /// `retain` that debits removed frames from the byte accounting;
    /// returns whether anything was removed.
    fn retain_accounted(&mut self, keep: impl Fn(&QueuedControlFrame) -> bool) -> bool {
        let before = self.frames.len();
        let mut removed_bytes = 0usize;
        self.frames.retain(|frame| {
            if keep(frame) {
                true
            } else {
                removed_bytes = removed_bytes.saturating_add(frame.queued_bytes());
                false
            }
        });
        self.queued_bytes = self.queued_bytes.saturating_sub(removed_bytes);
        self.frames.len() != before
    }
}

#[cfg(test)]
mod fs_scope_grant_tests {
    use super::*;

    fn browser_grant_for_role(role_id: &str, fingerprint: &str) -> DashboardControlGrant {
        let mut state = crate::access::iam::LocalIamState::default();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session(
            "test",
            "dashboard-control",
        );
        crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some(fingerprint.to_string()),
                role_id: Some(role_id.to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let principal =
            crate::access::iam::principal_for_browser_mtls_cert(&state, fingerprint, "https")
                .unwrap();
        DashboardControlGrant::UserClient {
            principal,
            iam_state: std::sync::Arc::new(state),
            iam_cert_dir: None,
            authority_memo: Default::default(),
        }
    }

    #[test]
    fn scoped_human_and_files_read_cannot_enter_unfiltered_websocket_bootstrap() {
        for (role, fingerprint) in [("role:scoped-human", "AA:01"), ("role:files-read", "AA:02")] {
            let grant = browser_grant_for_role(role, fingerprint);
            assert!(grant.has_any_effective_operation());
            assert!(
                !grant.allows_unfiltered_websocket_stream(),
                "{role} must be rejected before /ws upgrade and bootstrap"
            );
        }

        assert!(
            browser_grant_for_role("role:observer", "AA:03").allows_unfiltered_websocket_stream(),
            "the complete observer read set is the conservative /ws floor"
        );
        assert!(DashboardControlGrant::TrustedLocal.allows_unfiltered_websocket_stream());
    }

    #[test]
    fn private_display_owner_authority_requires_local_or_current_root() {
        let local = DashboardControlGrant::TrustedLocal;
        let root = browser_grant_for_role("role:root", "AA:ROOT");
        let observer = browser_grant_for_role("role:observer", "AA:OBSERVER");
        assert!(local.has_owner_dashboard_authority());
        assert!(root.has_owner_dashboard_authority());
        assert!(
            !browser_grant_for_role("role:operator", "AA:OPERATOR").has_owner_dashboard_authority()
        );
        assert!(!observer.has_owner_dashboard_authority());

        let private_ready = r#"{"event":"display_ready","display_id":9,"agent_visible":false}"#;
        let public_ready = r#"{"event":"display_ready","display_id":8,"agent_visible":true}"#;
        let request = r#"{"event":"display_request_raised","id":1}"#;
        let approval = r#"{"event":"display_approval_pending","display_id":9,"backend":"wayland"}"#;
        assert!(root.allows_dashboard_event_line(private_ready));
        assert!(!observer.allows_dashboard_event_line(private_ready));
        assert!(observer.allows_dashboard_event_line(public_ready));
        assert!(!observer.allows_dashboard_event_line(request));
        assert!(!observer.allows_dashboard_event_line(approval));

        let replay = serde_json::json!({
            "t": "log_replay",
            "entries": [
                serde_json::from_str::<serde_json::Value>(private_ready).unwrap(),
                serde_json::from_str::<serde_json::Value>(public_ready).unwrap(),
                serde_json::from_str::<serde_json::Value>(request).unwrap(),
                serde_json::from_str::<serde_json::Value>(approval).unwrap(),
                {"event": "display_capture_lost", "display_id": 9, "reason": "closed"},
            ],
        });
        let mut owner_replay = replay.clone();
        root.filter_dashboard_replay_payload(&mut owner_replay);
        assert_eq!(owner_replay, replay);

        let mut replay = replay;
        observer.filter_dashboard_replay_payload(&mut replay);
        let events = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry.get("event").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(events, vec!["display_ready", "display_capture_lost"]);
    }

    #[test]
    fn live_iam_change_or_reload_failure_invalidates_opening_authority() {
        use crate::peer::access_policy::PeerOperation;

        assert!(LIVE_AUTHORITY_RECHECK_INTERVAL <= Duration::from_millis(500));
        let tmp = tempfile::TempDir::new().unwrap();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("test", "unit");
        let mut state = crate::access::iam::LocalIamState::default();
        let created = crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("AA:55".to_string()),
                role_id: Some("role:root".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        crate::access::iam::save_state(tmp.path(), &state).unwrap();
        let principal = crate::access::iam::principal_for_browser_mtls_cert(
            &state,
            "AA:55",
            "webrtc-datachannel",
        )
        .unwrap();
        let grant = DashboardControlGrant::UserClient {
            principal,
            iam_state: std::sync::Arc::new(state.clone()),
            iam_cert_dir: Some(tmp.path().to_path_buf()),
            authority_memo: Default::default(),
        };
        assert!(grant.opening_authority_is_current());
        assert!(grant.has_owner_dashboard_authority());
        assert!(grant.access_decision(PeerOperation::RuntimeControl).allowed);

        crate::access::iam::update_user_client_grant(
            &mut state,
            crate::access::iam::IamGrantUpdateRequest {
                grant_id: created.grant.id,
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        crate::access::iam::save_state(tmp.path(), &state).unwrap();
        assert!(!grant.opening_authority_is_current());
        assert!(!grant.has_owner_dashboard_authority());
        assert!(!grant.access_decision(PeerOperation::RuntimeControl).allowed);
        assert!(matches!(
            grant.terminal_actor(),
            crate::terminal::TerminalActor::Principal(_)
        ));

        std::fs::write(crate::access::iam::iam_state_path(tmp.path()), b"not-json").unwrap();
        assert!(!grant.opening_authority_is_current());
        let denied = grant.access_decision(PeerOperation::PresenceRead);
        assert!(!denied.allowed);
        assert!(denied.reason.contains("unavailable"), "{}", denied.reason);
    }

    /// Trust pin for the `OpeningAuthorityMemo` fast path: a memo hit
    /// (same state `Arc`, no IAM edit in between) must still re-check the
    /// grant's expiry instant on every call — a grant expiring between two
    /// input events is caught even though the state snapshot never changed.
    #[test]
    fn opening_authority_memo_hit_still_rechecks_expiry_instant() {
        let tmp = tempfile::TempDir::new().unwrap();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("test", "unit");
        let mut state = crate::access::iam::LocalIamState::default();
        // Wide enough that upsert + save + the two pre-expiry assertions
        // comfortably fit before the instant, even on a loaded CI box;
        // the post-expiry wait below is bounded by the same margin.
        let expires_at = crate::access::client_key::now_unix_ms() as u64 + 1_500;
        crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("AA:57".to_string()),
                role_id: Some("role:observer".to_string()),
                expires_at_unix_ms: Some(expires_at),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        crate::access::iam::save_state(tmp.path(), &state).unwrap();
        let principal = crate::access::iam::principal_for_browser_mtls_cert(
            &state,
            "AA:57",
            "webrtc-datachannel",
        )
        .unwrap();
        let memo = OpeningAuthorityMemo::default();
        let grant = DashboardControlGrant::UserClient {
            principal,
            iam_state: std::sync::Arc::new(state.clone()),
            iam_cert_dir: Some(tmp.path().to_path_buf()),
            authority_memo: memo.clone(),
        };
        // Full validation primes the memo; the repeat call is the hit path.
        assert!(grant.opening_authority_is_current());
        assert!(memo.is_primed());
        assert!(grant.opening_authority_is_current());
        // Cross the expiry instant WITHOUT touching iam.json: the snapshot
        // Arc is unchanged, so only the hit path's expiry re-check can (and
        // must) flip the verdict.
        while (crate::access::client_key::now_unix_ms() as u64) < expires_at {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(memo.is_primed());
        assert!(!grant.opening_authority_is_current());
    }

    /// Trust pin for the `OpeningAuthorityMemo` fast path: any IAM edit
    /// reaches the session as a NEW state `Arc` from the fingerprint cache,
    /// so a primed memo never carries a stale verdict across a revocation.
    #[test]
    fn opening_authority_memo_never_outlives_a_state_change() {
        let tmp = tempfile::TempDir::new().unwrap();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("test", "unit");
        let mut state = crate::access::iam::LocalIamState::default();
        let created = crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("AA:58".to_string()),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        crate::access::iam::save_state(tmp.path(), &state).unwrap();
        let principal = crate::access::iam::principal_for_browser_mtls_cert(
            &state,
            "AA:58",
            "webrtc-datachannel",
        )
        .unwrap();
        let memo = OpeningAuthorityMemo::default();
        let grant = DashboardControlGrant::UserClient {
            principal,
            iam_state: std::sync::Arc::new(state.clone()),
            iam_cert_dir: Some(tmp.path().to_path_buf()),
            authority_memo: memo.clone(),
        };
        // Prime the memo through the full validation, then exercise the
        // hit path once so the fast path is what the revocation must beat.
        assert!(grant.opening_authority_is_current());
        assert!(memo.is_primed());
        assert!(grant.opening_authority_is_current());

        crate::access::iam::update_user_client_grant(
            &mut state,
            crate::access::iam::IamGrantUpdateRequest {
                grant_id: created.grant.id,
                status: Some("revoked".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        crate::access::iam::save_state(tmp.path(), &state).unwrap();
        // The revoked state loads as a new Arc → pointer mismatch → full
        // revalidation → stale session.
        assert!(!grant.opening_authority_is_current());
        // And the memo must not have been re-primed by the failed check.
        assert!(!grant.opening_authority_is_current());
    }

    #[test]
    fn live_custom_role_downgrade_invalidates_opening_authority() {
        let tmp = tempfile::TempDir::new().unwrap();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session("test", "unit");
        let mut state = crate::access::iam::LocalIamState::default();
        let created = crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("AA:56".to_string()),
                role_id: Some("role:observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let mut custom_role = state
            .roles
            .iter()
            .find(|role| role.id == "role:observer")
            .unwrap()
            .clone();
        custom_role.id = "role:test-custom-observer".to_string();
        custom_role.label = "Test custom observer".to_string();
        custom_role.source = "local-test".to_string();
        state.roles.push(custom_role);
        crate::access::iam::update_user_client_grant(
            &mut state,
            crate::access::iam::IamGrantUpdateRequest {
                grant_id: created.grant.id,
                role_id: Some("role:test-custom-observer".to_string()),
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        crate::access::iam::save_state(tmp.path(), &state).unwrap();
        let principal = crate::access::iam::principal_for_browser_mtls_cert(
            &state,
            "AA:56",
            "webrtc-datachannel",
        )
        .unwrap();
        let grant = DashboardControlGrant::UserClient {
            principal,
            iam_state: std::sync::Arc::new(state.clone()),
            iam_cert_dir: Some(tmp.path().to_path_buf()),
            authority_memo: Default::default(),
        };
        assert!(grant.opening_authority_is_current());
        assert!(grant.allows_unfiltered_websocket_stream());

        let role = state
            .roles
            .iter_mut()
            .find(|role| role.id == "role:test-custom-observer")
            .unwrap();
        role.permissions
            .retain(|permission| permission != "presence.read");
        crate::access::iam::save_state(tmp.path(), &state).unwrap();
        assert!(!grant.opening_authority_is_current());
        assert!(!grant.allows_unfiltered_websocket_stream());
    }

    #[test]
    fn live_peer_revocation_invalidates_opening_authority() {
        use crate::peer::access_policy::PeerOperation;

        let tmp = tempfile::TempDir::new().unwrap();
        let fingerprint = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let record = crate::peer::access_policy::write_approved_identity(
            tmp.path(),
            fingerprint,
            "peer-a",
            "peer-operator",
            None,
            None,
        )
        .unwrap();
        let grant = DashboardControlGrant::Peer {
            fingerprint: record.fingerprint.clone(),
            label: record.label.clone(),
            profile: record.profile.clone(),
            filesystem: record.filesystem.clone(),
            identity_record: Some(record),
            iam_cert_dir: Some(tmp.path().to_path_buf()),
            attributed: None,
        };
        assert!(grant.opening_authority_is_current());
        assert!(grant.access_decision(PeerOperation::PresenceRead).allowed);

        crate::peer::access_policy::revoke_identity(tmp.path(), fingerprint).unwrap();
        assert!(!grant.opening_authority_is_current());
        let denied = grant.access_decision(PeerOperation::PresenceRead);
        assert!(!denied.allowed);
        assert!(denied.reason.contains("peer identity changed"));
    }

    #[test]
    fn followup_signaling_rejects_unknown_revoked_and_different_openers() {
        let opening = browser_grant_for_role("role:files-read", "AA:10");
        let (command_tx, _command_rx) = mpsc::channel(1);
        let peer = DashboardControlPeer {
            command_tx,
            shutdown: CancellationToken::new(),
            owner: opening.signaling_owner(),
        };
        assert!(peer.belongs_to(&opening));

        let different_principal = browser_grant_for_role("role:files-read", "BB:20");
        assert!(!peer.belongs_to(&different_principal));

        // A single human principal may carry several certificates. Matching
        // principal/grant ids are insufficient: the exact certificate that
        // opened the session remains part of the signaling binding.
        let mut different_certificate = opening.clone();
        if let DashboardControlGrant::UserClient { principal, .. } = &mut different_certificate {
            principal.authn_binding = Some("cc30".to_string());
        }
        assert!(!peer.belongs_to(&different_certificate));

        // Revocation keeps the binding recognizable for audit, but the HTTP
        // pre-gate denies it before ICE/close can reach the owner comparison.
        let mut revoked = opening.clone();
        if let DashboardControlGrant::UserClient { iam_state, .. } = &mut revoked {
            let state = std::sync::Arc::make_mut(iam_state);
            state.grants[0].status = "revoked".to_string();
            state.grants[0].revoked_at_unix_ms = Some(1);
        }
        assert!(peer.belongs_to(&revoked));
        assert!(!revoked.has_any_effective_operation());

        let unknown = DashboardControlGrant::UserClient {
            principal: crate::access::iam::AccessPrincipal::ungranted_browser_mtls(
                Some("DD:40"),
                "webrtc-datachannel",
            ),
            iam_state: Default::default(),
            iam_cert_dir: None,
            authority_memo: Default::default(),
        };
        assert!(!peer.belongs_to(&unknown));
        assert!(!unknown.has_any_effective_operation());

        let peer_a = DashboardControlGrant::Peer {
            fingerprint: "peer-a".to_string(),
            label: "Peer A".to_string(),
            profile: "peer-operator".to_string(),
            filesystem: Default::default(),
            identity_record: None,
            iam_cert_dir: None,
            attributed: None,
        };
        let peer_b = DashboardControlGrant::Peer {
            fingerprint: "peer-b".to_string(),
            label: "Peer B".to_string(),
            profile: "peer-operator".to_string(),
            filesystem: Default::default(),
            identity_record: None,
            iam_cert_dir: None,
            attributed: None,
        };
        let (command_tx, _command_rx) = mpsc::channel(1);
        let peer_session = DashboardControlPeer {
            command_tx,
            shutdown: CancellationToken::new(),
            owner: peer_a.signaling_owner(),
        };
        assert!(peer_session.belongs_to(&peer_a));
        assert!(!peer_session.belongs_to(&peer_b));
    }

    #[test]
    fn caller_selected_session_collision_never_replaces_existing_owner() {
        let peer_a = DashboardControlGrant::Peer {
            fingerprint: "peer-a".to_string(),
            label: "Peer A".to_string(),
            profile: "peer-operator".to_string(),
            filesystem: Default::default(),
            identity_record: None,
            iam_cert_dir: None,
            attributed: None,
        };
        let peer_b = DashboardControlGrant::Peer {
            fingerprint: "peer-b".to_string(),
            label: "Peer B".to_string(),
            profile: "peer-operator".to_string(),
            filesystem: Default::default(),
            identity_record: None,
            iam_cert_dir: None,
            attributed: None,
        };
        let mut peers = HashMap::new();
        let (tx_a, _rx_a) = mpsc::channel(1);
        assert!(insert_dashboard_control_peer_if_vacant(
            &mut peers,
            "chosen-id".to_string(),
            DashboardControlPeer {
                command_tx: tx_a,
                shutdown: CancellationToken::new(),
                owner: peer_a.signaling_owner(),
            },
        )
        .is_ok());
        let (tx_b, _rx_b) = mpsc::channel(1);
        let rejected = insert_dashboard_control_peer_if_vacant(
            &mut peers,
            "chosen-id".to_string(),
            DashboardControlPeer {
                command_tx: tx_b,
                shutdown: CancellationToken::new(),
                owner: peer_b.signaling_owner(),
            },
        )
        .unwrap_err();
        assert!(rejected.belongs_to(&peer_b));
        assert_eq!(peers.len(), 1);
        let existing = peers.get("chosen-id").unwrap();
        assert!(existing.belongs_to(&peer_a));
        assert!(!existing.belongs_to(&peer_b));
    }

    #[test]
    fn user_client_grant_resolves_fs_scope_and_owner_paths_stay_open() {
        // Platform-absolute fixture path: `/srv/shared` is not absolute on
        // Windows, so prefix a drive and flip separators there.
        let srv_shared = if cfg!(windows) {
            "C:\\srv\\shared"
        } else {
            "/srv/shared"
        };
        let mut state = crate::access::iam::LocalIamState::default();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session(
            "test",
            "dashboard-control",
        );
        let result = crate::access::iam::upsert_user_client_grant(
            &mut state,
            crate::access::iam::UserClientGrantUpsertRequest {
                kind: "browser_certificate".to_string(),
                fingerprint: Some("AA:11".to_string()),
                role_id: Some("role:files-read".to_string()),
                fs_read_roots: vec![srv_shared.to_string()],
                ..Default::default()
            },
            &actor,
        )
        .unwrap();
        let principal = crate::access::iam::AccessPrincipal {
            grant_id: Some(result.grant.id.clone()),
            ..crate::access::iam::AccessPrincipal::root_dashboard_session(
                "scoped",
                "dashboard-control",
            )
        };
        let scoped = DashboardControlGrant::UserClient {
            principal,
            iam_state: std::sync::Arc::new(state.clone()),
            iam_cert_dir: None,
            authority_memo: Default::default(),
        };
        let scope = scoped.filesystem().expect("scoped grant exposes fs scope");
        assert_eq!(scope.read_roots, vec![std::path::PathBuf::from(srv_shared)]);

        // Owner surfaces stay unrestricted.
        assert!(DashboardControlGrant::TrustedLocal.filesystem().is_none());
    }
}

fn runtime_allows_operation(
    runtime: &ControlRuntime,
    op: crate::peer::access_policy::PeerOperation,
) -> bool {
    runtime_operation_decision(runtime, op).allowed
}

fn runtime_operation_decision(
    runtime: &ControlRuntime,
    op: crate::peer::access_policy::PeerOperation,
) -> crate::access::iam::AccessDecision {
    runtime.grant.access_decision(op)
}

/// Map a typed tunnel frame to the `PeerOperation` it exercises — the
/// datachannel lookup into the shared `access_policy::FRAME_LANES`
/// declaration (`web_gateway::ws_frame_operation` reads the same table),
/// so the same IAM grant answers the same way whichever transport a client
/// speaks — parity by construction. `None` means the frame carries no
/// blanket authority of its own here; each table row's `note` says why.
pub(crate) fn dashboard_control_frame_operation(
    t: &str,
) -> Option<crate::peer::access_policy::PeerOperation> {
    crate::peer::access_policy::frame_operation(crate::peer::access_policy::FrameLane::Tunnel, t)
}

/// Paths a filesystem method touches, for scope checks and the audit trail.
/// Rename is the two-legged case: removing the source and creating the
/// destination are both writes, so both paths must clear the grant's scope.
fn dashboard_control_filesystem_paths(
    method: &str,
    params: Option<&serde_json::Value>,
) -> Vec<String> {
    let Some(params) = params else {
        return Vec::new();
    };
    if method == "api_fs_rename" {
        return ["from", "to"]
            .iter()
            .filter_map(|key| params.get(*key).and_then(|v| v.as_str()))
            .map(str::to_string)
            .collect();
    }
    optional_string_param(params, &["path", "source_path", "sourcePath", "source"])
        .into_iter()
        .collect()
}

/// On success carries an optional audit-path override — the transfer
/// re-check's resolved job path (or the create's kind-aware target),
/// which the method's params don't name — for
/// `audit_dashboard_control_filesystem`; `Ok(None)` means the audit
/// trail derives its path from the params as before.
fn authorize_dashboard_control_filesystem(
    runtime: &ControlRuntime,
    method: &str,
    op: crate::peer::access_policy::PeerOperation,
    params: Option<&serde_json::Value>,
) -> Result<Option<String>, String> {
    use crate::peer::access_policy::{FilesystemAccessKind, PeerOperation};
    let kind = match op {
        PeerOperation::FilesystemRead => FilesystemAccessKind::Read,
        PeerOperation::FilesystemWrite => FilesystemAccessKind::Write,
        _ => return Ok(None),
    };
    let Some(policy) = runtime.grant.filesystem() else {
        return Ok(None);
    };
    // The transfer family: create scope-checks the kind-aware path the
    // create will actually target; the job-addressed methods re-check
    // the resolved job's *real* filesystem path through the shared
    // transport-neutral helper (HTTP's rows call the same fn, so both
    // lanes decide and word identically); the list method is never
    // blanket-denied — its handler scope-filters the listing instead.
    match method {
        "api_transfer_jobs" => return Ok(None),
        "api_transfer_job_create" => {
            let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
            let target = crate::web_gateway::classify_transfer_create(&params)
                .ok()
                .and_then(|request| match request {
                    crate::web_gateway::TransferCreateRequest::Path(kind) => {
                        crate::web_gateway::transfer_create_target_path(&params, kind)
                    }
                    // Artifact-shaped creates resolve daemon-internal
                    // sources no user grant names (divergence #24):
                    // pathless, so scoped callers fail closed below.
                    crate::web_gateway::TransferCreateRequest::Artifact(_) => None,
                });
            let Some(target) = target else {
                return Err("filesystem request missing path".to_string());
            };
            let path = crate::web_gateway::expand_dashboard_fs_path(&target)?;
            crate::peer::access_policy::filesystem_access_allowed(&policy, kind, &path)?;
            return Ok(Some(target));
        }
        "api_transfer_upload_chunk"
        | "api_transfer_upload_commit"
        | "api_transfer_job_delete"
        | "api_transfer_download_read" => {
            let access = match method {
                "api_transfer_download_read" => crate::web_gateway::TransferJobAccess::ReadSource,
                "api_transfer_job_delete" => crate::web_gateway::TransferJobAccess::WriteJobPath,
                _ => crate::web_gateway::TransferJobAccess::WriteDestination,
            };
            let handle = params
                .map(crate::web_gateway::transfer_id_param)
                .unwrap_or_default();
            let store = transfer_store_scope(runtime);
            let check =
                crate::web_gateway::check_scoped_transfer_job(&store, &policy, &handle, access);
            if check.allowed {
                return Ok(check.path.map(|path| path.display().to_string()));
            }
            return Err(crate::web_gateway::TRANSFER_JOB_SCOPE_DENIED.to_string());
        }
        _ => {}
    }
    let raw_paths = dashboard_control_filesystem_paths(method, params);
    // Fail closed on missing params: a rename that names only one leg must
    // not slip past the scope check and let the handler report a plain 400.
    if raw_paths.is_empty() || (method == "api_fs_rename" && raw_paths.len() != 2) {
        return Err("filesystem request missing path".to_string());
    }
    for raw_path in &raw_paths {
        let path = crate::web_gateway::expand_dashboard_fs_path(raw_path)?;
        crate::peer::access_policy::filesystem_access_allowed(&policy, kind, &path)?;
    }
    Ok(None)
}

fn authorize_dashboard_control_method(
    runtime: &ControlRuntime,
    method: &str,
    params: Option<&serde_json::Value>,
) -> Result<(), String> {
    // Fail closed: a method must be declared — as a route row's tunnel
    // column or a residue `CONTROL_ONLY_METHODS` entry — to be callable at
    // all; a dispatch arm added without a declaration is denied here
    // instead of shipping ungated.
    let Some(spec) = control_method_spec(method) else {
        return Err(format!("unknown dashboard-control method: {method}"));
    };
    if !runtime.grant.hosted_dashboard_method_allowed(method) {
        return Err(format!(
            "dashboard-control method {method} is outside the hosted lease method wall"
        ));
    }
    let Some(op) = spec.op else {
        return Ok(());
    };
    let result = runtime_operation_decision(runtime, op)
        .ensure_allowed()
        .map_err(|reason| format!("dashboard-control method {method} is not allowed: {reason}"))
        .and_then(|()| authorize_dashboard_control_filesystem(runtime, method, op, params));
    audit_dashboard_control_filesystem(runtime, method, op, params, &result);
    result.map(|_| ())
}

/// Audit twin of the HTTP lane's `[peer-fs]` / `[grant-fs]` lines
/// (`web_gateway::audit_peer_filesystem_access`) for filesystem methods that
/// arrive over the dashboard-control tunnel, so both transports leave the
/// same trail: peer grants log allow and deny, other grants log denials.
/// A successful transfer re-check overrides the logged path with the
/// resolved job path (the params name only a job handle).
fn audit_dashboard_control_filesystem(
    runtime: &ControlRuntime,
    method: &str,
    op: crate::peer::access_policy::PeerOperation,
    params: Option<&serde_json::Value>,
    result: &Result<Option<String>, String>,
) {
    use crate::peer::access_policy::PeerOperation;
    if !matches!(
        op,
        PeerOperation::FilesystemRead | PeerOperation::FilesystemWrite
    ) {
        return;
    }
    let path = match result {
        Ok(Some(resolved)) => resolved.clone(),
        _ => dashboard_control_filesystem_paths(method, params).join(" -> "),
    };
    match &runtime.grant {
        DashboardControlGrant::Peer {
            fingerprint,
            label,
            profile,
            ..
        } => {
            let (allowed, detail) = match result {
                Ok(_) => (true, "allowed".to_string()),
                Err(e) => (false, e.clone()),
            };
            runtime.bus.send(AppEvent::PresenceLog {
                message: format!(
                    "[peer-fs] {} peer={} fingerprint={} profile={} op={:?} path={} detail={}",
                    if allowed { "allowed" } else { "denied" },
                    label,
                    fingerprint,
                    profile,
                    op,
                    path,
                    detail,
                ),
                level: Some(if allowed {
                    LogLevel::Info
                } else {
                    LogLevel::Warn
                }),
                turn: None,
            });
        }
        grant => {
            if let Err(e) = result {
                runtime.bus.send(AppEvent::PresenceLog {
                    message: format!(
                        "[grant-fs] denied principal={} op={:?} path={} detail={}",
                        grant.label(),
                        op,
                        path,
                        e,
                    ),
                    level: Some(LogLevel::Warn),
                    turn: None,
                });
            }
        }
    }
}

/// Upload frames are authorized by the method they deliver — the same
/// operation that method needs on the direct routes — not by a blanket
/// filesystem grant. Path scoping runs at `upload_end`, where the params
/// are final: `api_fs_write` re-authorizes through the full method gate,
/// and a transfer chunk's job-path re-check
/// (`web_gateway::check_scoped_transfer_job`) resolves the named job and
/// scope-checks its destination the same way.
fn authorize_dashboard_control_upload(
    runtime: &ControlRuntime,
    method: &str,
) -> Result<(), String> {
    // Fail closed twice over: the method must be declared upload-deliverable
    // (route-row tunnel column or residue `CONTROL_ONLY_METHODS` entry), and
    // upload methods are always operation-gated.
    let Some(op) = control_method_spec(method)
        .filter(|spec| spec.upload)
        .and_then(|spec| spec.op)
    else {
        return Err(format!("unknown upload method: {method}"));
    };
    if !runtime.grant.hosted_dashboard_method_allowed(method) {
        return Err(format!(
            "dashboard-control upload {method} is outside the hosted lease method wall"
        ));
    }
    runtime_operation_decision(runtime, op)
        .ensure_allowed()
        .map_err(|reason| format!("dashboard-control upload {method} is not allowed: {reason}"))
}

fn authorize_dashboard_control_frame(
    runtime: &ControlRuntime,
    frame_type: &str,
) -> Result<(), String> {
    if !runtime.grant.hosted_tunnel_frame_allowed(frame_type) {
        return Err(format!(
            "dashboard-control frame {frame_type} is outside the hosted lease frame wall"
        ));
    }
    let Some(op) = dashboard_control_frame_operation(frame_type) else {
        return Ok(());
    };
    runtime_operation_decision(runtime, op)
        .ensure_allowed()
        .map_err(|reason| format!("dashboard-control frame {frame_type} is not allowed: {reason}"))
}

fn json_body_response(id: String, body: String, label: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(result) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": true,
            "result": result,
        }),
        Err(_) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("{label} returned invalid JSON"),
        }),
    }
}

/// Splice a core-serialized JSON body into a complete, PRE-SERIALIZED
/// `{"t":"response","id":…,"ok":true,"result":<body>}` envelope by string
/// concatenation — the response lane's twin of the event lane's
/// `event_lane_frame` splice. The core memoizes its hottest bodies as
/// strings (routes_sessions' documented multi-MB list cache); parsing
/// such a body into a full `Value` tree and re-serializing the envelope
/// made every tunnel poll pay three complete JSON passes.
///
/// The caller must have validated `body` as JSON (see
/// `json_body_response_preserialized`); `id` is JSON-escaped here.
fn spliced_response_frame_text(id: &str, body: &str) -> String {
    let id_json = serde_json::to_string(id).unwrap_or_else(|_| "\"\"".to_string());
    let mut text = String::with_capacity(
        body.len() + id_json.len() + "{\"t\":\"response\",\"id\":,\"ok\":true,\"result\":}".len(),
    );
    text.push_str("{\"t\":\"response\",\"id\":");
    text.push_str(&id_json);
    text.push_str(",\"ok\":true,\"result\":");
    text.push_str(body);
    text.push('}');
    text
}

/// `json_body_response`, pre-serialized: the returned frame is
/// `Value::String(<complete envelope text>)` — the task lane's
/// pre-serialized carrier. Response objects never serialize as top-level
/// JSON strings, so the variant is unambiguous;
/// `send_control_task_response` sends the text verbatim (chunking on the
/// same thresholds, keyed by the task's own request id). Only legal on
/// the spawned task-response lane. The body is still validated — one
/// full parse into `IgnoredAny`, no tree allocation — so an invalid body
/// answers with the historical error frame.
fn json_body_response_preserialized(id: &str, body: String, label: &str) -> serde_json::Value {
    if serde_json::from_str::<serde::de::IgnoredAny>(&body).is_err() {
        return serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("{label} returned invalid JSON"),
        });
    }
    serde_json::Value::String(spliced_response_frame_text(id, &body))
}

fn http_body_response(id: String, status: u16, body: String, label: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(mut result) => {
            if let serde_json::Value::Object(map) = &mut result {
                map.insert("_httpStatus".to_string(), serde_json::json!(status));
                map.insert(
                    "_httpOk".to_string(),
                    serde_json::json!((200..300).contains(&status)),
                );
            }
            serde_json::json!({
                "t": "response",
                "id": id,
                "ok": true,
                "result": result,
            })
        }
        Err(_) => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": format!("{label} returned invalid JSON"),
        }),
    }
}

// One status-line parser across both lanes (the api core's (status,
// body) helper vocabulary).
pub(crate) use crate::web_gateway::status_line_code;

fn params_body_text(params: Option<&serde_json::Value>) -> String {
    // Serialize the borrow — cloning the params subtree first cost a
    // second deep copy of every request's params just to stringify it.
    match params {
        Some(params) => serde_json::to_string(params).unwrap_or_else(|_| "{}".to_string()),
        None => "{}".to_string(),
    }
}

fn missing_param_response(id: String, name: &str) -> serde_json::Value {
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": false,
        "error": format!("missing {name}"),
    })
}

fn peer_registry_unavailable_response(id: String) -> serde_json::Value {
    serde_json::json!({
        "t": "response",
        "id": id,
        "ok": false,
        "error": "peer registry unavailable",
    })
}

async fn api_sessions_response(
    id: String,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    // Transport edge: resolve the real home once; the parity fixtures
    // drive the `_from_home` variant with an injected temp home.
    api_sessions_response_from_home(id, params, &crate::platform::home_dir()).await
}

async fn api_sessions_response_from_home(
    id: String,
    params: Option<&serde_json::Value>,
    home: &std::path::Path,
) -> serde_json::Value {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let limit = control_session_limit(&params);
    let ids = control_session_ids(&params);
    let usage_view = params.get("view").and_then(|v| v.as_str()) == Some("usage");
    // Transport-owned param mapping onto the neutral core: the tunnel's
    // ids path historically never applied the limit truncation (and its
    // ids vocabulary cannot express HTTP's present-but-empty filter), so
    // an ids request passes no limit.
    let (ids_filter, limit) = if ids.is_empty() {
        (None, limit)
    } else {
        (Some(ids), None)
    };
    let home = home.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::sessions_list_api_response(&home, ids_filter, limit, usage_view)
    })
    .await;
    let response = match result {
        Ok(response) => response,
        Err(e) => {
            return serde_json::json!({
                "t": "response",
                "id": id,
                "ok": false,
                "error": format!("session list task failed: {e}"),
            });
        }
    };
    let crate::web_gateway::ApiResponse::Json { body, .. } = response else {
        return serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": "session list returned an unexpected byte response",
        });
    };
    // Historical result-shape guard: the list must be a JSON array —
    // enforced without materializing the multi-MB Value tree (the SPA
    // polls this with limit:'all' every 15s per tab, and the core already
    // serves the body from its serialized-string cache): a full validating
    // parse into `IgnoredAny` plus the leading-token check, then the body
    // splices verbatim into a pre-serialized envelope.
    let body = body.into_string();
    let is_array = body.trim_start().starts_with('[')
        && serde_json::from_str::<serde::de::IgnoredAny>(&body).is_ok();
    if !is_array {
        return serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": "session list returned invalid JSON",
        });
    }
    serde_json::Value::String(spliced_response_frame_text(&id, &body))
}

fn control_session_limit(params: &serde_json::Value) -> Option<usize> {
    match params.get("limit") {
        Some(serde_json::Value::String(value)) => {
            let value = value.trim();
            if value.eq_ignore_ascii_case("all") || value.eq_ignore_ascii_case("full") {
                None
            } else {
                Some(
                    value
                        .parse::<usize>()
                        .ok()
                        .filter(|limit| *limit > 0)
                        .unwrap_or(CONTROL_DEFAULT_SESSION_LIMIT),
                )
            }
        }
        Some(serde_json::Value::Number(value)) => Some(
            value
                .as_u64()
                .and_then(|limit| usize::try_from(limit).ok())
                .filter(|limit| *limit > 0)
                .unwrap_or(CONTROL_DEFAULT_SESSION_LIMIT),
        ),
        _ => Some(CONTROL_DEFAULT_SESSION_LIMIT),
    }
}

fn control_session_ids(params: &serde_json::Value) -> Vec<String> {
    match params.get("ids") {
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(|value| value.as_str())
            .flat_map(split_control_session_ids)
            .collect(),
        Some(serde_json::Value::String(value)) => split_control_session_ids(value).collect(),
        Some(value) => split_control_session_ids(&value.to_string()).collect(),
        None => Vec::new(),
    }
}

fn control_session_detail_limit(params: &serde_json::Value) -> Option<usize> {
    match params.get("limit") {
        Some(serde_json::Value::String(value)) => {
            let value = value.trim();
            if value.is_empty()
                || value.eq_ignore_ascii_case("all")
                || value.eq_ignore_ascii_case("full")
            {
                None
            } else {
                value.parse::<usize>().ok().filter(|limit| *limit > 0)
            }
        }
        Some(serde_json::Value::Number(value)) => value
            .as_u64()
            .and_then(|limit| usize::try_from(limit).ok())
            .filter(|limit| *limit > 0),
        _ => None,
    }
}

fn control_session_detail_before(params: &serde_json::Value) -> Option<usize> {
    for name in ["before", "page_before", "pageBefore"] {
        let Some(value) = params.get(name) else {
            continue;
        };
        if value.is_null() {
            return None;
        }
        if let Some(number) = value.as_u64() {
            return usize::try_from(number).ok();
        }
        if let Some(text) = value.as_str() {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            return text.parse::<usize>().ok();
        }
        return None;
    }
    None
}

fn control_project_filter(params: &serde_json::Value) -> Vec<String> {
    for name in ["projects", "project_filter", "projectFilter"] {
        let Some(value) = params.get(name) else {
            continue;
        };
        match value {
            serde_json::Value::Array(values) => {
                return values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .collect();
            }
            serde_json::Value::String(value) => {
                if let Ok(values) = serde_json::from_str::<Vec<String>>(value) {
                    return values
                        .into_iter()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                        .collect();
                }
                return value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .collect();
            }
            value if !value.is_null() => return vec![value.to_string()],
            _ => {}
        }
    }
    Vec::new()
}

fn control_capability_query(params: &serde_json::Value) -> String {
    let capabilities = match params.get("capabilities") {
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        Some(serde_json::Value::String(value)) => value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    capabilities
        .iter()
        .map(|cap| format!("capability={cap}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn managed_context_request_line(kind: &str, params: &serde_json::Value) -> Option<String> {
    let raw_query = string_param(params, &["query", "search"]);
    let query = if raw_query.trim().is_empty() {
        managed_context_query_from_params(params)
    } else {
        raw_query.trim().trim_start_matches('?').to_string()
    };
    if query.is_empty() {
        return None;
    }
    Some(format!("GET /api/managed-context/{kind}?{query} HTTP/1.1"))
}

fn managed_context_query_from_params(params: &serde_json::Value) -> String {
    let mut pairs = Vec::new();
    for name in [
        "session_id",
        "session",
        "backend_session_id",
        "intendant_session_id",
        "wrapper_session_id",
    ] {
        let value = string_param(params, &[name]);
        if !value.is_empty() {
            pairs.push(format!("{name}={}", percent_encode_query_value(&value)));
        }
    }
    pairs.join("&")
}

fn changes_request_line(params: Option<&serde_json::Value>) -> String {
    let params = params.cloned().unwrap_or_else(|| serde_json::json!({}));
    let path = string_param(&params, &["path", "file", "file_path", "filePath"]);
    let query = request_query_string_param(&params);
    let mut target = "/api/session/current/changes".to_string();
    if !path.trim().is_empty() {
        target.push('/');
        target.push_str(&percent_encode_path_value(path.trim()));
    }
    if !query.is_empty() {
        target.push('?');
        target.push_str(&query);
    }
    format!("GET {target} HTTP/1.1")
}

fn request_query_string_param(params: &serde_json::Value) -> String {
    string_param(params, &["query", "search"])
        .trim()
        .trim_start_matches('?')
        .chars()
        .take_while(|ch| !ch.is_whitespace() && *ch != '#')
        .collect()
}

fn percent_encode_path_value(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn percent_encode_query_value(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

async fn active_session_log_dir(runtime: &ControlRuntime) -> Result<Option<PathBuf>, String> {
    let session_log = {
        let session = runtime.shared_session.read().await;
        session.session_log.clone()
    };
    let Some(session_log) = session_log else {
        return Ok(None);
    };
    session_log
        .lock()
        .map(|log| Some(log.dir().to_path_buf()))
        .map_err(|_| "session log lock poisoned".to_string())
}

async fn active_history_handles(
    runtime: &ControlRuntime,
) -> (
    Option<crate::file_watcher::SharedFileWatcher>,
    Option<Arc<std::sync::Mutex<crate::presence::AgentStateSnapshot>>>,
) {
    let session = runtime.shared_session.read().await;
    let file_watcher = session.file_watcher.clone();
    let agent_state = session
        .query_ctx
        .as_ref()
        .map(|ctx| Arc::clone(&ctx.agent_state));
    (file_watcher, agent_state)
}

async fn active_changes_handles(runtime: &ControlRuntime) -> (Option<PathBuf>, Option<PathBuf>) {
    let session = runtime.shared_session.read().await;
    (
        session.snapshot_dir.clone(),
        session.project_root_for_changes.clone(),
    )
}

async fn active_upload_handles(
    runtime: &ControlRuntime,
) -> Result<(Option<PathBuf>, Option<PathBuf>), String> {
    let (project_root, session_log) = {
        let session = runtime.shared_session.read().await;
        (
            session.project_root_for_changes.clone(),
            session.session_log.clone(),
        )
    };
    let session_dir = match session_log {
        Some(log) => Some(
            log.lock()
                .map_err(|_| "session log lock poisoned".to_string())?
                .dir()
                .to_path_buf(),
        ),
        None => None,
    };
    Ok((project_root, session_dir))
}

async fn active_recording_registry(
    runtime: &ControlRuntime,
) -> Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>> {
    let session = runtime.shared_session.read().await;
    session.recording_registry.clone()
}

// The lenient alias-param readers moved to the neutral api core
// (`web_gateway::api_core`) with the S9 transfer conversion — the
// re-export keeps every dashboard_control reference compiling.
pub(crate) use crate::web_gateway::{optional_string_param, optional_u64_param, string_param};

fn split_control_session_ids(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
}

fn to_rtc_ice_servers(servers: &[crate::display::IceServer]) -> Vec<RTCIceServer> {
    servers
        .iter()
        .map(|server| RTCIceServer {
            urls: server.urls.clone(),
            username: server.username.clone().unwrap_or_default(),
            credential: server.credential.clone().unwrap_or_default(),
        })
        .collect()
}

fn new_control_ice_fragment() -> String {
    uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(12)
        .collect()
}

fn new_control_ice_password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn udp_host_candidate_init(addr: SocketAddr) -> Result<RTCIceCandidateInit, CallerError> {
    let candidate = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: addr.ip().to_string(),
            port: addr.port(),
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()
    .map_err(|e| CallerError::WebRtc(format!("build UDP host candidate: {e}")))?;
    RTCIceCandidate::from(&candidate)
        .to_json()
        .map_err(|e| CallerError::WebRtc(format!("serialize UDP host candidate: {e}")))
}

fn tcp_host_candidate_init(addr: SocketAddr) -> RTCIceCandidateInit {
    RTCIceCandidateInit {
        candidate: format!(
            "candidate:9001 1 tcp 1677721855 {} {} typ host tcptype passive generation 0",
            addr.ip(),
            addr.port()
        ),
        sdp_mid: Some(String::new()),
        sdp_mline_index: Some(0),
        username_fragment: None,
        url: None,
    }
}

fn sha256_b64u(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    b64u(&digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty/absent request ids are rejected at dispatch BEFORE any
    /// spawn: an id-less request cannot receive a correlated response,
    /// and an empty-id carrier would bypass the outbound queue and
    /// chunking (the last congestion bypass). The rejection is an
    /// error-class frame, so the budget seam bounds the spam.
    #[tokio::test]
    async fn empty_id_requests_are_rejected_before_spawning() {
        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();

        for frame in [
            r#"{"t":"request","method":"api_sessions"}"#,
            r#"{"t":"request","id":"","method":"api_sessions"}"#,
            r#"{"t":"request","id":"","method":"status"}"#,
        ] {
            let rejected =
                test_control_frame_response(frame, &mut rt, &tx, &mut pending, &mut outbound)
                    .expect("id-less requests answer inline");
            assert_eq!(rejected["ok"], false, "{rejected}");
            assert!(rejected["error"].as_str().unwrap().contains("non-empty id"));
            assert!(
                is_error_class_frame(&rejected),
                "the rejection must ride the budgeted error-class seam"
            );
        }
        assert_eq!(
            pending.live_work(),
            0,
            "nothing may spawn for id-less requests"
        );
        assert!(
            rx.try_recv().is_err(),
            "no task response lane traffic for id-less requests"
        );
    }

    /// Committing uploads hold their BYTE weight against the aggregate
    /// declared-bytes budget until the commit completes: with enough
    /// bytes mid-commit, a new upload_start is refused on bytes even
    /// though `inbound_uploads` itself is empty.
    #[tokio::test]
    async fn committing_upload_bytes_count_against_the_aggregate() {
        let mut rt = runtime();
        let (tx, _rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();

        // Two slow 90 MiB commits still in flight (left inbound_uploads,
        // commit not finished): 180 MiB held by the ledger.
        let weight = 90 * 1024 * 1024;
        let _slow_commits = [
            pending.upload_commit_slot(weight),
            pending.upload_commit_slot(weight),
        ];
        assert_eq!(pending.committing_upload_bytes(), 2 * weight);

        // A third 90 MiB upload_start busts the 256 MiB aggregate.
        let start = serde_json::json!({
            "t": "upload_start",
            "id": "u-agg",
            "method": "api_session_current_upload",
            "params": { "name": "big.bin", "mime": "application/octet-stream" },
            "total_bytes": weight,
            "chunks": 90 * 64,
        });
        let refused = test_control_frame_response(
            &start.to_string(),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .expect("over-budget upload_start must answer");
        assert_eq!(refused["result"]["_httpStatus"], 429, "{refused}");
        assert!(refused["result"]["error"]
            .as_str()
            .unwrap()
            .contains("declared-bytes budget"));

        // Commits finishing (slots dropping) release the budget.
        drop(_slow_commits);
        assert_eq!(pending.committing_upload_bytes(), 0);
        let admitted = test_control_frame_response(
            &start.to_string(),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(
            admitted.is_none(),
            "with the commits finished the same start admits: {admitted:?}"
        );
    }

    /// Immediate-frame spam is bounded by the queue's frame-count cap —
    /// the seam R4-3 routes congested pre-serialized frames through.
    #[test]
    fn immediate_frame_cap_bounds_queued_spam() {
        let mut queue = OutboundControlQueue::new();
        for index in 0..CONTROL_OUTBOUND_QUEUE_MAX_FRAMES {
            assert!(queue.enqueue_immediate(format!("r{index}"), "{\"ok\":true}".to_string()));
        }
        assert!(
            !queue.enqueue_immediate("over".to_string(), "{}".to_string()),
            "the frame-count cap must refuse the next enqueue"
        );
    }

    /// The direct-error budget: while congested, exactly
    /// [`DIRECT_ERROR_BUDGET_FRAMES`] error frames pass, further ones drop
    /// and are counted; an uncongested beat (the wire drained) refills.
    #[test]
    fn direct_error_budget_bounds_congested_error_frames() {
        let mut budget = DirectErrorBudget::new("test");
        for index in 0..DIRECT_ERROR_BUDGET_FRAMES {
            assert!(budget.allow(true), "frame {index} rides the budget");
        }
        assert!(!budget.allow(true), "budget exhausted while congested");
        assert!(!budget.allow(true));
        assert_eq!(budget.dropped(), 2, "drops are counted per connection");
        // A drained wire refills the budget.
        assert!(budget.allow(false));
        assert!(budget.allow(true));
        assert_eq!(budget.dropped(), 2);
    }

    /// The immediate-frame seam's ERROR classification: the plain error
    /// envelope and the injected-status error shape are budgeted; success
    /// shapes never are.
    #[test]
    fn error_class_frames_are_classified() {
        assert!(is_error_class_frame(&dashboard_control_error_response(
            "r1".into(),
            "denied"
        )));
        assert!(is_error_class_frame(&serde_json::json!({
            "t": "response",
            "id": "u1",
            "ok": true,
            "result": { "_httpStatus": 429, "_httpOk": false, "error": "too many" },
        })));
        assert!(!is_error_class_frame(&serde_json::json!({
            "t": "response",
            "id": "ok1",
            "ok": true,
            "result": { "_httpStatus": 200, "_httpOk": true },
        })));
        assert!(!is_error_class_frame(&serde_json::json!({
            "t": "hello_ack",
            "id": "h1",
        })));
    }

    /// Generation-keyed reservations: same-id replacement cancels its
    /// predecessor and mints a new generation; a superseded generation
    /// can neither claim ownership nor free the replacement's
    /// reservation.
    #[test]
    fn pending_request_generations_protect_replacements() {
        let mut pending = PendingControlRequests::new();
        let (first_cancel, first_generation, _first_slot) = pending.admit("r1");
        let (second_cancel, second_generation, _second_slot) = pending.admit("r1");
        assert!(
            first_cancel.is_cancelled(),
            "replacement cancels the predecessor"
        );
        assert!(!second_cancel.is_cancelled());
        assert_ne!(first_generation, second_generation);
        assert!(!pending.matches("r1", first_generation));
        assert!(pending.matches("r1", second_generation));
        assert!(
            !pending.complete("r1", first_generation),
            "a superseded completion must not free the replacement"
        );
        assert!(pending.contains_key("r1"));
        assert!(pending.complete("r1", second_generation));
        assert!(!pending.contains_key("r1"));
    }

    /// The admission bound counts LIVE WORK, not addressable entries:
    /// rapid same-id cycling accumulates one slot per still-draining
    /// predecessor (held via RAII until that task actually exits), so a
    /// spammer saturates the bound and gets refused instead of stacking
    /// untracked work onto the blocking pool — and capacity frees only
    /// when a predecessor's own slot drops.
    #[test]
    fn live_work_slots_bound_same_id_cycling() {
        let mut pending = PendingControlRequests::new();
        let mut draining = Vec::new();
        for _ in 0..MAX_PENDING_CONTROL_REQUESTS {
            let (_cancel, _generation, slot) = pending.admit("spam");
            draining.push(slot);
        }
        assert_eq!(pending.len(), 1, "one addressable entry");
        assert_eq!(
            pending.live_work(),
            MAX_PENDING_CONTROL_REQUESTS,
            "every draining predecessor still holds its slot"
        );
        assert!(
            pending.at_capacity(),
            "cycling one id saturates the live-work bound"
        );
        // Only a predecessor's own exit frees capacity.
        draining.pop();
        assert!(!pending.at_capacity());
        // Entry removal (cancel frame) does not free the drained work.
        assert!(pending.cancel_remove("spam"));
        assert_eq!(pending.live_work(), MAX_PENDING_CONTROL_REQUESTS - 1);
        drop(draining);
        assert_eq!(pending.live_work(), 0);

        // The committing-upload ledger is RAII the same way, bytes
        // included.
        let commit_slot = pending.upload_commit_slot(1024);
        assert_eq!(pending.committing_uploads(), 1);
        assert_eq!(pending.committing_upload_bytes(), 1024);
        drop(commit_slot);
        assert_eq!(pending.committing_uploads(), 0);
        assert_eq!(pending.committing_upload_bytes(), 0);
    }

    /// The pre-serialized response carrier: byte-verbatim body embedding,
    /// JSON-equivalence with the parse-path envelope, escaped ids, and
    /// the historical error frame for invalid bodies.
    #[test]
    fn preserialized_response_envelope_matches_the_parsed_shape() {
        let body = r#"{"a":1,"nested":{"b":[1,2,3]}}"#;
        let frame = json_body_response_preserialized("req-1", body.to_string(), "test");
        let serde_json::Value::String(text) = &frame else {
            panic!("expected the pre-serialized carrier, got {frame}");
        };
        assert!(
            text.contains(body),
            "the body must embed verbatim (proof of no reserialization): {text}"
        );
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["t"], "response");
        assert_eq!(parsed["id"], "req-1");
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["result"]["nested"]["b"][1], 2);
        // JSON-equivalent to the parse-path envelope.
        let legacy = json_body_response("req-1".into(), body.to_string(), "test");
        assert_eq!(parsed, legacy);

        // Invalid bodies keep the historical error frame (an object).
        let error = json_body_response_preserialized("req-2", "not json".into(), "test");
        assert_eq!(error["ok"], false);
        assert!(error["error"].as_str().unwrap().contains("invalid JSON"));

        // Ids embed JSON-escaped.
        let quoted = json_body_response_preserialized("has\"quote", "{}".to_string(), "test");
        let serde_json::Value::String(text) = quoted else {
            panic!("expected the pre-serialized carrier");
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["id"], "has\"quote");

        // Non-ASCII and control characters survive both positions: the
        // body embeds verbatim (the core already escaped it), the id is
        // escaped here.
        let unicode_body = serde_json::json!({
            "name": "résumé — 日本語 🚀",
            "ctrl": "tab\tnewline\nbell\u{7}",
        })
        .to_string();
        let frame = json_body_response_preserialized("id-é\u{1}", unicode_body.clone(), "test");
        let serde_json::Value::String(text) = &frame else {
            panic!("expected the pre-serialized carrier, got {frame}");
        };
        assert!(text.contains(&unicode_body), "body embeds verbatim");
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["id"], "id-é\u{1}");
        assert_eq!(parsed["result"]["name"], "résumé — 日本語 🚀");
        assert_eq!(parsed["result"]["ctrl"], "tab\tnewline\nbell\u{7}");
        assert_eq!(
            parsed,
            json_body_response("id-é\u{1}".into(), unicode_body, "test"),
            "JSON-equivalent to the parse-path envelope"
        );
    }

    pub(crate) fn test_upload_state(
        method: &str,
        params: serde_json::Value,
        bytes: &[u8],
    ) -> InboundUploadState {
        let mut spool = UploadSpool::for_declared_size(bytes.len()).unwrap();
        spool.append(bytes).unwrap();
        spool.finish().unwrap();
        InboundUploadState {
            method: method.to_string(),
            params,
            spool,
            total_bytes: bytes.len(),
            expected_chunks: if bytes.is_empty() { 0 } else { 1 },
            next_seq: if bytes.is_empty() { 0 } else { 1 },
            received_bytes: bytes.len(),
            generation: 0,
            slot: None,
        }
    }

    /// Both spool variants round-trip bytes identically through both
    /// consumption shapes (take_bytes for the media handlers, the
    /// SpooledBody tempfile for the commit handlers), and the memory
    /// threshold routes small payloads off disk.
    #[test]
    fn upload_spool_round_trips_both_variants() {
        for (len, expect_memory) in [
            (16usize, true),
            (UPLOAD_MEMORY_SPOOL_MAX_BYTES, true),
            (UPLOAD_MEMORY_SPOOL_MAX_BYTES + 1, false),
        ] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();

            let mut spool = UploadSpool::for_declared_size(len).unwrap();
            assert_eq!(
                matches!(spool, UploadSpool::Memory(_)),
                expect_memory,
                "threshold routing for {len} bytes"
            );
            // Chunked appends like the wire delivers them.
            for chunk in payload.chunks(7 * 1024) {
                spool.append(chunk).unwrap();
            }
            assert_eq!(spool.take_bytes(len).unwrap(), payload);

            let mut spool = UploadSpool::for_declared_size(len).unwrap();
            for chunk in payload.chunks(7 * 1024) {
                spool.append(chunk).unwrap();
            }
            let mut tmp = spool.into_spooled_tempfile().unwrap();
            let mut on_disk = Vec::new();
            tmp.as_file_mut().seek(std::io::SeekFrom::Start(0)).unwrap();
            tmp.as_file_mut().read_to_end(&mut on_disk).unwrap();
            assert_eq!(on_disk, payload);

            // A short payload is caught by the byte-count guard.
            let mut spool = UploadSpool::for_declared_size(len).unwrap();
            spool.append(&payload[..len - 1]).unwrap();
            assert!(spool.take_bytes(len).is_err());
        }
    }

    pub(crate) fn runtime() -> ControlRuntime {
        ControlRuntime {
            session_id: "session-1".into(),
            daemon_public_key: "pubkey".into(),
            created_unix_ms: 123,
            events_subscribed: false,
            events_sent: 0,
            response_credit_enabled: false,
            config: Arc::new(serde_json::json!({"provider":"openai"})),
            agent_card: Arc::new(serde_json::json!({
                "id": "intendant:test-daemon",
                "label": "test-daemon",
            })),
            bus: crate::event::EventBus::new(),
            peer_registry: None,
            mcp_server: None,
            shared_session: crate::web_gateway::ActiveSessionState::empty(),
            project_root: None,
            worktree_inventory_cache: Arc::new(std::sync::Mutex::new(None)),
            terminal_registry: Arc::new(crate::terminal::TerminalRegistry::new(
                std::env::temp_dir(),
            )),
            task_tx: None,
            bootstrap_caches: DashboardBootstrapCaches::default(),
            display_authority: None,
            presence: None,
            ice_config: crate::display::IceConfig::default(),
            tcp_peer_registry: crate::display::webrtc::TcpPeerRegistry::new(),
            tcp_advertised: None,
            media_clip_ops: Arc::new(Mutex::new(HashMap::new())),
            control_frames_tx: None,
            display_peer_id: crate::display_peer_ids::allocate_dashboard_control_display_peer_id()
                .expect("test dashboard-control peer id"),
            display_peer_sessions: Arc::new(Mutex::new(Vec::new())),
            grant: DashboardControlGrant::TrustedLocal,
            shutdown: CancellationToken::new(),
            tabs: crate::web_gateway::DashboardTabsRegistry::new(
                Arc::new(std::sync::Mutex::new(None)),
                Arc::new(crate::web_gateway::DisplayInputAuthority::default()),
            ),
            // Per-instance scratch (never the machine's real ~/.intendant):
            // projectless adapters resolve the daemon-global store under
            // this root. PID+nanos, per the state_paths uniqueness rule.
            state_root: std::env::temp_dir().join(format!(
                "intendant-test-state-root-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            )),
        }
    }

    pub(crate) fn scoped_user_client_grant() -> DashboardControlGrant {
        let mut iam_state = crate::access::iam::LocalIamState::default();
        iam_state.principals.push(crate::access::iam::IamPrincipal {
            id: "principal:browser-cert:ab123".to_string(),
            kind: "browser_certificate".to_string(),
            label: "Alice browser".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            account: None,
            organization: None,
            authn: vec![serde_json::json!({
                "kind": "browser_mtls_cert",
                "fingerprint": "ab123"
            })],
            notes: None,
            created_at_unix_ms: Some(100),
        });
        iam_state.grants.push(crate::access::iam::IamGrant {
            id: "grant:browser-cert:ab123:inspect".to_string(),
            principal_id: "principal:browser-cert:ab123".to_string(),
            target_id: "local".to_string(),
            role_id: "role:scoped-human".to_string(),
            policy_id: "policy:local-user-client".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            reason: "test grant".to_string(),
            created_at_unix_ms: Some(101),
            revoked_at_unix_ms: None,
            expires_at_unix_ms: None,
            issued_via: None,
            fs_scope: None,
        });
        let principal =
            crate::access::iam::principal_for_browser_mtls_cert(&iam_state, "ab123", "https")
                .unwrap();
        DashboardControlGrant::UserClient {
            principal,
            iam_state: std::sync::Arc::new(iam_state),
            iam_cert_dir: None,
            authority_memo: Default::default(),
        }
    }

    fn test_control_frame_response(
        text: &str,
        runtime: &mut ControlRuntime,
        task_tx: &mpsc::Sender<SequencedTaskResponse>,
        pending_requests: &mut PendingControlRequests,
        outbound_queue: &mut OutboundControlQueue,
    ) -> Option<serde_json::Value> {
        let mut inbound_uploads = HashMap::new();
        let (terminal_tx, _terminal_rx) = mpsc::unbounded_channel();
        let (terminal_output_tx, _terminal_output_rx) = mpsc::channel(TERMINAL_OUTPUT_LANE_CAP);
        let mut terminal_forwarders = HashMap::new();
        let display_input_tx = DisplayInputForwarder::test_sink();
        control_frame_response(
            text,
            runtime,
            task_tx,
            pending_requests,
            outbound_queue,
            &mut inbound_uploads,
            &terminal_tx,
            &terminal_output_tx,
            &mut terminal_forwarders,
            &display_input_tx,
        )
    }

    #[test]
    fn binding_signature_payload_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let identity = DaemonIdentity::load_or_create(dir.path().join("identity.pk8")).unwrap();
        let binding = DashboardControlBinding::new(
            &identity,
            "session-1".into(),
            "offer",
            "answer",
            None,
            None,
        );
        assert!(crate::daemon_identity::verify_b64u(
            &binding.daemon_public_key,
            binding.signing_payload().as_bytes(),
            &binding.signature,
        ));
        assert_eq!(binding.protocol, CONTROL_SIGNATURE_CONTEXT);
        assert_eq!(binding.offer_sha256, sha256_b64u(b"offer"));
        assert_eq!(binding.answer_sha256, sha256_b64u(b"answer"));
        assert!(binding.expires_unix_ms > binding.created_unix_ms);
        assert_eq!(
            binding.expires_unix_ms - binding.created_unix_ms,
            CONTROL_BINDING_TTL_MS
        );
        assert_eq!(binding.client_nonce, None);
        assert_eq!(binding.session_grant_sha256, None);

        let granted = DashboardControlBinding::new(
            &identity,
            "session-2".into(),
            "offer-2",
            "answer-2",
            Some("connect-session-grant"),
            Some("browser-client-nonce"),
        );
        let expected_grant_hash = sha256_b64u(b"connect-session-grant");
        assert_eq!(
            granted.client_nonce.as_deref(),
            Some("browser-client-nonce")
        );
        assert_eq!(
            granted.session_grant_sha256.as_deref(),
            Some(expected_grant_hash.as_str())
        );
        assert!(granted.signing_payload().ends_with(
            granted
                .session_grant_sha256
                .as_deref()
                .expect("grant hash should be present")
        ));
        assert!(crate::daemon_identity::verify_b64u(
            &granted.daemon_public_key,
            granted.signing_payload().as_bytes(),
            &granted.signature,
        ));
    }

    #[test]
    fn peer_dashboard_grants_split_access_and_peer_permissions() {
        let (tx, _rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();
        let mut peer_root = runtime();
        peer_root.grant = DashboardControlGrant::Peer {
            fingerprint: "fingerprint".into(),
            label: "peer-root".into(),
            profile: "peer-root".into(),
            filesystem: crate::peer::access_policy::FilesystemAccessPolicy::default(),
            attributed: None,
            identity_record: None,
            iam_cert_dir: None,
        };

        let status = test_control_frame_response(
            r#"{"t":"request","id":"s1","method":"status"}"#,
            &mut peer_root,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(status["ok"], true);
        assert_eq!(status["result"]["access_inspect_available"], true);
        assert_eq!(status["result"]["access_manage_available"], false);
        assert_eq!(status["result"]["peer_inspect_available"], true);
        assert_eq!(status["result"]["peer_manage_available"], true);
        assert_eq!(status["result"]["api_access_overview_available"], true);
        assert_eq!(status["result"]["api_access_iam_state_available"], true);
        assert_eq!(status["result"]["api_dashboard_targets_available"], true);
        assert_eq!(status["result"]["access_principal"]["kind"], "peer_daemon");
        assert_eq!(
            status["result"]["access_principal"]["peer_profile"],
            "peer-root"
        );
        assert_eq!(
            status["result"]["iam_enforcement"]["operation_evaluator"],
            true
        );
        assert_eq!(
            status["result"]["iam_enforcement"]["principal_binding"],
            "peer_daemon"
        );
        assert_eq!(status["result"]["api_peer_pairing_invite_available"], false);
        assert_eq!(status["result"]["api_peer_pairing_join_available"], true);
        assert_eq!(
            status["result"]["api_peer_pairing_request_decision_available"],
            false
        );
        assert_eq!(
            status["result"]["api_peer_pairing_identity_revoke_available"],
            false
        );

        let overview = test_control_frame_response(
            r#"{"t":"request","id":"a1","method":"api_access_overview"}"#,
            &mut peer_root,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(overview["ok"], true);

        let iam_state = test_control_frame_response(
            r#"{"t":"request","id":"iam1","method":"api_access_iam_state"}"#,
            &mut peer_root,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(iam_state["ok"], true);
        assert_eq!(
            iam_state["result"]["iam"]["capabilities"]["enforce_user_client_grants"],
            true
        );

        let revoke = test_control_frame_response(
            r#"{"t":"request","id":"r1","method":"api_peer_pairing_identity_revoke","params":{"identity":"peer-a"}}"#,
            &mut peer_root,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(revoke["ok"], false);
        assert!(revoke["error"]
            .as_str()
            .unwrap_or("")
            .contains("peer profile peer-root does not allow access.manage"));

        let invite = test_control_frame_response(
            r#"{"t":"request","id":"i1","method":"api_peer_pairing_invite","params":{}}"#,
            &mut peer_root,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(invite["ok"], false);
        assert!(invite["error"]
            .as_str()
            .unwrap_or("")
            .contains("peer profile peer-root does not allow access.manage"));

        let mut peer_operator = runtime();
        peer_operator.grant = DashboardControlGrant::Peer {
            fingerprint: "fingerprint".into(),
            label: "peer-operator".into(),
            profile: "peer-operator".into(),
            filesystem: crate::peer::access_policy::FilesystemAccessPolicy::default(),
            attributed: None,
            identity_record: None,
            iam_cert_dir: None,
        };
        let denied = test_control_frame_response(
            r#"{"t":"request","id":"a2","method":"api_access_overview"}"#,
            &mut peer_operator,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(denied["ok"], false);
        assert!(denied["error"]
            .as_str()
            .unwrap_or("")
            .contains("peer profile peer-operator does not allow access.inspect"));
    }

    #[tokio::test]
    async fn control_frames_answer_hello_ping_and_config() {
        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();
        let hello = test_control_frame_response(
            r#"{"t":"hello","id":"h1"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(hello["t"], "hello_ack");
        assert_eq!(hello["session_id"], "session-1");

        let ping = test_control_frame_response(
            r#"{"t":"ping","id":"p1"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(ping["t"], "pong");
        assert_eq!(ping["id"], "p1");

        let config = test_control_frame_response(
            r#"{"t":"request","id":"r1","method":"config"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(config["t"], "response");
        assert_eq!(config["ok"], true);
        assert_eq!(config["result"]["provider"], "openai");

        let card = test_control_frame_response(
            r#"{"t":"request","id":"c1","method":"api_agent_card"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(card["t"], "response");
        assert_eq!(card["ok"], true);
        assert_eq!(card["result"]["id"], "intendant:test-daemon");
        assert_eq!(card["result"]["label"], "test-daemon");

        {
            let mut guard = rt.bootstrap_caches.last_status_json.lock().unwrap();
            *guard = Some(r#"{"event":"status","session_id":"s-1"}"#.to_string());
        }
        {
            let mut guard = rt.bootstrap_caches.last_autonomy_json.lock().unwrap();
            *guard = Some(r#"{"event":"autonomy_changed","mode":"ask"}"#.to_string());
        }
        {
            // Per-session change-detected state joins the bootstrap so a
            // late-joining tunnel client sees vitals/goals that last
            // changed before it connected.
            let mut guard = rt.bootstrap_caches.session_state_lines.lock().unwrap();
            guard.entry("s-1".to_string()).or_default().insert(
                "session_vitals",
                r#"{"event":"session_vitals","session_id":"s-1","vitals":{"git":{"branch":"main","dirtyFiles":1,"ahead":0,"behind":0}}}"#.to_string(),
            );
        }
        let cached_bootstrap = test_control_frame_response(
            r#"{"t":"request","id":"b1","method":"api_cached_bootstrap_events"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(cached_bootstrap["t"], "response");
        assert_eq!(cached_bootstrap["ok"], true);
        assert_eq!(cached_bootstrap["result"]["event_count"], 3);
        assert_eq!(cached_bootstrap["result"]["events"][0]["event"], "status");
        assert_eq!(
            cached_bootstrap["result"]["events"][1]["event"],
            "autonomy_changed"
        );
        assert_eq!(
            cached_bootstrap["result"]["events"][2]["event"],
            "session_vitals"
        );
        assert_eq!(
            cached_bootstrap["result"]["events"][2]["vitals"]["git"]["dirtyFiles"],
            1
        );

        let status = test_control_frame_response(
            r#"{"t":"request","id":"s1","method":"status"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(status["t"], "response");
        assert_eq!(status["ok"], true);
        assert_eq!(status["result"]["session_id"], "session-1");
        assert_eq!(status["result"]["created_unix_ms"], 123);
        assert_eq!(status["result"]["transport"], "webrtc-datachannel");
        assert_eq!(status["result"]["events_subscribed"], false);
        assert_eq!(status["result"]["response_credit_enabled"], false);
        assert_eq!(status["result"]["access_principal"]["kind"], "root_session");
        assert_eq!(status["result"]["access_principal"]["role_id"], "role:root");
        assert_eq!(
            status["result"]["iam_enforcement"]["operation_evaluator"],
            true
        );
        assert_eq!(
            status["result"]["iam_enforcement"]["principal_binding"],
            "root_session"
        );
        assert_eq!(status["result"]["api_peers_available"], false);
        assert_eq!(status["result"]["api_agent_card_available"], true);
        assert_eq!(
            status["result"]["api_cached_bootstrap_events_available"],
            true
        );
        assert_eq!(
            status["result"]["api_browser_workspace_snapshot_available"],
            true
        );
        assert_eq!(status["result"]["api_state_snapshot_available"], true);
        assert_eq!(status["result"]["api_display_bootstrap_available"], true);
        assert_eq!(
            status["result"]["api_display_webrtc_signal_available"],
            true
        );
        assert_eq!(status["result"]["api_session_log_replay_available"], true);
        assert_eq!(
            status["result"]["api_external_session_activity_replay_available"],
            true
        );
        assert_eq!(status["result"]["api_dashboard_bootstrap_available"], true);
        assert_eq!(
            status["result"]["api_access_iam_update_grant_available"],
            true
        );
        assert_eq!(status["result"]["byte_streams_available"], true);
        assert_eq!(status["result"]["upload_frames_available"], true);
        assert_eq!(status["result"]["presence_frames_available"], true);
        assert_eq!(status["result"]["presence_active_handoff_available"], false);
        assert_eq!(status["result"]["presence_tool_request_available"], true);
        assert_eq!(status["result"]["access_inspect_available"], true);
        assert_eq!(status["result"]["access_manage_available"], true);
        assert_eq!(
            status["result"]["api_access_iam_upsert_user_client_grant_available"],
            true
        );
        assert_eq!(status["result"]["peer_inspect_available"], true);
        assert_eq!(status["result"]["peer_manage_available"], true);
        assert_eq!(status["result"]["api_presence_video_frame_available"], true);
        assert_eq!(status["result"]["api_sessions_available"], true);
        assert_eq!(status["result"]["api_sessions_stream_available"], true);
        assert_eq!(status["result"]["api_session_detail_available"], true);
        assert_eq!(status["result"]["api_session_report_available"], true);
        assert_eq!(status["result"]["api_session_delete_available"], true);
        assert_eq!(status["result"]["api_session_agent_output_available"], true);
        assert_eq!(
            status["result"]["api_session_current_agent_output_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_history_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_rollback_available"],
            true
        );
        assert_eq!(status["result"]["api_session_current_redo_available"], true);
        assert_eq!(
            status["result"]["api_session_current_prune_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_changes_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_context_snapshot_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_uploads_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_upload_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_upload_raw_available"],
            true
        );
        assert_eq!(
            status["result"]["api_session_current_upload_delete_available"],
            true
        );
        // Projectless runtime: transfers stay available — the store resolves
        // through the daemon-global StoreScope fallback, matching the S9
        // HTTP rows (the old project_root gate made the tunnel lane lie).
        assert_eq!(status["result"]["api_transfer_jobs_available"], true);
        assert_eq!(status["result"]["api_transfer_job_create_available"], true);
        assert_eq!(status["result"]["api_transfer_job_delete_available"], true);
        assert_eq!(
            status["result"]["api_transfer_download_read_available"],
            true
        );
        assert_eq!(
            status["result"]["api_transfer_upload_chunk_available"],
            true
        );
        assert_eq!(
            status["result"]["api_transfer_upload_commit_available"],
            true
        );
        assert_eq!(status["result"]["api_fs_stat_available"], true);
        assert_eq!(status["result"]["api_fs_list_available"], true);
        assert_eq!(status["result"]["api_fs_mkdir_available"], true);
        assert_eq!(status["result"]["api_fs_read_available"], true);
        assert_eq!(status["result"]["api_fs_write_available"], true);
        assert_eq!(status["result"]["api_fs_rename_available"], true);
        assert_eq!(status["result"]["api_fs_delete_available"], true);
        assert_eq!(status["result"]["api_sessions_search_available"], true);
        assert_eq!(status["result"]["api_settings_available"], true);
        assert_eq!(status["result"]["api_settings_save_available"], true);
        assert_eq!(status["result"]["api_control_msg_available"], true);
        assert_eq!(status["result"]["api_session_control_msg_available"], true);
        assert_eq!(status["result"]["api_dashboard_action_msg_available"], true);
        assert_eq!(
            status["result"]["api_diagnostics_visual_freshness_available"],
            true
        );
        assert_eq!(status["result"]["api_key_status_available"], true);
        assert_eq!(status["result"]["api_api_keys_save_available"], true);
        assert_eq!(status["result"]["api_voice_session_available"], true);
        assert_eq!(status["result"]["api_project_root_available"], true);
        assert_eq!(status["result"]["api_displays_available"], true);
        assert_eq!(status["result"]["api_recordings_available"], true);
        assert_eq!(status["result"]["api_recording_asset_available"], true);
        assert_eq!(status["result"]["api_session_recordings_available"], true);
        assert_eq!(
            status["result"]["api_session_recording_asset_available"],
            true
        );
        assert_eq!(status["result"]["api_worktrees_available"], true);
        assert_eq!(status["result"]["api_worktrees_scan_available"], true);
        assert_eq!(status["result"]["api_worktrees_remove_available"], true);
        assert_eq!(status["result"]["api_mcp_tool_call_available"], false);
        assert_eq!(status["result"]["api_peer_mutations_available"], false);
        assert_eq!(status["result"]["api_peer_webrtc_signal_available"], false);
        assert_eq!(status["result"]["api_peer_pairing_available"], true);
        assert_eq!(status["result"]["api_peer_pairing_invite_available"], true);
        assert_eq!(status["result"]["api_peer_pairing_join_available"], true);
        assert_eq!(
            status["result"]["api_peer_pairing_request_access_available"],
            true
        );
        assert_eq!(
            status["result"]["api_peer_pairing_request_decision_available"],
            true
        );
        assert_eq!(
            status["result"]["api_peer_pairing_requests_available"],
            true
        );
        assert_eq!(
            status["result"]["api_peer_pairing_identities_available"],
            true
        );
        assert_eq!(
            status["result"]["api_peer_pairing_identity_revoke_available"],
            true
        );
        assert_eq!(status["result"]["api_coordinator_available"], false);

        let peers = test_control_frame_response(
            r#"{"t":"request","id":"a1","method":"api_peers"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(peers["t"], "response");
        assert_eq!(peers["ok"], false);
        assert_eq!(peers["error"], "peer registry unavailable");

        let subscribed = test_control_frame_response(
            r#"{"t":"request","id":"e1","method":"subscribe_events"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(subscribed["t"], "response");
        assert_eq!(subscribed["ok"], true);
        assert_eq!(subscribed["result"]["subscribed"], true);
        assert!(rt.events_subscribed);

        let project_root = test_control_frame_response(
            r#"{"t":"request","id":"pr1","method":"api_project_root"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(project_root.is_none());
        assert!(pending.contains_key("pr1"));
        let (_, project_root) = rx.recv().await.unwrap();
        assert!(pending.cancel_remove(&project_root.id));
        assert_eq!(project_root.id, "pr1");
        assert!(project_root.done);
        let project_root = project_root.frame;
        assert_eq!(project_root["t"], "response");
        assert_eq!(project_root["ok"], true);
        assert!(project_root["result"].get("project_root").is_some());

        let queued = test_control_frame_response(
            r#"{"t":"request","id":"q1","method":"api_sessions","params":{"limit":1}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(queued.is_none());
        assert!(pending.contains_key("q1"));
        let cancelled = test_control_frame_response(
            r#"{"t":"cancel","id":"q1"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(cancelled["t"], "response");
        assert_eq!(cancelled["ok"], false);
        assert_eq!(cancelled["cancelled"], true);
        assert!(!pending.contains_key("q1"));
    }

    /// The operation a method's declaration carries (route-row tunnel
    /// column or residue `CONTROL_ONLY_METHODS` entry — the effective table).
    fn method_operation(method: &str) -> Option<crate::peer::access_policy::PeerOperation> {
        control_method_spec(method).and_then(|spec| spec.op)
    }

    #[test]
    fn credential_lease_methods_sit_behind_credentials_manage() {
        use crate::peer::access_policy::PeerOperation;
        for method in [
            "api_credential_lease_grant",
            "api_credential_lease_renew",
            "api_credential_lease_revoke",
            "api_credential_lease_status",
            "api_credential_custody_trail",
        ] {
            assert_eq!(
                method_operation(method),
                Some(PeerOperation::CredentialsManage),
                "{method} must ride the credentials.manage gate"
            );
        }

        // A scoped guest session (role:scoped-human) can neither fuel nor
        // inspect fueling; the trusted-local and operator lanes can.
        let mut rt = runtime();
        rt.grant = scoped_user_client_grant();
        assert!(
            authorize_dashboard_control_method(&rt, "api_credential_lease_status", None).is_err()
        );
        assert!(
            authorize_dashboard_control_method(&rt, "api_credential_lease_grant", None).is_err()
        );

        rt.grant = DashboardControlGrant::TrustedLocal;
        assert!(
            authorize_dashboard_control_method(&rt, "api_credential_lease_grant", None).is_ok()
        );
    }

    #[test]
    fn cached_bootstrap_hides_private_display_grants_from_non_owners() {
        let mut rt = runtime();
        rt.grant = scoped_user_client_grant();
        *rt.bootstrap_caches.last_user_display_json.lock().unwrap() = Some(
            r#"{"event":"user_display_granted","display_id":9,"agent_visible":false}"#.to_string(),
        );

        let filtered = cached_bootstrap_events_response_frame(
            "private-cache".to_string(),
            &rt.bootstrap_caches,
            &rt.grant,
        );
        assert_eq!(filtered["result"]["event_count"], 0);
        assert_eq!(filtered["result"]["events"], serde_json::json!([]));
    }

    #[test]
    fn control_method_table_is_coherent() {
        // Coherence holds over the effective union (route-row tunnel
        // specs ∪ the CONTROL_ONLY_METHODS residue): a name declared on both
        // sides is a duplicate here just like two residue rows were.
        let mut seen = HashSet::new();
        for spec in all_control_methods() {
            assert!(
                seen.insert(spec.name),
                "duplicate method row: {}",
                spec.name
            );
            assert!(
                !spec.upload || spec.op.is_some(),
                "upload method {} must be operation-gated",
                spec.name
            );
        }
        let features = control_features();
        let unique: HashSet<_> = features.iter().collect();
        assert_eq!(
            unique.len(),
            features.len(),
            "wire features must not collide with method names"
        );
    }

    #[test]
    fn hosted_method_wall_is_pinned_to_the_effective_method_table() {
        use crate::access::hosted_control::{
            hosted_dashboard_method_allowed, preset_allows_operation, HostedPreset,
        };
        let view: std::collections::BTreeSet<&str> = [
            "ping",
            "status",
            "api_agent_card",
            "api_cached_bootstrap_events",
            "subscribe_events",
            "unsubscribe_events",
            "api_sessions",
            "api_sessions_stream",
            "api_sessions_search",
            "api_sessions_message_search",
            "api_session_detail",
            "api_session_agent_output",
            "api_session_context_snapshot",
            "api_session_fork_points",
            "api_displays",
            "api_display_bootstrap",
            "api_display_webrtc_signal",
            "api_state_snapshot",
            "api_session_log_replay",
            "api_external_session_activity_replay",
            "api_dashboard_bootstrap",
        ]
        .into_iter()
        .collect();
        let tasks = view
            .iter()
            .copied()
            .chain(["api_control_msg"])
            .collect::<std::collections::BTreeSet<_>>();
        let operate = tasks
            .iter()
            .copied()
            .chain([
                "api_session_control_msg",
                "api_dashboard_action_msg",
                "api_fs_stat",
                "api_fs_list",
                "api_fs_read",
                "api_fs_mkdir",
                "api_fs_write",
                "api_fs_rename",
                "api_fs_delete",
                "api_transfer_jobs",
                "api_transfer_job_create",
                "api_transfer_upload_chunk",
                "api_transfer_upload_commit",
                "api_transfer_job_delete",
                "api_transfer_download_read",
                "api_display_input_authority_snapshot",
                "api_display_input_authority_request",
                "api_display_input_authority_release",
            ])
            .collect::<std::collections::BTreeSet<_>>();

        for (preset, expected) in [
            (HostedPreset::View, view),
            (HostedPreset::Tasks, tasks),
            (HostedPreset::Operate, operate),
        ] {
            let actual = all_control_methods()
                .iter()
                .filter(|spec| hosted_dashboard_method_allowed(preset, spec.name))
                .map(|spec| spec.name)
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                actual,
                expected,
                "{} hosted method projection drifted",
                preset.as_str(),
            );
            for spec in all_control_methods()
                .iter()
                .filter(|spec| hosted_dashboard_method_allowed(preset, spec.name))
            {
                assert!(
                    spec.op
                        .is_none_or(|operation| preset_allows_operation(preset, operation)),
                    "{} admits {} without its {:?} IAM floor",
                    preset.as_str(),
                    spec.name,
                    spec.op,
                );
            }
        }
        assert!(!hosted_dashboard_method_allowed(
            HostedPreset::Operate,
            "api_future_method"
        ));
    }

    /// Transport-unification S3 differential pin (design §8, risks
    /// R2/R4): the complete tunnel-method partition — every wire method
    /// name, the declaration source that carries it (`Row` = a `tunnel:`
    /// column on a `gateway_routes::ROUTES` row, `Residue` = a
    /// `CONTROL_ONLY_METHODS` entry), and the IAM operation gating it —
    /// frozen as a literal table. Re-gating a method (operation change
    /// on either side), losing one (gone from both sources), duplicating
    /// one (declared on both), or moving one between sources fails here
    /// until this table is updated in the same change, deliberately.
    /// Permanent program infrastructure, and the S11 flip's union-
    /// equality proof: the assertions below check the live union against
    /// the frozen partition in BOTH directions (every pinned name lives
    /// with the pinned source and operation; every live name is pinned),
    /// so the full name × operation union is provably unchanged by the
    /// dispatch flip — the residue below is the final tunnel-only set,
    /// and the union never changes by accident.
    #[test]
    fn tunnel_method_partition_is_pinned() {
        use crate::peer::access_policy::PeerOperation as Op;
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Source {
            Row,
            Residue,
        }
        use Source::{Residue, Row};
        let frozen: &[(&str, Source, Option<Op>)] = &[
            ("ping", Residue, None),
            ("config", Residue, Some(Op::PresenceRead)),
            ("status", Residue, Some(Op::PresenceRead)),
            ("api_agent_card", Residue, Some(Op::PresenceRead)),
            (
                "api_cached_bootstrap_events",
                Residue,
                Some(Op::SessionInspect),
            ),
            ("subscribe_events", Residue, Some(Op::SessionInspect)),
            ("unsubscribe_events", Residue, Some(Op::SessionInspect)),
            ("api_access_overview", Row, Some(Op::AccessInspect)),
            ("api_access_iam_state", Row, Some(Op::AccessInspect)),
            (
                "api_access_enrollment_requests",
                Row,
                Some(Op::AccessInspect),
            ),
            ("api_dashboard_targets", Row, Some(Op::AccessInspect)),
            ("api_dashboard_tabs", Row, Some(Op::AccessInspect)),
            ("api_access_connect_status", Row, Some(Op::AccessInspect)),
            ("api_access_connect_claim_code", Row, Some(Op::AccessManage)),
            ("api_access_connect_config", Row, Some(Op::AccessManage)),
            ("api_access_connect_unclaim", Row, Some(Op::AccessManage)),
            ("api_access_set_tier", Row, Some(Op::AccessManage)),
            ("api_fleet_cert_request", Row, Some(Op::AccessManage)),
            (
                "api_credential_lease_grant",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_credential_lease_renew",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_credential_lease_revoke",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_credential_lease_status",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_credential_custody_trail",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_daemon_vault_fetch",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_daemon_vault_publish",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_daemon_vault_deposit_key_fetch",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_daemon_vault_deposit_key_publish",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_daemon_vault_deposits_fetch",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_daemon_vault_deposits_consume",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_credential_egress_register",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_credential_egress_unregister",
                Residue,
                Some(Op::CredentialsManage),
            ),
            (
                "api_credential_egress_probe",
                Residue,
                Some(Op::CredentialsManage),
            ),
            // The Claude sign-in ceremony is row-declared custody: every
            // leaf (the status read included) gates on credentials.manage.
            ("api_claude_auth_start", Row, Some(Op::CredentialsManage)),
            ("api_claude_auth_status", Row, Some(Op::CredentialsManage)),
            ("api_claude_auth_code", Row, Some(Op::CredentialsManage)),
            ("api_claude_auth_cancel", Row, Some(Op::CredentialsManage)),
            (
                "api_access_iam_upsert_user_client_grant",
                Row,
                Some(Op::AccessManage),
            ),
            ("api_access_iam_update_grant", Row, Some(Op::AccessManage)),
            ("api_access_enrollment_decide", Row, Some(Op::AccessManage)),
            ("api_access_org_trust", Row, Some(Op::AccessManage)),
            ("api_access_org_revoke", Row, Some(Op::AccessManage)),
            ("api_access_org_issue", Row, Some(Op::AccessManage)),
            ("api_access_org_revoke_member", Row, Some(Op::AccessManage)),
            ("api_access_org_issuer_init", Row, Some(Op::AccessManage)),
            (
                "api_access_org_issuer_delegate",
                Row,
                Some(Op::AccessManage),
            ),
            ("api_access_org_issuer_install", Row, Some(Op::AccessManage)),
            ("api_access_org_present", Row, Some(Op::AccessInspect)),
            ("api_access_org_orl", Row, Some(Op::AccessInspect)),
            ("api_access_org_renew", Row, Some(Op::AccessInspect)),
            ("api_access_org_orl_apply", Row, Some(Op::PresenceRead)),
            // The peers/coordinator family flipped Residue → Row in S7
            // with every operation unchanged (the no-re-gating proof);
            // the ops now derive from federation_http_operation on each
            // row's canonical leaf, asserted per method by
            // gateway_routes::peers_family_tunnel_ops_assert_against_the_federation_ladder.
            ("api_peer_pairing_requests", Row, Some(Op::AccessInspect)),
            ("api_peer_pairing_identities", Row, Some(Op::AccessInspect)),
            (
                "api_peer_pairing_request_decision",
                Row,
                Some(Op::AccessManage),
            ),
            (
                "api_peer_pairing_identity_revoke",
                Row,
                Some(Op::AccessManage),
            ),
            ("api_peer_pairing_invite", Row, Some(Op::AccessManage)),
            ("api_peers", Row, Some(Op::PeerInspect)),
            ("api_peer_eligible", Row, Some(Op::PeerInspect)),
            ("api_peer_webrtc_signal", Row, Some(Op::PeerUse)),
            ("api_peer_file_transfer_signal", Row, Some(Op::PeerUse)),
            ("api_peer_dashboard_control_signal", Row, Some(Op::PeerUse)),
            ("api_peer_message", Row, Some(Op::PeerUse)),
            ("api_peer_task", Row, Some(Op::PeerUse)),
            ("api_peer_approval", Row, Some(Op::PeerUse)),
            ("api_peer_add", Row, Some(Op::PeerManage)),
            ("api_peer_remove", Row, Some(Op::PeerManage)),
            ("api_peer_pairing_join", Row, Some(Op::PeerManage)),
            ("api_peer_pairing_request_access", Row, Some(Op::PeerManage)),
            (
                "api_peer_pairing_request_access_poll",
                Row,
                Some(Op::PeerManage),
            ),
            // Coordinator routing derives PeerUse from the ladder like
            // the quick controls (owner decision 2026-07-11; its
            // historical PeerManage override is gone).
            ("api_coordinator_route", Row, Some(Op::PeerUse)),
            ("api_sessions", Row, Some(Op::SessionInspect)),
            ("api_sessions_stream", Row, Some(Op::SessionInspect)),
            ("api_session_detail", Row, Some(Op::SessionInspect)),
            ("api_session_fork_points", Row, Some(Op::SessionInspect)),
            (
                "api_session_background_tasks",
                Row,
                Some(Op::SessionInspect),
            ),
            (
                "api_session_background_task_output",
                Row,
                Some(Op::SessionInspect),
            ),
            ("api_session_report", Row, Some(Op::SessionInspect)),
            ("api_session_agent_output", Row, Some(Op::SessionInspect)),
            (
                "api_session_context_snapshot",
                Row,
                Some(Op::SessionInspect),
            ),
            ("api_sessions_search", Row, Some(Op::SessionInspect)),
            ("api_sessions_message_search", Row, Some(Op::SessionInspect)),
            ("api_session_recordings", Row, Some(Op::SessionInspect)),
            ("api_session_recording_asset", Row, Some(Op::SessionInspect)),
            ("api_session_frame_asset", Row, Some(Op::SessionInspect)),
            ("api_worktrees", Row, Some(Op::SessionInspect)),
            ("api_worktrees_inspect", Row, Some(Op::SessionInspect)),
            ("api_session_delete", Row, Some(Op::SessionManage)),
            ("api_session_current_history", Row, Some(Op::SessionManage)),
            ("api_session_current_rollback", Row, Some(Op::SessionManage)),
            ("api_agenda_list", Row, Some(Op::AgendaRead)),
            ("api_agenda_op", Row, Some(Op::AgendaWrite)),
            ("api_agenda_reminder_policy", Row, Some(Op::Settings)),
            ("api_memory_search", Row, Some(Op::MemoryRead)),
            ("api_memory_claim", Row, Some(Op::MemoryRead)),
            ("api_memory_propose", Row, Some(Op::MemoryWrite)),
            ("api_session_current_redo", Row, Some(Op::SessionManage)),
            ("api_session_current_prune", Row, Some(Op::SessionManage)),
            ("api_session_current_changes", Row, Some(Op::SessionManage)),
            ("api_session_current_uploads", Row, Some(Op::SessionManage)),
            (
                "api_session_current_upload_raw",
                Row,
                Some(Op::SessionManage),
            ),
            (
                "api_session_current_upload_delete",
                Row,
                Some(Op::SessionManage),
            ),
            (
                "api_session_current_agent_output",
                Row,
                Some(Op::SessionManage),
            ),
            ("api_session_control_msg", Residue, Some(Op::SessionManage)),
            ("api_worktrees_scan", Row, Some(Op::SessionManage)),
            ("api_worktrees_remove", Row, Some(Op::SessionManage)),
            ("api_worktrees_clean", Row, Some(Op::SessionManage)),
            ("api_worktrees_merge", Row, Some(Op::SessionManage)),
            ("api_session_current_upload", Row, Some(Op::SessionManage)),
            // The transfer family flipped Residue → Row with its
            // /api/transfers rows (S9, task #6): ops now derive from
            // the rows, same classes as always.
            ("api_transfer_jobs", Row, Some(Op::FilesystemRead)),
            ("api_transfer_download_read", Row, Some(Op::FilesystemRead)),
            ("api_fs_stat", Row, Some(Op::FilesystemRead)),
            ("api_fs_list", Row, Some(Op::FilesystemRead)),
            ("api_fs_read", Row, Some(Op::FilesystemRead)),
            ("api_transfer_job_create", Row, Some(Op::FilesystemWrite)),
            ("api_transfer_job_delete", Row, Some(Op::FilesystemWrite)),
            ("api_transfer_upload_chunk", Row, Some(Op::FilesystemWrite)),
            ("api_transfer_upload_commit", Row, Some(Op::FilesystemWrite)),
            ("api_fs_mkdir", Row, Some(Op::FilesystemWrite)),
            ("api_fs_write", Row, Some(Op::FilesystemWrite)),
            ("api_fs_rename", Row, Some(Op::FilesystemWrite)),
            ("api_fs_delete", Row, Some(Op::FilesystemWrite)),
            ("api_display_bootstrap", Residue, Some(Op::DisplayView)),
            ("api_display_webrtc_signal", Residue, Some(Op::DisplayView)),
            ("api_displays", Row, Some(Op::DisplayView)),
            (
                "api_display_input_authority_snapshot",
                Residue,
                Some(Op::DisplayInput),
            ),
            (
                "api_display_input_authority_request",
                Residue,
                Some(Op::DisplayInput),
            ),
            (
                "api_display_input_authority_release",
                Residue,
                Some(Op::DisplayInput),
            ),
            (
                "api_diagnostics_visual_freshness",
                Row,
                Some(Op::DisplayInput),
            ),
            ("api_control_msg", Residue, Some(Op::Message)),
            ("api_dashboard_action_msg", Residue, Some(Op::Message)),
            ("api_mcp_tool_call", Residue, Some(Op::Message)),
            ("api_settings", Row, Some(Op::Settings)),
            ("api_settings_save", Row, Some(Op::Settings)),
            ("api_key_status", Row, Some(Op::Settings)),
            ("api_api_keys_save", Row, Some(Op::Settings)),
            ("api_project_root", Row, Some(Op::Settings)),
            ("api_voice_session", Residue, Some(Op::RuntimeControl)),
            (
                "api_presence_video_frame",
                Residue,
                Some(Op::RuntimeControl),
            ),
            (
                "api_media_annotation_attach",
                Residue,
                Some(Op::RuntimeControl),
            ),
            (
                "api_media_annotation_submit",
                Residue,
                Some(Op::RuntimeControl),
            ),
            ("api_media_clip_start", Residue, Some(Op::RuntimeControl)),
            ("api_media_clip_frame", Residue, Some(Op::RuntimeControl)),
            ("api_media_clip_end", Residue, Some(Op::RuntimeControl)),
            ("api_media_clip_cancel", Residue, Some(Op::RuntimeControl)),
            ("api_recordings", Residue, Some(Op::RuntimeControl)),
            ("api_recording_asset", Residue, Some(Op::RuntimeControl)),
            (
                "api_browser_workspace_snapshot",
                Residue,
                Some(Op::SessionInspect),
            ),
            ("api_state_snapshot", Residue, Some(Op::SessionInspect)),
            ("api_session_log_replay", Residue, Some(Op::SessionInspect)),
            (
                "api_external_session_activity_replay",
                Residue,
                Some(Op::SessionInspect),
            ),
            ("api_dashboard_bootstrap", Residue, Some(Op::SessionInspect)),
            ("api_managed_context_records", Row, Some(Op::SessionInspect)),
            ("api_managed_context_anchors", Row, Some(Op::SessionInspect)),
            ("api_managed_context_fission", Row, Some(Op::SessionInspect)),
            ("api_external_agents", Row, Some(Op::SessionInspect)),
        ];

        // Live partition: rows first (the resolution order), then the
        // residue; a name on both sides is declared twice — an error.
        let mut live: BTreeMap<&str, (Source, Option<Op>)> = BTreeMap::new();
        for (route, spec) in crate::gateway_routes::tunnel_specs() {
            let op = route.tunnel_operation();
            assert!(
                op.is_some(),
                "{}: tunnel row must derive an IAM operation",
                spec.name
            );
            assert!(
                live.insert(spec.name, (Row, op)).is_none(),
                "{} is declared on more than one route row",
                spec.name
            );
        }
        for spec in CONTROL_ONLY_METHODS {
            assert!(
                live.insert(spec.name, (Residue, spec.op)).is_none(),
                "{} is declared BOTH as a route-row tunnel column and in \
                 CONTROL_ONLY_METHODS — remove the residue entry",
                spec.name
            );
        }

        let mut pinned: BTreeMap<&str, (Source, Option<Op>)> = BTreeMap::new();
        for (name, source, op) in frozen.iter().copied() {
            assert!(
                pinned.insert(name, (source, op)).is_none(),
                "frozen partition lists {name} twice"
            );
        }
        for (name, (source, op)) in &pinned {
            let Some((live_source, live_op)) = live.get(name) else {
                panic!(
                    "{name} vanished from the live declarations (frozen as \
                     {source:?} {op:?}); if the removal is deliberate, drop \
                     it from the frozen partition in the same change"
                );
            };
            assert_eq!(
                live_source, source,
                "{name} moved declaration source; update the frozen \
                 partition deliberately"
            );
            assert_eq!(
                live_op, op,
                "{name} was re-gated; an IAM operation change must update \
                 the frozen partition deliberately"
            );
        }
        for name in live.keys() {
            assert!(
                pinned.contains_key(name),
                "{name} is a new tunnel method not in the frozen partition; \
                 add it (source + operation) deliberately"
            );
        }
    }

    /// The SPA's `daemonApi` facade mirrors the HTTP twins of its tunnel
    /// methods as `DAEMON_API_HTTP_MAP` (static/app/32-daemon-api.js). That
    /// copy can't derive from `gateway_routes::ROUTES`, so pin every entry
    /// against the table — same pattern as
    /// `spa_action_msg_rpc_set_mirrors_dashboard_action_allowlist`
    /// (api_control.rs). Four facts per entry: the tunnel twin exists in
    /// `CONTROL_ONLY_METHODS`; the verb + instantiated path resolve to a
    /// declared route whose verb is declared exactly (never via `Any`);
    /// the row's IAM operation equals the tunnel method's (the signed-org
    /// courier rows are Public on HTTP by design — public authenticates no
    /// caller and creates no control session; the verified document can only
    /// affect its named subject — and instead pin their
    /// documented tunnel op-override; the peers/coordinator federation
    /// rows pin the row's own derivation — the federation ladder on the
    /// canonical leaf); and the path
    /// template restates the row's declared pattern (captures by name).
    /// Plus the exact coverage set, so entries appear and disappear
    /// deliberately. When the route table grows its `tunnel:` column
    /// (transport program S3), this hand-derivation collapses into the
    /// table itself.
    #[test]
    fn daemon_api_http_map_mirrors_gateway_routes() {
        use crate::gateway_routes::{
            match_route, PathPattern, RouteAuthz, RouteMethod, SegmentSpec,
        };

        let app = include_str!("../../../../static/app.html");
        let start = "const DAEMON_API_HTTP_MAP = Object.freeze({";
        let from = app
            .find(start)
            .expect("DAEMON_API_HTTP_MAP not found in app.html")
            + start.len();
        let rest = &app[from..];
        let to = rest
            .find("});")
            .expect("DAEMON_API_HTTP_MAP is unterminated");

        // One `name: { verb: '…', path: '…', … },` entry per line — the
        // fragment documents that contract next to the literal.
        fn quoted(entry: &str, key: &str) -> Option<String> {
            let marker = format!("{key}: '");
            let at = entry.find(&marker)? + marker.len();
            let rest = &entry[at..];
            Some(rest[..rest.find('\'')?].to_string())
        }
        let mut entries: std::collections::BTreeMap<String, (String, String)> = Default::default();
        for line in rest[..to].lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let name = line
                .split(':')
                .next()
                .expect("descriptor entry names a method")
                .trim()
                .to_string();
            let verb = quoted(line, "verb").unwrap_or_else(|| panic!("{name}: missing verb"));
            let path = quoted(line, "path").unwrap_or_else(|| panic!("{name}: missing path"));
            assert!(
                entries.insert(name.clone(), (verb, path)).is_none(),
                "duplicate descriptor entry: {name}"
            );
        }

        // Coverage pin: the F1 family's twinned methods (fs + staged
        // uploads), the F2 sessions-family reads (managed-context,
        // worktrees, the session list and its NDJSON stream, search,
        // detail, report, context snapshots) plus the F8a sessions
        // stragglers (recordings list, agent output by id and for the
        // current session, session delete via the {id}/{target} shape,
        // and the current-session rounds family — changes, history,
        // rollback, redo, prune), the F3 settings/keys
        // family (settings GET/POST, api-keys save, key-status,
        // project-root, external-agents, displays), and the F4 access
        // family: the dialogs set (overview, IAM state, enrollment reads
        // + decide, IAM grant upsert/update, the connect admin quartet,
        // the tier pair, fleet-cert request, dashboard targets) plus the
        // org set (trust/revoke, issuance, issuer keys, and the
        // signed-org doorbell quartet), and the F5 peers/coordinator
        // family (registry list/add/remove, eligible, the quick
        // controls — message/task/approval, the three signal relays,
        // the pairing set, and the coordinator route), plus the
        // `api_transfer_*` sextet riding the S9 /api/transfers rows
        // (task #6 / F1c — the SPA feature-detects the rows with one
        // GET probe before using the lane, so old daemons keep honest
        // availability); adding or dropping an entry updates this list
        // in the same change, deliberately. The F6 credential-custody
        // family (api_credential_*,
        // api_daemon_vault_*) is deliberately NOT here and stays out:
        // custody is tunnel-scoped by design with no HTTP rows planned
        // (docs/src/credential-custody.md; the transport design parks
        // custody rows as an explicit future decision), so its facade
        // calls run with no fallback lane — absence below is the contract,
        // not a gap. The F7 control-msg trio (api_control_msg,
        // api_session_control_msg, api_dashboard_action_msg) is likewise
        // deliberately absent: WS-twin residue whose HTTP-era twin is the
        // /ws intent stream, not a route (transport design §2.7) — the
        // facade serves the tunnel leg only and the dispatchers keep
        // their own /ws fallback. So is the F7 display residue
        // (api_display_bootstrap, api_display_webrtc_signal, the
        // api_display_input_authority_* trio): their HTTP-era twin is
        // the /ws signaling socket. api_diagnostics_visual_freshness IS
        // here — the family's one twinned row (S5), and the descriptor's
        // one rawBody entry (the tunnel carries the NDJSON transcript as
        // a `body` param; the HTTP twin appends its raw body verbatim,
        // so the adapter must never JSON-encode it).
        let expected: std::collections::BTreeSet<&str> = [
            "api_fs_stat",
            "api_fs_list",
            "api_fs_read",
            "api_fs_mkdir",
            "api_fs_write",
            "api_fs_rename",
            "api_fs_delete",
            "api_transfer_jobs",
            "api_transfer_job_create",
            "api_transfer_upload_chunk",
            "api_transfer_upload_commit",
            "api_transfer_job_delete",
            "api_transfer_download_read",
            "api_session_current_uploads",
            "api_session_current_upload",
            "api_session_current_upload_raw",
            "api_session_current_upload_delete",
            "api_sessions",
            "api_sessions_stream",
            "api_sessions_search",
            "api_sessions_message_search",
            "api_session_detail",
            "api_session_fork_points",
            "api_session_background_tasks",
            "api_session_background_task_output",
            "api_session_report",
            "api_session_context_snapshot",
            "api_session_recordings",
            "api_session_agent_output",
            "api_session_delete",
            "api_session_current_changes",
            "api_session_current_history",
            "api_session_current_rollback",
            "api_agenda_list",
            "api_agenda_op",
            "api_agenda_reminder_policy",
            "api_memory_search",
            "api_memory_claim",
            "api_memory_propose",
            "api_session_current_redo",
            "api_session_current_prune",
            "api_session_current_agent_output",
            "api_managed_context_records",
            "api_managed_context_anchors",
            "api_managed_context_fission",
            "api_worktrees",
            "api_worktrees_inspect",
            "api_worktrees_scan",
            "api_worktrees_remove",
            "api_worktrees_clean",
            "api_worktrees_merge",
            "api_settings",
            "api_settings_save",
            "api_api_keys_save",
            "api_key_status",
            "api_claude_auth_start",
            "api_claude_auth_status",
            "api_claude_auth_code",
            "api_claude_auth_cancel",
            "api_project_root",
            "api_external_agents",
            "api_displays",
            "api_diagnostics_visual_freshness",
            "api_access_overview",
            "api_access_iam_state",
            "api_access_enrollment_requests",
            "api_access_enrollment_decide",
            "api_access_iam_upsert_user_client_grant",
            "api_access_iam_update_grant",
            "api_access_connect_status",
            "api_access_connect_claim_code",
            "api_access_connect_config",
            "api_access_connect_unclaim",
            "api_access_set_tier",
            "api_fleet_cert_request",
            "api_dashboard_targets",
            "api_dashboard_tabs",
            "api_access_org_trust",
            "api_access_org_revoke",
            "api_access_org_issue",
            "api_access_org_revoke_member",
            "api_access_org_issuer_init",
            "api_access_org_issuer_delegate",
            "api_access_org_issuer_install",
            "api_access_org_present",
            "api_access_org_renew",
            "api_access_org_orl",
            "api_access_org_orl_apply",
            "api_peers",
            "api_peer_add",
            "api_peer_remove",
            "api_peer_eligible",
            "api_peer_message",
            "api_peer_task",
            "api_peer_approval",
            "api_peer_webrtc_signal",
            "api_peer_file_transfer_signal",
            "api_peer_dashboard_control_signal",
            "api_peer_pairing_invite",
            "api_peer_pairing_join",
            "api_peer_pairing_request_access",
            "api_peer_pairing_request_access_poll",
            "api_peer_pairing_requests",
            "api_peer_pairing_request_decision",
            "api_peer_pairing_identities",
            "api_peer_pairing_identity_revoke",
            "api_coordinator_route",
        ]
        .into_iter()
        .collect();
        let actual: std::collections::BTreeSet<&str> = entries.keys().map(String::as_str).collect();
        assert_eq!(actual, expected, "DAEMON_API_HTTP_MAP coverage drifted");

        for (method_name, (verb, template)) in &entries {
            let spec = control_method_spec(method_name).unwrap_or_else(|| {
                panic!("{method_name}: descriptor entry has no CONTROL_ONLY_METHODS row")
            });
            let tunnel_op = spec
                .op
                .unwrap_or_else(|| panic!("{method_name}: twinned methods must be op-gated"));

            // Resolve the template through the real router, with sample
            // segments standing in for the captures.
            let concrete = template
                .split('/')
                .map(|segment| {
                    if segment.starts_with('{') && segment.ends_with('}') {
                        "cap-sample"
                    } else {
                        segment
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            let (route, _captures) = match_route(verb, &concrete).unwrap_or_else(|| {
                panic!("{method_name}: {verb} {concrete} matches no declared route")
            });

            // The verb must be declared exactly — a map entry riding an
            // `Any` row would hide a method-tightening regression.
            let declared = match verb.as_str() {
                "GET" => RouteMethod::Get,
                "POST" => RouteMethod::Post,
                "DELETE" => RouteMethod::Delete,
                other => panic!("{method_name}: unsupported descriptor verb {other}"),
            };
            assert_eq!(
                route.method, declared,
                "{method_name}: route declares {:?}, descriptor says {verb}",
                route.method
            );

            // IAM twin agreement: the same operation gates the method on
            // both transports.
            match route.authz {
                RouteAuthz::Operation(op) => assert_eq!(
                    op, tunnel_op,
                    "{method_name}: tunnel op {tunnel_op:?} != route op {op:?}"
                ),
                // The signed-org courier rows are Public on HTTP by
                // design. Public means no caller/session authentication:
                // the verified document authorizes only subject-bound
                // processing, never daemon control by its courier.
                // while their tunnel twins gate stricter through
                // documented op-overrides (F4). Require the row to carry
                // this method's tunnel column with an override matching
                // the effective tunnel operation; the override list
                // itself is pinned closed by the gateway's
                // tunnel_op_overrides_are_a_closed_documented_enumeration.
                RouteAuthz::Public => {
                    let tunnel = route.tunnel.as_ref().unwrap_or_else(|| {
                        panic!("{method_name}: Public descriptor row lost its tunnel column")
                    });
                    assert_eq!(
                        tunnel.name,
                        method_name.as_str(),
                        "{method_name}: resolved Public row carries a different tunnel method"
                    );
                    let (override_op, _reason) = tunnel.op_override.unwrap_or_else(|| {
                        panic!(
                            "{method_name}: Public twinned rows must carry a \
                             documented tunnel op-override"
                        )
                    });
                    assert_eq!(
                        override_op, tunnel_op,
                        "{method_name}: tunnel op {tunnel_op:?} != declared override {override_op:?}"
                    );
                }
                // The peers/coordinator family rows delegate HTTP authz to
                // the federation ladder (F5). Their tunnel op derives from
                // the row itself — `Route::tunnel_operation` applies the
                // same ladder to the row's canonical leaf
                // (api_coordinator_route included: both of its lanes gate
                // on PeerUse since the 2026-07-11 owner decision retired
                // its historical PeerManage override). Require the
                // resolved row to carry THIS method's tunnel column and
                // its derivation to equal the effective tunnel operation.
                RouteAuthz::PeerFederation => {
                    let tunnel = route.tunnel.as_ref().unwrap_or_else(|| {
                        panic!("{method_name}: federation descriptor row lost its tunnel column")
                    });
                    assert_eq!(
                        tunnel.name,
                        method_name.as_str(),
                        "{method_name}: resolved federation row carries a different tunnel method"
                    );
                    let derived = route.tunnel_operation().unwrap_or_else(|| {
                        panic!(
                            "{method_name}: federation twinned rows must derive \
                             a fail-closed tunnel operation"
                        )
                    });
                    assert_eq!(
                        derived, tunnel_op,
                        "{method_name}: tunnel op {tunnel_op:?} != row derivation {derived:?}"
                    );
                }
                _ => panic!("{method_name}: twinned rows must be Operation-gated"),
            }

            // The template must restate the row's declared shape, not just
            // happen to resolve through it.
            match route.pattern {
                PathPattern::Exact(base) => {
                    assert_eq!(
                        template, base,
                        "{method_name}: template != exact route path"
                    )
                }
                PathPattern::Under(base) => assert!(
                    template == base || template.starts_with(&format!("{base}/")),
                    "{method_name}: template {template} is not under {base}"
                ),
                PathPattern::Segments(base, segments) => {
                    let mut rendered = String::from(base);
                    for segment in segments {
                        match segment {
                            SegmentSpec::Capture(name) => {
                                rendered.push_str("/{");
                                rendered.push_str(name);
                                rendered.push('}');
                            }
                            SegmentSpec::Literal(literal) => {
                                rendered.push('/');
                                rendered.push_str(literal);
                            }
                            SegmentSpec::OneOf(_) => {
                                panic!("{method_name}: OneOf rows are not in the twinned set")
                            }
                        }
                    }
                    assert_eq!(
                        template, &rendered,
                        "{method_name}: template != rendered Segments pattern"
                    );
                }
            }
        }
    }

    #[test]
    fn unknown_dashboard_control_methods_are_denied_fail_closed() {
        let rt = runtime();
        assert!(
            authorize_dashboard_control_method(&rt, "api_added_without_table_row", None).is_err()
        );
        assert!(authorize_dashboard_control_upload(&rt, "api_added_without_table_row").is_err());
        // Request methods are not upload-deliverable unless declared so.
        assert!(authorize_dashboard_control_upload(&rt, "api_sessions").is_err());
        // ping stays reachable for any bound session; declared upload
        // methods authorize by their table operation.
        assert!(authorize_dashboard_control_method(&rt, "ping", None).is_ok());
        assert!(authorize_dashboard_control_upload(&rt, "api_fs_write").is_ok());
    }

    #[test]
    fn contract_pins_for_deliberate_method_gates() {
        use crate::peer::access_policy::PeerOperation;
        // ORL apply is courierable by any session. The root signature
        // authorizes only application of signed revocation facts; it does
        // not authenticate the courier or grant that session daemon control.
        assert_eq!(
            method_operation("api_access_org_orl_apply"),
            Some(PeerOperation::PresenceRead)
        );
        // Peer quick controls ride peer.use, not peer administration.
        for method in ["api_peer_message", "api_peer_task", "api_peer_approval"] {
            assert_eq!(
                method_operation(method),
                Some(PeerOperation::PeerUse),
                "{method} must ride peer.use"
            );
        }
        // Both delivery lanes of a dual-delivery method share one gate.
        assert_eq!(
            method_operation("api_fs_write"),
            Some(PeerOperation::FilesystemWrite)
        );
        assert_eq!(
            method_operation("api_transfer_upload_chunk"),
            Some(PeerOperation::FilesystemWrite)
        );
    }

    /// The two methods whose tunnel rows historically diverged from their
    /// HTTP route rows are pinned equal across BOTH lanes. Their handlers
    /// are pure session-log reads (`session_agent_output_api_response`
    /// fetches persisted output chunks by id;
    /// `session_context_snapshot_api_response` replays one archived
    /// snapshot), so both lanes gate them inspect-grade. Since S4a their
    /// tunnel ops DERIVE from the route rows (`tunnel:` columns), making
    /// the drift class unrepresentable — this test stays as the
    /// end-to-end assertion that classification and method table agree.
    #[test]
    fn formerly_divergent_twins_gate_identically_on_both_lanes() {
        use crate::gateway_routes::{classify, TableClassification};
        use crate::peer::access_policy::PeerOperation;
        for (method, http_method, http_path) in [
            (
                "api_session_agent_output",
                "POST",
                "/api/session/abc123/agent-output",
            ),
            (
                "api_session_context_snapshot",
                "GET",
                "/api/session/abc123/context-snapshot",
            ),
        ] {
            let tunnel_op = method_operation(method);
            assert_eq!(
                tunnel_op,
                Some(PeerOperation::SessionInspect),
                "{method}: session-log reads are inspect-grade"
            );
            let TableClassification::Matched(route_op) = classify(http_method, http_path) else {
                panic!("{http_method} {http_path} must classify via the route table");
            };
            assert_eq!(
                tunnel_op, route_op,
                "{method} must gate identically on the tunnel and on {http_method} {http_path}"
            );
        }
    }

    #[tokio::test]
    async fn presence_frame_routes_voice_log() {
        let mut rt = runtime();
        let mut events = rt.bus.subscribe();
        let (task_tx, _task_rx) = mpsc::channel(1);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();

        let ack = test_control_frame_response(
            r#"{"t":"presence_frame","id":"p1","frame":{"t":"voice_log","text":"hello from connect","seq":7,"tool_context":"debug"}}"#,
            &mut rt,
            &task_tx,
            &mut pending,
            &mut outbound,
        )
        .expect("presence frame should ack when id is present");

        assert_eq!(ack["t"], "presence_ack");
        assert_eq!(ack["id"], "p1");
        assert_eq!(ack["ok"], true);

        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("voice log event should arrive")
            .expect("event bus should be open");
        match event {
            AppEvent::VoiceLog {
                text,
                seq,
                tool_context,
            } => {
                assert_eq!(text, "hello from connect");
                assert_eq!(seq, 7);
                assert_eq!(tool_context.as_deref(), Some("debug"));
            }
            other => panic!("expected VoiceLog, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn presence_frame_routes_tool_request_response() {
        let mut rt = runtime();
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        rt.control_frames_tx = Some(control_tx);
        let (task_tx, _task_rx) = mpsc::channel(1);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();

        let ack = test_control_frame_response(
            r#"{"t":"presence_frame","id":"p1","frame":{"t":"tool_request","id":"req_1","tool":"check_status","args":{}}}"#,
            &mut rt,
            &task_tx,
            &mut pending,
            &mut outbound,
        )
        .expect("presence frame should ack when id is present");

        assert_eq!(ack["t"], "presence_ack");
        assert_eq!(ack["id"], "p1");
        assert_eq!(ack["ok"], true);

        let frame = tokio::time::timeout(Duration::from_secs(1), control_rx.recv())
            .await
            .expect("tool response event should arrive")
            .expect("control frame channel should stay open");
        assert_eq!(frame["t"], "event");
        let payload = &frame["payload"];
        assert_eq!(payload["t"], "tool_response");
        assert_eq!(payload["id"], "req_1");
        assert!(!payload["result"].as_str().unwrap_or("").is_empty());
    }

    #[tokio::test]
    async fn control_frame_routes_session_control_msg_requests() {
        let mut rt = runtime();
        let mut events = rt.bus.subscribe();
        let (tx, mut rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();
        let immediate = test_control_frame_response(
            r#"{"t":"request","id":"session-ctrl-frame","method":"api_session_control_msg","params":{"message":{"action":"interrupt","session_id":"session-frame"}}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(immediate.is_none(), "session control request should spawn");

        let (_, task) = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task.id, "session-ctrl-frame");
        assert!(task.done);
        assert_eq!(task.frame["t"], "response");
        assert_eq!(task.frame["ok"], true);
        assert_eq!(task.frame["result"]["action"], "interrupt");

        let mut saw_control = false;
        for _ in 0..4 {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AppEvent::ControlCommand(ControlMsg::Interrupt { session_id, .. }) = event {
                assert_eq!(session_id.as_deref(), Some("session-frame"));
                saw_control = true;
                break;
            }
        }
        assert!(
            saw_control,
            "frame-routed session control did not reach bus"
        );
    }

    #[tokio::test]
    async fn control_frame_routes_dashboard_action_msg_requests() {
        let mut rt = runtime();
        let mut events = rt.bus.subscribe();
        let (tx, mut rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();
        let immediate = test_control_frame_response(
            r#"{"t":"request","id":"dash-action-frame","method":"api_dashboard_action_msg","params":{"message":{"action":"close_browser_workspace","workspace_id":"workspace-frame"}}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(immediate.is_none(), "dashboard action request should spawn");

        let (_, task) = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task.id, "dash-action-frame");
        assert!(task.done);
        assert_eq!(task.frame["t"], "response");
        assert_eq!(task.frame["ok"], true);
        assert_eq!(task.frame["result"]["action"], "close_browser_workspace");

        let mut saw_control = false;
        for _ in 0..4 {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            if let AppEvent::ControlCommand(ControlMsg::CloseBrowserWorkspace {
                workspace_id,
                ..
            }) = event
            {
                assert_eq!(workspace_id, "workspace-frame");
                saw_control = true;
                break;
            }
        }
        assert!(
            saw_control,
            "frame-routed dashboard action did not reach bus"
        );
    }

    #[tokio::test]
    async fn current_agent_output_without_active_log_preserves_http_status() {
        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();

        let queued = test_control_frame_response(
            r#"{"t":"request","id":"out1","method":"api_session_current_agent_output","params":{"ids":["missing-output"]}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(queued.is_none());
        assert!(pending.contains_key("out1"));

        let (_, response) = rx.recv().await.unwrap();
        assert!(pending.cancel_remove(&response.id));
        assert_eq!(response.id, "out1");
        assert!(response.done);
        assert_eq!(response.frame["t"], "response");
        assert_eq!(response.frame["ok"], true);
        assert_eq!(response.frame["result"]["error"], "no active session log");
        assert_eq!(response.frame["result"]["_httpStatus"], 404);
        assert_eq!(response.frame["result"]["_httpOk"], false);
    }

    #[tokio::test]
    async fn current_history_without_file_watcher_preserves_http_status() {
        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();

        for (idx, (method, params)) in [
            ("api_session_current_history", serde_json::json!({})),
            (
                "api_session_current_rollback",
                serde_json::json!({
                    "round_id": 1,
                    "revert_files": true,
                    "revert_conversation": false,
                }),
            ),
            ("api_session_current_redo", serde_json::json!({})),
            ("api_session_current_prune", serde_json::json!({})),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("hist{idx}");
            let frame = serde_json::json!({
                "t": "request",
                "id": id,
                "method": method,
                "params": params,
            })
            .to_string();
            let queued =
                test_control_frame_response(&frame, &mut rt, &tx, &mut pending, &mut outbound);
            assert!(queued.is_none());
            assert!(pending.contains_key(&id));

            let (_, response) = rx.recv().await.unwrap();
            assert!(pending.cancel_remove(&response.id));
            assert_eq!(response.id, id);
            assert!(response.done);
            assert_eq!(response.frame["t"], "response");
            assert_eq!(response.frame["ok"], true);
            assert_eq!(response.frame["result"]["error"], "file watcher not active");
            assert_eq!(response.frame["result"]["_httpStatus"], 503);
            assert_eq!(response.frame["result"]["_httpOk"], false);
        }
    }

    #[tokio::test]
    async fn current_changes_without_file_watcher_preserves_http_status() {
        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();

        let queued = test_control_frame_response(
            r#"{"t":"request","id":"chg1","method":"api_session_current_changes","params":{}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(queued.is_none());
        assert!(pending.contains_key("chg1"));

        let (_, response) = rx.recv().await.unwrap();
        assert!(pending.cancel_remove(&response.id));
        assert_eq!(response.id, "chg1");
        assert!(response.done);
        assert_eq!(response.frame["t"], "response");
        assert_eq!(response.frame["ok"], true);
        assert_eq!(response.frame["result"]["error"], "file watcher not active");
        assert_eq!(response.frame["result"]["_httpStatus"], 503);
        assert_eq!(response.frame["result"]["_httpOk"], false);
    }

    #[tokio::test]
    async fn fs_stat_and_list_preserve_http_status() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), b"hello").unwrap();

        let mut rt = runtime();
        let (tx, mut rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();

        for (idx, (method, path)) in [
            ("api_fs_stat", dir.path().to_string_lossy().to_string()),
            ("api_fs_list", dir.path().to_string_lossy().to_string()),
            ("api_fs_stat", "relative/path".to_string()),
            ("api_fs_mkdir", dir.path().to_string_lossy().to_string()),
            ("api_fs_mkdir", "relative/path".to_string()),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("fs{idx}");
            let is_list = method == "api_fs_list";
            let is_mkdir = method == "api_fs_mkdir";
            let is_bad_path = path == "relative/path";
            let frame = serde_json::json!({
                "t": "request",
                "id": id,
                "method": method,
                "params": { "path": path.clone() },
            })
            .to_string();
            let queued =
                test_control_frame_response(&frame, &mut rt, &tx, &mut pending, &mut outbound);
            assert!(queued.is_none());
            assert!(pending.contains_key(&id));

            let (_, response) = rx.recv().await.unwrap();
            assert!(pending.cancel_remove(&response.id));
            assert_eq!(response.id, id);
            assert!(response.done);
            assert_eq!(response.frame["t"], "response");
            assert_eq!(response.frame["ok"], true);

            if is_mkdir && is_bad_path {
                assert_eq!(response.frame["result"]["_httpStatus"], 400);
                assert_eq!(response.frame["result"]["_httpOk"], false);
                assert!(response.frame["result"]["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("path must be absolute"));
            } else if is_mkdir {
                assert_eq!(response.frame["result"]["ok"], true);
                assert_eq!(response.frame["result"]["already_exists"], true);
                assert_eq!(response.frame["result"]["_httpStatus"], 200);
                assert_eq!(response.frame["result"]["_httpOk"], true);
            } else if is_list {
                assert!(response.frame["result"]["entries"].is_array());
                assert_eq!(response.frame["result"]["_httpStatus"], 200);
                assert_eq!(response.frame["result"]["_httpOk"], true);
            } else if is_bad_path {
                assert_eq!(response.frame["result"]["_httpStatus"], 400);
                assert_eq!(response.frame["result"]["_httpOk"], false);
                assert!(response.frame["result"]["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("path must be absolute"));
            } else {
                assert_eq!(response.frame["result"]["exists"], true);
                assert_eq!(response.frame["result"]["is_dir"], true);
                assert_eq!(response.frame["result"]["_httpStatus"], 200);
                assert_eq!(response.frame["result"]["_httpOk"], true);
            }
        }
    }

    #[tokio::test]
    async fn fs_rename_and_delete_enforce_scope_on_both_legs() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let scoped_runtime = || {
            let mut rt = runtime();
            rt.grant = DashboardControlGrant::Peer {
                fingerprint: "fp".into(),
                label: "peer".into(),
                profile: "file-operator".into(),
                filesystem: crate::peer::access_policy::FilesystemAccessPolicy {
                    read_roots: vec![],
                    write_roots: vec![dir.path().to_path_buf()],
                },
                attributed: None,
                identity_record: None,
                iam_cert_dir: None,
            };
            rt
        };
        let (tx, mut rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();
        let request = |id: &str, method: &str, params: serde_json::Value| {
            serde_json::json!({
                "t": "request",
                "id": id,
                "method": method,
                "params": params,
            })
            .to_string()
        };

        // A rename whose destination leaves the write roots is refused
        // inline — before any disk IO — and the audit line names both legs.
        let from = dir.path().join("a.txt");
        std::fs::write(&from, b"payload").unwrap();
        let escape = outside.path().join("stolen.txt");
        let mut rt = scoped_runtime();
        let mut events = rt.bus.subscribe();
        let denied = test_control_frame_response(
            &request(
                "r1",
                "api_fs_rename",
                serde_json::json!({
                    "from": from.to_string_lossy(),
                    "to": escape.to_string_lossy(),
                }),
            ),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .expect("denied inline");
        assert_eq!(denied["ok"], false);
        assert!(from.exists());
        assert!(!escape.exists());
        let mut audited = false;
        while let Ok(event) = events.try_recv() {
            if let AppEvent::PresenceLog { message, .. } = event {
                if message.contains("[peer-fs] denied") && message.contains(" -> ") {
                    audited = true;
                }
            }
        }
        assert!(audited, "expected a [peer-fs] denied line naming both legs");

        // ... and so is a rename whose *source* is outside (write-scope on
        // the removal leg), and one that omits a leg entirely.
        let foreign = outside.path().join("theirs.txt");
        std::fs::write(&foreign, b"foreign").unwrap();
        let denied = test_control_frame_response(
            &request(
                "r2",
                "api_fs_rename",
                serde_json::json!({
                    "from": foreign.to_string_lossy(),
                    "to": dir.path().join("mine.txt").to_string_lossy(),
                }),
            ),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .expect("denied inline");
        assert_eq!(denied["ok"], false);
        assert!(foreign.exists());
        let denied = test_control_frame_response(
            &request(
                "r3",
                "api_fs_rename",
                serde_json::json!({ "from": from.to_string_lossy() }),
            ),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .expect("denied inline");
        assert_eq!(denied["ok"], false);
        assert!(denied["error"]
            .as_str()
            .unwrap_or_default()
            .contains("missing path"));

        // Both legs inside: the rename flows through the task path.
        let to = dir.path().join("b.txt");
        let queued = test_control_frame_response(
            &request(
                "r4",
                "api_fs_rename",
                serde_json::json!({
                    "from": from.to_string_lossy(),
                    "to": to.to_string_lossy(),
                }),
            ),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(queued.is_none());
        let (_, response) = rx.recv().await.unwrap();
        assert!(pending.cancel_remove(&response.id));
        assert_eq!(response.frame["result"]["_httpStatus"], 200);
        assert_eq!(response.frame["result"]["renamed"], true);
        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"payload");

        // Delete outside the write roots is refused; inside it lands, and a
        // non-empty directory's 409 survives the tunnel envelope.
        let denied = test_control_frame_response(
            &request(
                "d1",
                "api_fs_delete",
                serde_json::json!({ "path": foreign.to_string_lossy() }),
            ),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .expect("denied inline");
        assert_eq!(denied["ok"], false);
        assert!(foreign.exists());

        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.txt"), b"x").unwrap();
        let queued = test_control_frame_response(
            &request(
                "d2",
                "api_fs_delete",
                serde_json::json!({ "path": sub.to_string_lossy() }),
            ),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(queued.is_none());
        let (_, response) = rx.recv().await.unwrap();
        assert!(pending.cancel_remove(&response.id));
        assert_eq!(response.frame["result"]["_httpStatus"], 409);
        assert_eq!(response.frame["result"]["code"], "not_empty");
        assert!(sub.exists());

        let queued = test_control_frame_response(
            &request(
                "d3",
                "api_fs_delete",
                serde_json::json!({ "path": sub.to_string_lossy(), "recursive": true }),
            ),
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(queued.is_none());
        let (_, response) = rx.recv().await.unwrap();
        assert!(pending.cancel_remove(&response.id));
        assert_eq!(response.frame["result"]["_httpStatus"], 200);
        assert_eq!(response.frame["result"]["deleted"], true);
        assert!(!sub.exists());
    }

    #[tokio::test]
    async fn control_frame_routes_transfer_jobs_request() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let mut rt = runtime();
        rt.project_root = Some(project);
        let (tx, mut rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();

        let queued = test_control_frame_response(
            r#"{"t":"request","id":"transfer-jobs-frame","method":"api_transfer_jobs"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(queued.is_none());
        assert!(pending.contains_key("transfer-jobs-frame"));

        let (_, response) = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(pending.cancel_remove(&response.id));
        assert_eq!(response.id, "transfer-jobs-frame");
        assert!(response.done);
        assert_eq!(response.frame["t"], "response");
        assert_eq!(response.frame["ok"], true);
        assert_eq!(response.frame["result"]["ok"], true);
        assert_eq!(
            response.frame["result"]["jobs"].as_array().unwrap().len(),
            0
        );
    }

    #[test]
    fn changes_rpc_params_build_request_lines() {
        let params = serde_json::json!({
            "path": "src/file name.rs",
            "query": "session_id=abc&source=codex",
        });
        assert_eq!(
            changes_request_line(Some(&params)),
            "GET /api/session/current/changes/src%2Ffile%20name.rs?session_id=abc&source=codex HTTP/1.1"
        );

        let params = serde_json::json!({
            "path": "/tmp/a+b c",
            "query": "?backend_session_id=thread%2F1#ignored",
        });
        assert_eq!(
            changes_request_line(Some(&params)),
            "GET /api/session/current/changes/%2Ftmp%2Fa%2Bb%20c?backend_session_id=thread%2F1 HTTP/1.1"
        );

        assert_eq!(
            changes_request_line(None),
            "GET /api/session/current/changes HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn control_frames_negotiate_and_apply_response_credit() {
        let mut rt = runtime();
        let (tx, _rx) = mpsc::channel::<SequencedTaskResponse>(8);
        let mut pending = PendingControlRequests::new();
        let mut outbound = OutboundControlQueue::new();

        let hello = test_control_frame_response(
            r#"{"t":"hello","id":"h1","features":["response_credit"]}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(hello["t"], "hello_ack");
        assert!(rt.response_credit_enabled);

        let status = test_control_frame_response(
            r#"{"t":"request","id":"s1","method":"status"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(status["result"]["response_credit_enabled"], true);

        assert!(outbound.enqueue_chunked(ChunkedFramePlan::response(
            "large".into(),
            "large:0".into(),
            "start".into(),
            "end".into(),
            b"chunk".to_vec(),
            CONTROL_RESPONSE_CHUNK_BYTES,
        )));
        if let Some(QueuedControlFrame::Chunked(queued)) = outbound.frames.front_mut() {
            queued.credit = 0;
        }
        assert!(test_control_frame_response(
            r#"{"t":"credit","id":"large","chunk_id":"large:0","chunks":3}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .is_none());
        let Some(QueuedControlFrame::Chunked(queued)) = outbound.frames.front() else {
            panic!("expected queued chunked frame");
        };
        assert_eq!(queued.credit, 3);

        let cancelled = test_control_frame_response(
            r#"{"t":"cancel","id":"large"}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        )
        .unwrap();
        assert_eq!(cancelled["cancelled"], true);
        assert!(outbound.frames.is_empty());
    }

    #[test]
    fn managed_context_rpc_params_build_request_lines() {
        assert_eq!(
            managed_context_request_line(
                "records",
                &serde_json::json!({"query": "session_id=wrapper&backend_session_id=thread"})
            )
            .unwrap(),
            "GET /api/managed-context/records?session_id=wrapper&backend_session_id=thread HTTP/1.1"
        );
        assert_eq!(
            managed_context_request_line(
                "anchors",
                &serde_json::json!({
                    "session_id": "wrapper id",
                    "backend_session_id": "thread/1",
                    "intendant_session_id": "daemon+session"
                })
            )
            .unwrap(),
            "GET /api/managed-context/anchors?session_id=wrapper+id&backend_session_id=thread%2F1&intendant_session_id=daemon%2Bsession HTTP/1.1"
        );
        assert!(managed_context_request_line("fission", &serde_json::json!({})).is_none());
    }
}
