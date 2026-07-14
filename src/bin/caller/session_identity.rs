//! Single reader for persisted external-session identity.
//!
//! External-session identity — which backend (`source`) owns a wrapper
//! session and which backend conversation id (`backend_session_id`) resumes
//! it — is recorded as structured `session_identity` events in a session
//! dir's `session.jsonl` (written via `SessionLog::session_identity`; see
//! `emit_external_session_identity` and `persist_native_backend_session_id`
//! in main.rs for the producers). Every resolver that answers "what external
//! identity does this session have?" scans through this module, so the
//! matching and parsing rules cannot diverge per call site again (audit
//! finding 15.1: mcp and the session supervisor grew twin resolvers with
//! different rules).
//!
//! Callers keep their own *policy* on top of the scan: the supervisor's
//! resume authority accepts only the latest wrapper-matching event with a
//! canonically-shaped backend id, while MCP start-target discovery also
//! falls back to an unambiguous sole identity, the launch config, and —
//! last — the legacy scrape below.
//!
//! Session dirs written before 2026-07 may predate the structured event;
//! for those dirs only, the legacy parsers at the bottom recover identity
//! from human log lines. That grammar is frozen — never extend it. New
//! identity facts must be written as `session_identity` events at the
//! moment they become known, not mined from prose later.

use std::path::Path;

/// One persisted identity fact from a `session_identity` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedExternalIdentity {
    /// Wrapper (Intendant) session id recorded on the event, if any.
    pub wrapper_id: Option<String>,
    /// Normalized short backend source ("codex", "claude-code", …).
    pub source: String,
    /// Backend conversation id that resumes the session.
    pub backend_session_id: String,
}

/// Everything one pass over a `session.jsonl` yields about identity.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct IdentityScan {
    /// Every parseable structured identity fact, in log order. Callers that
    /// scan multiple candidate directories must match these exact facts
    /// rather than applying the single-directory fallback policy below.
    pub identities: Vec<PersistedExternalIdentity>,
    /// Latest parseable event whose wrapper id matches the requested
    /// session under [`wrapper_matches`]. Later events supersede earlier
    /// ones — identity upgrades (placeholder → native id) append, never
    /// rewrite.
    pub latest_matching: Option<PersistedExternalIdentity>,
    /// First parseable identity event in the log, regardless of wrapper.
    pub first: Option<PersistedExternalIdentity>,
    /// Count of parseable identity events in the log.
    pub count: usize,
    /// LEGACY: source scraped from pre-2026-07 human log lines.
    pub legacy_source: Option<String>,
    /// LEGACY: resume id scraped from pre-2026-07 human log lines.
    pub legacy_resume_id: Option<String>,
}

impl IdentityScan {
    /// The identity that names the requested wrapper, else the log's sole
    /// identity when there is exactly one (unambiguous even though it names
    /// another id form — e.g. a dir addressed by an alias the events don't
    /// carry). Multiple foreign identities resolve to nothing rather than
    /// guessing.
    pub(crate) fn matching_or_unique(self) -> Option<PersistedExternalIdentity> {
        let count = self.count;
        self.latest_matching
            .or_else(|| (count == 1).then_some(self.first).flatten())
    }
}

/// Scan a session dir: canonical id from `session_meta.json`, then the
/// identity pass over `session.jsonl` (`None` when the log is unreadable).
pub(crate) fn scan_session_dir(log_dir: &Path, requested_id: &str) -> Option<IdentityScan> {
    let canonical_session_id = canonical_session_id_from_meta(log_dir);
    let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).ok()?;
    Some(scan_session_log(
        &contents,
        requested_id,
        canonical_session_id.as_deref(),
    ))
}

