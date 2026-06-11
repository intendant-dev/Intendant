//! Peer relationship policy.
//!
//! Pairing produces an mTLS identity; this module gives that identity human
//! meaning. Approved peer client certificates are recorded by fingerprint with
//! a trust profile. The gateway can then authorize daemon-mode HTTP/WS
//! operations from the certificate fingerprint instead of treating every cert
//! signed by the access CA as equivalent.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::CallerError;
use crate::event::ControlMsg;

pub const DEFAULT_PROFILE: &str = "peer-daemon";
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
    pub created_at_unix: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_unix: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileClass {
    PresenceOnly,
    Stats,
    ReadOnlyDisplay,
    SharedSessionSpectator,
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
    PeerManage,
    SessionManage,
    Settings,
    RuntimeControl,
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

pub fn profile_class(profile: &str) -> ProfileClass {
    match profile.trim().to_ascii_lowercase().as_str() {
        "presence-only" | "presence" => ProfileClass::PresenceOnly,
        "stats" | "stats-only" => ProfileClass::Stats,
        "read-only-display" | "display-read-only" => ProfileClass::ReadOnlyDisplay,
        "shared-session-spectator" | "spectator" => ProfileClass::SharedSessionSpectator,
        "task-runner" => ProfileClass::TaskRunner,
        "operator" => ProfileClass::Operator,
        "admin-peer" | "admin" | DEFAULT_PROFILE => ProfileClass::AdminPeer,
        _ => ProfileClass::PresenceOnly,
    }
}

pub fn profile_allows_operation(profile: &str, op: PeerOperation) -> bool {
    use PeerOperation::*;
    use ProfileClass::*;

    match profile_class(profile) {
        PresenceOnly => matches!(op, PresenceRead),
        Stats => matches!(op, PresenceRead | StatsRead),
        ReadOnlyDisplay => matches!(op, PresenceRead | StatsRead | DisplayView),
        SharedSessionSpectator => matches!(op, PresenceRead | StatsRead | DisplayView),
        TaskRunner => matches!(op, PresenceRead | StatsRead | Message | Task),
        Operator => matches!(
            op,
            PresenceRead | StatsRead | DisplayView | DisplayInput | Message | Task | Approval
        ),
        AdminPeer => true,
    }
}

pub fn profile_allows_control_msg(profile: &str, ctrl: &ControlMsg) -> bool {
    let op = control_msg_operation(ctrl);
    profile_allows_operation(profile, op)
}

pub fn control_msg_operation(ctrl: &ControlMsg) -> PeerOperation {
    match ctrl {
        ControlMsg::Status { .. } => PeerOperation::PresenceRead,
        ControlMsg::Usage => PeerOperation::StatsRead,
        ControlMsg::WebRtcSignal { .. } => PeerOperation::DisplayView,
        ControlMsg::RequestDisplayInputAuthority { .. }
        | ControlMsg::ReleaseDisplayInputAuthority { .. }
        | ControlMsg::TakeDisplay { .. }
        | ControlMsg::ReleaseDisplay { .. }
        | ControlMsg::GrantUserDisplay { .. }
        | ControlMsg::RevokeUserDisplay { .. }
        | ControlMsg::SetDiagnosticsVisualMarker { .. } => PeerOperation::DisplayInput,
        ControlMsg::Input { .. }
        | ControlMsg::FollowUp { .. }
        | ControlMsg::CancelFollowUp { .. } => PeerOperation::Message,
        ControlMsg::StartTask { .. }
        | ControlMsg::CreateSession { .. }
        | ControlMsg::ResumeSession { .. }
        | ControlMsg::EditUserMessage { .. } => PeerOperation::Task,
        ControlMsg::Approve { .. }
        | ControlMsg::Deny { .. }
        | ControlMsg::Skip { .. }
        | ControlMsg::ApproveAll { .. } => PeerOperation::Approval,
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
        | ControlMsg::SetGeminiModel { .. }
        | ControlMsg::SetGeminiApprovalMode { .. }
        | ControlMsg::SetGeminiSandbox { .. }
        | ControlMsg::SetGeminiExtensions { .. }
        | ControlMsg::SetGeminiAllowedMcpServers { .. }
        | ControlMsg::SetGeminiIncludeDirectories { .. }
        | ControlMsg::SetGeminiDebug { .. }
        | ControlMsg::SetVerbosity { .. } => PeerOperation::Settings,
        ControlMsg::CodexThreadAction { .. }
        | ControlMsg::GeminiThreadAction { .. }
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

pub fn profile_allows_federated_display_input(profile: &str) -> bool {
    profile_allows_operation(profile, PeerOperation::DisplayInput)
}

pub fn profile_allows_federation_http(profile: &str, request_line: &str) -> bool {
    if request_line.contains(" /api/peers/pairing/") {
        return profile_allows_operation(profile, PeerOperation::PeerManage);
    }
    if request_line.contains(" /api/peers") {
        if request_line.starts_with("GET") {
            return profile_allows_operation(profile, PeerOperation::PresenceRead);
        }
        return profile_allows_operation(profile, PeerOperation::PeerManage);
    }
    if request_line.contains(" /api/coordinator/") {
        return profile_allows_operation(profile, PeerOperation::Task);
    }
    if request_line.contains(" /api/sessions") || request_line.contains(" /api/worktrees") {
        return profile_allows_operation(profile, PeerOperation::SessionManage);
    }
    true
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
        created_at_unix: crate::peer::pairing::unix_timestamp(),
        revoked_at_unix: None,
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
    record.revoked_at_unix = Some(crate::peer::pairing::unix_timestamp());
    write_identity_record(cert_dir, &record)?;
    Ok(record)
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

fn write_identity_record(cert_dir: &Path, record: &PeerIdentityRecord) -> Result<(), CallerError> {
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

fn normalize_fingerprint(raw: &str) -> Result<String, CallerError> {
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

        let revoked = revoke_identity(tmp.path(), "peer-a").unwrap();
        assert_eq!(revoked.status, PeerIdentityStatus::Revoked);
        assert!(revoked.revoked_at_unix.is_some());
    }
}
