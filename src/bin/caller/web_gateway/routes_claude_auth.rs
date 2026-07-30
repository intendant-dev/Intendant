//! Claude sign-in ceremony routes (`/api/claude-auth/*`).
//!
//! Thin transport shims over [`crate::claude_auth_ceremony`]: the shared
//! `*_api_response` cores serve both the HTTP rows and their datachannel
//! twins. Beyond the rows' `credentials.manage` IAM gate, every leaf
//! hard-refuses hosted-provenance clients (defense in depth — the central
//! evaluator already denies hosted sessions, but a credential ceremony
//! must never ride a rendezvous-mediated transport even if that gate
//! regressed), and `start` refuses daemons whose Claude/Anthropic
//! credentials are custody-managed off-box ([`custody_refusal`]).

use super::*;
use crate::auth_ceremony::{self, CodeRefusal, Provider, StartRefusal};
use crate::claude_auth_ceremony::{self, custody_refusal, SUPPORTED_MODE};

/// Mandated refusal copy for rendezvous-mediated clients (shared with
/// the Codex ceremony routes).
pub(crate) const HOSTED_REFUSAL: &str = "credential ceremonies require a trusted direct connection";

/// True when this request's session has hosted provenance
/// ([`crate::access::iam::is_hosted_session`] when the IAM snapshot is on
/// hand; the principal-borne facts otherwise).
pub(crate) fn request_authority_is_hosted(access: &RequestAuthority) -> bool {
    match access.iam_state.as_ref() {
        Some(state) => crate::access::iam::is_hosted_session(state, &access.principal),
        None => {
            access.principal.hosted_connect
                || access.principal.authn_kind.as_deref() == Some("connect_account")
        }
    }
}

pub(crate) fn hosted_refusal_response() -> ApiResponse {
    ApiResponse::json(
        403,
        JsonBody::Value(serde_json::json!({
            "error": HOSTED_REFUSAL,
            "refusal": "hosted_provenance",
        })),
    )
}

/// POST /api/claude-auth/start + the tunnel's `api_claude_auth_start`.
/// `hosted_provenance` and `project_root` arrive from the transport edge.
pub(crate) fn claude_auth_start_api_response(
    hosted_provenance: bool,
    body_text: &str,
    project_root: Option<&std::path::Path>,
) -> ApiResponse {
    if hosted_provenance {
        return hosted_refusal_response();
    }
    if let Some(refusal) = custody_refusal() {
        return ApiResponse::json(
            403,
            JsonBody::Value(serde_json::json!({
                "error": refusal,
                "refusal": "custody",
            })),
        );
    }
    let mode = match start_mode_from_body(body_text) {
        Ok(mode) => mode,
        Err(error) => return ApiResponse::json_error(400, error),
    };
    let command = claude_auth_ceremony::configured_claude_command(project_root);
    match claude_auth_ceremony::start_ceremony(&command, &mode) {
        Ok(()) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "ok": true,
                "status": auth_ceremony::manager().status_value_for(Provider::Claude),
            })),
        ),
        Err(StartRefusal::Busy) => {
            ApiResponse::json_error(409, "a sign-in ceremony is already running on this daemon")
        }
        Err(StartRefusal::BadRequest(error)) => ApiResponse::json_error(400, error),
        Err(StartRefusal::Spawn(error)) => {
            ApiResponse::json_error(500, format!("could not start the sign-in process: {error}"))
        }
    }
}

/// The `{"mode": …}` body (empty body = the claude.ai default).
fn start_mode_from_body(body_text: &str) -> Result<String, String> {
    let trimmed = body_text.trim();
    if trimmed.is_empty() {
        return Ok(SUPPORTED_MODE.to_string());
    }
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON body: {e}"))?;
    match value.get("mode") {
        None | Some(serde_json::Value::Null) => Ok(SUPPORTED_MODE.to_string()),
        Some(serde_json::Value::String(mode)) => Ok(mode.trim().to_string()),
        Some(_) => Err("\"mode\" must be a string".to_string()),
    }
}

/// Whether a status payload carries `reload_candidates`: exactly at
/// `success`, the moment the Vault card offers the reload chips.
pub(crate) fn status_wants_reload_candidates(status: &serde_json::Value) -> bool {
    status.get("phase").and_then(|v| v.as_str()) == Some("success")
}

/// Merge the live registry's reload candidates into a status payload —
/// one body, so the list arrives atomically with the polled `success`
/// response (no separate fetch, no cache window, no truncation).
pub(crate) fn status_with_reload_candidates(
    mut status: serde_json::Value,
    candidates: Vec<crate::session_supervisor::ReloadCandidate>,
) -> serde_json::Value {
    status["reload_candidates"] =
        serde_json::to_value(candidates).unwrap_or_else(|_| serde_json::json!([]));
    status
}

