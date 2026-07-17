use super::*;

pub(crate) fn is_public_hosted_control_path(method: &str, path: &str) -> bool {
    matches!(
        (method, path),
        ("GET", "/api/hosted-control/bootstrap")
            | ("POST", "/api/hosted-control/requests")
            | ("POST", "/api/hosted-control/requests/poll")
            | ("POST", "/api/hosted-control/anchor-decisions")
            | ("GET", "/api/hosted-control/certificate-ledger")
            | ("POST", "/api/hosted-control/witness-reports")
            | ("POST", "/api/hosted-control/passkey/start")
            | ("POST", "/api/hosted-control/passkey/finish")
            | ("POST", "/api/hosted-control/passkey/register/start")
            | ("POST", "/api/hosted-control/passkey/register/finish")
    )
}

pub(crate) fn is_custom_domain_only_hosted_control_path(method: &str, path: &str) -> bool {
    matches!(
        (method, path),
        ("POST", "/api/hosted-control/passkey/start")
            | ("POST", "/api/hosted-control/passkey/finish")
            | ("POST", "/api/hosted-control/passkey/register/start")
            | ("POST", "/api/hosted-control/passkey/register/finish")
    )
}

pub(crate) fn is_fleet_only_hosted_control_path(method: &str, path: &str) -> bool {
    is_public_hosted_control_path(method, path)
        && !matches!(
            (method, path),
            ("GET", "/api/hosted-control/certificate-ledger")
        )
}

pub(crate) fn is_public_lease_ingress(discovery_only: bool, custom_domain_sni: bool) -> bool {
    discovery_only || custom_domain_sni
}

