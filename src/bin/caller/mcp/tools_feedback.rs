//! Exception-only dogfood feedback intake for MCP clients.
//!
//! The agent-facing surface is deliberately narrow: callers describe one
//! concrete Intendant issue or efficiency opportunity; the daemon computes the
//! dedupe key, stamps build/provenance out of band, and asks Agenda to perform
//! the create-or-annotate decision under its writer lock.  Nothing here grants
//! generic agenda authority and nothing publishes outside the local daemon.

use super::*;
use std::fmt::Write as _;

const MAX_SURFACE_CHARS: usize = 80;
const MAX_SUMMARY_CHARS: usize = 240;
const MAX_DETAILS_CHARS: usize = 4_000;
const MAX_RECOMMENDATION_CHARS: usize = 2_000;
const MAX_EVIDENCE_REF_CHARS: usize = 320;
const MAX_CLIENT_CONTEXT_CHARS: usize = 160;
const MAX_AVOIDABLE_CALLS: u32 = 10_000;

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DogfoodReportKind {
    Issue,
    Efficiency,
}

impl DogfoodReportKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Efficiency => "efficiency",
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DogfoodImpact {
    Blocked,
    Workaround,
    ExtraCalls,
    ExtraLatency,
    ExtraContext,
    Confusing,
    Minor,
}

impl DogfoodImpact {
    fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Workaround => "workaround",
            Self::ExtraCalls => "extra_calls",
            Self::ExtraLatency => "extra_latency",
            Self::ExtraContext => "extra_context",
            Self::Confusing => "confusing",
            Self::Minor => "minor",
        }
    }
}

/// One exceptional observation about using Intendant.
///
/// This is intentionally structured and bounded instead of accepting an
/// arbitrary transcript dump. `evidence_ref` and `client_context` are
/// self-described caller data; trusted principal/session provenance is stamped
/// by the MCP gate into the Agenda operation and never accepted from fields.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct DogfoodReportParams {
    /// `issue` for incorrect/missing/confusing behavior; `efficiency` for an
    /// avoidable-call/context/latency improvement recommendation.
    pub kind: DogfoodReportKind,
    /// Short Intendant surface name, e.g. `facade`, `chatgpt-plugin`,
    /// `relay`, `agenda`, `display`, or `remote-compute`.
    pub surface: String,
    /// One-line distinct friction/opportunity. The daemon normalizes this with
    /// kind + surface to form the dedupe key.
    pub summary: String,
    /// Concise description of what happened and what was needed instead.
    #[serde(default)]
    pub details: Option<String>,
    /// Concrete suggested improvement when known.
    #[serde(default)]
    pub recommendation: Option<String>,
    /// User-visible effect of the problem/opportunity.
    #[serde(default)]
    pub impact: Option<DogfoodImpact>,
    /// Estimated number of avoidable MCP/tool round trips for this occurrence.
    #[serde(default)]
    pub avoidable_calls: Option<u32>,
    /// Short, scrubbed opaque reference (session/item/tool-call id), not raw
    /// output or a transcript.
    #[serde(default)]
    pub evidence_ref: Option<String>,
    /// Optional caller/host label. This is explicitly self-described and is
    /// never treated as authenticated identity.
    #[serde(default)]
    pub client_context: Option<String>,
}

fn required_text(name: &str, value: String, max_chars: usize) -> Result<String, String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(format!("{name} cannot be empty"));
    }
    let chars = value.chars().count();
    if chars > max_chars {
        return Err(format!(
            "{name} is too long ({chars} chars; max {max_chars})"
        ));
    }
    Ok(value)
}

fn optional_text(
    name: &str,
    value: Option<String>,
    max_chars: usize,
) -> Result<Option<String>, String> {
    value
        .map(|value| required_text(name, value, max_chars))
        .transpose()
}