/// The provider's auth-status payload; at `success` it carries
/// `reload_candidates` derived from the LIVE session registry — the
/// exact candidate set `route_reload_credentials` accepts, replacing the
/// disk-catalog filtering that listed dead pre-restart sessions as live
/// and aged parked ones off. Shared by all three providers' status
/// routes and their tunnel twins.
pub(crate) async fn auth_status_payload(provider: Provider) -> serde_json::Value {
    let status = auth_ceremony::manager().status_value_for(provider);
    if !status_wants_reload_candidates(&status) {
        return status;
    }
    let source = provider.agent_backend().as_short_str();
    let candidates = match crate::session_supervisor::published_live_session_registry() {
        Some(registry) => registry.reload_candidates_for_source(source).await,
        // No supervisor ⇒ no supervised sessions ⇒ nothing reloadable:
        // an empty list is the truthful answer, not an omission.
        None => Vec::new(),
    };
    status_with_reload_candidates(status, candidates)
}

/// GET /api/claude-auth/status + the tunnel's `api_claude_auth_status`.
pub(crate) async fn claude_auth_status_api_response(hosted_provenance: bool) -> ApiResponse {
    if hosted_provenance {
        return hosted_refusal_response();
    }
    ApiResponse::json(
        200,
        JsonBody::Value(auth_status_payload(Provider::Claude).await),
    )
}

/// POST /api/claude-auth/code + the tunnel's `api_claude_auth_code`.
pub(crate) fn claude_auth_code_api_response(
    hosted_provenance: bool,
    body_text: &str,
) -> ApiResponse {
    if hosted_provenance {
        return hosted_refusal_response();
    }
    let code = match serde_json::from_str::<serde_json::Value>(body_text.trim()) {
        Ok(value) => match value.get("code").and_then(|v| v.as_str()) {
            Some(code) => code.to_string(),
            None => return ApiResponse::json_error(400, "body must carry a \"code\" string"),
        },
        Err(e) => return ApiResponse::json_error(400, format!("invalid JSON body: {e}")),
    };
    match auth_ceremony::manager().submit_code(&code) {
        Ok(phase) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "ok": true,
                "phase": phase.as_str(),
            })),
        ),
        Err(CodeRefusal::Invalid(error)) => ApiResponse::json_error(400, error),
        Err(CodeRefusal::State(error)) => ApiResponse::json_error(409, error),
    }
}

/// POST /api/claude-auth/cancel + the tunnel's `api_claude_auth_cancel`.
pub(crate) fn claude_auth_cancel_api_response(hosted_provenance: bool) -> ApiResponse {
    if hosted_provenance {
        return hosted_refusal_response();
    }
    match auth_ceremony::manager().cancel() {
        Ok(()) => ApiResponse::json(
            200,
            JsonBody::Value(serde_json::json!({
                "ok": true,
                "phase": "cancelled",
            })),
        ),
        Err(error) => ApiResponse::json_error(409, error),
    }
}

