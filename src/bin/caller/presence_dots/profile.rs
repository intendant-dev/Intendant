//! Read-only stable-dot/root cross-check. Provider-selected primary metadata
//! is discovery, not consent to bind Presence or to mutate that dot. No IDs,
//! names, account details, room contents, or raw responses enter the report.

use super::http::{HttpReport, ProfileClient, ProfileRoute};
use serde::Serialize;
use serde_json::Value;
use std::future::Future;

// No Debug/Serialize: these are private provider identities, not diagnostics.
#[derive(PartialEq, Eq)]
struct Selection {
    thread: String,
    generation: String,
    selected_at: String,
    // Optional internal aeon identity is NOT the TBO profile's route id.
    // Preserve it only for primary-selection consistency; never build a URL
    // or infer a stable profile, account or local authority from it.
    runtime_aeon: Option<String>,
    room: Option<String>,
}

struct Profile {
    dot: String,
    active_root: Option<String>,
    room: Option<String>,
}

/// Closed diagnostic vocabulary: field names are chosen here, and only JSON
/// type/contract booleans escape. Never copy a key, value, length, scalar or
/// parse error from a provider body into this observation.
#[derive(Debug, Serialize)]
pub(super) struct SchemaObservation {
    phase: &'static str,
    document_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    selection_type: Option<&'static str>,
    fields: Vec<SchemaField>,
}

#[derive(Debug, Serialize)]
struct SchemaField {
    name: &'static str,
    json_type: &'static str,
    contract_valid: bool,
}

fn json_type(value: Option<&Value>) -> &'static str {
    match value {
        None => "missing",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

impl SchemaObservation {
    fn new(
        phase: &'static str,
        document: &Value,
        value: &Value,
        checks: &[(&'static str, bool)],
    ) -> Self {
        Self {
            phase,
            document_type: json_type(Some(document)),
            selection_type: None,
            fields: checks
                .iter()
                .map(|&(name, contract_valid)| SchemaField {
                    name,
                    json_type: json_type(value.get(name)),
                    contract_valid,
                })
                .collect(),
        }
    }

    fn selection(document: &Value) -> Self {
        let null = Value::Null;
        let value = document.get("selection").unwrap_or(&null);
        let mut observed = Self::new(
            "primary_selection",
            document,
            value,
            &[
                (
                    "thread_id",
                    value
                        .get("thread_id")
                        .and_then(Value::as_str)
                        .is_some_and(valid_thread_id),
                ),
                ("generation", bounded_token(value, "generation").is_some()),
                ("selected_at", bounded_token(value, "selected_at").is_some()),
                (
                    "available",
                    value.get("available").is_some_and(Value::is_boolean),
                ),
                ("aeon_id", optional_opaque(value, "aeon_id").is_ok()),
                (
                    "messaging_room_id",
                    optional_id(value, "messaging_room_id", false).is_ok(),
                ),
            ],
        );
        observed.selection_type = Some(json_type(document.get("selection")));
        observed
    }

    fn profile(value: &Value) -> Self {
        Self::new(
            "dot_profile",
            value,
            value,
            &[
                (
                    "id",
                    value
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(valid_segment),
                ),
                (
                    "aeon_kind",
                    value.get("aeon_kind").and_then(Value::as_str) == Some("orbit"),
                ),
                (
                    "status",
                    value.get("status").and_then(Value::as_str) == Some("active"),
                ),
                (
                    "active_root_thread_id",
                    optional_id(value, "active_root_thread_id", true).is_ok(),
                ),
                (
                    "messaging_room_id",
                    optional_id(value, "messaging_room_id", false).is_ok(),
                ),
            ],
        )
    }

    fn root(value: &Value) -> Self {
        Self::new(
            "current_root",
            value,
            value,
            &[(
                "root_thread_id",
                matches!(value.get("root_thread_id"), Some(Value::Null))
                    || value
                        .get("root_thread_id")
                        .and_then(Value::as_str)
                        .is_some_and(valid_thread_id),
            )],
        )
    }
}

#[derive(Clone, Copy)]
enum Failure {
    SelectionSchema,
    PrimaryUnavailable,
    ProfileSchema,
    ProfileInactive,
    IdentityMismatch,
    RootSchema,
    RootUnavailable,
    RootChanged,
    SelectionChanged,
    RootMetadata,
}

impl Failure {
    fn report(self) -> HttpReport {
        let state = match self {
            Self::SelectionSchema => "primary_selection_schema_changed",
            Self::PrimaryUnavailable => "primary_dot_unavailable",
            Self::ProfileSchema => "dot_profile_schema_changed",
            Self::ProfileInactive => "dot_profile_inactive_or_unsupported",
            Self::IdentityMismatch => "dot_identity_mismatch",
            Self::RootSchema => "dot_root_schema_changed",
            Self::RootUnavailable => "dot_current_root_unavailable",
            Self::RootChanged => "dot_root_changed_during_inspection",
            Self::SelectionChanged => "primary_selection_changed_during_inspection",
            Self::RootMetadata => "dot_current_root_metadata_refused",
        };
        HttpReport::new(state, true, Some(200))
    }
}

// Path atoms are deliberately narrower than URL escaping. Unknown identity
// encodings fail closed; neither a provider body nor argv can redirect auth.
pub(super) fn valid_segment(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

pub(super) fn valid_thread_id(id: &str) -> bool {
    // Require a canonical UUID path, not parse_str's braced/URN forms.
    id.len() == 36 && uuid::Uuid::parse_str(id).is_ok() && valid_segment(id)
}

fn optional_id(value: &Value, field: &str, thread: bool) -> Result<Option<String>, ()> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id))
            if if thread {
                valid_thread_id(id)
            } else {
                valid_segment(id)
            } =>
        {
            Ok(Some(id.clone()))
        }
        _ => Err(()),
    }
}

