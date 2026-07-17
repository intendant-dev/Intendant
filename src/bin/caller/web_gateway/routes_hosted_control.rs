use super::*;

pub(crate) fn is_public_hosted_control_path(method: &str, path: &str) -> bool {
    matches!(
        (method, path),
        ("GET", "/api/hosted-control/bootstrap")
            | ("POST", "/api/hosted-control/requests")
            | ("POST", "/api/hosted-control/requests/poll")
            | ("POST", "/api/hosted-control/anchor-decisions")
    )
}

fn json_value<T: serde::Serialize>(value: T) -> ApiResponse {
    match serde_json::to_value(value) {
        Ok(value) => ApiResponse::json(200, JsonBody::Value(value)),
        Err(error) => {
            ApiResponse::json_error(500, format!("serialize hosted-control response: {error}"))
        }
    }
}

pub(crate) fn fleet_origin_from_request(header_text: &str, is_tls: bool) -> Result<String, String> {
    if !is_tls {
        return Err("hosted control requires daemon-terminated HTTPS".to_string());
    }
    let authority = extract_host_header(header_text)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "request has no Host authority".to_string())?;
    let url = url::Url::parse(&format!("https://{authority}"))
        .map_err(|_| "request Host authority is invalid".to_string())?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("request Host authority is invalid".to_string());
    }
    Ok(url.origin().ascii_serialization())
}

pub(crate) fn hosted_request_proof_from_headers(
    header_text: &str,
) -> Result<crate::access::hosted_control::HostedRequestProof, String> {
    let required = |name: &str| {
        http_header_value(header_text, name)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| format!("missing {name} header"))
    };
    Ok(crate::access::hosted_control::HostedRequestProof {
        lease_id: required("x-intendant-hosted-lease")?,
        nonce: required("x-intendant-hosted-nonce")?,
        timestamp_unix_ms: required("x-intendant-hosted-timestamp")?
            .parse()
            .map_err(|_| "x-intendant-hosted-timestamp is invalid".to_string())?,
        signature: required("x-intendant-hosted-proof")?,
    })
}

pub(crate) async fn handle_hosted_control_bootstrap(
    stream: DemuxStream,
    runtime: Arc<crate::access::hosted_control::HostedControlRuntime>,
    header_text: &str,
    is_tls: bool,
    cors: crate::gateway_routes::CorsPosture,
) {
    let response = match fleet_origin_from_request(header_text, is_tls)
        .and_then(|origin| runtime.bootstrap(&origin))
    {
        Ok(bootstrap) => json_value(bootstrap),
        Err(error) if !runtime.configured() => ApiResponse::json_error(404, error),
        Err(error) if !runtime.enabled() => ApiResponse::json_error(503, error),
        Err(error) => ApiResponse::json_error(400, error),
    };
    write_api_response(stream, response, cors, None).await;
}

pub(crate) async fn handle_hosted_control_request_create(
    stream: DemuxStream,
    body: String,
    runtime: Arc<crate::access::hosted_control::HostedControlRuntime>,
    header_text: &str,
    is_tls: bool,
    source_bucket: Option<&str>,
    cors: crate::gateway_routes::CorsPosture,
) {
    let input =
        serde_json::from_str::<crate::access::hosted_control::HostedLeaseRequestInput>(&body)
            .map_err(|error| format!("invalid hosted lease request: {error}"));
    let response = match input.and_then(|input| {
        let origin = fleet_origin_from_request(header_text, is_tls)?;
        runtime.create_request(input, &origin, source_bucket)
    }) {
        Ok(request) => {
            tokio::spawn(async {
                let _ =
                    crate::connect_rendezvous::notify_attention("hosted_lease", "Hosted control")
                        .await;
            });
            json_value(request)
        }
        Err(error) if !runtime.configured() => ApiResponse::json_error(404, error),
        Err(error) if !runtime.enabled() => ApiResponse::json_error(503, error),
        Err(error) => ApiResponse::json_error(400, error),
    };
    write_api_response(stream, response, cors, None).await;
}

