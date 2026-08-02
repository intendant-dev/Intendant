//! The voice tool lane: presence tools as dynamicTools on the backing
//! thread (R2), and the pure halves of the authority machinery (R3).
//!
//! R2 — profile/tool-lane composition: the hardened profile pins
//! `approval_policy="never"`, which auto-rejects MCP tool calls; the
//! presence toolset therefore rides the **dynamicTools** lane, which is
//! client-executed (the app-server forwards `item/tool/call` to the
//! broker) and composes with the hardened profile — proven live in the
//! Stage B b4-dynamic spike under exactly this profile. The toolset is
//! declared at `thread/start`, so the hardened default can never
//! silently reject it — pinned by test here and exercised end-to-end in
//! the mock-app-server tests.
//!
//! R3 — every authority-bearing dispatch requires (a) a live owner
//! anchor (the authenticated dashboard presence connection that owns
//! the call — no anchor, no dispatch, fail-closed, including mid-flight
//! loss), (b) verbatim spoken-instruction evidence mechanically
//! verified against the live session's user-role sideband transcript,
//! and (c) a durable audit line naming BOTH principals — the acting
//! broker surface and the attributed owner surface — never styled as a
//! direct owner act. Attribution never widens authority: authorization
//! remains the owner's authenticated active connection, exactly the
//! authority browser-presence approvals ride today.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Authority-bearing presence tools: dispatch requires the owner
/// anchor + verified spoken evidence (R3). This is the §3.4 delegated
/// set as extended by the Stage A §7.2 ruling to all authority acts.
pub(crate) const VOICE_AUTHORITY_TOOLS: &[&str] = &[
    "submit_task",
    "approve_action",
    "deny_action",
    "skip_action",
    "respond_to_question",
    "set_autonomy",
    "send_message",
];

/// Read-only presence tools: no evidence required (they mutate
/// nothing), still only servable while a voice session is live.
pub(crate) const VOICE_READ_TOOLS: &[&str] =
    &["check_status", "query_detail", "search_transcripts"];

/// The required evidence argument added to every authority tool's
/// schema on the voice lane.
pub(crate) const SPOKEN_INSTRUCTION_ARG: &str = "spoken_instruction";

/// Normalized evidence shorter than this is refused as insufficient —
/// a two-letter fragment would match almost any transcript.
pub(crate) const MIN_EVIDENCE_CHARS: usize = 8;

/// How many trailing transcript segments the evidence check scans.
pub(crate) const EVIDENCE_TRANSCRIPT_WINDOW: usize = 50;

/// Build the dynamicTools declaration for the backing thread: the ten
/// text presence tools (the frame-inspection pair is a later slice —
/// its outputs are images), with `spoken_instruction` REQUIRED on every
/// authority-bearing tool.
pub(crate) fn dynamic_tool_specs() -> Vec<serde_json::Value> {
    presence_core::presence_tools()
        .into_iter()
        .filter(|tool| {
            VOICE_AUTHORITY_TOOLS.contains(&tool.name.as_str())
                || VOICE_READ_TOOLS.contains(&tool.name.as_str())
        })
        .map(|tool| {
            let mut schema = tool.parameters.clone();
            if VOICE_AUTHORITY_TOOLS.contains(&tool.name.as_str()) {
                if let Some(props) = schema
                    .get_mut("properties")
                    .and_then(|p| p.as_object_mut())
                {
                    props.insert(
                        SPOKEN_INSTRUCTION_ARG.to_string(),
                        serde_json::json!({
                            "type": "string",
                            "description": "The owner's verbatim spoken words authorizing this action, quoted exactly from their most recent speech. Required: the broker mechanically verifies this text against the live transcript and refuses the action if it was not actually said.",
                        }),
                    );
                }
                match schema.get_mut("required").and_then(|r| r.as_array_mut()) {
                    Some(required) => {
                        required.push(serde_json::json!(SPOKEN_INSTRUCTION_ARG));
                    }
                    None => {
                        schema["required"] = serde_json::json!([SPOKEN_INSTRUCTION_ARG]);
                    }
                }
            }
            serde_json::json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "inputSchema": schema,
            })
        })
        .collect()
}