fn optional_opaque(value: &Value, field: &str) -> Result<Option<String>, ()> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.len() <= 256 && !s.chars().any(char::is_control) => {
            Ok(Some(s.clone()))
        }
        _ => Err(()),
    }
}

fn bounded_token(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(str::to_string)
}

fn parse_selection(value: &Value) -> Result<Selection, Failure> {
    let value = match value.get("selection") {
        Some(Value::Null) => return Err(Failure::PrimaryUnavailable),
        Some(value) if value.is_object() => value,
        _ => return Err(Failure::SelectionSchema),
    };
    match value.get("available").and_then(Value::as_bool) {
        Some(true) => {}
        Some(false) => return Err(Failure::PrimaryUnavailable),
        None => return Err(Failure::SelectionSchema),
    }
    let thread = value
        .get("thread_id")
        .and_then(Value::as_str)
        .filter(|id| valid_thread_id(id))
        .ok_or(Failure::SelectionSchema)?;
    Ok(Selection {
        thread: thread.into(),
        generation: bounded_token(value, "generation").ok_or(Failure::SelectionSchema)?,
        selected_at: bounded_token(value, "selected_at").ok_or(Failure::SelectionSchema)?,
        runtime_aeon: optional_opaque(value, "aeon_id").map_err(|_| Failure::SelectionSchema)?,
        room: optional_id(value, "messaging_room_id", false)
            .map_err(|_| Failure::SelectionSchema)?,
    })
}

fn parse_profile(value: &Value) -> Result<Profile, Failure> {
    if !value.is_object() {
        return Err(Failure::ProfileSchema);
    }
    let dot = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| valid_segment(id))
        .ok_or(Failure::ProfileSchema)?;
    let kind = value.get("aeon_kind").and_then(Value::as_str);
    let status = value.get("status").and_then(Value::as_str);
    if kind.is_none() || status.is_none() {
        return Err(Failure::ProfileSchema);
    }
    if kind != Some("orbit") || status != Some("active") {
        return Err(Failure::ProfileInactive);
    }
    Ok(Profile {
        dot: dot.into(),
        active_root: optional_id(value, "active_root_thread_id", true)
            .map_err(|_| Failure::ProfileSchema)?,
        room: optional_id(value, "messaging_room_id", false).map_err(|_| Failure::ProfileSchema)?,
    })
}