/// One pass over `session.jsonl` contents collecting structured identity
/// events and (for pre-event dirs) the legacy prose scrape.
pub(crate) fn scan_session_log(
    contents: &str,
    requested_id: &str,
    canonical_session_id: Option<&str>,
) -> IdentityScan {
    let mut scan = IdentityScan::default();
    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("event").and_then(|v| v.as_str()) == Some("session_identity") {
            if let Some(identity) = identity_from_event(&value) {
                scan.count += 1;
                if wrapper_matches(
                    identity.wrapper_id.as_deref(),
                    requested_id,
                    canonical_session_id,
                ) {
                    scan.latest_matching = Some(identity.clone());
                }
                if scan.first.is_none() {
                    scan.first = Some(identity.clone());
                }
                scan.identities.push(identity);
            }
            continue;
        }
        let message = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
        if scan.legacy_source.is_none() {
            scan.legacy_source = legacy_source_from_log_message(message);
        }
        if scan.legacy_resume_id.is_none() {
            scan.legacy_resume_id = legacy_resume_id_from_log_message(message);
        }
    }
    scan
}

fn identity_from_event(value: &serde_json::Value) -> Option<PersistedExternalIdentity> {
    let data = value.get("data")?;
    let source =
        json_str_field(data, "source").and_then(|source| normalized_external_source(&source))?;
    let backend_session_id =
        json_str_field(data, "backend_session_id").and_then(|id| clean_external_resume_id(&id))?;
    Some(PersistedExternalIdentity {
        wrapper_id: json_str_field(data, "session_id"),
        source,
        backend_session_id,
    })
}

/// Whether an identity event's wrapper id names the requested session:
/// equal, an extension of the requested id (dirs are often addressed by a
/// prefix of their full name), or equal to / extending toward the
/// `session_meta.json` canonical id the dir was renamed to.
pub(crate) fn wrapper_matches(
    identity_session_id: Option<&str>,
    requested_id: &str,
    canonical_session_id: Option<&str>,
) -> bool {
    let Some(identity_session_id) = identity_session_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return false;
    };
    identity_session_id == requested_id
        || identity_session_id.starts_with(requested_id)
        || canonical_session_id
            .map(|canonical| {
                identity_session_id == canonical || canonical.starts_with(requested_id)
            })
            .unwrap_or(false)
}

/// Normalize a free-form source string to a known backend's short form
/// ("codex", "claude-code", …); `None` for non-external sources.
pub(crate) fn normalized_external_source(source: &str) -> Option<String> {
    let normalized = crate::session_names::normalize_source(source);
    crate::external_agent::AgentBackend::from_str_loose(&normalized)
        .map(|backend| backend.as_short_str().to_string())
}

/// The canonical session id a dir's `session_meta.json` records (dirs get
/// renamed; the meta keeps the id identity events were written under).
pub(crate) fn canonical_session_id_from_meta(log_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(log_dir.join("session_meta.json")).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    json_str_field(&value, "session_id")
}

fn json_str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

// ---------------------------------------------------------------------------
// LEGACY prose scrape — pre-2026-07 session dirs only. Sessions from before
// the structured `session_identity` event recorded identity only as human
// log lines; these parsers recover it for those dirs. The grammar is frozen:
// never extend it, never make new code depend on it.
// ---------------------------------------------------------------------------

/// LEGACY: source from a `"Mode: external agent (<source>)"` log line.
fn legacy_source_from_log_message(message: &str) -> Option<String> {
    let mode = message.strip_prefix("Mode: external agent (")?;
    let (source, _) = mode.split_once(')')?;
    normalized_external_source(source)
}

/// LEGACY: resume id from `"External agent thread: <id>"` or a
/// `"Mode: external agent … thread: <id>"` log line.
fn legacy_resume_id_from_log_message(message: &str) -> Option<String> {
    if let Some(thread_id) = message.strip_prefix("External agent thread: ") {
        return clean_external_resume_id(thread_id);
    }
    if message.starts_with("Mode: external agent") {
        if let Some((_, thread_id)) = message.rsplit_once("thread: ") {
            return clean_external_resume_id(thread_id);
        }
    }
    None
}

