//! Daemon-scoped WebRTC control tunnel for dashboard RPC experiments.
//!
//! The dashboard still uses HTTP plus the main WebSocket by default. This
//! module provides the first substrate for a future public-origin dashboard:
//! WebSocket signaling creates a direct browser-to-daemon WebRTC data channel,
//! then the channel carries small JSON RPC frames.

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
use std::sync::atomic::{AtomicU64, Ordering};
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
const CONTROL_PROTOCOL_VERSION: u32 = 1;
const CONTROL_SIGNATURE_CONTEXT: &str = "intendant-dashboard-control-v1";
const CONTROL_DEFAULT_SESSION_LIMIT: usize = 600;
const CONTROL_RESPONSE_CHUNK_THRESHOLD_BYTES: usize = 64 * 1024;
const CONTROL_RESPONSE_CHUNK_BYTES: usize = 16 * 1024;
const CONTROL_BYTE_STREAM_CHUNK_BYTES: usize = 16 * 1024;
const CONTROL_RESPONSE_INITIAL_CHUNK_CREDIT: usize = 16;
const CONTROL_RESPONSE_MAX_CREDIT_GRANT: usize = 64;
const CONTROL_BINDING_TTL_MS: i64 = 5 * 60 * 1000;
const DASHBOARD_MEDIA_CLIP_MAX_FRAMES: usize = 1000;
static NEXT_DASHBOARD_DISPLAY_PEER_ID: AtomicU64 = AtomicU64::new(1);
/// One dashboard-control method's declared surface. The effective method
/// table (`all_control_methods`) is the single source the method authorizer
/// (`authorize_dashboard_control_method`), the advertised feature list
/// (`control_features`), the per-method `<method>_available` status booleans,
/// and the upload-frame allowlist (`authorize_dashboard_control_upload`) all
/// derive from — a method added or re-gated in one place cannot drift out of
/// sync in the others. It is the union of two declaration sources, resolved
/// rows-first: tunnel columns on `gateway_routes::ROUTES` rows (twinned
/// methods, whose IAM operation derives from the route row — transport-
/// unification design §2.2) and the residue `CONTROL_METHODS` below (methods
/// whose family has not yet ported, plus tunnel-only methods with no HTTP
/// twin). The `tunnel_method_partition_is_pinned` differential test freezes
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

