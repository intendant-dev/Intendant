//! Peer relationship policy.
//!
//! Pairing produces a daemon-to-daemon mTLS identity; this module gives that
//! identity human meaning. Approved peer client certificates are recorded by
//! fingerprint with a peer profile. The gateway can then authorize daemon-mode HTTP/WS
//! operations from the certificate fingerprint instead of treating every cert
//! signed by the access CA as equivalent.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::CallerError;
use crate::event::ControlMsg;

pub(crate) fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

pub const DEFAULT_PROFILE: &str = "peer-operator";
const POLICY_DIR: &str = "peer-access-identities";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerIdentityStatus {
    Approved,
    Revoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerIdentityRecord {
    pub version: u8,
    pub fingerprint: String,
    pub label: String,
    pub profile: String,
    pub status: PeerIdentityStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "FilesystemAccessPolicy::is_empty")]
    pub filesystem: FilesystemAccessPolicy,
    pub created_at_unix: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_unix: Option<i64>,
    /// Unix seconds after which the identity no longer authenticates.
    /// Org-materialized identities always carry one (documents expire);
    /// manually approved identities may not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix: Option<i64>,
    /// Provenance, e.g. `org:acme` for identities materialized from an
    /// org grant document. Org trust revocation and revocation lists
    /// sweep by this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The org grant document id that materialized this identity, so a
    /// revocation list can revoke it by grant id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_grant_id: Option<String>,
    /// The delegated issuer key that signed the materialized document,
    /// when it was not the org root (phase 6 step 6b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_via: Option<String>,
}