pub(crate) async fn handle_hosted_control_request_poll(
    stream: DemuxStream,
    body: String,
    runtime: Arc<crate::access::hosted_control::HostedControlRuntime>,
    cors: crate::gateway_routes::CorsPosture,
) {
    let response =
        match serde_json::from_str::<crate::access::hosted_control::HostedLeasePollProof>(&body)
            .map_err(|error| format!("invalid hosted lease poll proof: {error}"))
            .and_then(|proof| runtime.poll_request(&proof))
        {
            Ok(result) => json_value(result),
            Err(error) if !runtime.configured() => ApiResponse::json_error(404, error),
            Err(error) if !runtime.enabled() => ApiResponse::json_error(503, error),
            Err(error) => ApiResponse::json_error(400, error),
        };
    write_api_response(stream, response, cors, None).await;
}

pub(crate) async fn handle_hosted_control_anchor_decision(
    stream: DemuxStream,
    runtime: Arc<crate::access::hosted_control::HostedControlRuntime>,
    cors: crate::gateway_routes::CorsPosture,
) {
    let response = if !runtime.configured() {
        ApiResponse::json_error(404, "hosted control is disabled")
    } else {
        ApiResponse::json_error(
            503,
            "no qualifying signed application distribution is enabled in this build",
        )
    };
    write_api_response(stream, response, cors, None).await;
}

pub(crate) async fn handle_hosted_control_ws_ticket(
    stream: DemuxStream,
    runtime: Arc<crate::access::hosted_control::HostedControlRuntime>,
    verified: Option<crate::access::hosted_control::VerifiedHostedLease>,
    cors: crate::gateway_routes::CorsPosture,
) {
    let response = match verified {
        Some(verified) => match runtime.mint_ws_ticket(&verified) {
            Ok(ticket) => json_value(ticket),
            Err(error) => ApiResponse::json_error(403, error),
        },
        None => ApiResponse::json_error(401, "a valid hosted request proof is required"),
    };
    write_api_response(stream, response, cors, None).await;
}