/// The residue half of the tunnel method table: methods not (yet) declared
/// as a `tunnel:` column on a `gateway_routes::ROUTES` row. Do NOT add a
/// method here if its HTTP twin has a route row — declare it on the row
/// (`Route::with_tunnel`) so its IAM operation derives from the shared
/// declaration; this list is for tunnel-only methods and families the
/// migration has not reached. Never read directly outside this module's
/// union plumbing and pins — consume `all_control_methods()` /
/// `control_method_spec()`.
const CONTROL_METHODS: &[ControlMethodSpec] = &[
    ControlMethodSpec {
        name: "ping",
        op: None,
        advertised: true,
        upload: false,
    },
    method("config", PeerOperation::RuntimeControl),
    method("status", PeerOperation::PresenceRead),
    method("api_agent_card", PeerOperation::PresenceRead),
    method("api_cached_bootstrap_events", PeerOperation::SessionInspect),
    internal("subscribe_events", PeerOperation::SessionInspect),
    internal("unsubscribe_events", PeerOperation::SessionInspect),
    method("api_access_overview", PeerOperation::AccessInspect),
    method("api_access_iam_state", PeerOperation::AccessInspect),
    method("api_access_enrollment_requests", PeerOperation::AccessInspect),
    method("api_dashboard_targets", PeerOperation::AccessInspect),
    // Connect rendezvous administration. Status is inspect-grade and
    // never carries the claim phrase; the phrase reveal, config toggle,
    // and unclaim are manage-gated (mirrors the HTTP route rows).
    method("api_access_connect_status", PeerOperation::AccessInspect),
    method("api_access_connect_claim_code", PeerOperation::AccessManage),
    method("api_access_connect_config", PeerOperation::AccessManage),
    method("api_access_connect_unclaim", PeerOperation::AccessManage),
    // Trust-tier settings (docs/src/trust-tiers.md): the tier label and
    // the hosted-control ceiling knob (mirrors the HTTP route rows).
    method("api_access_set_tier", PeerOperation::AccessManage),
    method("api_access_set_hosted_ceiling", PeerOperation::AccessManage),
    // Fleet certificate: publish this daemon's addresses for its fleet
    // name and run the ACME DNS-01 order (fleet_cert.rs). Slow flow,
    // started async; progress rides the connect status payload.
    method("api_fleet_cert_request", PeerOperation::AccessManage),
    // Credential custody (vault leases + client egress): granting, renewing,
    // revoking, and even reading lease status all sit behind the dedicated
    // gate — a scoped guest session can neither fuel nor drain a daemon, nor
    // see which providers are fueled. Raw egress_* relay frames are a
    // separate wire family and deliberately not methods here.
    method("api_credential_lease_grant", PeerOperation::CredentialsManage),
    method("api_credential_lease_renew", PeerOperation::CredentialsManage),
    method("api_credential_lease_revoke", PeerOperation::CredentialsManage),
    method("api_credential_lease_status", PeerOperation::CredentialsManage),
    method("api_credential_custody_trail", PeerOperation::CredentialsManage),
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
    method("api_daemon_vault_deposit_key_fetch", PeerOperation::CredentialsManage),
    method("api_daemon_vault_deposit_key_publish", PeerOperation::CredentialsManage),
    method("api_daemon_vault_deposits_fetch", PeerOperation::CredentialsManage),
    method("api_daemon_vault_deposits_consume", PeerOperation::CredentialsManage),
    method(
        "api_credential_egress_register",
        PeerOperation::CredentialsManage,
    ),
    method(
        "api_credential_egress_unregister",
        PeerOperation::CredentialsManage,
    ),
    method("api_credential_egress_probe", PeerOperation::CredentialsManage),
    method(
        "api_access_iam_upsert_user_client_grant",
        PeerOperation::AccessManage,
    ),
    method("api_access_iam_update_grant", PeerOperation::AccessManage),
    method("api_access_enrollment_decide", PeerOperation::AccessManage),
    method("api_access_org_trust", PeerOperation::AccessManage),
    method("api_access_org_revoke", PeerOperation::AccessManage),
    method("api_access_org_issue", PeerOperation::AccessManage),
    method("api_access_org_revoke_member", PeerOperation::AccessManage),
    method("api_access_org_issuer_init", PeerOperation::AccessManage),
    method("api_access_org_issuer_delegate", PeerOperation::AccessManage),
    method("api_access_org_issuer_install", PeerOperation::AccessManage),
    // Presenting a signed org document (or list) only requires a session;
    // the document itself is the authorization and is fully re-verified.
    // Same for reading the org's public revocation list and renewing a
    // still-valid document.
    method("api_access_org_present", PeerOperation::AccessInspect),
    method("api_access_org_orl", PeerOperation::AccessInspect),
    method("api_access_org_renew", PeerOperation::AccessInspect),
    // Applying a root-signed revocation list mirrors a PUBLIC doorbell
    // (`POST /api/access/orgs/revocations/apply`): the signature is the
    // authority, so any session may courier one through the tunnel.
    method("api_access_org_orl_apply", PeerOperation::PresenceRead),
    method("api_peer_pairing_requests", PeerOperation::AccessInspect),
    method("api_peer_pairing_identities", PeerOperation::AccessInspect),
    method(
        "api_peer_pairing_request_decision",
        PeerOperation::AccessManage,
    ),
    method(
        "api_peer_pairing_identity_revoke",
        PeerOperation::AccessManage,
    ),
    method("api_peer_pairing_invite", PeerOperation::AccessManage),
    method("api_peers", PeerOperation::PeerInspect),
    method("api_peer_eligible", PeerOperation::PeerInspect),
    // Acting through a connected peer — signaling relays that open tunnels,
    // and the message/task/approval quick controls — is peer use, not peer
    // administration: the receiving peer authorizes each action against its
    // own grants for this daemon. Mirrors the HTTP lane's
    // `federation_http_operation`.
    method("api_peer_webrtc_signal", PeerOperation::PeerUse),
    method("api_peer_file_transfer_signal", PeerOperation::PeerUse),
    method("api_peer_dashboard_control_signal", PeerOperation::PeerUse),
    method("api_peer_message", PeerOperation::PeerUse),
    method("api_peer_task", PeerOperation::PeerUse),
    method("api_peer_approval", PeerOperation::PeerUse),
    method("api_peer_add", PeerOperation::PeerManage),
    method("api_peer_remove", PeerOperation::PeerManage),
    method("api_peer_pairing_join", PeerOperation::PeerManage),
    method("api_peer_pairing_request_access", PeerOperation::PeerManage),
    method(
        "api_peer_pairing_request_access_poll",
        PeerOperation::PeerManage,
    ),
    method("api_coordinator_route", PeerOperation::PeerManage),
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
    // prune, changes, agent-output, uploads list/raw/delete, and the
    // upload-frame-only api_session_current_upload (S4c).
    method("api_sessions_stream", PeerOperation::SessionInspect),
    method("api_session_control_msg", PeerOperation::SessionManage),
    method("api_transfer_jobs", PeerOperation::FilesystemRead),
    method("api_transfer_download_read", PeerOperation::FilesystemRead),
    // The api_fs_* methods live as tunnel columns on their route rows
    // (gateway_routes::ROUTES, /api/fs/*) — the first family whose tunnel
    // ops derive from the rows instead of entries here. The api_transfer_*
    // methods join them when their HTTP rows land (design §4, task #6).
    method("api_transfer_job_create", PeerOperation::FilesystemWrite),
    method("api_transfer_job_delete", PeerOperation::FilesystemWrite),
    // Transfer chunks arrive only as upload frames; their destination was
    // path-scoped when the transfer job was created, so the chunk itself
    // only needs the write operation (`authorize_dashboard_control_upload`).
    uploadable("api_transfer_upload_chunk", PeerOperation::FilesystemWrite),
    method("api_transfer_upload_commit", PeerOperation::FilesystemWrite),
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
    method("api_browser_workspace_snapshot", PeerOperation::SessionInspect),
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
/// `Route::tunnel_operation`), then the residue `CONTROL_METHODS`.
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
        methods.extend_from_slice(CONTROL_METHODS);
        methods
    })
}

fn control_method_spec(method: &str) -> Option<&'static ControlMethodSpec> {
    all_control_methods()
        .iter()
        .find(|spec| spec.name == method)
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
/// effective table (route-row tunnel specs ∪ the `CONTROL_METHODS`
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
        "api_settings_save" => runtime.project_root.is_some(),
        "api_access_connect_config" | "api_access_connect_unclaim" => {
            runtime.project_root.is_some()
        }
        "api_mcp_tool_call" => runtime.mcp_server.is_some(),
        method if method.starts_with("api_transfer_") => runtime.project_root.is_some(),
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
    identity: Mutex<Option<Arc<DaemonIdentity>>>,
    peers: Mutex<HashMap<String, DashboardControlPeer>>,
}

#[derive(Clone, Debug, Default)]
pub enum DashboardControlGrant {
    #[default]
    TrustedLocal,
    UserClientRoot {
        principal: crate::access::iam::AccessPrincipal,
    },
    UserClient {
        principal: crate::access::iam::AccessPrincipal,
        iam_state: crate::access::iam::LocalIamState,
    },
    Peer {
        fingerprint: String,
        label: String,
        profile: String,
        filesystem: crate::peer::access_policy::FilesystemAccessPolicy,
    },
}