/// Normalize text for the mechanical evidence check: lowercase,
/// alphanumerics only, single spaces. ASR punctuation and casing must
/// not defeat a verbatim quote.
pub(crate) fn normalize_for_evidence(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            for lc in c.to_lowercase() {
                out.push(lc);
            }
        } else {
            pending_space = true;
        }
    }
    out
}

/// Outcome of the mechanical evidence check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EvidenceVerdict {
    Verified,
    /// Evidence absent or too short after normalization.
    Insufficient,
    /// Evidence not found in the recent user-role transcript.
    NotInTranscript,
}

/// Verify claimed spoken evidence against the recent user-role
/// transcript segments (newest last). Containment over normalized
/// text: the quote must appear inside one segment or inside the
/// normalized concatenation of consecutive segments (ASR sometimes
/// splits one utterance).
pub(crate) fn verify_spoken_evidence(
    claimed: &str,
    user_segments: &[String],
) -> EvidenceVerdict {
    let needle = normalize_for_evidence(claimed);
    if needle.chars().count() < MIN_EVIDENCE_CHARS {
        return EvidenceVerdict::Insufficient;
    }
    let window: Vec<&String> = user_segments
        .iter()
        .rev()
        .take(EVIDENCE_TRANSCRIPT_WINDOW)
        .collect();
    let joined = window
        .iter()
        .rev()
        .map(|s| normalize_for_evidence(s))
        .collect::<Vec<_>>()
        .join(" ");
    if joined.contains(&needle) {
        EvidenceVerdict::Verified
    } else {
        EvidenceVerdict::NotInTranscript
    }
}

/// The attributed owner surface for the audit line: the authenticated
/// dashboard presence connection anchoring the call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct VoiceOwnerAnchor {
    pub(crate) connection_id: String,
}

/// One durable authority-audit record (JSONL under the presence state
/// dir). Names BOTH principals on every dispatch AND every refusal;
/// `machine_mediated` is always true — this line is never styled as a
/// direct owner act (attribution never widens authority).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VoiceAuthorityAuditRecord {
    pub(crate) ts_epoch: u64,
    pub(crate) call_id: String,
    pub(crate) tool: String,
    /// The acting principal: the broker surface, by name.
    pub(crate) acting_principal: String,
    /// The attributed owner surface (None exactly when the refusal IS
    /// the missing anchor).
    pub(crate) attributed_owner: Option<VoiceOwnerAnchor>,
    pub(crate) machine_mediated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spoken_instruction: Option<String>,
    pub(crate) evidence_verified: bool,
    /// dispatched | refused-anchor | refused-anchor-midflight |
    /// refused-evidence-insufficient | refused-evidence-unmatched |
    /// failed
    pub(crate) verdict: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

pub(crate) const VOICE_ACTING_PRINCIPAL: &str = "presence-voice-broker";