fn normalized_key(value: &str) -> String {
    value
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn fingerprint(kind: DogfoodReportKind, surface: &str, summary: &str) -> String {
    let canonical = format!(
        "{}\n{}\n{}",
        kind.as_str(),
        normalized_key(surface),
        normalized_key(summary)
    );
    let digest = ring::digest::digest(&ring::digest::SHA256, canonical.as_bytes());
    let mut out = String::with_capacity(32);
    for byte in digest.as_ref().iter().take(16) {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn surface_tag(surface: &str) -> String {
    let mut out = String::new();
    let mut dashed = false;
    for ch in surface.chars() {
        if ch.is_ascii_alphanumeric() {
            if out.len() >= 48 {
                break;
            }
            out.push(ch.to_ascii_lowercase());
            dashed = false;
        } else if !dashed && !out.is_empty() {
            out.push('-');
            dashed = true;
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "other".to_string()
    } else {
        out.to_string()
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the validated report vocabulary
fn render_occurrence(
    kind: DogfoodReportKind,
    surface: &str,
    summary: &str,
    details: Option<&str>,
    recommendation: Option<&str>,
    impact: Option<DogfoodImpact>,
    avoidable_calls: Option<u32>,
    evidence_ref: Option<&str>,
    client_context: Option<&str>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Dogfood occurrence");
    let _ = writeln!(out, "- kind: `{}`", kind.as_str());
    let _ = writeln!(out, "- surface: `{surface}`");
    if let Some(impact) = impact {
        let _ = writeln!(out, "- impact: `{}`", impact.as_str());
    }
    if let Some(calls) = avoidable_calls {
        let _ = writeln!(out, "- avoidable_calls: {calls}");
    }
    if let Some(reference) = evidence_ref {
        let _ = writeln!(out, "- evidence_ref (self-described): `{reference}`");
    }
    if let Some(context) = client_context {
        let _ = writeln!(out, "- client_context (self-described): `{context}`");
    }
    let _ = writeln!(
        out,
        "- intendant_build: `{}` (`{}`)",
        env!("CARGO_PKG_VERSION"),
        env!("INTENDANT_GIT_SHA")
    );
    let _ = writeln!(out, "\nSummary\n{summary}");
    if let Some(details) = details {
        let _ = writeln!(out, "\nDetails\n{details}");
    }
    if let Some(recommendation) = recommendation {
        let _ = writeln!(out, "\nRecommendation\n{recommendation}");
    }
    out
}

impl IntendantServer {
    pub(crate) async fn report_dogfood_inner(
        &self,
        params: DogfoodReportParams,
        actor: &crate::access::actor::ActorBinding,
    ) -> Result<serde_json::Value, String> {
        let surface = required_text("surface", params.surface, MAX_SURFACE_CHARS)?;
        let summary = required_text("summary", params.summary, MAX_SUMMARY_CHARS)?;
        let details = optional_text("details", params.details, MAX_DETAILS_CHARS)?;
        let recommendation = optional_text(
            "recommendation",
            params.recommendation,
            MAX_RECOMMENDATION_CHARS,
        )?;
        let evidence_ref =
            optional_text("evidence_ref", params.evidence_ref, MAX_EVIDENCE_REF_CHARS)?;
        let client_context = optional_text(
            "client_context",
            params.client_context,
            MAX_CLIENT_CONTEXT_CHARS,
        )?;
        if params
            .avoidable_calls
            .is_some_and(|calls| calls > MAX_AVOIDABLE_CALLS)
        {
            return Err(format!(
                "avoidable_calls is too large (max {MAX_AVOIDABLE_CALLS})"
            ));
        }

        let Some(agenda) = self.agenda_handle().await else {
            return Err("agenda unavailable on this daemon".to_string());
        };
        let fp = fingerprint(params.kind, &surface, &summary);
        let fingerprint_tag = format!("dogfood-fp-{fp}");
        let title = format!("{}: {}", surface, summary);
        let occurrence = render_occurrence(
            params.kind,
            &surface,
            &summary,
            details.as_deref(),
            recommendation.as_deref(),
            params.impact,
            params.avoidable_calls,
            evidence_ref.as_deref(),
            client_context.as_deref(),
        );
        let first_body = format!(
            "Exception-only Intendant dogfood feedback. Routine successful use is deliberately not recorded.\n\n{occurrence}"
        );
        let tags = vec![
            "dogfood-feedback".to_string(),
            format!("dogfood-{}", params.kind.as_str()),
            format!("surface-{}", surface_tag(&surface)),
            fingerprint_tag.clone(),
        ];

        agenda
            .apply_dogfood_report(
                fingerprint_tag,
                title,
                first_body,
                occurrence,
                tags,
                crate::agenda::AgendaActor::from_binding(actor),
            )
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_normalizes_case_and_whitespace() {
        assert_eq!(
            fingerprint(DogfoodReportKind::Issue, "MCP Facade", "Too   many calls"),
            fingerprint(DogfoodReportKind::Issue, "mcp facade", " too many CALLS ")
        );
        assert_ne!(
            fingerprint(DogfoodReportKind::Issue, "facade", "extra calls"),
            fingerprint(DogfoodReportKind::Efficiency, "facade", "extra calls")
        );
    }

    #[test]
    fn dogfood_report_uses_narrow_feedback_operation() {
        use crate::peer::access_policy::PeerOperation;

        assert_eq!(
            crate::mcp::mcp_tool_operation("report"),
            PeerOperation::FeedbackWrite
        );
        assert_ne!(
            crate::mcp::mcp_tool_operation("report"),
            PeerOperation::AgendaWrite
        );
        assert!(crate::peer::access_policy::profile_allows_operation(
            crate::peer::access_policy::AGENT_OPERATOR_PROFILE,
            PeerOperation::FeedbackWrite
        ));

        let roles = crate::access::iam::builtin_role_templates();
        let permissions = |role_id: &str| {
            roles
                .iter()
                .find(|role| role.id == role_id)
                .unwrap_or_else(|| panic!("{role_id} missing"))
                .permissions
                .as_slice()
        };
        assert!(permissions("role:operator")
            .iter()
            .any(|permission| permission == "feedback.write"));
        assert!(!permissions("role:observer")
            .iter()
            .any(|permission| permission == "feedback.write"));
    }

    #[test]
    fn repeated_report_merges_into_one_open_agenda_item() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = crate::agenda::AgendaHandle::new(
            crate::agenda::AgendaStore::open(dir.path()).expect("agenda"),
            EventBus::new(),
            dir.path(),
        );
        let actor = Some(crate::agenda::AgendaActor {
            principal: Some("principal:agent:test".to_string()),
            session_id: Some("session-test".to_string()),
            kind: Some("agent_session".to_string()),
        });
        let tags = vec![
            "dogfood-feedback".to_string(),
            "dogfood-issue".to_string(),
            "surface-facade".to_string(),
            "dogfood-fp-0123456789abcdef".to_string(),
        ];
        let first = handle
            .apply_dogfood_report(
                "dogfood-fp-0123456789abcdef".to_string(),
                "facade: duplicate friction".to_string(),
                "first body".to_string(),
                "first occurrence".to_string(),
                tags.clone(),
                actor.clone(),
            )
            .expect("first report");
        assert_eq!(first["status"], "created");
        assert_eq!(first["occurrences"], 1);

        let item_id = first["item_id"]
            .as_str()
            .expect("created report item id")
            .to_string();
        handle
            .apply(
                crate::agenda::AgendaCommand::Annotate {
                    id: item_id,
                    text: "owner triage note".to_string(),
                    source: Some("owner-triage".to_string()),
                },
                Some(crate::agenda::AgendaActor {
                    principal: Some("principal:owner:test".to_string()),
                    session_id: None,
                    kind: Some("dashboard".to_string()),
                }),
            )
            .expect("triage annotation");

        let second = handle
            .apply_dogfood_report(
                "dogfood-fp-0123456789abcdef".to_string(),
                "facade: duplicate friction".to_string(),
                "unused body".to_string(),
                "second occurrence".to_string(),
                tags,
                actor,
            )
            .expect("repeat report");
        assert_eq!(second["status"], "merged");
        assert_eq!(second["occurrences"], 2);
        assert_eq!(first["item_id"], second["item_id"]);

        let items = handle.snapshot();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].annotations.len(), 2);
        assert_eq!(
            items[0]
                .annotations
                .iter()
                .filter(|annotation| annotation.source.as_deref() == Some("dogfood-report"))
                .count(),
            1
        );
        assert_eq!(
            items[0].provenance.principal.as_deref(),
            Some("principal:agent:test")
        );
        assert_eq!(
            items[0].provenance.session_id.as_deref(),
            Some("session-test")
        );
    }
}