pub(crate) async fn handle_claude_auth_start(
    stream: DemuxStream,
    body_text: String,
    project_root: Option<PathBuf>,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = claude_auth_start_api_response(
        request_authority_is_hosted(access),
        &body_text,
        project_root.as_deref(),
    );
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_claude_auth_status(
    stream: DemuxStream,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = claude_auth_status_api_response(request_authority_is_hosted(access)).await;
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_claude_auth_code(
    stream: DemuxStream,
    body_text: String,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = claude_auth_code_api_response(request_authority_is_hosted(access), &body_text);
    write_api_response(stream, response, cors, fleet_origin).await;
}

pub(crate) async fn handle_claude_auth_cancel(
    stream: DemuxStream,
    access: &RequestAuthority,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = claude_auth_cancel_api_response(request_authority_is_hosted(access));
    write_api_response(stream, response, cors, fleet_origin).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_status_and_body(response: ApiResponse) -> (u16, serde_json::Value) {
        match response {
            ApiResponse::Json { status, body, .. } => {
                let text = body.into_string();
                (status, serde_json::from_str(&text).unwrap())
            }
            _ => panic!("claude-auth responses are JSON"),
        }
    }

    fn hosted_principal() -> crate::access::iam::AccessPrincipal {
        let mut principal =
            crate::access::iam::AccessPrincipal::root_dashboard_session("test", "test");
        principal.hosted_connect = true;
        principal
    }

    #[tokio::test]
    async fn hosted_provenance_is_refused_on_every_leaf() {
        for response in [
            claude_auth_start_api_response(true, "", None),
            claude_auth_status_api_response(true).await,
            claude_auth_code_api_response(true, "{\"code\":\"x\"}"),
            claude_auth_cancel_api_response(true),
        ] {
            let (status, body) = response_status_and_body(response);
            assert_eq!(status, 403);
            assert_eq!(
                body["error"].as_str().unwrap(),
                "credential ceremonies require a trusted direct connection"
            );
            assert_eq!(body["refusal"], "hosted_provenance");
        }
    }

    #[test]
    fn request_authority_hosted_detection_uses_principal_facts_without_state() {
        let hosted = RequestAuthority {
            principal: hosted_principal(),
            iam_state: None,
        };
        assert!(request_authority_is_hosted(&hosted));
        let direct = RequestAuthority {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session("test", "test"),
            iam_state: None,
        };
        assert!(!request_authority_is_hosted(&direct));
        // With a state snapshot the central evaluator's provenance rules
        // decide — hosted_connect stays authoritative through it.
        let hosted_with_state = RequestAuthority {
            principal: hosted_principal(),
            iam_state: Some(std::sync::Arc::new(
                crate::access::iam::LocalIamState::default(),
            )),
        };
        assert!(request_authority_is_hosted(&hosted_with_state));
    }

    /// The reload list rides INSIDE the status payload: the same JSON
    /// body that announces `success` carries the candidates, so the
    /// Vault card can never render success with a list fetched at a
    /// different moment (the old separate `api_sessions` fetch could be
    /// served a stale cache body, or be skipped entirely on a fast
    /// "Sign in again"). Non-success phases carry no list at all.
    #[test]
    fn vault_list_arrives_atomically_with_success() {
        let success = serde_json::json!({
            "provider": "claude",
            "phase": "success",
        });
        assert!(status_wants_reload_candidates(&success));
        let merged = status_with_reload_candidates(
            success,
            vec![
                crate::session_supervisor::ReloadCandidate {
                    session_id: "wrapper-1".to_string(),
                    source: "claude-code".to_string(),
                    name: Some("steward pass".to_string()),
                    phase: "waiting_rate_limit".to_string(),
                    reload: None,
                },
                crate::session_supervisor::ReloadCandidate {
                    session_id: "wrapper-2".to_string(),
                    source: "claude-code".to_string(),
                    name: None,
                    phase: "running".to_string(),
                    reload: Some(crate::session_supervisor::ReloadLifecycle {
                        state: crate::session_supervisor::ReloadLifecycleState::Failed,
                        at_unix_ms: 1_785_365_411_029,
                        error: Some("could not respawn claude-code: exec failed".to_string()),
                    }),
                },
            ],
        );
        assert_eq!(merged["phase"], "success");
        assert_eq!(
            merged["reload_candidates"],
            serde_json::json!([
                {
                    "session_id": "wrapper-1",
                    "source": "claude-code",
                    "name": "steward pass",
                    "phase": "waiting_rate_limit",
                },
                {
                    "session_id": "wrapper-2",
                    "source": "claude-code",
                    "phase": "running",
                    "reload": {
                        "state": "failed",
                        "at_unix_ms": 1_785_365_411_029u64,
                        "error": "could not respawn claude-code: exec failed",
                    },
                },
            ]),
            "candidates and the success phase share one body; the daemon's \
             reload lifecycle rides each row verbatim"
        );

        // An alive daemon with nothing to reload states that explicitly.
        let empty =
            status_with_reload_candidates(serde_json::json!({ "phase": "success" }), Vec::new());
        assert_eq!(empty["reload_candidates"], serde_json::json!([]));

        for phase in ["idle", "starting", "awaiting_user", "verifying", "failed"] {
            assert!(
                !status_wants_reload_candidates(&serde_json::json!({ "phase": phase })),
                "{phase} payloads never carry a reload list"
            );
        }
    }

    /// "Sign in again" (and every success render) can only show the
    /// list the daemon just served: the vault fragment renders the
    /// status payload's own `reload_candidates`, and the legacy
    /// second-fetch lane — a truncated `api_sessions` snapshot behind
    /// freshness guards, whose phase-transition force a fast re-sign-in
    /// skipped (`lastPhase` never reset) — is gone. Sliced to the
    /// fragment so other panes' legitimate `api_sessions` uses don't
    /// blur the pin.
    #[test]
    fn sign_in_again_always_refreshes() {
        let fragment = vault_fragment();

        assert!(
            fragment.contains("reload_candidates"),
            "the success card must render the status payload's reload_candidates"
        );
        for legacy in [
            "api_sessions",
            "lastPhase",
            "AGENT_SIGNIN_TERMINAL_STATUSES",
        ] {
            assert!(
                !fragment.contains(legacy),
                "the vault sign-in lane must not rebuild its own session list ({legacy} found)"
            );
        }
    }

    fn vault_fragment() -> &'static str {
        let app = include_str!("../../../../static/app.html");
        let banner = "/* ── static/app/32-vault-custody.js ── */";
        let start = app
            .find(banner)
            .expect("vault fragment banner not found in app.html");
        let rest = &app[start + banner.len()..];
        let end = rest.find("/* ── static/app/").unwrap_or(rest.len());
        &rest[..end]
    }

    /// The Vault card holds NO reload-request memory of its own: the
    /// page-lifetime request Set that latched delivered requests forever
    /// (blocking re-reload at the button level while the daemon would
    /// happily accept a repeat) is gone entirely, and the row chips
    /// render the candidate's served `reload` lifecycle instead.
    #[test]
    fn stale_requests_never_block_a_fresh_ceremony() {
        let fragment = vault_fragment();
        assert!(
            !fragment.contains("reloadRequested"),
            "no client-side reload-request memory may exist — the served lifecycle is the only state"
        );
        assert!(
            fragment.contains("session.reload"),
            "row chips must render the candidate's served reload lifecycle"
        );
    }

    /// The Reload button is gated ONLY by an in-flight served state:
    /// terminal states (done/failed) and rows with no lifecycle always
    /// render it — re-reload is always available, and a lingering
    /// request can never block a fresh ceremony.
    #[test]
    fn terminal_states_always_restore_the_button() {
        let fragment = vault_fragment();
        assert!(
            fragment.contains("!lifecycle || !lifecycle.inFlight"),
            "the button must render for every terminal or absent lifecycle state"
        );
        assert!(
            fragment.contains("'Reload credentials'"),
            "the per-row reload button must exist"
        );
    }

    /// Every row renders the session grid's short-id dialect (first 8)
    /// beside the name, with the full id on the tooltip — duplicate
    /// derived names stay distinguishable with data the payload already
    /// serves.
    #[test]
    fn rows_render_short_session_ids() {
        let fragment = vault_fragment();
        assert!(
            fragment.contains("sessionId.slice(0, 8)"),
            "rows must render the first-8 short-id chip"
        );
        assert!(
            fragment.contains("idChip.title = sessionId"),
            "the id chip must carry the full session id as its tooltip"
        );
    }

    /// "Reload all" dispatches the daemon fan-out (never a client loop),
    /// and takes its `source` off the served candidate rows rather than
    /// a client-side provider→backend map.
    #[test]
    fn reload_all_rides_the_served_candidate_set() {
        let fragment = vault_fragment();
        assert!(
            fragment.contains("reload_credentials_all"),
            "reload-all must dispatch the daemon fan-out intent"
        );
        assert!(
            fragment.contains("candidates[0]?.source"),
            "the fan-out source must derive from the served candidates"
        );
    }

    #[test]
    fn code_body_shapes_are_validated() {
        let (status, _) =
            response_status_and_body(claude_auth_code_api_response(false, "not json"));
        assert_eq!(status, 400);
        let (status, _) =
            response_status_and_body(claude_auth_code_api_response(false, "{\"nope\":1}"));
        assert_eq!(status, 400);
        // A well-formed body with no live ceremony is a state conflict.
        let (status, _) =
            response_status_and_body(claude_auth_code_api_response(false, "{\"code\":\"abc\"}"));
        assert_eq!(status, 409);
    }

    #[test]
    fn start_mode_parsing_defaults_to_claudeai() {
        assert_eq!(start_mode_from_body("").unwrap(), SUPPORTED_MODE);
        assert_eq!(start_mode_from_body("{}").unwrap(), SUPPORTED_MODE);
        assert_eq!(
            start_mode_from_body("{\"mode\":\"claudeai\"}").unwrap(),
            "claudeai"
        );
        assert_eq!(
            start_mode_from_body("{\"mode\":\"console\"}").unwrap(),
            "console"
        );
        assert!(start_mode_from_body("{\"mode\":3}").is_err());
        assert!(start_mode_from_body("nope").is_err());
    }

    #[test]
    fn cancel_without_ceremony_is_a_state_conflict() {
        let (status, body) = response_status_and_body(claude_auth_cancel_api_response(false));
        assert_eq!(status, 409);
        assert!(body["error"].as_str().unwrap().contains("no sign-in"));
    }
}