pub(crate) fn is_public_control_lane_configured(
    discovery_only: bool,
    custom_domain_sni: bool,
    fleet_lane_enabled: bool,
) -> bool {
    custom_domain_sni || (discovery_only && fleet_lane_enabled)
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

pub(crate) fn public_origin_from_request(
    header_text: &str,
    is_tls: bool,
    tls_custom_domain: Option<&str>,
) -> Result<String, String> {
    let origin = fleet_origin_from_request(header_text, is_tls)?;
    if let Some(custom_name) = tls_custom_domain {
        let expected = format!("https://{custom_name}");
        if origin != expected {
            return Err(
                "request Host must equal the exact custom-domain TLS server name".to_string(),
            );
        }
    }
    Ok(origin)
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
    custom_domain: Arc<crate::custom_domain::CustomDomainRuntime>,
    header_text: &str,
    is_tls: bool,
    tls_custom_domain: Option<&str>,
    cors: crate::gateway_routes::CorsPosture,
) {
    let response = match public_origin_from_request(header_text, is_tls, tls_custom_domain)
        .and_then(|origin| {
            runtime
                .bootstrap(&origin)
                .map(|bootstrap| (origin, bootstrap))
        }) {
        Ok((origin, mut bootstrap)) => {
            if custom_domain.matches_origin(&origin) {
                bootstrap.custom_domain = true;
                bootstrap.rp_id = custom_domain.snapshot().rp_id;
                bootstrap.passkey_available = custom_domain.passkey_available();
            }
            json_value(bootstrap)
        }
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
    public_origin: Result<String, String>,
    source_bucket: Option<&str>,
    cors: crate::gateway_routes::CorsPosture,
) {
    let input =
        serde_json::from_str::<crate::access::hosted_control::HostedLeaseRequestInput>(&body)
            .map_err(|error| format!("invalid hosted lease request: {error}"));
    let response = match input.and_then(|input| {
        let origin = public_origin?;
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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_custom_domain_passkey(
    stream: DemuxStream,
    path: &str,
    body: String,
    custom_domain: Arc<crate::custom_domain::CustomDomainRuntime>,
    header_text: &str,
    is_tls: bool,
    tls_custom_domain: Option<&str>,
    source_bucket: Option<&str>,
    cors: crate::gateway_routes::CorsPosture,
) {
    let result =
        public_origin_from_request(header_text, is_tls, tls_custom_domain).and_then(|origin| {
            if !custom_domain.matches_origin(&origin) {
                return Err("custom-domain passkey endpoint is unavailable on this origin".into());
            }
            match path {
                "/api/hosted-control/passkey/register/start" => {
                    let input =
                        serde_json::from_str::<crate::custom_domain::RegistrationStartInput>(&body)
                            .map_err(|error| {
                                format!("invalid passkey registration start: {error}")
                            })?;
                    custom_domain
                        .registration_start(input, &origin)
                        .and_then(|value| {
                            serde_json::to_value(value).map_err(|error| error.to_string())
                        })
                }
                "/api/hosted-control/passkey/register/finish" => {
                    let input =
                        serde_json::from_str::<crate::custom_domain::RegistrationFinishInput>(
                            &body,
                        )
                        .map_err(|error| format!("invalid passkey registration finish: {error}"))?;
                    custom_domain.registration_finish(input).map(|value| {
                        serde_json::json!({
                            "ok": true,
                            "passkey": value,
                        })
                    })
                }
                "/api/hosted-control/passkey/start" => {
                    let input = serde_json::from_str::<
                        crate::custom_domain::AuthenticationStartInput,
                    >(&body)
                    .map_err(|error| format!("invalid passkey start request: {error}"))?;
                    custom_domain
                        .authentication_start(input, &origin, source_bucket)
                        .and_then(|value| {
                            serde_json::to_value(value).map_err(|error| error.to_string())
                        })
                }
                "/api/hosted-control/passkey/finish" => {
                    let input = serde_json::from_str::<
                        crate::custom_domain::AuthenticationFinishInput,
                    >(&body)
                    .map_err(|error| format!("invalid passkey finish request: {error}"))?;
                    custom_domain
                        .authentication_finish(input, &origin)
                        .and_then(|value| {
                            serde_json::to_value(value).map_err(|error| error.to_string())
                        })
                }
                _ => Err("custom-domain passkey endpoint was not found".to_string()),
            }
        });
    let response = match result {
        Ok(value) => ApiResponse::json(200, JsonBody::Value(value)),
        Err(error) if !custom_domain.configured() => ApiResponse::json_error(404, error),
        Err(error) if !custom_domain.enabled() => ApiResponse::json_error(503, error),
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

pub(crate) async fn handle_hosted_control_certificate_ledger(
    stream: DemuxStream,
    runtime: Arc<crate::access::hosted_control::HostedControlRuntime>,
    cors: crate::gateway_routes::CorsPosture,
) {
    let response = match runtime.certificate_ledger() {
        Ok(ledger) => json_value(ledger),
        Err(error) if !runtime.configured() => ApiResponse::json_error(404, error),
        Err(error) if !runtime.enabled() => ApiResponse::json_error(503, error),
        Err(error) => ApiResponse::json_error(503, error),
    };
    write_api_response(stream, response, cors, None).await;
}

pub(crate) async fn handle_hosted_control_witness_report(
    stream: DemuxStream,
    body: String,
    runtime: Arc<crate::access::hosted_control::HostedControlRuntime>,
    header_text: &str,
    is_tls: bool,
    cors: crate::gateway_routes::CorsPosture,
) {
    let response = match serde_json::from_str::<
        crate::access::hosted_control::HostedCertificateWitnessReport,
    >(&body)
    .map_err(|error| format!("invalid certificate witness report: {error}"))
    .and_then(|report| {
        let request_origin = fleet_origin_from_request(header_text, is_tls)?;
        if report.fleet_origin != request_origin {
            return Err("certificate witness fleet origin does not match the request".to_string());
        }
        runtime.receive_signed_app_witness(report)
    }) {
        Ok(guard) => json_value(crate::access::hosted_control::HostedPublicLaneGuard {
            status: guard.status,
        }),
        Err(error) if !runtime.configured() => ApiResponse::json_error(404, error),
        Err(error) if !runtime.enabled() => ApiResponse::json_error(503, error),
        Err(error) if error.contains("no qualifying signed application distribution") => {
            ApiResponse::json_error(503, error)
        }
        Err(error) => ApiResponse::json_error(403, error),
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HostedWitnessConfirmInput {
    serial_hex: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HostedWitnessOverrideInput {
    evidence_sha256: String,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_hosted_control_management(
    stream: DemuxStream,
    method: &str,
    path: &str,
    body: String,
    runtime: Arc<crate::access::hosted_control::HostedControlRuntime>,
    custom_domain: Arc<crate::custom_domain::CustomDomainRuntime>,
    authority: HttpAccessContext,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    if !runtime.configured() && !custom_domain.configured() {
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
        ("GET", "/api/access/hosted-control") => {
            let value = if runtime.configured() {
                runtime
                    .management_snapshot()
                    .map_err(|error| error.to_string())
                    .and_then(|snapshot| {
                        serde_json::to_value(snapshot).map_err(|error| error.to_string())
                    })
            } else {
                Ok(serde_json::json!({"configured": false, "enabled": false}))
            };
            match value {
                Ok(mut value) => {
                    value["custom_domain"] = serde_json::to_value(custom_domain.snapshot())
                        .unwrap_or(serde_json::Value::Null);
                    ApiResponse::json(200, JsonBody::Value(value))
                }
                Err(error) => ApiResponse::json_error(500, error),
            }
        }
        ("POST", "/api/access/hosted-control/passkeys/enrollment") => {
            match serde_json::from_str::<crate::custom_domain::RegistrationInviteInput>(&body)
                .map_err(|error| format!("invalid passkey enrollment invitation: {error}"))
                .and_then(|input| custom_domain.registration_invite(input))
            {
                Ok(invitation) => json_value(invitation),
                Err(error) => ApiResponse::json_error(400, error),
            }
        }
        ("POST", "/api/access/hosted-control/passkeys/revoke") => {
            match serde_json::from_str::<crate::custom_domain::RevokeInput>(&body)
                .map_err(|error| format!("invalid passkey revocation: {error}"))
                .and_then(|input| custom_domain.revoke(input))
            {
                Ok(revoked) => json_value(serde_json::json!({"ok": true, "revoked": revoked})),
                Err(error) => ApiResponse::json_error(400, error),
            }
        }
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
        ("POST", "/api/access/hosted-control/witnesses/confirm") => {
            match serde_json::from_str::<HostedWitnessConfirmInput>(&body)
                .map_err(|error| format!("invalid certificate confirmation: {error}"))
                .and_then(|input| {
                    runtime
                        .confirm_witness_serial(&input.serial_hex, actor)
                        .map_err(|error| error.to_string())
                }) {
                Ok(guard) => json_value(guard),
                Err(error) => ApiResponse::json_error(400, error),
            }
        }
        ("POST", "/api/access/hosted-control/witnesses/override") => {
            match serde_json::from_str::<HostedWitnessOverrideInput>(&body)
                .map_err(|error| format!("invalid certificate override: {error}"))
                .and_then(|input| {
                    runtime
                        .override_witness_guard(&input.evidence_sha256, actor)
                        .map_err(|error| error.to_string())
                }) {
                Ok(guard) => json_value(guard),
                Err(error)
                    if error == crate::access::hosted_control::WITNESS_EVIDENCE_CHANGED_ERROR =>
                {
                    ApiResponse::json_error(409, error)
                }
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
            ("GET", "/api/hosted-control/certificate-ledger"),
            ("POST", "/api/hosted-control/witness-reports"),
            ("POST", "/api/hosted-control/passkey/register/start"),
            ("POST", "/api/hosted-control/passkey/register/finish"),
            ("POST", "/api/hosted-control/passkey/start"),
            ("POST", "/api/hosted-control/passkey/finish"),
        ] {
            assert!(is_public_hosted_control_path(method, path));
        }
        assert!(!is_fleet_only_hosted_control_path(
            "GET",
            "/api/hosted-control/certificate-ledger"
        ));
        assert!(is_fleet_only_hosted_control_path(
            "POST",
            "/api/hosted-control/requests"
        ));
        assert!(is_custom_domain_only_hosted_control_path(
            "POST",
            "/api/hosted-control/passkey/start"
        ));
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
    fn custom_origin_requires_exact_tls_sni_and_host_agreement() {
        let request = "GET / HTTP/1.1\r\nHost: box.example.test\r\n\r\n";
        assert_eq!(
            public_origin_from_request(request, true, Some("box.example.test")).unwrap(),
            "https://box.example.test"
        );
        assert!(public_origin_from_request(request, true, Some("other.example.test")).is_err());
        assert!(public_origin_from_request(request, false, Some("box.example.test")).is_err());
    }

    #[test]
    fn custom_lane_enablement_does_not_open_the_fleet_name_lane() {
        assert!(!is_public_control_lane_configured(true, false, false));
        assert!(is_public_control_lane_configured(false, true, false));
        assert!(is_public_control_lane_configured(true, false, true));
        assert!(!is_public_control_lane_configured(false, false, true));
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