/// Append an audit record to the durable JSONL (0600, presence dir).
pub(crate) fn append_audit_record(
    state_root: &Path,
    record: &VoiceAuthorityAuditRecord,
) -> Result<(), String> {
    let path = super::store::audit_log_path(state_root);
    if let Some(dir) = path.parent() {
        intendant_core::state_paths::create_private_dir_all(dir)
            .map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let mut line = serde_json::to_string(record).map_err(|e| e.to_string())?;
    line.push('\n');
    use std::io::Write;
    let mut file = intendant_core::state_paths::private_file_options()
        .append(true)
        .create(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("append audit record: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // R2 pin: the declared toolset is exactly the ten text presence
    // tools, every authority tool REQUIRES spoken_instruction, and the
    // read tools don't. The presence toolset can never be silently
    // rejected by the hardened profile because it is declared on the
    // thread itself and executed by the broker, not by an MCP server.
    #[test]
    fn dynamic_toolset_pins_composition_and_evidence_requirement() {
        let specs = dynamic_tool_specs();
        assert_eq!(specs.len(), VOICE_AUTHORITY_TOOLS.len() + VOICE_READ_TOOLS.len());
        for spec in &specs {
            assert_eq!(spec["type"], "function");
            let name = spec["name"].as_str().unwrap();
            let required: Vec<&str> = spec["inputSchema"]["required"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            if VOICE_AUTHORITY_TOOLS.contains(&name) {
                assert!(
                    required.contains(&SPOKEN_INSTRUCTION_ARG),
                    "{name} must require spoken evidence"
                );
                assert!(spec["inputSchema"]["properties"][SPOKEN_INSTRUCTION_ARG].is_object());
            } else {
                assert!(
                    !required.contains(&SPOKEN_INSTRUCTION_ARG),
                    "{name} is read-only and must not demand evidence"
                );
            }
        }
        // The frame-inspection pair stays off the voice lane this slice.
        assert!(!specs.iter().any(|s| s["name"] == "inspect_frame"));
        assert!(!specs.iter().any(|s| s["name"] == "inspect_frames"));
        // set_autonomy keeps the presence ceiling (never "full") — the
        // voice lane reuses the presence-core schema verbatim.
        let set_autonomy = specs.iter().find(|s| s["name"] == "set_autonomy").unwrap();
        let levels = set_autonomy["inputSchema"]["properties"]["level"]["enum"]
            .as_array()
            .unwrap();
        assert!(!levels.iter().any(|l| l == "full"));
    }

    #[test]
    fn evidence_normalization_survives_asr_punctuation() {
        assert_eq!(
            normalize_for_evidence("Approve the pending action, ID alpha-7!"),
            "approve the pending action id alpha 7"
        );
        assert_eq!(normalize_for_evidence("  "), "");
    }

    // R3(b) pin: verification is mechanical containment over the
    // user-role transcript; unmatched or trivial evidence refuses.
    #[test]
    fn evidence_verification_matches_only_real_speech() {
        let transcript = vec![
            "Hello there.".to_string(),
            "Please approve the pending action with ID alpha-7.".to_string(),
        ];
        assert_eq!(
            verify_spoken_evidence("approve the pending action with ID alpha-7", &transcript),
            EvidenceVerdict::Verified
        );
        assert_eq!(
            verify_spoken_evidence("delete every repository", &transcript),
            EvidenceVerdict::NotInTranscript
        );
        assert_eq!(
            verify_spoken_evidence("ok", &transcript),
            EvidenceVerdict::Insufficient
        );
        assert_eq!(
            verify_spoken_evidence("", &transcript),
            EvidenceVerdict::Insufficient
        );
    }

    #[test]
    fn evidence_spanning_split_segments_verifies() {
        let transcript = vec![
            "Please approve the pending".to_string(),
            "action with ID alpha-7".to_string(),
        ];
        assert_eq!(
            verify_spoken_evidence("approve the pending action with ID alpha-7", &transcript),
            EvidenceVerdict::Verified
        );
    }

    // R3(c) pin: the audit record is durable JSONL, names both
    // principals, and is always machine-mediated.
    #[test]
    fn audit_records_append_with_both_principals() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let record = VoiceAuthorityAuditRecord {
            ts_epoch: 1,
            call_id: "call-1".to_string(),
            tool: "approve_action".to_string(),
            acting_principal: VOICE_ACTING_PRINCIPAL.to_string(),
            attributed_owner: Some(VoiceOwnerAnchor {
                connection_id: "conn-9".to_string(),
            }),
            machine_mediated: true,
            spoken_instruction: Some("approve alpha-7".to_string()),
            evidence_verified: true,
            verdict: "dispatched".to_string(),
            detail: None,
        };
        append_audit_record(root, &record).unwrap();
        let refusal = VoiceAuthorityAuditRecord {
            verdict: "refused-anchor".to_string(),
            attributed_owner: None,
            evidence_verified: false,
            ..record.clone()
        };
        append_audit_record(root, &refusal).unwrap();
        let raw = std::fs::read_to_string(super::super::store::audit_log_path(root)).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: VoiceAuthorityAuditRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first.acting_principal, VOICE_ACTING_PRINCIPAL);
        assert_eq!(
            first.attributed_owner.as_ref().unwrap().connection_id,
            "conn-9"
        );
        assert!(first.machine_mediated, "never styled as a direct owner act");
        let second: VoiceAuthorityAuditRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second.verdict, "refused-anchor");
        assert!(second.machine_mediated);
    }
}