fn parse_root(value: &Value) -> Result<String, Failure> {
    match value.get("root_thread_id") {
        Some(Value::Null) => Err(Failure::RootUnavailable),
        Some(Value::String(id)) if valid_thread_id(id) => Ok(id.clone()),
        _ => Err(Failure::RootSchema),
    }
}

fn selection_response(value: Value) -> Result<Selection, HttpReport> {
    parse_selection(&value).map_err(|failure| {
        failure
            .report()
            .with_schema(SchemaObservation::selection(&value))
    })
}

fn profile_response(value: Value) -> Result<Profile, HttpReport> {
    parse_profile(&value).map_err(|failure| {
        failure
            .report()
            .with_schema(SchemaObservation::profile(&value))
    })
}

fn root_response(value: Value) -> Result<String, HttpReport> {
    parse_root(&value).map_err(|failure| {
        failure
            .report()
            .with_schema(SchemaObservation::root(&value))
    })
}

fn cross_check(
    selection: &Selection,
    selected_profile: &Profile,
    root: &str,
    current_profile: &Profile,
) -> Result<bool, Failure> {
    if selected_profile.dot != current_profile.dot {
        return Err(Failure::IdentityMismatch);
    }
    if [selected_profile, current_profile]
        .iter()
        .any(|p| p.active_root.as_deref().is_some_and(|id| id != root))
    {
        return Err(Failure::RootChanged);
    }
    let rooms: Vec<&str> = [
        selection.room.as_deref(),
        selected_profile.room.as_deref(),
        current_profile.room.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if rooms.iter().any(|room| room != &rooms[0]) {
        return Err(Failure::IdentityMismatch);
    }
    // Missing room metadata does not invalidate the stable dot. It also does
    // not establish an address for messaging, so keep that fact distinct.
    Ok(current_profile.room.is_some()
        && (selection.room.is_some() || selected_profile.room.is_some()))
}

pub(super) async fn diagnose<F, Fut>(client: &ProfileClient, verify_root: F) -> HttpReport
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    match inspect(client, verify_root).await {
        Ok((rotated, room_linked)) => HttpReport::identity_verified(rotated, room_linked),
        Err(report) => report,
    }
}