impl PeerIdentityRecord {
    /// Approved and unexpired — the only state that authenticates.
    pub fn is_active(&self, now_unix: i64) -> bool {
        matches!(self.status, PeerIdentityStatus::Approved)
            && self
                .expires_at_unix
                .map(|expires| expires > now_unix)
                .unwrap_or(true)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilesystemAccessPolicy {
    #[serde(default)]
    pub read_roots: Vec<PathBuf>,
    #[serde(default)]
    pub write_roots: Vec<PathBuf>,
}

impl FilesystemAccessPolicy {
    pub fn is_empty(&self) -> bool {
        self.read_roots.is_empty() && self.write_roots.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileClass {
    PresenceOnly,
    Stats,
    SessionReader,
    ReadOnlyDisplay,
    SharedSessionSpectator,
    FileReader,
    FileOperator,
    TerminalOperator,
    TaskRunner,
    Operator,
    AdminPeer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerOperation {
    PresenceRead,
    StatsRead,
    DisplayView,
    DisplayInput,
    Message,
    Task,
    Approval,
    AccessInspect,
    AccessManage,
    PeerInspect,
    PeerManage,
    /// Open a tunnel to a connected peer through this daemon's peer
    /// credentials (dashboard-control, file-transfer, or display
    /// signaling relay). Deliberately distinct from `PeerManage`: using a
    /// peer relationship delegates this daemon's peer identity — what the
    /// tunnel may then do is decided by the *peer's* grants for this
    /// daemon, not by the local grant that opened it.
    PeerUse,
    SessionInspect,
    SessionManage,
    /// Attach to a visible shell session: scrollback replay + live output.
    TerminalView,
    /// Send input to (or resize/close) an existing visible shell session.
    TerminalWrite,
    /// Create a new PTY shell session on this daemon.
    ShellSpawn,
    Settings,
    CredentialsManage,
    RuntimeControl,
    FilesystemRead,
    FilesystemWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemAccessKind {
    Read,
    Write,
}

pub fn normalize_profile(raw: &str) -> Result<String, CallerError> {
    let profile = raw.trim();
    if profile.is_empty() {
        return Err(CallerError::Config("profile cannot be empty".into()));
    }
    if profile.len() > 64 {
        return Err(CallerError::Config(
            "profile must be at most 64 bytes".into(),
        ));
    }
    let valid = profile
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'));
    if !valid {
        return Err(CallerError::Config(
            "profile may contain only letters, numbers, '-', '_', ':', or '.'".into(),
        ));
    }
    Ok(profile.to_ascii_lowercase())
}

pub const ALL_OPERATIONS: [PeerOperation; 22] = [
    PeerOperation::PresenceRead,
    PeerOperation::StatsRead,
    PeerOperation::DisplayView,
    PeerOperation::DisplayInput,
    PeerOperation::Message,
    PeerOperation::Task,
    PeerOperation::Approval,
    PeerOperation::AccessInspect,
    PeerOperation::AccessManage,
    PeerOperation::PeerInspect,
    PeerOperation::PeerManage,
    PeerOperation::PeerUse,
    PeerOperation::SessionInspect,
    PeerOperation::SessionManage,
    PeerOperation::TerminalView,
    PeerOperation::TerminalWrite,
    PeerOperation::ShellSpawn,
    PeerOperation::Settings,
    PeerOperation::CredentialsManage,
    PeerOperation::RuntimeControl,
    PeerOperation::FilesystemRead,
    PeerOperation::FilesystemWrite,
];

/// True when `granted` allows no operation that `cap` does not. Profiles
/// are not a strict ladder (file-reader and session-reader are siblings),
/// so the cap relation is operation-set containment, mirroring how role
/// ceilings intersect permissions in the human lane.
pub fn profile_fits_under(granted: &str, cap: &str) -> bool {
    ALL_OPERATIONS
        .iter()
        .all(|op| !profile_allows_operation(granted, *op) || profile_allows_operation(cap, *op))
}

/// Canonical peer-profile vocabulary: every profile name a grant may carry,
/// with the operation class it maps to. The dashboard's
/// `PEER_PROFILE_OPTIONS` (static/app.html) mirrors the canonical names and
/// `peerProfileMeta`'s alias map mirrors [`PROFILE_ALIASES`] — both pinned
/// by parity tests below, so adding or renaming a profile here without
/// updating the picker fails the suite instead of shipping drift.
pub(crate) const PROFILES: &[(&str, ProfileClass)] = &[
    ("presence-only", ProfileClass::PresenceOnly),
    ("stats", ProfileClass::Stats),
    ("session-reader", ProfileClass::SessionReader),
    ("read-only-display", ProfileClass::ReadOnlyDisplay),
    (
        "shared-session-spectator",
        ProfileClass::SharedSessionSpectator,
    ),
    ("file-reader", ProfileClass::FileReader),
    ("file-operator", ProfileClass::FileOperator),
    ("terminal-operator", ProfileClass::TerminalOperator),
    ("task-runner", ProfileClass::TaskRunner),
    ("peer-operator", ProfileClass::Operator),
    ("peer-root", ProfileClass::AdminPeer),
];

/// Accepted alternate spellings, each canonicalizing to a [`PROFILES`] name.
pub(crate) const PROFILE_ALIASES: &[(&str, &str)] = &[
    ("presence", "presence-only"),
    ("stats-only", "stats"),
    ("sessions-read", "session-reader"),
    ("session-inspect", "session-reader"),
    ("logs-read", "session-reader"),
    ("display-read-only", "read-only-display"),
    ("spectator", "shared-session-spectator"),
    ("files-read", "file-reader"),
    ("filesystem-read-only", "file-reader"),
    ("files", "file-operator"),
    ("filesystem-operator", "file-operator"),
    ("peer-terminal-operator", "terminal-operator"),
    ("terminal", "terminal-operator"),
    ("shell", "terminal-operator"),
    ("operator", "peer-operator"),
    ("admin-peer", "peer-root"),
    ("admin", "peer-root"),
    ("peer-daemon", "peer-root"),
];

/// Resolve a trimmed, lowercased profile string to its canonical
/// [`PROFILES`] entry, applying [`PROFILE_ALIASES`]. `None` when the name
/// is outside the vocabulary.
fn canonical_profile_entry(normalized: &str) -> Option<(&'static str, ProfileClass)> {
    let canonical = PROFILE_ALIASES
        .iter()
        .find(|(alias, _)| *alias == normalized)
        .map(|(_, canonical)| *canonical)
        .unwrap_or(normalized);
    PROFILES
        .iter()
        .find(|(name, _)| *name == canonical)
        .copied()
}

pub fn profile_class(profile: &str) -> ProfileClass {
    let normalized = profile.trim().to_ascii_lowercase();
    canonical_profile_entry(&normalized)
        .map(|(_, class)| class)
        // Unknown profiles degrade to the least-capable class. This is the
        // wire-side contract: a profile string this daemon does not know
        // (stored by an older build, minted by a newer one) fails closed
        // instead of failing the request. Locally-typed profile names are
        // validated loudly before they get here — see
        // [`require_known_profile`].
        .unwrap_or(ProfileClass::PresenceOnly)
}

/// Strict, operator-facing counterpart to [`normalize_profile`]:
/// canonicalize a locally-typed profile name against the [`PROFILES`]
/// vocabulary, resolving [`PROFILE_ALIASES`] to the canonical spelling,
/// and error loudly on anything else. CLI-entered profiles (`peer
/// request/approve/set-profile --profile`) go through here so a typo
/// fails with the vocabulary listed instead of silently landing as a
/// presence-only grant. Wire-side parsing is deliberately *not* strict:
/// an unknown profile arriving on the wire is stored as-is and stays
/// fail-closed via [`profile_class`]'s presence-only degrade.
pub fn require_known_profile(raw: &str) -> Result<String, CallerError> {
    let normalized = normalize_profile(raw)?;
    if let Some((name, _)) = canonical_profile_entry(&normalized) {
        return Ok(name.to_string());
    }
    let known = PROFILES
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    let aliases = PROFILE_ALIASES
        .iter()
        .map(|(alias, canonical)| format!("{alias} = {canonical}"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(CallerError::Config(format!(
        "unknown peer profile '{normalized}'; known profiles: {known}; accepted aliases: {aliases}"
    )))
}

pub fn profile_allows_operation(profile: &str, op: PeerOperation) -> bool {
    use PeerOperation::*;
    use ProfileClass::*;

    match profile_class(profile) {
        PresenceOnly => matches!(op, PresenceRead),
        Stats => matches!(op, PresenceRead | StatsRead),
        SessionReader => matches!(op, PresenceRead | StatsRead | SessionInspect),
        ReadOnlyDisplay => matches!(op, PresenceRead | StatsRead | DisplayView),
        SharedSessionSpectator => {
            matches!(op, PresenceRead | StatsRead | DisplayView | SessionInspect)
        }
        FileReader => matches!(op, PresenceRead | StatsRead | FilesystemRead),
        FileOperator => matches!(
            op,
            PresenceRead | StatsRead | FilesystemRead | FilesystemWrite
        ),
        TerminalOperator => matches!(
            op,
            PresenceRead | StatsRead | SessionInspect | TerminalView | TerminalWrite | ShellSpawn
        ),
        TaskRunner => matches!(op, PresenceRead | StatsRead | Message | Task),
        Operator => matches!(
            op,
            PresenceRead
                | StatsRead
                | SessionInspect
                | DisplayView
                | DisplayInput
                | Message
                | Task
                | Approval
        ),
        // Credential leases stay out of the peer lane entirely in v1: a
        // peer daemon never fuels or drains another daemon's credentials,
        // matching the org peer-cap philosophy for access.manage.
        AdminPeer => !matches!(op, AccessManage | CredentialsManage),
    }
}

#[allow(dead_code)]
pub fn profile_allows_control_msg(profile: &str, ctrl: &ControlMsg) -> bool {
    if matches!(ctrl, ControlMsg::PeerDashboardControlSignal { .. }) {
        return profile_allows_dashboard_control_tunnel(profile);
    }
    let op = control_msg_operation(ctrl);
    profile_allows_operation(profile, op)
}

/// Every capability family the dashboard-control tunnel carries. The
/// tunnel's WebRTC signaling relay is a transport door, not a single
/// operation: it opens for an identity that can use at least one of these,
/// and every method/frame inside is then authorized individually against
/// the same identity (`dashboard_control_method_operation` /
/// `dashboard_control_frame_operation` and their `/ws` twins). Presence-
/// and stats-only profiles have nothing reachable inside, so the door
/// stays shut for them.
pub const DASHBOARD_CONTROL_TUNNEL_OPERATIONS: &[PeerOperation] = &[
    PeerOperation::SessionInspect,
    PeerOperation::FilesystemRead,
    PeerOperation::FilesystemWrite,
    PeerOperation::TerminalView,
    PeerOperation::DisplayView,
    PeerOperation::Message,
];

pub fn profile_allows_dashboard_control_tunnel(profile: &str) -> bool {
    DASHBOARD_CONTROL_TUNNEL_OPERATIONS
        .iter()
        .any(|op| profile_allows_operation(profile, *op))
}

pub fn control_msg_operation(ctrl: &ControlMsg) -> PeerOperation {
    match ctrl {
        ControlMsg::Status { .. } => PeerOperation::PresenceRead,
        ControlMsg::Usage => PeerOperation::StatsRead,
        ControlMsg::WebRtcSignal { .. } => PeerOperation::DisplayView,
        // Fallback classification only: gates special-case this variant
        // through `profile_allows_dashboard_control_tunnel` (the tunnel is
        // multi-capability, so its door is any-of, not this single op).
        ControlMsg::PeerDashboardControlSignal { .. } => PeerOperation::SessionInspect,
        ControlMsg::PeerFileTransferSignal { .. } => PeerOperation::FilesystemRead,
        ControlMsg::RequestDisplayInputAuthority { .. }
        | ControlMsg::ReleaseDisplayInputAuthority { .. }
        | ControlMsg::TakeDisplay { .. }
        | ControlMsg::ReleaseDisplay { .. }
        | ControlMsg::GrantUserDisplay { .. }
        | ControlMsg::RevokeUserDisplay { .. }
        | ControlMsg::CreateVirtualDisplay { .. }
        | ControlMsg::SetDiagnosticsVisualMarker { .. } => PeerOperation::DisplayInput,
        ControlMsg::Input { .. }
        | ControlMsg::FollowUp { .. }
        | ControlMsg::CancelFollowUp { .. } => PeerOperation::Message,
        ControlMsg::StartTask { .. }
        | ControlMsg::CreateSession { .. }
        | ControlMsg::SpawnSubAgent { .. }
        | ControlMsg::ResumeSession { .. }
        | ControlMsg::EditUserMessage { .. } => PeerOperation::Task,
        ControlMsg::Approve { .. }
        | ControlMsg::Deny { .. }
        | ControlMsg::Skip { .. }
        | ControlMsg::ApproveAll { .. }
        | ControlMsg::AnswerQuestion { .. } => PeerOperation::Approval,
        ControlMsg::SetAutonomy { .. }
        | ControlMsg::SetApprovalRule { .. }
        | ControlMsg::SetExternalAgent { .. }
        | ControlMsg::SetCodexCommand { .. }
        | ControlMsg::SetCodexManagedCommand { .. }
        | ControlMsg::SetCodexSandbox { .. }
        | ControlMsg::SetCodexApprovalPolicy { .. }
        | ControlMsg::SetCodexModel { .. }
        | ControlMsg::SetCodexReasoningEffort { .. }
        | ControlMsg::SetCodexServiceTier { .. }
        | ControlMsg::SetCodexWebSearch { .. }
        | ControlMsg::SetCodexNetworkAccess { .. }
        | ControlMsg::SetCodexWritableRoots { .. }
        | ControlMsg::SetCodexManagedContext { .. }
        | ControlMsg::SetCodexContextArchive { .. }
        | ControlMsg::ConfigureSessionAgent { .. }
        | ControlMsg::SetClaudeModel { .. }
        | ControlMsg::SetClaudePermissionMode { .. }
        | ControlMsg::SetClaudeAllowedTools { .. }
        | ControlMsg::SetVerbosity { .. } => PeerOperation::Settings,
        ControlMsg::CodexThreadAction { .. }
        | ControlMsg::RenameSession { .. }
        | ControlMsg::StopSession { .. }
        | ControlMsg::RestartSession { .. }
        | ControlMsg::Interrupt { .. } => PeerOperation::SessionManage,
        ControlMsg::Steer { .. } | ControlMsg::CancelSteer { .. } => PeerOperation::Message,
        ControlMsg::ListDisplays => PeerOperation::DisplayView,
        ControlMsg::QueryDetail { .. } => PeerOperation::StatsRead,
        ControlMsg::CreateBrowserWorkspace { .. }
        | ControlMsg::CloseBrowserWorkspace { .. }
        | ControlMsg::AcquireBrowserWorkspace { .. }
        | ControlMsg::ReleaseBrowserWorkspace { .. }
        | ControlMsg::RecallMemory { .. }
        | ControlMsg::InvokeSkill { .. }
        | ControlMsg::Quit
        | ControlMsg::SetupDebugScreen
        | ControlMsg::TeardownDebugScreen
        | ControlMsg::StartDebugRecording
        | ControlMsg::StopDebugRecording
        | ControlMsg::StartRecording { .. }
        | ControlMsg::StopRecording { .. }
        | ControlMsg::DeleteRecording { .. } => PeerOperation::RuntimeControl,
        ControlMsg::ScheduleControllerRestart { .. }
        | ControlMsg::ControllerTurnComplete { .. }
        | ControlMsg::GetRestartStatus
        | ControlMsg::CancelControllerRestart { .. }
        | ControlMsg::RequestControllerLoopHalt { .. }
        | ControlMsg::ClearControllerLoopHalt
        | ControlMsg::InterveneControllerLoop { .. }
        | ControlMsg::GetControllerLoopStatus => PeerOperation::RuntimeControl,
    }
}

#[allow(dead_code)]
pub fn profile_allows_federated_display_input(profile: &str) -> bool {
    profile_allows_operation(profile, PeerOperation::DisplayInput)
}

pub fn filesystem_access_allowed(
    policy: &FilesystemAccessPolicy,
    kind: FilesystemAccessKind,
    path: &Path,
) -> Result<(), String> {
    let root_candidates: Vec<&PathBuf> = match kind {
        FilesystemAccessKind::Read => policy
            .read_roots
            .iter()
            .chain(policy.write_roots.iter())
            .collect(),
        FilesystemAccessKind::Write => policy.write_roots.iter().collect(),
    };
    if root_candidates.is_empty() {
        return Err(match kind {
            FilesystemAccessKind::Read => "peer identity has no filesystem read roots".to_string(),
            FilesystemAccessKind::Write => {
                "peer identity has no filesystem write roots".to_string()
            }
        });
    }

    let access_subject = match kind {
        FilesystemAccessKind::Read => path.to_path_buf(),
        FilesystemAccessKind::Write => nearest_existing_path(path)
            .ok_or_else(|| format!("{} has no existing parent", path.display()))?,
    };
    let canonical_subject = std::fs::canonicalize(&access_subject)
        .map_err(|e| format!("{} is not accessible: {e}", access_subject.display()))?;

    for root in root_candidates {
        let canonical_root = match std::fs::canonicalize(root) {
            Ok(root) => root,
            Err(_) => continue,
        };
        if canonical_subject == canonical_root || canonical_subject.starts_with(&canonical_root) {
            return Ok(());
        }
    }

    Err(format!(
        "{} is outside this peer identity's filesystem roots",
        canonical_subject.display()
    ))
}

#[allow(dead_code)]
pub fn profile_allows_federation_http(profile: &str, request_line: &str) -> bool {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let path = target.split('?').next().unwrap_or("");
    let Some(op) = federation_http_operation(method, path) else {
        return true;
    };
    profile_allows_operation(profile, op)
}

/// Map a federation API request to the operation it needs. `path` must be
/// the parsed request path with the query string stripped — matching is on
/// exact routes and their `/`-nested sub-routes, never on substrings, so a
/// query parameter or a longer look-alike path cannot change the class.
pub fn federation_http_operation(method: &str, path: &str) -> Option<PeerOperation> {
    let under = |base: &str| {
        path == base
            || path
                .strip_prefix(base)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    if path == "/api/access/overview"
        || path == "/api/access/iam/state"
        || path == "/api/dashboard/targets"
    {
        return Some(PeerOperation::AccessInspect);
    }
    if under("/api/peers/pairing/requests") || under("/api/peers/pairing/identities") {
        if method == "GET" {
            return Some(PeerOperation::AccessInspect);
        }
        return Some(PeerOperation::AccessManage);
    }
    if under("/api/peers/pairing/invite") {
        return Some(PeerOperation::AccessManage);
    }
    if path.starts_with("/api/peers/pairing/") {
        return Some(PeerOperation::PeerManage);
    }
    // Acting through a connected peer (`/api/peers/{id}/<op>`) is peer
    // *use*, not peer administration. That covers the signaling relays
    // (which open tunnels) and the quick controls (message / task /
    // approval): every one of them delegates this daemon's peer identity,
    // and the receiving peer authorizes the action against its own grants
    // for this daemon. Keeping the quick controls on peer.manage would be a
    // hollow boundary anyway — a peer.use principal can reach the same
    // effects through the dashboard-control tunnel it may already open.
    // Registry and pairing mutations stay peer.manage.
    if method == "POST" {
        let mut segments = path
            .strip_prefix("/api/peers/")
            .into_iter()
            .flat_map(|rest| rest.split('/'));
        if let (Some(id), Some(op), None) = (segments.next(), segments.next(), segments.next()) {
            if !id.is_empty()
                && matches!(
                    op,
                    "webrtc"
                        | "file-transfer-webrtc"
                        | "dashboard-control-webrtc"
                        | "message"
                        | "task"
                        | "approval"
                )
            {
                return Some(PeerOperation::PeerUse);
            }
        }
    }
    if under("/api/peers") {
        if method == "GET" {
            return Some(PeerOperation::PeerInspect);
        }
        return Some(PeerOperation::PeerManage);
    }
    if path.starts_with("/api/coordinator/") {
        return Some(PeerOperation::Task);
    }
    if under("/api/sessions") {
        return Some(PeerOperation::SessionInspect);
    }
    if under("/api/worktrees") {
        return Some(PeerOperation::SessionInspect);
    }
    None
}

pub fn write_approved_identity(
    cert_dir: &Path,
    fingerprint: &str,
    label: &str,
    profile: &str,
    card_url: Option<&str>,
    request_id: Option<&str>,
) -> Result<PeerIdentityRecord, CallerError> {
    let fingerprint = normalize_fingerprint(fingerprint)?;
    let profile = normalize_profile(profile)?;
    let record = PeerIdentityRecord {
        version: 1,
        fingerprint,
        label: label.trim().to_string(),
        profile,
        status: PeerIdentityStatus::Approved,
        card_url: card_url.map(str::to_string),
        request_id: request_id.map(str::to_string),
        filesystem: FilesystemAccessPolicy::default(),
        created_at_unix: unix_timestamp(),
        revoked_at_unix: None,
        expires_at_unix: None,
        source: None,
        org_grant_id: None,
        issued_via: None,
    };
    write_identity_record(cert_dir, &record)?;
    Ok(record)
}

pub fn lookup_identity(
    cert_dir: &Path,
    fingerprint: &str,
) -> Result<Option<PeerIdentityRecord>, CallerError> {
    let fingerprint = normalize_fingerprint(fingerprint)?;
    let path = identity_path(cert_dir, &fingerprint);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    let record: PeerIdentityRecord = serde_json::from_str(&text)?;
    Ok(Some(record))
}

pub fn list_identities(cert_dir: &Path) -> Result<Vec<PeerIdentityRecord>, CallerError> {
    let dir = identities_dir(cert_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<PeerIdentityRecord> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry.path().extension().and_then(|s| s.to_str()) == Some("json")
        {
            let text = std::fs::read_to_string(entry.path())?;
            out.push(serde_json::from_str(&text)?);
        }
    }
    out.sort_by(|a, b| {
        a.label
            .cmp(&b.label)
            .then(a.fingerprint.cmp(&b.fingerprint))
    });
    Ok(out)
}

pub fn revoke_identity(
    cert_dir: &Path,
    fingerprint_or_label: &str,
) -> Result<PeerIdentityRecord, CallerError> {
    let needle = fingerprint_or_label.trim();
    if needle.is_empty() {
        return Err(CallerError::Config("peer identity is required".into()));
    }
    let mut record = if let Ok(fp) = normalize_fingerprint(needle) {
        lookup_identity(cert_dir, &fp)?.ok_or_else(|| {
            CallerError::Config(format!("no peer identity found for fingerprint {needle}"))
        })?
    } else {
        let matches: Vec<_> = list_identities(cert_dir)?
            .into_iter()
            .filter(|r| r.label == needle || r.request_id.as_deref() == Some(needle))
            .collect();
        match matches.len() {
            1 => matches.into_iter().next().unwrap(),
            0 => {
                return Err(CallerError::Config(format!(
                    "no peer identity found for {needle}"
                )))
            }
            _ => {
                return Err(CallerError::Config(format!(
                    "multiple peer identities match {needle}; use fingerprint"
                )))
            }
        }
    };
    record.status = PeerIdentityStatus::Revoked;
    record.revoked_at_unix = Some(unix_timestamp());
    write_identity_record(cert_dir, &record)?;
    Ok(record)
}

/// Outcome of [`set_identity_profile`]: the updated record plus the
/// profile it replaced.
#[derive(Debug, Clone)]
pub struct ProfileChange {
    pub record: PeerIdentityRecord,
    pub previous_profile: String,
}

/// Change the profile of an approved inbound peer identity in place — no
/// revoke/re-pair ceremony. `selector` is the identity's certificate
/// fingerprint as printed by `intendant peer identities`: the full 64-hex
/// value or an unambiguous hex prefix. The profile must be a known name
/// or alias ([`require_known_profile`]) — a local edit has no legitimate
/// use for an unknown profile, unlike wire ingestion.
///
/// This is the same offline state-file write `peer approve` performs: the
/// gateway resolves a presented client certificate to its stored profile
/// per request, so the change takes effect on the peer's next request
/// with no daemon restart.
pub fn set_identity_profile(
    cert_dir: &Path,
    selector: &str,
    profile: &str,
) -> Result<ProfileChange, CallerError> {
    let profile = require_known_profile(profile)?;
    let mut record = find_identity_by_fingerprint(cert_dir, selector)?;
    if !matches!(record.status, PeerIdentityStatus::Approved) {
        return Err(CallerError::Config(format!(
            "peer identity {} ({}) is revoked; approve a new pairing instead of changing its profile",
            record.fingerprint, record.label
        )));
    }
    let previous_profile = std::mem::replace(&mut record.profile, profile);
    write_identity_record(cert_dir, &record)?;
    Ok(ProfileChange {
        record,
        previous_profile,
    })
}

/// Find exactly one recorded identity by fingerprint — full 64-hex or an
/// unambiguous prefix (':' separators tolerated, as in `normalize_fingerprint`).
/// Errors loudly on no match and lists the candidates on an ambiguous one.
fn find_identity_by_fingerprint(
    cert_dir: &Path,
    selector: &str,
) -> Result<PeerIdentityRecord, CallerError> {
    let needle: String = selector
        .trim()
        .chars()
        .filter(|c| *c != ':')
        .collect::<String>()
        .to_ascii_lowercase();
    if needle.is_empty() {
        return Err(CallerError::Config(
            "peer identity fingerprint is required".into(),
        ));
    }
    if needle.len() > 64 || !needle.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CallerError::Config(format!(
            "invalid fingerprint selector {selector:?}: fingerprints are hex — copy one from `intendant peer identities`"
        )));
    }
    let mut matches: Vec<PeerIdentityRecord> = list_identities(cert_dir)?
        .into_iter()
        .filter(|record| record.fingerprint.starts_with(&needle))
        .collect();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(CallerError::Config(format!(
            "no peer identity matches fingerprint {selector:?}; run `intendant peer identities` to list them"
        ))),
        _ => {
            let candidates = matches
                .iter()
                .map(|record| format!("{} ({})", record.fingerprint, record.label))
                .collect::<Vec<_>>()
                .join(", ");
            Err(CallerError::Config(format!(
                "fingerprint prefix {selector:?} is ambiguous; candidates: {candidates}"
            )))
        }
    }
}

pub fn fingerprint_der(der: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(der);
    let fp: [u8; 32] = hasher.finalize().into();
    let mut s = String::with_capacity(64);
    for byte in fp {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

pub fn fingerprint_pem(pem_text: &str) -> Result<String, CallerError> {
    let pem = pem::parse(pem_text.as_bytes())
        .map_err(|e| CallerError::Config(format!("parse certificate PEM: {e}")))?;
    Ok(fingerprint_der(pem.contents()))
}

pub fn write_identity_record(
    cert_dir: &Path,
    record: &PeerIdentityRecord,
) -> Result<(), CallerError> {
    std::fs::create_dir_all(identities_dir(cert_dir))?;
    let body = serde_json::to_string_pretty(record)?;
    std::fs::write(identity_path(cert_dir, &record.fingerprint), body)?;
    Ok(())
}

fn identities_dir(cert_dir: &Path) -> PathBuf {
    cert_dir.join(POLICY_DIR)
}

fn identity_path(cert_dir: &Path, fingerprint: &str) -> PathBuf {
    identities_dir(cert_dir).join(format!("{fingerprint}.json"))
}

fn nearest_existing_path(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub fn normalize_fingerprint(raw: &str) -> Result<String, CallerError> {
    let fp = raw
        .trim()
        .chars()
        .filter(|c| *c != ':')
        .collect::<String>()
        .to_ascii_lowercase();
    let valid = fp.len() == 64 && fp.bytes().all(|b| b.is_ascii_hexdigit());
    if !valid {
        return Err(CallerError::Config(format!(
            "invalid certificate fingerprint {raw:?}"
        )));
    }
    Ok(fp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dashboard's profile picker (`PEER_PROFILE_OPTIONS`) and alias
    /// map (`peerProfileMeta`) are static mirrors of [`PROFILES`] /
    /// [`PROFILE_ALIASES`] — they can't derive from this file, so this test
    /// pins them: a profile added or renamed here without updating the
    /// picker fails the suite instead of shipping drift.
    #[test]
    fn dashboard_profile_picker_mirrors_the_canonical_vocabulary() {
        let app = include_str!("../../../../static/app.html");
        let slice = |start: &str, end: &str| {
            let from = app
                .find(start)
                .unwrap_or_else(|| panic!("marker {start:?} not found in app.html"))
                + start.len();
            let rest = &app[from..];
            &rest[..rest
                .find(end)
                .unwrap_or_else(|| panic!("end marker {end:?} missing after {start:?}"))]
        };

        let options = slice("const PEER_PROFILE_OPTIONS = [", "\n];");
        let picker: std::collections::BTreeSet<&str> = regex::Regex::new(r"profile: '([a-z-]+)'")
            .unwrap()
            .captures_iter(options)
            .map(|caps| caps.get(1).unwrap().as_str())
            .collect();
        let canonical: std::collections::BTreeSet<&str> =
            PROFILES.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            picker, canonical,
            "PEER_PROFILE_OPTIONS (static/app.html) drifted from PROFILES"
        );

        let alias_block = slice("function peerProfileMeta(", "const canonical");
        let js_aliases: std::collections::BTreeSet<(&str, &str)> =
            regex::Regex::new(r"(?m)^\s+'?([a-z][a-z-]*)'?: '([a-z-]+)',")
                .unwrap()
                .captures_iter(alias_block)
                .map(|caps| {
                    (
                        caps.get(1).unwrap().as_str(),
                        caps.get(2).unwrap().as_str(),
                    )
                })
                .collect();
        let rust_aliases: std::collections::BTreeSet<(&str, &str)> =
            PROFILE_ALIASES.iter().copied().collect();
        assert_eq!(
            js_aliases, rust_aliases,
            "peerProfileMeta aliases (static/app.html) drifted from PROFILE_ALIASES"
        );

        // Every alias lands on a canonical profile and classes agree.
        for (alias, target) in PROFILE_ALIASES {
            assert!(canonical.contains(target), "alias {alias} → unknown {target}");
            assert_eq!(profile_class(alias), profile_class(target));
        }
    }

    #[test]
    fn profile_permissions_downgrade_task_runner() {
        let task = ControlMsg::StartTask {
            session_id: None,
            task: "run".into(),
            orchestrate: None,
            direct: None,
            reference_frame_ids: Vec::new(),
            display_target: None,
            attachments: Vec::new(),
            follow_up_id: None,
        };
        let approval = ControlMsg::Approve {
            session_id: None,
            id: 7,
        };

        assert!(profile_allows_control_msg("task-runner", &task));
        assert!(!profile_allows_control_msg("task-runner", &approval));
    }

    #[test]
    fn profile_permissions_read_only_display_cannot_request_input() {
        let view = ControlMsg::WebRtcSignal {
            display_id: 0,
            session_id: "s".into(),
            signal: crate::peer::WebRtcSignal::Unknown,
        };
        let input = ControlMsg::RequestDisplayInputAuthority { display_id: 0 };

        assert!(profile_allows_control_msg("read-only-display", &view));
        assert!(!profile_allows_control_msg("read-only-display", &input));
        assert!(!profile_allows_federated_display_input("read-only-display"));
        assert!(profile_allows_federated_display_input("operator"));
    }

    #[test]
    fn dashboard_control_tunnel_door_opens_for_any_tunnel_capability() {
        // Every profile that can use something inside the tunnel gets
        // through the door; per-method authorization inside does the rest.
        for profile in [
            "file-operator",
            "file-reader",
            "session-reader",
            "terminal-operator",
            "read-only-display",
            "task-runner",
            "operator",
            "peer-root",
        ] {
            assert!(
                profile_allows_dashboard_control_tunnel(profile),
                "{profile} should reach the dashboard-control tunnel"
            );
        }
        // Nothing inside the tunnel is reachable for these; door stays shut.
        for profile in ["presence-only", "stats"] {
            assert!(
                !profile_allows_dashboard_control_tunnel(profile),
                "{profile} should not reach the dashboard-control tunnel"
            );
        }

        let signal = ControlMsg::PeerDashboardControlSignal {
            session_id: "s".into(),
            signal: crate::peer::WebRtcSignal::Unknown,
        };
        assert!(profile_allows_control_msg("file-operator", &signal));
        assert!(profile_allows_control_msg("file-reader", &signal));
        assert!(!profile_allows_control_msg("stats", &signal));
    }

    #[test]
    fn peer_prefixed_profile_aliases_keep_legacy_permissions() {
        assert_eq!(profile_class("peer-operator"), ProfileClass::Operator);
        assert_eq!(profile_class("peer-root"), ProfileClass::AdminPeer);
        assert_eq!(profile_class("peer-daemon"), ProfileClass::AdminPeer);
        assert!(profile_allows_operation(
            "peer-root",
            PeerOperation::RuntimeControl
        ));
        assert!(!profile_allows_operation(
            "peer-operator",
            PeerOperation::RuntimeControl
        ));
        assert!(profile_allows_operation(
            "peer-root",
            PeerOperation::AccessInspect
        ));
        assert!(profile_allows_operation(
            "peer-root",
            PeerOperation::PeerInspect
        ));
        assert!(profile_allows_operation(
            "peer-root",
            PeerOperation::PeerManage
        ));
        assert!(!profile_allows_operation(
            "peer-root",
            PeerOperation::AccessManage
        ));
        assert!(!profile_allows_operation(
            "peer-operator",
            PeerOperation::AccessInspect
        ));
        assert!(!profile_allows_operation(
            "peer-operator",
            PeerOperation::PeerInspect
        ));
        assert!(profile_allows_federation_http(
            "peer-root",
            "GET /api/access/iam/state HTTP/1.1"
        ));
        assert_eq!(
            federation_http_operation("GET", "/api/access/iam/state"),
            Some(PeerOperation::AccessInspect)
        );
        assert_eq!(
            federation_http_operation("POST", "/api/peers/pairing/invite"),
            Some(PeerOperation::AccessManage)
        );
        assert_eq!(
            federation_http_operation("GET", "/api/peers"),
            Some(PeerOperation::PeerInspect)
        );
        assert_eq!(
            federation_http_operation("POST", "/api/peers"),
            Some(PeerOperation::PeerManage)
        );
        assert_eq!(federation_http_operation("GET", "/config"), None);
        // Matching is on parsed routes, never substrings: look-alike paths
        // and query strings that mention a route do not classify as it.
        assert_eq!(federation_http_operation("GET", "/api/peersonal"), None);
        assert_eq!(
            federation_http_operation("GET", "/api/peers/pairing/requests/r-1"),
            Some(PeerOperation::AccessInspect)
        );
        assert!(!profile_allows_federation_http(
            "peer-operator",
            "GET /api/access/iam/state HTTP/1.1"
        ));
    }

    #[test]
    fn peer_signal_relays_classify_as_peer_use() {
        // Acting through a connected peer — the three signaling relays and
        // the three quick controls — is peer use, not peer administration.
        for op in [
            "webrtc",
            "file-transfer-webrtc",
            "dashboard-control-webrtc",
            "message",
            "task",
            "approval",
        ] {
            assert_eq!(
                federation_http_operation("POST", &format!("/api/peers/intendant:peer-b/{op}")),
                Some(PeerOperation::PeerUse),
                "{op}"
            );
        }
        // Everything else under /api/peers keeps its class: registry and
        // pairing mutations are manage, pairing arms win over the id/op
        // shape, GETs are inspect, and deeper or look-alike op segments
        // never classify as use.
        assert_eq!(
            federation_http_operation("POST", "/api/peers/pairing/join"),
            Some(PeerOperation::PeerManage)
        );
        assert_eq!(
            federation_http_operation(
                "GET",
                "/api/peers/intendant:peer-b/dashboard-control-webrtc"
            ),
            Some(PeerOperation::PeerInspect)
        );
        assert_eq!(
            federation_http_operation("POST", "/api/peers/intendant:peer-b/webrtc/extra"),
            Some(PeerOperation::PeerManage)
        );
        // Only the admin peer profile may use relays transitively.
        assert!(profile_allows_operation(
            "peer-root",
            PeerOperation::PeerUse
        ));
        assert!(!profile_allows_operation(
            "file-operator",
            PeerOperation::PeerUse
        ));
        assert!(!profile_allows_operation(
            "peer-operator",
            PeerOperation::PeerUse
        ));
    }

    #[test]
    fn identity_round_trip_and_revoke() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fp = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let record = write_approved_identity(
            tmp.path(),
            fp,
            "peer-a",
            "operator",
            Some("https://peer/.well-known/agent-card.json"),
            Some("req-1"),
        )
        .unwrap();
        assert_eq!(record.status, PeerIdentityStatus::Approved);

        let loaded = lookup_identity(tmp.path(), fp).unwrap().unwrap();
        assert_eq!(loaded.profile, "operator");
        assert!(loaded.filesystem.is_empty());

        let revoked = revoke_identity(tmp.path(), "peer-a").unwrap();
        assert_eq!(revoked.status, PeerIdentityStatus::Revoked);
        assert!(revoked.revoked_at_unix.is_some());
    }

    #[test]
    fn require_known_profile_accepts_canonical_and_resolves_aliases() {
        for (name, _) in PROFILES {
            assert_eq!(require_known_profile(name).unwrap(), *name);
        }
        for (alias, canonical) in PROFILE_ALIASES {
            assert_eq!(require_known_profile(alias).unwrap(), *canonical);
        }
        // The documented upgrade path: the peer-daemon alias keeps working.
        assert_eq!(require_known_profile("peer-daemon").unwrap(), "peer-root");
        assert_eq!(require_known_profile("  Peer-Root ").unwrap(), "peer-root");
    }

    #[test]
    fn require_known_profile_errors_loudly_with_the_vocabulary() {
        // The typo class that used to silently degrade to presence-only.
        let err = require_known_profile("read-only-dsplay").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown peer profile 'read-only-dsplay'"), "{msg}");
        for (name, _) in PROFILES {
            assert!(msg.contains(name), "missing {name} in: {msg}");
        }
        assert!(msg.contains("peer-daemon = peer-root"), "{msg}");
        // Charset violations keep their dedicated diagnostic.
        let err = require_known_profile("peer root").unwrap_err();
        assert!(err.to_string().contains("may contain only"), "{err}");
    }

    #[test]
    fn unknown_profiles_still_degrade_fail_closed_on_the_wire_side() {
        // The strict CLI check must not tighten wire semantics: a stored
        // profile this build does not know keeps authorizing as the
        // least-capable class rather than erroring.
        assert_eq!(
            profile_class("future-profile"),
            ProfileClass::PresenceOnly
        );
        assert!(profile_allows_operation(
            "future-profile",
            PeerOperation::PresenceRead
        ));
        assert!(!profile_allows_operation(
            "future-profile",
            PeerOperation::StatsRead
        ));
    }

    #[test]
    fn set_identity_profile_updates_the_stored_record_in_place() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fp = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        write_approved_identity(
            tmp.path(),
            fp,
            "peer-a",
            "read-only-display",
            Some("https://peer/.well-known/agent-card.json"),
            Some("req-1"),
        )
        .unwrap();

        let change = set_identity_profile(tmp.path(), fp, "peer-operator").unwrap();
        assert_eq!(change.previous_profile, "read-only-display");
        assert_eq!(change.record.profile, "peer-operator");
        assert_eq!(change.record.status, PeerIdentityStatus::Approved);
        // Only the profile changes; provenance fields survive the edit.
        assert_eq!(change.record.request_id.as_deref(), Some("req-1"));

        // Persisted through the same store `peer approve` writes and the
        // gateway rereads per request.
        let loaded = lookup_identity(tmp.path(), fp).unwrap().unwrap();
        assert_eq!(loaded.profile, "peer-operator");

        // Aliases keep working and land canonicalized.
        let change = set_identity_profile(tmp.path(), fp, "peer-daemon").unwrap();
        assert_eq!(change.record.profile, "peer-root");
    }

    #[test]
    fn set_identity_profile_resolves_unambiguous_prefixes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fp_a = "aa11111111111111111111111111111111111111111111111111111111111111";
        let fp_b = "ab22222222222222222222222222222222222222222222222222222222222222";
        write_approved_identity(tmp.path(), fp_a, "peer-a", "stats", None, None).unwrap();
        write_approved_identity(tmp.path(), fp_b, "peer-b", "stats", None, None).unwrap();

        let change = set_identity_profile(tmp.path(), "aa11", "file-reader").unwrap();
        assert_eq!(change.record.fingerprint, fp_a);
        assert_eq!(change.record.profile, "file-reader");

        // A shared prefix is ambiguous and must list the candidates.
        let err = set_identity_profile(tmp.path(), "a", "file-reader").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ambiguous"), "{msg}");
        assert!(msg.contains(fp_a) && msg.contains(fp_b), "{msg}");
        assert!(msg.contains("peer-a") && msg.contains("peer-b"), "{msg}");
    }

    #[test]
    fn set_identity_profile_rejects_unknown_selectors_and_profiles() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fp = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        write_approved_identity(tmp.path(), fp, "peer-a", "stats", None, None).unwrap();

        let err = set_identity_profile(tmp.path(), "ffff", "stats").unwrap_err();
        assert!(
            err.to_string().contains("no peer identity matches"),
            "{err}"
        );

        let err = set_identity_profile(tmp.path(), "not-hex!", "stats").unwrap_err();
        assert!(
            err.to_string().contains("invalid fingerprint selector"),
            "{err}"
        );

        // Unknown profile fails loudly and leaves the record untouched.
        let err = set_identity_profile(tmp.path(), fp, "read-only-dsplay").unwrap_err();
        assert!(err.to_string().contains("unknown peer profile"), "{err}");
        let loaded = lookup_identity(tmp.path(), fp).unwrap().unwrap();
        assert_eq!(loaded.profile, "stats");
    }

    #[test]
    fn set_identity_profile_refuses_revoked_identities() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fp = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        write_approved_identity(tmp.path(), fp, "peer-a", "stats", None, None).unwrap();
        revoke_identity(tmp.path(), fp).unwrap();

        let err = set_identity_profile(tmp.path(), fp, "peer-operator").unwrap_err();
        assert!(err.to_string().contains("is revoked"), "{err}");
        let loaded = lookup_identity(tmp.path(), fp).unwrap().unwrap();
        assert_eq!(loaded.status, PeerIdentityStatus::Revoked);
        assert_eq!(loaded.profile, "stats");
    }

    #[test]
    fn filesystem_access_requires_explicit_roots() {
        assert!(profile_allows_operation(
            "admin-peer",
            PeerOperation::FilesystemRead
        ));
        let tmp = tempfile::TempDir::new().unwrap();
        let policy = FilesystemAccessPolicy::default();
        let denied =
            filesystem_access_allowed(&policy, FilesystemAccessKind::Read, tmp.path()).unwrap_err();
        assert!(denied.contains("no filesystem read roots"));
    }

    #[test]
    fn filesystem_access_allows_canonical_child() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("allowed");
        let child = root.join("nested").join("file.txt");
        std::fs::create_dir_all(child.parent().unwrap()).unwrap();
        std::fs::write(&child, b"ok").unwrap();

        let policy = FilesystemAccessPolicy {
            read_roots: vec![root],
            write_roots: Vec::new(),
        };
        filesystem_access_allowed(&policy, FilesystemAccessKind::Read, &child).unwrap();
    }

    #[test]
    fn filesystem_access_rejects_dotdot_escape() {
        let tmp = tempfile::TempDir::new().unwrap();
        let allowed = tmp.path().join("allowed");
        let secret = tmp.path().join("secret");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        let escaped = allowed.join("..").join("secret").join("file.txt");
        std::fs::write(secret.join("file.txt"), b"secret").unwrap();

        let policy = FilesystemAccessPolicy {
            read_roots: vec![allowed],
            write_roots: Vec::new(),
        };
        let denied =
            filesystem_access_allowed(&policy, FilesystemAccessKind::Read, &escaped).unwrap_err();
        assert!(denied.contains("outside"));
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_access_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let allowed = tmp.path().join("allowed");
        let secret = tmp.path().join("secret");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("file.txt"), b"secret").unwrap();
        symlink(&secret, allowed.join("secret-link")).unwrap();

        let policy = FilesystemAccessPolicy {
            read_roots: vec![allowed.clone()],
            write_roots: Vec::new(),
        };
        let denied = filesystem_access_allowed(
            &policy,
            FilesystemAccessKind::Read,
            &allowed.join("secret-link").join("file.txt"),
        )
        .unwrap_err();
        assert!(denied.contains("outside"));
    }
}
