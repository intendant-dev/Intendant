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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PublicControlLane<'a> {
    pub(crate) discovery_only: bool,
    pub(crate) public_lease_ingress: bool,
    pub(crate) configured: bool,
    pub(crate) live_custom_domain: Option<&'a str>,
    pub(crate) custom_domain_revoked: bool,
}

/// Derive the public-lane classification from live custom-name eligibility.
/// Retaining the selected custom SNI separately from its current eligibility
/// prevents a revoked owner-name lane from falling through to the fleet-lane
/// switch on an already-open TLS connection.
pub(crate) fn classify_public_control_lane<'a>(
    base_discovery_only: bool,
    selected_custom_domain: Option<&'a str>,
    custom_domain_enabled: bool,
    fleet_lane_enabled: bool,
) -> PublicControlLane<'a> {
    let live_custom_domain = selected_custom_domain.filter(|_| custom_domain_enabled);
    let custom_domain_revoked = selected_custom_domain.is_some() && live_custom_domain.is_none();
    let discovery_only = base_discovery_only || custom_domain_revoked;
    let public_lease_ingress =
        is_public_lease_ingress(discovery_only, live_custom_domain.is_some());
    let configured = !custom_domain_revoked
        && is_public_control_lane_configured(
            discovery_only,
            live_custom_domain.is_some(),
            fleet_lane_enabled,
        );
    PublicControlLane {
        discovery_only,
        public_lease_ingress,
        configured,
        live_custom_domain,
        custom_domain_revoked,
    }
}

/// A proof or ticket can cross an authority-store await after the TLS lane was
/// classified. Recheck the mutable owner-name gate before converting that
/// result into HTTP or WebSocket authority.
pub(crate) fn custom_domain_hosted_authority_revoked(
    custom_domain_selected: bool,
    hosted_authority_present: bool,
    custom_domain_enabled: impl FnOnce() -> bool,
) -> bool {
    custom_domain_selected && hosted_authority_present && !custom_domain_enabled()
}

fn json_value<T: serde::Serialize>(value: T) -> ApiResponse {
    match serde_json::to_value(value) {
        Ok(value) => ApiResponse::json(200, JsonBody::Value(value)),
        Err(error) => {
            ApiResponse::json_error(500, format!("serialize hosted-control response: {error}"))
        }
    }
}

pub(crate) const HOSTED_AUTHORITY_BUSY_ERROR: &str =
    "hosted-control authority work is busy; retry the request";
static HOSTED_AUTHORITY_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// Live authority carried across an HTTP request after its proof nonce has
/// been consumed. Request-body reads can be long enough for the lease, IAM
/// snapshot, certificate guard, or owner-name eligibility to change, so
/// handlers refresh this value immediately before dispatching effects.
#[derive(Clone)]
pub(crate) struct HostedHttpAuthority {
    runtime: Arc<crate::access::hosted_control::HostedControlRuntime>,
    custom_domain: Option<Arc<crate::custom_domain::CustomDomainRuntime>>,
    verified: crate::access::hosted_control::VerifiedHostedLease,
}

impl HostedHttpAuthority {
    pub(crate) fn new(
        runtime: Arc<crate::access::hosted_control::HostedControlRuntime>,
        custom_domain: Option<Arc<crate::custom_domain::CustomDomainRuntime>>,
        verified: crate::access::hosted_control::VerifiedHostedLease,
    ) -> Self {
        Self {
            runtime,
            custom_domain,
            verified,
        }
    }

    pub(crate) fn verified(&self) -> &crate::access::hosted_control::VerifiedHostedLease {
        &self.verified
    }

    pub(crate) fn access_context(&self) -> HttpAccessContext {
        HttpAccessContext {
            principal: self.verified.principal.clone(),
            iam_state: Some(Arc::clone(&self.verified.iam_state)),
        }
    }

    pub(crate) fn has_custom_domain_guard(&self) -> bool {
        self.custom_domain.is_some()
    }

    pub(crate) fn ensure_custom_domain_live(&self) -> Result<(), String> {
        if self
            .custom_domain
            .as_ref()
            .is_some_and(|runtime| !runtime.enabled())
        {
            return Err("custom-domain control became unavailable during the request".to_string());
        }
        Ok(())
    }