async fn inspect<F, Fut>(client: &ProfileClient, verify_root: F) -> Result<(bool, bool), HttpReport>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let selection = selection_response(client.read(ProfileRoute::Primary).await?)?;
    let selected_profile = profile_response(
        client
            .read(ProfileRoute::ByThread(&selection.thread))
            .await?,
    )?;
    let root = root_response(
        client
            .read(ProfileRoute::RootThread(&selected_profile.dot))
            .await?,
    )?;
    let current_profile = profile_response(client.read(ProfileRoute::ByThread(&root)).await?)?;
    let room_linked = cross_check(&selection, &selected_profile, &root, &current_profile)
        .map_err(Failure::report)?;

    verify_root(root.clone())
        .await
        .map_err(|_| Failure::RootMetadata.report())?;

    // Roots and selections are mutable provider state. Reject observed drift
    // without automatic retry; these are consistency observations, not locks.
    let final_root = root_response(
        client
            .read(ProfileRoute::RootThread(&current_profile.dot))
            .await?,
    )?;
    if final_root != root {
        return Err(Failure::RootChanged.report());
    }
    let final_value = client.read(ProfileRoute::Primary).await?;
    let final_selection = parse_selection(&final_value).map_err(|_| {
        Failure::SelectionChanged
            .report()
            .with_schema(SchemaObservation::selection(&final_value))
    })?;
    if final_selection != selection {
        return Err(Failure::SelectionChanged.report());
    }
    Ok((root != selection.thread, room_linked))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const OLD: &str = "00000000-0000-7000-8000-000000000001";
    const ROOT: &str = "00000000-0000-7000-8000-000000000002";
    const DOT: &str = "fixture-dot-private";
    const ROOM: &str = "fixture-room-private";
    const RUNTIME: &str = "runtime:fixture/private-aeon";

    fn selection(thread: &str) -> Value {
        json!({"selection": {"thread_id": thread, "generation": "generation-1",
            "selected_at": "2026-10-03T00:00:00Z", "available": true,
            "aeon_id": RUNTIME, "messaging_room_id": ROOM}})
    }

    fn profile(root: &str) -> Value {
        json!({"id": DOT, "aeon_kind": "orbit", "status": "active",
            "active_root_thread_id": root, "messaging_room_id": ROOM,
            "display_name": "private dot name", "preview": "private transcript"})
    }

    #[test]
    fn primary_is_a_selection_envelope_not_a_flat_profile() {
        assert!(parse_selection(&profile(ROOT)).is_err());
        assert!(matches!(
            parse_selection(&json!({"selection": null})),
            Err(Failure::PrimaryUnavailable)
        ));
        let mut value = selection(ROOT);
        assert!(parse_selection(&value).is_ok());
        value["selection"]["available"] = json!(false);
        assert!(matches!(
            parse_selection(&value),
            Err(Failure::PrimaryUnavailable)
        ));
        value["selection"]["available"] = json!(true);
        for field in ["thread_id", "generation", "selected_at"] {
            let mut changed = value.clone();
            changed["selection"][field] = Value::Null;
            assert!(parse_selection(&changed).is_err());
        }
        for invalid in [
            "",
            "../escape",
            "dot/escape",
            "dot%2fescape",
            "dot?query",
            "dot\r\nheader",
            "ü",
        ] {
            let mut changed = value.clone();
            changed["selection"]["messaging_room_id"] = json!(invalid);
            assert!(parse_selection(&changed).is_err());
            assert!(!valid_segment(invalid));
        }
        assert!(!valid_thread_id(&format!("urn:uuid:{ROOT}")));
        assert!(!valid_segment(&"a".repeat(129)));
    }

    #[test]
    fn internal_aeon_metadata_is_opaque_not_a_profile_or_route_identity() {
        let selected = parse_selection(&selection(ROOT)).ok().unwrap();
        assert_eq!(selected.runtime_aeon.as_deref(), Some(RUNTIME));
        assert!(!valid_segment(RUNTIME));
        let first = parse_profile(&profile(ROOT)).ok().unwrap();
        let current = parse_profile(&profile(ROOT)).ok().unwrap();
        assert!(cross_check(&selected, &first, ROOT, &current).is_ok());
        let mut changed = selection(ROOT);
        changed["selection"]["aeon_id"] = json!("different:internal/aeon");
        assert!(parse_selection(&changed).ok().unwrap() != selected);
        for invalid in [
            json!(true),
            json!([RUNTIME]),
            json!({"id": RUNTIME}),
            json!("a".repeat(257)),
            json!("private\r\nheader"),
        ] {
            changed["selection"]["aeon_id"] = invalid;
            assert!(parse_selection(&changed).is_err());
        }
        for allowed in [Value::Null, json!(""), json!("../opaque-not-a-path")] {
            changed["selection"]["aeon_id"] = allowed;
            assert!(parse_selection(&changed).is_ok());
        }
    }

    #[test]
    fn schema_observations_use_only_closed_field_names_types_and_contract_booleans() {
        let mut value = selection(ROOT);
        value["selection"]["thread_id"] = json!("fixture-token private transcript");
        value["selection"]["generation"] = json!(42);
        value["selection"]["selected_at"] = json!({"fixture-account": "private dot name"});
        value["selection"]
            .as_object_mut()
            .unwrap()
            .remove("available");
        value["selection"]["fixture-account"] = json!("provider-secret");
        let report = selection_response(value).err().unwrap();
        assert_redacted(&report);
        let observed = serde_json::to_value(&report).unwrap()["schema_observation"].clone();
        assert_eq!(observed["phase"], "primary_selection");
        assert_eq!(observed["document_type"], "object");
        assert_eq!(observed["selection_type"], "object");
        assert_eq!(
            observed["fields"][0],
            json!({"name": "thread_id", "json_type": "string", "contract_valid": false})
        );
        assert_eq!(
            observed["fields"][1],
            json!({"name": "generation", "json_type": "number", "contract_valid": false})
        );
        assert_eq!(
            observed["fields"][2],
            json!({"name": "selected_at", "json_type": "object", "contract_valid": false})
        );
        assert_eq!(
            observed["fields"][3],
            json!({"name": "available", "json_type": "missing", "contract_valid": false})
        );
        for value in [
            Value::Null,
            json!(["provider-secret"]),
            json!({"provider-secret": "fixture-account"}),
        ] {
            let observed = SchemaObservation::selection(&value);
            assert_eq!(observed.selection_type, Some("missing"));
            assert!(!serde_json::to_string(&observed)
                .unwrap()
                .contains("provider-secret"));
        }
        let report = root_response(json!({"root_thread_id": {"provider-secret": "fixture-token"}}))
            .unwrap_err();
        assert_eq!(
            serde_json::to_value(&report).unwrap()["schema_observation"]["phase"],
            "current_root"
        );
        assert_redacted(&report);
        let report = profile_response(
            json!({"id": "provider-secret", "aeon_kind": ["fixture-token"], "status": "active"}),
        )
        .err()
        .unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap()["schema_observation"]["phase"],
            "dot_profile"
        );
        assert_redacted(&report);
        for report in [
            HttpReport::new("profile_response_unvalidated", true, Some(200)),
            HttpReport::identity_verified(false, false),
        ] {
            assert!(serde_json::to_value(report)
                .unwrap()
                .get("schema_observation")
                .is_none());
        }
    }

    #[test]
    fn rotated_roots_keep_identity_but_mismatches_never_bind() {
        let selected = parse_selection(&selection(OLD)).ok().unwrap();
        let first = parse_profile(&profile(ROOT)).ok().unwrap();
        let current = parse_profile(&profile(ROOT)).ok().unwrap();
        assert!(cross_check(&selected, &first, ROOT, &current).ok().unwrap());
        let mut changed = profile(ROOT);
        for field in ["id", "messaging_room_id"] {
            changed[field] = json!("different-private-identity");
            let current = parse_profile(&changed).ok().unwrap();
            assert!(matches!(
                cross_check(&selected, &first, ROOT, &current),
                Err(Failure::IdentityMismatch)
            ));
            changed = profile(ROOT);
        }
        changed["active_root_thread_id"] = json!(OLD);
        let current = parse_profile(&changed).ok().unwrap();
        assert!(matches!(
            cross_check(&selected, &first, ROOT, &current),
            Err(Failure::RootChanged)
        ));
        for (field, value) in [("status", "deleted"), ("aeon_kind", "unknown")] {
            let mut changed = profile(ROOT);
            changed[field] = json!(value);
            assert!(matches!(
                parse_profile(&changed),
                Err(Failure::ProfileInactive)
            ));
        }
        assert!(matches!(
            parse_root(&json!({"root_thread_id": null})),
            Err(Failure::RootUnavailable)
        ));
        assert!(parse_root(&json!({"root_thread_id": "../escape"})).is_err());

        let mut no_room = profile(ROOT);
        no_room.as_object_mut().unwrap().remove("messaging_room_id");
        let first = parse_profile(&no_room).ok().unwrap();
        let current = parse_profile(&no_room).ok().unwrap();
        let mut no_selection_room = selection(OLD);
        no_selection_room["selection"]
            .as_object_mut()
            .unwrap()
            .remove("messaging_room_id");
        let selection = parse_selection(&no_selection_room).ok().unwrap();
        assert!(!cross_check(&selection, &first, ROOT, &current)
            .ok()
            .unwrap());
    }

    async fn fixture(
        responses: Vec<(String, Value)>,
    ) -> (ProfileClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/tbo", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for (path, value) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(socket.read_u8().await.unwrap());
                    assert!(request.len() < 8192);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(request.starts_with(&format!("get /tbo/{path} http/1.1\r\n")));
                assert!(request.contains("authorization: bearer fixture-token\r\n"));
                assert!(request.contains("chatgpt-account-id: fixture-account\r\n"));
                assert!(request.contains("originator: intendant\r\n"));
                assert!(!request.contains("cookie:"));
                assert!(!request.contains(&RUNTIME.to_ascii_lowercase()));
                let body = value.to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let auth =
            crate::codex_cloud::fixture_subscription_auth("fixture-token", "fixture-account");
        (
            ProfileClient::fixture(base, auth.diagnostic_http_headers().unwrap()),
            server,
        )
    }

    fn prefix() -> Vec<(String, Value)> {
        vec![
            ("primary".into(), selection(OLD)),
            (format!("by-thread/{OLD}"), profile(ROOT)),
            (
                format!("{DOT}/root-thread"),
                json!({"root_thread_id": ROOT}),
            ),
            (format!("by-thread/{ROOT}"), profile(ROOT)),
        ]
    }

    fn assert_redacted(report: &HttpReport) {
        let json = serde_json::to_string(report).unwrap();
        let debug = format!("{report:?}");
        for secret in [
            OLD,
            ROOT,
            DOT,
            ROOM,
            RUNTIME,
            "fixture-token",
            "fixture-account",
            "provider-secret",
            "private transcript",
            "private dot name",
        ] {
            assert!(!json.contains(secret));
            assert!(!debug.contains(secret));
        }
        assert_eq!(
            serde_json::to_value(report).unwrap()["eligibility_validated"],
            false
        );
    }

    #[tokio::test]
    async fn identity_inspection_reads_only_metadata_and_sanitizes_its_report() {
        let mut responses = prefix();
        responses.push((
            format!("{DOT}/root-thread"),
            json!({"root_thread_id": ROOT}),
        ));
        responses.push(("primary".into(), selection(OLD)));
        let (client, server) = fixture(responses).await;
        let report = diagnose(&client, |root| async move {
            assert_eq!(root, ROOT);
            Ok(())
        })
        .await;
        assert_eq!(report.state_label(), "profile_identity_verified");
        assert!(report.identity_validated());
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["root_rotation_observed"], true);
        assert_eq!(value["messaging_room_linked"], true);
        assert_redacted(&report);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn drift_and_refused_root_metadata_do_not_claim_a_stable_binding() {
        for scenario in 0..4 {
            let mut responses = prefix();
            if scenario != 2 {
                responses.push((
                    format!("{DOT}/root-thread"),
                    json!({"root_thread_id": if scenario == 0 { OLD } else { ROOT }}),
                ));
            }
            if scenario == 1 || scenario == 3 {
                let mut changed = selection(OLD);
                if scenario == 1 {
                    changed["selection"]["generation"] = json!("generation-2");
                } else {
                    changed["selection"]["aeon_id"] = json!("different:internal/aeon");
                }
                responses.push(("primary".into(), changed));
            }
            let (client, server) = fixture(responses).await;
            let report = diagnose(&client, |_| async move {
                if scenario == 2 {
                    Err("fixture-token private transcript".into())
                } else {
                    Ok(())
                }
            })
            .await;
            assert_eq!(
                report.state_label(),
                match scenario {
                    0 => "dot_root_changed_during_inspection",
                    1 | 3 => "primary_selection_changed_during_inspection",
                    _ => "dot_current_root_metadata_refused",
                }
            );
            assert!(!report.identity_validated());
            assert_redacted(&report);
            server.await.unwrap();
        }
    }
}