fn trusted_confirmation_surface(principal: &crate::access::iam::AccessPrincipal) -> bool {
    principal.is_owner_surface() || principal.is_enrolled_root_mtls_user_client()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HostedRevokeInput {
    lease_id: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HostedPolicyInput {
    ceiling: crate::access::hosted_control::HostedPreset,
    max_ttl_secs: u64,
    #[serde(default)]
    operate_acknowledged: bool,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HostedEligibilityInput {
    session_id: String,
    eligible: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_hosted_control_management(
    stream: DemuxStream,
    method: &str,
    path: &str,
    body: String,
    runtime: Arc<crate::access::hosted_control::HostedControlRuntime>,
    authority: HttpAccessContext,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    if !runtime.configured() {
        write_api_response(
            stream,
            ApiResponse::json_error(404, "hosted control is disabled"),
            cors,
            fleet_origin,
        )
        .await;
        return;
    }
    if !trusted_confirmation_surface(&authority.principal) {
        write_api_response(
            stream,
            ApiResponse::json_error(
                403,
                "hosted lease management requires a local owner or enrolled direct-mTLS root surface",
            ),
            cors,
            fleet_origin,
        )
        .await;
        return;
    }
    let actor = &authority.principal;
    let response = match (method, path) {
        ("GET", "/api/access/hosted-control") => runtime
            .management_snapshot()
            .map(json_value)
            .unwrap_or_else(|error| ApiResponse::json_error(500, error.to_string())),
        ("POST", "/api/access/hosted-control/requests/decide") => match serde_json::from_str::<
            crate::access::hosted_control::HostedLeaseDecisionInput,
        >(&body)
        .map_err(|error| format!("invalid lease decision: {error}"))
        .and_then(|input| runtime.decide_request(input, actor))
        {
            Ok(lease) => ApiResponse::json(
                200,
                JsonBody::Value(serde_json::json!({"ok": true, "lease": lease})),
            ),
            Err(error) => ApiResponse::json_error(400, error),
        },
        ("POST", "/api/access/hosted-control/leases/revoke") => {
            match serde_json::from_str::<HostedRevokeInput>(&body)
                .map_err(|error| format!("invalid lease revocation: {error}"))
                .and_then(|input| {
                    runtime
                        .revoke_lease(&input.lease_id, actor)
                        .map_err(|error| error.to_string())
                }) {
                Ok(revoked) => ApiResponse::json(
                    200,
                    JsonBody::Value(serde_json::json!({"ok": true, "revoked": revoked})),
                ),
                Err(error) => ApiResponse::json_error(400, error),
            }
        }
        ("POST", "/api/access/hosted-control/policy") => {
            match serde_json::from_str::<HostedPolicyInput>(&body)
                .map_err(|error| format!("invalid hosted policy: {error}"))
                .and_then(|input| {
                    runtime
                        .set_policy(
                            input.ceiling,
                            input.max_ttl_secs,
                            actor,
                            input.operate_acknowledged,
                        )
                        .map_err(|error| error.to_string())
                }) {
                Ok(policy) => json_value(policy),
                Err(error) => ApiResponse::json_error(400, error),
            }
        }
        ("POST", "/api/access/hosted-control/sessions/eligibility") => {
            match serde_json::from_str::<HostedEligibilityInput>(&body)
                .map_err(|error| format!("invalid session eligibility update: {error}"))
                .and_then(|input| {
                    runtime
                        .set_session_eligibility(&input.session_id, input.eligible, actor)
                        .map_err(|error| error.to_string())
                }) {
                Ok(changed) => ApiResponse::json(
                    200,
                    JsonBody::Value(serde_json::json!({"ok": true, "changed": changed})),
                ),
                Err(error) => ApiResponse::json_error(400, error),
            }
        }
        ("POST", "/api/access/hosted-control/anchors") => {
            match serde_json::from_str::<crate::access::hosted_control::SignedAppAnchor>(&body)
                .map_err(|error| format!("invalid signed-app anchor enrollment: {error}"))
                .and_then(|anchor| {
                    runtime
                        .enroll_signed_app_anchor(anchor, actor)
                        .map_err(|error| error.to_string())
                }) {
                Ok(()) => ApiResponse::json(200, JsonBody::Value(serde_json::json!({"ok": true}))),
                Err(error) => ApiResponse::json_error(503, error),
            }
        }
        _ => ApiResponse::json_error(404, "unknown hosted-control management operation"),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_hosted_paths_are_method_and_path_exact() {
        for (method, path) in [
            ("GET", "/api/hosted-control/bootstrap"),
            ("POST", "/api/hosted-control/requests"),
            ("POST", "/api/hosted-control/requests/poll"),
            ("POST", "/api/hosted-control/anchor-decisions"),
        ] {
            assert!(is_public_hosted_control_path(method, path));
        }
        for (method, path) in [
            ("POST", "/api/hosted-control/bootstrap"),
            ("GET", "/api/hosted-control/requests"),
            ("POST", "/api/hosted-control/requests/extra"),
            ("GET", "/api/hosted-control/ws-ticket"),
            ("POST", "/api/hosted-control/ws-ticket"),
            ("GET", "/api/access/hosted-control"),
        ] {
            assert!(!is_public_hosted_control_path(method, path));
        }
    }

    #[test]
    fn fleet_origin_comes_only_from_daemon_terminated_https_host() {
        assert_eq!(
            fleet_origin_from_request(
                "GET / HTTP/1.1\r\nHost: laptop.fleet.example:8765\r\n\r\n",
                true,
            )
            .unwrap(),
            "https://laptop.fleet.example:8765"
        );
        assert!(fleet_origin_from_request(
            "GET / HTTP/1.1\r\nHost: laptop.fleet.example\r\n\r\n",
            false,
        )
        .is_err());
        for host in [
            "",
            "user@laptop.fleet.example",
            "laptop.fleet.example/path",
            "laptop.fleet.example?query",
        ] {
            let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n");
            assert!(
                fleet_origin_from_request(&request, true).is_err(),
                "invalid Host authority {host:?} was accepted",
            );
        }
    }

    #[test]
    fn hosted_proof_headers_are_closed_and_required() {
        let headers = concat!(
            "GET /api/sessions HTTP/1.1\r\n",
            "X-Intendant-Hosted-Lease: lease:test\r\n",
            "X-Intendant-Hosted-Nonce: nonce-test\r\n",
            "X-Intendant-Hosted-Timestamp: 12345\r\n",
            "X-Intendant-Hosted-Proof: signature-test\r\n",
            "\r\n",
        );
        let proof = hosted_request_proof_from_headers(headers).unwrap();
        assert_eq!(proof.lease_id, "lease:test");
        assert_eq!(proof.nonce, "nonce-test");
        assert_eq!(proof.timestamp_unix_ms, 12345);
        assert_eq!(proof.signature, "signature-test");
        assert!(hosted_request_proof_from_headers(
            "GET / HTTP/1.1\r\nX-Intendant-Hosted-Lease: lease:test\r\n\r\n"
        )
        .is_err());
    }
}