    pub(crate) async fn revalidate(&self) -> Result<Self, String> {
        self.ensure_custom_domain_live()?;
        let runtime = Arc::clone(&self.runtime);
        let opening = self.verified.clone();
        let verified =
            run_hosted_authority_io(move || runtime.revalidate_verified_lease(&opening)).await?;
        self.ensure_custom_domain_live()?;
        Ok(Self {
            runtime: Arc::clone(&self.runtime),
            custom_domain: self.custom_domain.clone(),
            verified,
        })
    }
}

pub(crate) fn hosted_authority_error_status(error: &str) -> u16 {
    if error == HOSTED_AUTHORITY_BUSY_ERROR {
        429
    } else {
        403
    }
}

async fn run_bounded_authority_io<T: Send + 'static>(
    workers: &'static tokio::sync::Semaphore,
    operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let permit = workers
        .try_acquire()
        .map_err(|_| HOSTED_AUTHORITY_BUSY_ERROR.to_string())?;
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err("hosted-control authority worker stopped unexpectedly".to_string()),
    }
}

pub(crate) async fn run_hosted_authority_io<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    run_bounded_authority_io(&HOSTED_AUTHORITY_WORKERS, operation).await
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
    let result = match public_origin_from_request(header_text, is_tls, tls_custom_domain) {
        Ok(origin) => {
            let runtime = Arc::clone(&runtime);
            let custom_domain = Arc::clone(&custom_domain);
            run_hosted_authority_io(move || {
                let mut bootstrap = runtime.bootstrap(&origin)?;
                if custom_domain.matches_origin(&origin) {
                    let snapshot = custom_domain.snapshot();
                    bootstrap.custom_domain = true;
                    bootstrap.rp_id = snapshot.rp_id;
                    bootstrap.passkey_available = !snapshot.passkeys.is_empty();
                }
                Ok(bootstrap)
            })
            .await
        }
        Err(error) => Err(error),
    };
    let response = match result {
        Ok(bootstrap) => json_value(bootstrap),
        Err(error) if !runtime.configured() => ApiResponse::json_error(404, error),
        Err(error) if !runtime.enabled() => ApiResponse::json_error(503, error),
        Err(error) if error == HOSTED_AUTHORITY_BUSY_ERROR => ApiResponse::json_error(429, error),
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
    let result = match input.and_then(|input| {
        let origin = public_origin?;
        Ok((input, origin))
    }) {
        Ok((input, origin)) => {
            let runtime = Arc::clone(&runtime);
            let source_bucket = source_bucket.map(str::to_string);
            run_hosted_authority_io(move || {
                runtime.create_request(input, &origin, source_bucket.as_deref())
            })
            .await
        }
        Err(error) => Err(error),
    };
    let response = match result {
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
        Err(error) if error == HOSTED_AUTHORITY_BUSY_ERROR => ApiResponse::json_error(429, error),
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
    let result = match public_origin_from_request(header_text, is_tls, tls_custom_domain) {
        Ok(origin) => {
            let custom_domain = Arc::clone(&custom_domain);
            let path = path.to_string();
            let source_bucket = source_bucket.map(str::to_string);
            run_hosted_authority_io(move || {
                let configured = custom_domain.configured();
                let enabled = custom_domain.enabled();
                let result = if !enabled || custom_domain.origin() != Some(origin.as_str()) {
                    Err("custom-domain passkey endpoint is unavailable on this origin".into())
                } else {
                    (|| -> Result<serde_json::Value, String> {
                        match path.as_str() {
                            "/api/hosted-control/passkey/register/start" => {
                                let input = serde_json::from_str::<
                                    crate::custom_domain::RegistrationStartInput,
                                >(&body)
                                .map_err(|error| {
                                    format!("invalid passkey registration start: {error}")
                                })?;
                                custom_domain
                                    .registration_start(input, &origin)
                                    .and_then(|value| {
                                        serde_json::to_value(value)
                                            .map_err(|error| error.to_string())
                                    })
                            }
                            "/api/hosted-control/passkey/register/finish" => {
                                let input = serde_json::from_str::<
                                    crate::custom_domain::RegistrationFinishInput,
                                >(&body)
                                .map_err(|error| {
                                    format!("invalid passkey registration finish: {error}")
                                })?;
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
                                .map_err(|error| {
                                    format!("invalid passkey start request: {error}")
                                })?;
                                custom_domain
                                    .authentication_start(input, &origin, source_bucket.as_deref())
                                    .and_then(|value| {
                                        serde_json::to_value(value)
                                            .map_err(|error| error.to_string())
                                    })
                            }
                            "/api/hosted-control/passkey/finish" => {
                                let input = serde_json::from_str::<
                                    crate::custom_domain::AuthenticationFinishInput,
                                >(&body)
                                .map_err(|error| {
                                    format!("invalid passkey finish request: {error}")
                                })?;
                                custom_domain
                                    .authentication_finish(input, &origin)
                                    .and_then(|value| {
                                        serde_json::to_value(value)
                                            .map_err(|error| error.to_string())
                                    })
                            }
                            _ => Err("custom-domain passkey endpoint was not found".to_string()),
                        }
                    })()
                };
                Ok((result, configured, enabled))
            })
            .await
        }
        Err(error) => {
            let custom_domain = Arc::clone(&custom_domain);
            run_hosted_authority_io(move || {
                Ok((
                    Err(error),
                    custom_domain.configured(),
                    custom_domain.enabled(),
                ))
            })
            .await
        }
    };
    let response = match result {
        Ok((Ok(value), _, _)) => ApiResponse::json(200, JsonBody::Value(value)),
        Ok((Err(error), false, _)) => ApiResponse::json_error(404, error),
        Ok((Err(error), _, false)) => ApiResponse::json_error(503, error),
        Ok((Err(error), _, _)) => ApiResponse::json_error(400, error),
        Err(error) if error == HOSTED_AUTHORITY_BUSY_ERROR => ApiResponse::json_error(429, error),
        Err(error) => ApiResponse::json_error(500, error),
    };
    write_api_response(stream, response, cors, None).await;
}

pub(crate) async fn handle_hosted_control_request_poll(
    stream: DemuxStream,
    body: String,
    runtime: Arc<crate::access::hosted_control::HostedControlRuntime>,
    cors: crate::gateway_routes::CorsPosture,
) {
    let result = serde_json::from_str::<crate::access::hosted_control::HostedLeasePollProof>(&body)
        .map_err(|error| format!("invalid hosted lease poll proof: {error}"));
    let result = match result {
        Ok(proof) => {
            let runtime = Arc::clone(&runtime);
            run_hosted_authority_io(move || runtime.poll_request(&proof)).await
        }
        Err(error) => Err(error),
    };
    let response = match result {
        Ok(result) => json_value(result),
        Err(error) if !runtime.configured() => ApiResponse::json_error(404, error),
        Err(error) if !runtime.enabled() => ApiResponse::json_error(503, error),
        Err(error) if error == HOSTED_AUTHORITY_BUSY_ERROR => ApiResponse::json_error(429, error),
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
    let ledger_runtime = Arc::clone(&runtime);
    let result = run_hosted_authority_io(move || ledger_runtime.certificate_ledger()).await;
    let response = match result {
        Ok(ledger) => json_value(ledger),
        Err(error) if !runtime.configured() => ApiResponse::json_error(404, error),
        Err(error) if !runtime.enabled() => ApiResponse::json_error(503, error),
        Err(error) if error == HOSTED_AUTHORITY_BUSY_ERROR => ApiResponse::json_error(429, error),
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
    let report = serde_json::from_str::<
        crate::access::hosted_control::HostedCertificateWitnessReport,
    >(&body)
    .map_err(|error| format!("invalid certificate witness report: {error}"))
    .and_then(|report| {
        let request_origin = fleet_origin_from_request(header_text, is_tls)?;
        if report.fleet_origin != request_origin {
            return Err("certificate witness fleet origin does not match the request".to_string());
        }
        Ok(report)
    });
    let result = match report {
        Ok(report) => {
            let runtime = Arc::clone(&runtime);
            run_hosted_authority_io(move || runtime.receive_signed_app_witness(report)).await
        }
        Err(error) => Err(error),
    };
    let response = match result {
        Ok(guard) => json_value(crate::access::hosted_control::HostedPublicLaneGuard {
            status: guard.status,
        }),
        Err(error) if !runtime.configured() => ApiResponse::json_error(404, error),
        Err(error) if !runtime.enabled() => ApiResponse::json_error(503, error),
        Err(error) if error == HOSTED_AUTHORITY_BUSY_ERROR => ApiResponse::json_error(429, error),
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
        Some(verified) => {
            let ticket_runtime = Arc::clone(&runtime);
            match run_hosted_authority_io(move || ticket_runtime.mint_ws_ticket(&verified)).await {
                Ok(ticket) => json_value(ticket),
                Err(error) if error == HOSTED_AUTHORITY_BUSY_ERROR => {
                    ApiResponse::json_error(429, error)
                }
                Err(error) => ApiResponse::json_error(403, error),
            }
        }
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

    #[tokio::test(flavor = "current_thread")]
    async fn hosted_authority_io_is_offloaded_and_admission_bounded() {
        static TEST_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
        let runtime_thread = std::thread::current().id();
        let authority_thread =
            run_bounded_authority_io(&TEST_WORKERS, || Ok(std::thread::current().id()))
                .await
                .unwrap();
        assert_ne!(authority_thread, runtime_thread);

        let permits = (0..TEST_WORKERS.available_permits())
            .map(|_| TEST_WORKERS.try_acquire().unwrap())
            .collect::<Vec<_>>();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_in_work = Arc::clone(&ran);
        let error = run_bounded_authority_io(&TEST_WORKERS, move || {
            ran_in_work.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap_err();
        assert_eq!(error, HOSTED_AUTHORITY_BUSY_ERROR);
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        drop(permits);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn held_authority_lock_does_not_stall_the_async_runtime() {
        static TEST_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
        let dir = tempfile::tempdir().unwrap();
        let cert_dir = dir.path().to_path_buf();
        let lock_held = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lock_held_in_worker = Arc::clone(&lock_held);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_dir = cert_dir.clone();
        let first = tokio::spawn(run_bounded_authority_io(&TEST_WORKERS, move || {
            crate::access::authority_store::with_lock(&first_dir, || {
                lock_held_in_worker.store(true, std::sync::atomic::Ordering::SeqCst);
                release_rx
                    .recv()
                    .map_err(|error| crate::access::AccessError(error.to_string()))?;
                Ok(())
            })
            .map_err(|error| error.to_string())
        }));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !lock_held.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let second = tokio::spawn(run_bounded_authority_io(&TEST_WORKERS, move || {
            crate::access::authority_store::with_lock(&cert_dir, || Ok(()))
                .map_err(|error| error.to_string())
        }));
        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            tokio::time::sleep(std::time::Duration::from_millis(10)),
        )
        .await
        .expect("a held durable lock must not block the current-thread runtime");
        release_tx.send(()).unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
    }

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
    fn revoked_custom_lane_never_falls_through_to_fleet_control() {
        let lane = classify_public_control_lane(true, Some("box.owner.example"), false, true);
        assert!(lane.discovery_only);
        assert!(lane.public_lease_ingress);
        assert!(!lane.configured);
        assert_eq!(lane.live_custom_domain, None);
        assert!(lane.custom_domain_revoked);
    }

    #[test]
    fn verified_custom_domain_authority_requires_a_fresh_live_gate() {
        assert!(custom_domain_hosted_authority_revoked(true, true, || false));
        assert!(!custom_domain_hosted_authority_revoked(true, true, || true));

        let checked = std::cell::Cell::new(false);
        assert!(!custom_domain_hosted_authority_revoked(false, true, || {
            checked.set(true);
            false
        }));
        assert!(
            !checked.get(),
            "fleet requests must not consult custom-domain state"
        );
        assert!(!custom_domain_hosted_authority_revoked(true, false, || {
            false
        }));
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