/// Strip the quoting/punctuation prose lines wrapped ids in.
fn clean_external_resume_id(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_matches(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | ',' | ';'));
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_line(wrapper: &str, source: &str, backend: &str) -> String {
        serde_json::json!({
            "event": "session_identity",
            "data": {
                "session_id": wrapper,
                "source": source,
                "backend_session_id": backend,
            }
        })
        .to_string()
    }

    #[test]
    fn structured_event_beats_legacy_prose() {
        let contents = format!(
            "{}\n{}\n",
            serde_json::json!({
                "event": "info",
                "message": "Mode: external agent (Codex) via presence, thread: prose-id"
            }),
            identity_line("sess-1", "codex", "event-id"),
        );
        let scan = scan_session_log(&contents, "sess-1", None);
        assert_eq!(scan.legacy_resume_id.as_deref(), Some("prose-id"));
        let identity = scan.matching_or_unique().expect("identity");
        assert_eq!(identity.backend_session_id, "event-id");
        assert_eq!(identity.source, "codex");
    }

    #[test]
    fn latest_matching_event_supersedes_earlier_ones() {
        let contents = format!(
            "{}\n{}\n",
            identity_line("sess-1", "claude-code", "placeholder-upgraded-from"),
            identity_line("sess-1", "claude-code", "native-id"),
        );
        let scan = scan_session_log(&contents, "sess-1", None);
        assert_eq!(
            scan.latest_matching.expect("match").backend_session_id,
            "native-id"
        );
    }

    #[test]
    fn sole_foreign_identity_resolves_but_ambiguity_does_not() {
        let sole = scan_session_log(&identity_line("other", "codex", "id-a"), "sess-1", None);
        assert!(sole.latest_matching.is_none());
        assert_eq!(
            sole.matching_or_unique()
                .expect("unique")
                .backend_session_id,
            "id-a"
        );

        let ambiguous = format!(
            "{}\n{}\n",
            identity_line("other-a", "codex", "id-a"),
            identity_line("other-b", "codex", "id-b"),
        );
        let scan = scan_session_log(&ambiguous, "sess-1", None);
        assert_eq!(scan.count, 2);
        assert!(scan.matching_or_unique().is_none());
    }

    #[test]
    fn wrapper_matching_covers_prefix_and_canonical_forms() {
        assert!(wrapper_matches(Some("sess-1"), "sess-1", None));
        assert!(wrapper_matches(Some("sess-1-full-name"), "sess-1", None));
        assert!(wrapper_matches(
            Some("canonical"),
            "sess-1",
            Some("canonical")
        ));
        assert!(wrapper_matches(
            Some("anything"),
            "sess-1",
            Some("sess-1-canonical")
        ));
        assert!(!wrapper_matches(Some("unrelated"), "sess-1", None));
        assert!(!wrapper_matches(None, "sess-1", None));
        assert!(!wrapper_matches(Some("  "), "sess-1", None));
    }

    #[test]
    fn legacy_only_dir_still_resolves_source_and_resume_id() {
        let contents = concat!(
            "{\"event\":\"info\",\"message\":\"Mode: external agent (Claude Code)\"}\n",
            "not json\n",
            "{\"event\":\"debug\",\"message\":\"External agent thread: `quoted-id`\"}\n",
        );
        let scan = scan_session_log(contents, "sess-1", None);
        assert_eq!(scan.count, 0);
        assert_eq!(scan.legacy_source.as_deref(), Some("claude-code"));
        assert_eq!(scan.legacy_resume_id.as_deref(), Some("quoted-id"));
    }

    #[test]
    fn non_external_and_malformed_events_are_skipped() {
        let contents = format!(
            "{}\n{}\n",
            identity_line("sess-1", "not-a-backend", "id-a"),
            serde_json::json!({"event": "session_identity", "data": {"session_id": "sess-1"}}),
        );
        let scan = scan_session_log(&contents, "sess-1", None);
        assert_eq!(scan.count, 0);
        assert!(scan.matching_or_unique().is_none());
    }

    #[test]
    fn canonical_session_id_reads_session_meta() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(canonical_session_id_from_meta(dir.path()), None);
        std::fs::write(
            dir.path().join("session_meta.json"),
            r#"{"session_id": "canonical-name", "project_root": "/tmp/x"}"#,
        )
        .expect("write meta");
        assert_eq!(
            canonical_session_id_from_meta(dir.path()).as_deref(),
            Some("canonical-name")
        );
    }
}