impl DashboardControlGrant {
    fn label(&self) -> &str {
        match self {
            Self::TrustedLocal => "trusted-local",
            Self::UserClientRoot { principal } => principal.label.as_str(),
            Self::UserClient { principal, .. } => principal.label.as_str(),
            Self::Peer { label, .. } => label.as_str(),
        }
    }

    fn profile(&self) -> Option<&str> {
        match self {
            Self::TrustedLocal | Self::UserClientRoot { .. } | Self::UserClient { .. } => None,
            Self::Peer { profile, .. } => Some(profile.as_str()),
        }
    }

    pub(crate) fn filesystem(&self) -> Option<&crate::peer::access_policy::FilesystemAccessPolicy> {
        match self {
            // TrustedLocal is the owner's own dashboard; a root client key
            // is equivalent. Scoping applies to granted principals.
            Self::TrustedLocal | Self::UserClientRoot { .. } => None,
            Self::UserClient {
                principal,
                iam_state,
            } => crate::access::iam::fs_scope_for_principal(iam_state, principal),
            Self::Peer { filesystem, .. } => Some(filesystem),
        }
    }

    fn access_principal(&self) -> crate::access::iam::AccessPrincipal {
        match self {
            Self::TrustedLocal => crate::access::iam::AccessPrincipal::root_dashboard_session(
                "dashboard-control",
                "webrtc-datachannel",
            ),
            Self::UserClientRoot { principal } => principal.clone(),
            Self::UserClient { principal, .. } => principal.clone(),
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
    /// their IAM snapshot's `hosted_origins`; the root-equivalent
    /// variants classify against the compiled default list.
    pub(crate) fn custody_origin_class(&self) -> &'static str {
        match self {
            Self::TrustedLocal => "local",
            Self::UserClientRoot { principal } => crate::access::iam::session_origin_class(
                &crate::access::iam::default_hosted_origins(),
                principal,
            ),
            Self::UserClient {
                principal,
                iam_state,
            } => crate::access::iam::session_origin_class(&iam_state.hosted_origins, principal),
            Self::Peer { .. } => "peer",
        }
    }

    /// The terminal actor lane for this connection: root-equivalent
    /// grants (trusted local, unbound mTLS root) own the root lane and
    /// see every shell session; everyone else acts as their principal id
    /// and sees only owned or shared sessions.
    pub(crate) fn terminal_actor(&self) -> crate::terminal::TerminalActor {
        let principal = self.access_principal();
        if principal.kind == "root_session" {
            crate::terminal::TerminalActor::Root
        } else {
            crate::terminal::TerminalActor::Principal(principal.id)
        }
    }

    pub(crate) fn access_decision(
        &self,
        op: crate::peer::access_policy::PeerOperation,
    ) -> crate::access::iam::AccessDecision {
        match self {
            Self::UserClient {
                principal,
                iam_state,
            } => crate::access::iam::evaluate_principal_operation_with_state(
                iam_state, principal, op,
            ),
            _ => crate::access::iam::evaluate_principal_operation(&self.access_principal(), op),
        }
    }

    pub(crate) fn wire_kind(&self) -> &'static str {
        match self {
            Self::TrustedLocal => "trusted-local",
            Self::UserClientRoot { .. } => "user-client-root",
            Self::UserClient { .. } => "user-client",
            Self::Peer { .. } => "peer",
        }
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

#[derive(Clone)]
pub struct DashboardDisplayAuthorityBridge {
    snapshot: Arc<dyn Fn(&str, &[u32]) -> Vec<serde_json::Value> + Send + Sync>,
    state_frame: Arc<dyn Fn(&str, u32) -> Option<serde_json::Value> + Send + Sync>,
    request: Arc<dyn Fn(&str, u32) -> Vec<serde_json::Value> + Send + Sync>,
    release: Arc<dyn Fn(&str, u32) -> Vec<serde_json::Value> + Send + Sync>,
    input_authorized: Arc<dyn Fn(&str, u32) -> bool + Send + Sync>,
    cleanup: Arc<dyn Fn(&str) + Send + Sync>,
    subscribe: Arc<dyn Fn() -> tokio::sync::broadcast::Receiver<u32> + Send + Sync>,
}

impl DashboardDisplayAuthorityBridge {
    pub fn new(
        snapshot: impl Fn(&str, &[u32]) -> Vec<serde_json::Value> + Send + Sync + 'static,
        state_frame: impl Fn(&str, u32) -> Option<serde_json::Value> + Send + Sync + 'static,
        request: impl Fn(&str, u32) -> Vec<serde_json::Value> + Send + Sync + 'static,
        release: impl Fn(&str, u32) -> Vec<serde_json::Value> + Send + Sync + 'static,
        input_authorized: impl Fn(&str, u32) -> bool + Send + Sync + 'static,
        cleanup: impl Fn(&str) + Send + Sync + 'static,
        subscribe: impl Fn() -> tokio::sync::broadcast::Receiver<u32> + Send + Sync + 'static,
    ) -> Self {
        Self {
            snapshot: Arc::new(snapshot),
            state_frame: Arc::new(state_frame),
            request: Arc::new(request),
            release: Arc::new(release),
            input_authorized: Arc::new(input_authorized),
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

    fn request(&self, session_id: &str, display_id: u32) -> Vec<serde_json::Value> {
        (self.request)(session_id, display_id)
    }

    fn release(&self, session_id: &str, display_id: u32) -> Vec<serde_json::Value> {
        (self.release)(session_id, display_id)
    }

    fn input_authorized(&self, session_id: &str, display_id: u32) -> bool {
        (self.input_authorized)(session_id, display_id)
    }

    fn cleanup(&self, session_id: &str) {
        (self.cleanup)(session_id)
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<u32> {
        (self.subscribe)()
    }
}

impl DashboardControlRegistry {
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
            identity: Mutex::new(None),
            peers: Mutex::new(HashMap::new()),
        }
    }

    #[allow(dead_code)]
    pub async fn answer_offer(
        &self,
        offer_sdp: String,
        session_grant: Option<String>,
        client_nonce: Option<String>,
    ) -> Result<DashboardControlAnswer, String> {
        self.answer_offer_with_grant(
            offer_sdp,
            session_grant,
            client_nonce,
            DashboardControlGrant::TrustedLocal,
        )
        .await
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
        )
        .await
        .map_err(|e| e.to_string())?;
        self.peers.lock().await.insert(session_id.clone(), peer);
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

    pub async fn close(&self, session_id: &str) {
        if let Some(bridge) = &self.display_authority {
            bridge.cleanup(session_id);
        }
        if let Some(bridge) = &self.presence {
            bridge.cleanup(session_id.to_string()).await;
        }
        if let Some(peer) = self.peers.lock().await.remove(session_id) {
            peer.close().await;
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
}

impl DashboardControlPeer {
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
    ) -> Result<(Self, String, DashboardControlBinding), CallerError> {
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
        let runtime = ControlRuntime {
            session_id,
            daemon_public_key: identity.public_key_b64u(),
            created_unix_ms: binding.created_unix_ms,
            events_subscribed: false,
            events_sent: 0,
            response_credit_enabled: false,
            config: serde_json::to_value(config).unwrap_or_else(|_| serde_json::json!({})),
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
            tcp_advertised,
            media_clip_ops: Arc::new(Mutex::new(HashMap::new())),
            control_frames_tx: None,
            display_peer_id: NEXT_DASHBOARD_DISPLAY_PEER_ID.fetch_add(1, Ordering::Relaxed),
            grant,
        };
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL);
        let shutdown = CancellationToken::new();
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
    config: serde_json::Value,
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
    /// The ICE-TCP tuple this control session itself advertised (the
    /// rendezvous-observed public address on the gateway port for hosted
    /// Connect sessions; `None` for locally-signaled sessions, whose
    /// browsers signal displays over the gateway WS instead). Display
    /// offers arriving on the control channel advertise the same tuple —
    /// the browser reached us through it, so display traffic can too.
    tcp_advertised: Option<SocketAddr>,
    media_clip_ops: Arc<Mutex<HashMap<String, DashboardMediaClipOperation>>>,
    control_frames_tx: Option<mpsc::UnboundedSender<serde_json::Value>>,
    display_peer_id: crate::display::PeerId,
    grant: DashboardControlGrant,
}

#[derive(Debug)]
pub(crate) struct DashboardMediaClipOperation {
    stream: String,
    note: String,
    inject: bool,
    in_secs: f64,
    out_secs: f64,
    fps: u32,
    expected_frames: usize,
    frames: Vec<(String, String)>,
}

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

pub(crate) struct ControlByteStream {
    id: String,
    stream_id: String,
    content_type: String,
    filename: Option<String>,
    bytes: Vec<u8>,
    result: serde_json::Value,
}

pub(crate) struct InboundUploadState {
    method: String,
    params: serde_json::Value,
    tmp: tempfile::NamedTempFile,
    total_bytes: usize,
    expected_chunks: usize,
    next_seq: usize,
    received_bytes: usize,
}

pub(crate) struct OutboundControlQueue {
    frames: VecDeque<QueuedControlFrame>,
}

enum QueuedControlFrame {
    Immediate { request_id: String, text: String },
    Chunked(QueuedChunkedFrame),
}

struct QueuedChunkedFrame {
    request_id: String,
    chunk_id: String,
    start: String,
    chunks: Vec<String>,
    end: String,
    next_chunk: usize,
    credit: usize,
    started: bool,
}

pub(crate) enum ControlFrameTexts {
    Immediate(Vec<String>),
    Chunked {
        request_id: String,
        chunk_id: String,
        start: String,
        chunks: Vec<String>,
        end: String,
    },
}

impl OutboundControlQueue {
    fn new() -> Self {
        Self {
            frames: VecDeque::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    fn enqueue_immediate(&mut self, request_id: String, text: String) {
        self.frames
            .push_back(QueuedControlFrame::Immediate { request_id, text });
    }

    fn enqueue_chunked(
        &mut self,
        request_id: String,
        chunk_id: String,
        start: String,
        chunks: Vec<String>,
        end: String,
    ) {
        self.cancel_chunk(&chunk_id);
        self.frames
            .push_back(QueuedControlFrame::Chunked(QueuedChunkedFrame {
                request_id,
                chunk_id,
                start,
                chunks,
                end,
                next_chunk: 0,
                credit: CONTROL_RESPONSE_INITIAL_CHUNK_CREDIT,
                started: false,
            }));
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
            let matches_chunk = chunk_id.map(|id| queued.chunk_id == id).unwrap_or(false);
            if matches_chunk || (chunk_id.is_none() && queued.request_id == request_id) {
                queued.credit = queued.credit.saturating_add(granted);
            }
        }
    }

    fn cancel(&mut self, request_id: &str) -> bool {
        let before = self.frames.len();
        self.frames.retain(|frame| match frame {
            QueuedControlFrame::Immediate {
                request_id: queued_id,
                ..
            } => queued_id != request_id,
            QueuedControlFrame::Chunked(queued) => {
                queued.request_id != request_id && queued.chunk_id != request_id
            }
        });
        self.frames.len() != before
    }

    fn cancel_chunk(&mut self, chunk_id: &str) -> bool {
        let before = self.frames.len();
        self.frames.retain(|frame| match frame {
            QueuedControlFrame::Immediate { .. } => true,
            QueuedControlFrame::Chunked(queued) => queued.chunk_id != chunk_id,
        });
        self.frames.len() != before
    }
}

#[cfg(test)]
mod fs_scope_grant_tests {
    use super::*;

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
            iam_state: state.clone(),
        };
        let scope = scoped.filesystem().expect("scoped grant exposes fs scope");
        assert_eq!(scope.read_roots, vec![std::path::PathBuf::from(srv_shared)]);

        // Owner surfaces stay unrestricted.
        assert!(DashboardControlGrant::TrustedLocal.filesystem().is_none());
        let unscoped_principal = crate::access::iam::AccessPrincipal::root_dashboard_session(
            "root-key",
            "dashboard-control",
        );
        assert!(DashboardControlGrant::UserClientRoot {
            principal: unscoped_principal
        }
        .filesystem()
        .is_none());
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

fn dashboard_control_frame_operation(t: &str) -> Option<crate::peer::access_policy::PeerOperation> {
    use crate::peer::access_policy::PeerOperation;
    match t {
        "display_input" => Some(PeerOperation::DisplayInput),
        // Floor operations: terminal_open may additionally require
        // shell.spawn (when the session doesn't exist yet) and every
        // terminal frame is scoped to sessions the actor can see — both
        // enforced statefully in the frame handlers.
        "terminal_open" => Some(PeerOperation::TerminalView),
        "terminal_input" | "terminal_resize" | "terminal_close" | "terminal_share" => {
            Some(PeerOperation::TerminalWrite)
        }
        "presence_frame" => Some(PeerOperation::Message),
        // Upload frames carry no blanket authority: upload_start is
        // authorized by the operation of the method it delivers (a media
        // annotation is runtime control, a transfer chunk is a filesystem
        // write, …) inside control_upload_start_frame, and chunk/end only
        // act on an entry an authorized start created on this connection.
        "upload_start" | "upload_chunk" | "upload_end" => None,
        // Client-egress response frames: only a session that could have
        // registered as a relay (credentials.manage) may answer, and the
        // handler additionally binds each frame to the request's own
        // registering session.
        "egress_response" | "egress_chunk" | "egress_end" | "egress_error" => {
            Some(PeerOperation::CredentialsManage)
        }
        _ => None,
    }
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

fn authorize_dashboard_control_filesystem(
    runtime: &ControlRuntime,
    method: &str,
    op: crate::peer::access_policy::PeerOperation,
    params: Option<&serde_json::Value>,
) -> Result<(), String> {
    use crate::peer::access_policy::{FilesystemAccessKind, PeerOperation};
    let kind = match op {
        PeerOperation::FilesystemRead => FilesystemAccessKind::Read,
        PeerOperation::FilesystemWrite => FilesystemAccessKind::Write,
        _ => return Ok(()),
    };
    let Some(policy) = runtime.grant.filesystem() else {
        return Ok(());
    };
    let raw_paths = dashboard_control_filesystem_paths(method, params);
    // Fail closed on missing params: a rename that names only one leg must
    // not slip past the scope check and let the handler report a plain 400.
    if raw_paths.is_empty() || (method == "api_fs_rename" && raw_paths.len() != 2) {
        return Err("filesystem request missing path".to_string());
    }
    for raw_path in &raw_paths {
        let path = crate::web_gateway::expand_dashboard_fs_path(raw_path)?;
        crate::peer::access_policy::filesystem_access_allowed(policy, kind, &path)?;
    }
    Ok(())
}

fn authorize_dashboard_control_method(
    runtime: &ControlRuntime,
    method: &str,
    params: Option<&serde_json::Value>,
) -> Result<(), String> {
    // Fail closed: a method must be declared — as a route row's tunnel
    // column or a residue `CONTROL_METHODS` entry — to be callable at
    // all; a dispatch arm added without a declaration is denied here
    // instead of shipping ungated.
    let Some(spec) = control_method_spec(method) else {
        return Err(format!("unknown dashboard-control method: {method}"));
    };
    let Some(op) = spec.op else {
        return Ok(());
    };
    let result = runtime_operation_decision(runtime, op)
        .ensure_allowed()
        .map_err(|reason| format!("dashboard-control method {method} is not allowed: {reason}"))
        .and_then(|()| authorize_dashboard_control_filesystem(runtime, method, op, params));
    audit_dashboard_control_filesystem(runtime, method, op, params, &result);
    result
}

/// Audit twin of the HTTP lane's `[peer-fs]` / `[grant-fs]` lines
/// (`web_gateway::audit_peer_filesystem_access`) for filesystem methods that
/// arrive over the dashboard-control tunnel, so both transports leave the
/// same trail: peer grants log allow and deny, other grants log denials.
fn audit_dashboard_control_filesystem(
    runtime: &ControlRuntime,
    method: &str,
    op: crate::peer::access_policy::PeerOperation,
    params: Option<&serde_json::Value>,
    result: &Result<(), String>,
) {
    use crate::peer::access_policy::PeerOperation;
    if !matches!(
        op,
        PeerOperation::FilesystemRead | PeerOperation::FilesystemWrite
    ) {
        return;
    }
    let path = dashboard_control_filesystem_paths(method, params).join(" -> ");
    match &runtime.grant {
        DashboardControlGrant::Peer {
            fingerprint,
            label,
            profile,
            ..
        } => {
            let (allowed, detail) = match result {
                Ok(()) => (true, "allowed".to_string()),
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
/// filesystem grant. Transfer chunks skip path scoping: their destination
/// was scoped when the transfer job was created and the chunk only names
/// that job.
fn authorize_dashboard_control_upload(
    runtime: &ControlRuntime,
    method: &str,
) -> Result<(), String> {
    // Fail closed twice over: the method must be declared upload-deliverable
    // (route-row tunnel column or residue `CONTROL_METHODS` entry), and
    // upload methods are always operation-gated.
    let Some(op) = control_method_spec(method)
        .filter(|spec| spec.upload)
        .and_then(|spec| spec.op)
    else {
        return Err(format!("unknown upload method: {method}"));
    };
    runtime_operation_decision(runtime, op)
        .ensure_allowed()
        .map_err(|reason| format!("dashboard-control upload {method} is not allowed: {reason}"))
}

fn authorize_dashboard_control_frame(
    runtime: &ControlRuntime,
    frame_type: &str,
) -> Result<(), String> {
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
    serde_json::to_string(&params.cloned().unwrap_or_else(|| serde_json::json!({})))
        .unwrap_or_else(|_| "{}".to_string())
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
    let result = tokio::task::spawn_blocking(move || {
        crate::web_gateway::sessions_list_api_response(ids_filter, limit, usage_view)
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
    // Historical result-shape guard: the list must be a JSON array.
    match serde_json::from_str::<serde_json::Value>(&body.into_string()) {
        Ok(result) if result.is_array() => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": true,
            "result": result,
        }),
        _ => serde_json::json!({
            "t": "response",
            "id": id,
            "ok": false,
            "error": "session list returned invalid JSON",
        }),
    }
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

fn string_param(params: &serde_json::Value, names: &[&str]) -> String {
    for name in names {
        if let Some(value) = params.get(*name) {
            if let Some(text) = value.as_str() {
                return text.trim().to_string();
            }
            if !value.is_null() {
                return value.to_string();
            }
        }
    }
    String::new()
}

fn optional_string_param(params: &serde_json::Value, names: &[&str]) -> Option<String> {
    let value = string_param(params, names);
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn optional_u64_param(params: &serde_json::Value, names: &[&str]) -> Result<Option<u64>, String> {
    for name in names {
        let Some(value) = params.get(*name) else {
            continue;
        };
        if value.is_null() {
            return Ok(None);
        }
        if let Some(number) = value.as_u64() {
            return Ok(Some(number));
        }
        if let Some(text) = value.as_str() {
            let text = text.trim();
            if text.is_empty() {
                return Ok(None);
            }
            return text
                .parse::<u64>()
                .map(Some)
                .map_err(|_| format!("invalid {name}"));
        }
        return Err(format!("invalid {name}"));
    }
    Ok(None)
}

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

    pub(crate) fn test_upload_state(
        method: &str,
        params: serde_json::Value,
        bytes: &[u8],
    ) -> InboundUploadState {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.as_file_mut().write_all(bytes).unwrap();
        tmp.as_file_mut().flush().unwrap();
        InboundUploadState {
            method: method.to_string(),
            params,
            tmp,
            total_bytes: bytes.len(),
            expected_chunks: if bytes.is_empty() { 0 } else { 1 },
            next_seq: if bytes.is_empty() { 0 } else { 1 },
            received_bytes: bytes.len(),
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
            config: serde_json::json!({"provider":"openai"}),
            agent_card: serde_json::json!({
                "id": "intendant:test-daemon",
                "label": "test-daemon",
            }),
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
            display_peer_id: 1,
            grant: DashboardControlGrant::TrustedLocal,
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
            iam_state,
        }
    }

    fn test_control_frame_response(
        text: &str,
        runtime: &mut ControlRuntime,
        task_tx: &mpsc::Sender<ControlTaskResponse>,
        pending_requests: &mut HashMap<String, CancellationToken>,
        outbound_queue: &mut OutboundControlQueue,
    ) -> Option<serde_json::Value> {
        let mut inbound_uploads = HashMap::new();
        let (terminal_tx, _terminal_rx) = mpsc::unbounded_channel();
        let mut terminal_forwarders = HashMap::new();
        control_frame_response(
            text,
            runtime,
            task_tx,
            pending_requests,
            outbound_queue,
            &mut inbound_uploads,
            &terminal_tx,
            &mut terminal_forwarders,
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
        let (tx, _rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let mut peer_root = runtime();
        peer_root.grant = DashboardControlGrant::Peer {
            fingerprint: "fingerprint".into(),
            label: "peer-root".into(),
            profile: "peer-root".into(),
            filesystem: crate::peer::access_policy::FilesystemAccessPolicy::default(),
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
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
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
        assert_eq!(status["result"]["api_transfer_jobs_available"], false);
        assert_eq!(status["result"]["api_transfer_job_create_available"], false);
        assert_eq!(status["result"]["api_transfer_job_delete_available"], false);
        assert_eq!(
            status["result"]["api_transfer_download_read_available"],
            false
        );
        assert_eq!(
            status["result"]["api_transfer_upload_chunk_available"],
            false
        );
        assert_eq!(
            status["result"]["api_transfer_upload_commit_available"],
            false
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
        assert_eq!(status["result"]["api_settings_save_available"], false);
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
        let project_root = rx.recv().await.unwrap();
        assert!(pending.remove(&project_root.id).is_some());
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
        assert!(pending.get("q1").is_none());
    }

    /// The operation a method's declaration carries (route-row tunnel
    /// column or residue `CONTROL_METHODS` entry — the effective table).
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
    fn control_method_table_is_coherent() {
        // Coherence holds over the effective union (route-row tunnel
        // specs ∪ the CONTROL_METHODS residue): a name declared on both
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

    /// Transport-unification S3 differential pin (design §8, risks
    /// R2/R4): the complete tunnel-method partition — every wire method
    /// name, the declaration source that carries it (`Row` = a `tunnel:`
    /// column on a `gateway_routes::ROUTES` row, `Residue` = a
    /// `CONTROL_METHODS` entry), and the IAM operation gating it —
    /// frozen as a literal table. Re-gating a method (operation change
    /// on either side), losing one (gone from both sources), duplicating
    /// one (declared on both), or moving one between sources fails here
    /// until this table is updated in the same change, deliberately.
    /// Permanent program infrastructure: the migration stages move
    /// families from `Residue` to `Row` by flipping tags here alongside
    /// their declarations, and the removal stage shrinks the residue to
    /// the tunnel-only set — the union below never changes by accident.
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
            ("config", Residue, Some(Op::RuntimeControl)),
            ("status", Residue, Some(Op::PresenceRead)),
            ("api_agent_card", Residue, Some(Op::PresenceRead)),
            (
                "api_cached_bootstrap_events",
                Residue,
                Some(Op::SessionInspect),
            ),
            ("subscribe_events", Residue, Some(Op::SessionInspect)),
            ("unsubscribe_events", Residue, Some(Op::SessionInspect)),
            ("api_access_overview", Residue, Some(Op::AccessInspect)),
            ("api_access_iam_state", Residue, Some(Op::AccessInspect)),
            (
                "api_access_enrollment_requests",
                Residue,
                Some(Op::AccessInspect),
            ),
            ("api_dashboard_targets", Residue, Some(Op::AccessInspect)),
            (
                "api_access_connect_status",
                Residue,
                Some(Op::AccessInspect),
            ),
            (
                "api_access_connect_claim_code",
                Residue,
                Some(Op::AccessManage),
            ),
            ("api_access_connect_config", Residue, Some(Op::AccessManage)),
            (
                "api_access_connect_unclaim",
                Residue,
                Some(Op::AccessManage),
            ),
            ("api_access_set_tier", Residue, Some(Op::AccessManage)),
            (
                "api_access_set_hosted_ceiling",
                Residue,
                Some(Op::AccessManage),
            ),
            ("api_fleet_cert_request", Residue, Some(Op::AccessManage)),
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
            (
                "api_access_iam_upsert_user_client_grant",
                Residue,
                Some(Op::AccessManage),
            ),
            (
                "api_access_iam_update_grant",
                Residue,
                Some(Op::AccessManage),
            ),
            (
                "api_access_enrollment_decide",
                Residue,
                Some(Op::AccessManage),
            ),
            ("api_access_org_trust", Residue, Some(Op::AccessManage)),
            ("api_access_org_revoke", Residue, Some(Op::AccessManage)),
            ("api_access_org_issue", Residue, Some(Op::AccessManage)),
            (
                "api_access_org_revoke_member",
                Residue,
                Some(Op::AccessManage),
            ),
            (
                "api_access_org_issuer_init",
                Residue,
                Some(Op::AccessManage),
            ),
            (
                "api_access_org_issuer_delegate",
                Residue,
                Some(Op::AccessManage),
            ),
            (
                "api_access_org_issuer_install",
                Residue,
                Some(Op::AccessManage),
            ),
            ("api_access_org_present", Residue, Some(Op::AccessInspect)),
            ("api_access_org_orl", Residue, Some(Op::AccessInspect)),
            ("api_access_org_renew", Residue, Some(Op::AccessInspect)),
            ("api_access_org_orl_apply", Residue, Some(Op::PresenceRead)),
            (
                "api_peer_pairing_requests",
                Residue,
                Some(Op::AccessInspect),
            ),
            (
                "api_peer_pairing_identities",
                Residue,
                Some(Op::AccessInspect),
            ),
            (
                "api_peer_pairing_request_decision",
                Residue,
                Some(Op::AccessManage),
            ),
            (
                "api_peer_pairing_identity_revoke",
                Residue,
                Some(Op::AccessManage),
            ),
            ("api_peer_pairing_invite", Residue, Some(Op::AccessManage)),
            ("api_peers", Residue, Some(Op::PeerInspect)),
            ("api_peer_eligible", Residue, Some(Op::PeerInspect)),
            ("api_peer_webrtc_signal", Residue, Some(Op::PeerUse)),
            ("api_peer_file_transfer_signal", Residue, Some(Op::PeerUse)),
            (
                "api_peer_dashboard_control_signal",
                Residue,
                Some(Op::PeerUse),
            ),
            ("api_peer_message", Residue, Some(Op::PeerUse)),
            ("api_peer_task", Residue, Some(Op::PeerUse)),
            ("api_peer_approval", Residue, Some(Op::PeerUse)),
            ("api_peer_add", Residue, Some(Op::PeerManage)),
            ("api_peer_remove", Residue, Some(Op::PeerManage)),
            ("api_peer_pairing_join", Residue, Some(Op::PeerManage)),
            (
                "api_peer_pairing_request_access",
                Residue,
                Some(Op::PeerManage),
            ),
            (
                "api_peer_pairing_request_access_poll",
                Residue,
                Some(Op::PeerManage),
            ),
            ("api_coordinator_route", Residue, Some(Op::PeerManage)),
            ("api_sessions", Row, Some(Op::SessionInspect)),
            ("api_sessions_stream", Residue, Some(Op::SessionInspect)),
            ("api_session_detail", Row, Some(Op::SessionInspect)),
            ("api_session_report", Row, Some(Op::SessionInspect)),
            ("api_session_agent_output", Row, Some(Op::SessionInspect)),
            (
                "api_session_context_snapshot",
                Row,
                Some(Op::SessionInspect),
            ),
            ("api_sessions_search", Row, Some(Op::SessionInspect)),
            ("api_session_recordings", Row, Some(Op::SessionInspect)),
            (
                "api_session_recording_asset",
                Row,
                Some(Op::SessionInspect),
            ),
            ("api_session_frame_asset", Row, Some(Op::SessionInspect)),
            ("api_worktrees", Row, Some(Op::SessionInspect)),
            ("api_worktrees_inspect", Row, Some(Op::SessionInspect)),
            ("api_session_delete", Row, Some(Op::SessionManage)),
            (
                "api_session_current_history",
                Row,
                Some(Op::SessionManage),
            ),
            (
                "api_session_current_rollback",
                Row,
                Some(Op::SessionManage),
            ),
            ("api_session_current_redo", Row, Some(Op::SessionManage)),
            (
                "api_session_current_prune",
                Row,
                Some(Op::SessionManage),
            ),
            (
                "api_session_current_changes",
                Row,
                Some(Op::SessionManage),
            ),
            (
                "api_session_current_uploads",
                Row,
                Some(Op::SessionManage),
            ),
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
            ("api_worktrees_merge", Row, Some(Op::SessionManage)),
            (
                "api_session_current_upload",
                Row,
                Some(Op::SessionManage),
            ),
            ("api_transfer_jobs", Residue, Some(Op::FilesystemRead)),
            (
                "api_transfer_download_read",
                Residue,
                Some(Op::FilesystemRead),
            ),
            ("api_fs_stat", Row, Some(Op::FilesystemRead)),
            ("api_fs_list", Row, Some(Op::FilesystemRead)),
            ("api_fs_read", Row, Some(Op::FilesystemRead)),
            (
                "api_transfer_job_create",
                Residue,
                Some(Op::FilesystemWrite),
            ),
            (
                "api_transfer_job_delete",
                Residue,
                Some(Op::FilesystemWrite),
            ),
            (
                "api_transfer_upload_chunk",
                Residue,
                Some(Op::FilesystemWrite),
            ),
            (
                "api_transfer_upload_commit",
                Residue,
                Some(Op::FilesystemWrite),
            ),
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
            (
                "api_managed_context_records",
                Row,
                Some(Op::SessionInspect),
            ),
            (
                "api_managed_context_anchors",
                Row,
                Some(Op::SessionInspect),
            ),
            (
                "api_managed_context_fission",
                Row,
                Some(Op::SessionInspect),
            ),
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
        for spec in CONTROL_METHODS {
            assert!(
                live.insert(spec.name, (Residue, spec.op)).is_none(),
                "{} is declared BOTH as a route-row tunnel column and in \
                 CONTROL_METHODS — remove the residue entry",
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
    /// `CONTROL_METHODS`; the verb + instantiated path resolve to a
    /// declared route whose verb is declared exactly (never via `Any`);
    /// the row's IAM operation equals the tunnel method's; and the path
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
        let to = rest.find("});").expect("DAEMON_API_HTTP_MAP is unterminated");

        // One `name: { verb: '…', path: '…', … },` entry per line — the
        // fragment documents that contract next to the literal.
        fn quoted(entry: &str, key: &str) -> Option<String> {
            let marker = format!("{key}: '");
            let at = entry.find(&marker)? + marker.len();
            let rest = &entry[at..];
            Some(rest[..rest.find('\'')?].to_string())
        }
        let mut entries: std::collections::BTreeMap<String, (String, String)> =
            Default::default();
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
        // uploads) plus the F2 sessions-family reads (managed-context,
        // worktrees, the session list and its NDJSON stream, search,
        // detail, report, context snapshots). The `api_transfer_*` methods
        // join when their HTTP rows land (task #6, /api/transfers); adding
        // or dropping an entry updates this list in the same change,
        // deliberately.
        let expected: std::collections::BTreeSet<&str> = [
            "api_fs_stat",
            "api_fs_list",
            "api_fs_read",
            "api_fs_mkdir",
            "api_fs_write",
            "api_fs_rename",
            "api_fs_delete",
            "api_session_current_uploads",
            "api_session_current_upload",
            "api_session_current_upload_raw",
            "api_session_current_upload_delete",
            "api_sessions",
            "api_sessions_stream",
            "api_sessions_search",
            "api_session_detail",
            "api_session_report",
            "api_session_context_snapshot",
            "api_managed_context_records",
            "api_managed_context_anchors",
            "api_managed_context_fission",
            "api_worktrees",
            "api_worktrees_inspect",
            "api_worktrees_scan",
            "api_worktrees_remove",
            "api_worktrees_merge",
        ]
        .into_iter()
        .collect();
        let actual: std::collections::BTreeSet<&str> =
            entries.keys().map(String::as_str).collect();
        assert_eq!(actual, expected, "DAEMON_API_HTTP_MAP coverage drifted");

        for (method_name, (verb, template)) in &entries {
            let spec = control_method_spec(method_name).unwrap_or_else(|| {
                panic!("{method_name}: descriptor entry has no CONTROL_METHODS row")
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
                _ => panic!("{method_name}: twinned rows must be Operation-gated"),
            }

            // The template must restate the row's declared shape, not just
            // happen to resolve through it.
            match route.pattern {
                PathPattern::Exact(base) => {
                    assert_eq!(template, base, "{method_name}: template != exact route path")
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
        // ORL apply is courierable by any session — the root signature is
        // the authority (see the table row comment).
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
        let mut pending = HashMap::new();
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
        let mut pending = HashMap::new();
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
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let immediate = test_control_frame_response(
            r#"{"t":"request","id":"session-ctrl-frame","method":"api_session_control_msg","params":{"message":{"action":"interrupt","session_id":"session-frame"}}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(immediate.is_none(), "session control request should spawn");

        let task = tokio::time::timeout(Duration::from_secs(1), rx.recv())
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
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
        let mut outbound = OutboundControlQueue::new();
        let immediate = test_control_frame_response(
            r#"{"t":"request","id":"dash-action-frame","method":"api_dashboard_action_msg","params":{"message":{"action":"close_browser_workspace","workspace_id":"workspace-frame"}}}"#,
            &mut rt,
            &tx,
            &mut pending,
            &mut outbound,
        );
        assert!(immediate.is_none(), "dashboard action request should spawn");

        let task = tokio::time::timeout(Duration::from_secs(1), rx.recv())
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
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
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

        let response = rx.recv().await.unwrap();
        assert!(pending.remove(&response.id).is_some());
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
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
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

            let response = rx.recv().await.unwrap();
            assert!(pending.remove(&response.id).is_some());
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
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
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

        let response = rx.recv().await.unwrap();
        assert!(pending.remove(&response.id).is_some());
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
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
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

            let response = rx.recv().await.unwrap();
            assert!(pending.remove(&response.id).is_some());
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
            };
            rt
        };
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
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
        let response = rx.recv().await.unwrap();
        assert!(pending.remove(&response.id).is_some());
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
        let response = rx.recv().await.unwrap();
        assert!(pending.remove(&response.id).is_some());
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
        let response = rx.recv().await.unwrap();
        assert!(pending.remove(&response.id).is_some());
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
        let (tx, mut rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
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

        let response = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(pending.remove(&response.id).is_some());
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
        let (tx, _rx) = mpsc::channel::<ControlTaskResponse>(8);
        let mut pending = HashMap::new();
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

        outbound.enqueue_chunked(
            "large".into(),
            "large:0".into(),
            "start".into(),
            vec!["chunk".into()],
            "end".into(),
        );
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
