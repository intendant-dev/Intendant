//! Web Push (RFC 8291 payload encryption + RFC 8292 VAPID) in pure ring:
//! the subscription API, test pushes, and the background monitors that
//! author presence/reclaim alerts.

use super::*;

// ── Web Push (RFC 8291 payload encryption + RFC 8292 VAPID), pure ring ──
//
// The service authors only presence alerts — facts it inherently knows
// from the polling it exists to do. Payloads are still encrypted to the
// browser subscription (the push relay in the middle sees ciphertext),
// and the VAPID key proves the sender to the push service.

pub(crate) struct HkdfLen(usize);
impl ring::hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        self.0
    }
}

pub(crate) fn hkdf_expand(prk: &ring::hkdf::Prk, info: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    prk.expand(&[info], HkdfLen(len))
        .expect("hkdf expand length is valid")
        .fill(&mut out)
        .expect("hkdf fill length matches");
    out
}

/// Encrypt `plaintext` for a browser push subscription (RFC 8291,
/// aes128gcm coding). Returns the full request body: the RFC 8188
/// header block (salt, record size, ephemeral public key) followed by
/// the single encrypted record.
pub(crate) fn webpush_encrypt(
    ua_public_b64u: &str,
    auth_secret_b64u: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>, String> {
    let ua_public = b64u_decode(ua_public_b64u.trim())
        .map_err(|_| "subscription p256dh is not valid base64url".to_string())?;
    let auth_secret = b64u_decode(auth_secret_b64u.trim())
        .map_err(|_| "subscription auth is not valid base64url".to_string())?;
    if ua_public.len() != 65 || auth_secret.len() != 16 {
        return Err("subscription keys have unexpected lengths".to_string());
    }

    let rng = ring::rand::SystemRandom::new();
    let eph_private =
        ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &rng)
            .map_err(|_| "ephemeral key generation failed".to_string())?;
    let eph_public = eph_private
        .compute_public_key()
        .map_err(|_| "ephemeral public key computation failed".to_string())?;
    let peer =
        ring::agreement::UnparsedPublicKey::new(&ring::agreement::ECDH_P256, ua_public.clone());
    let ecdh_secret =
        ring::agreement::agree_ephemeral(eph_private, &peer, |secret| secret.to_vec())
            .map_err(|_| "ECDH agreement failed (bad subscription key?)".to_string())?;

    // IKM = HKDF(salt=auth_secret, ikm=ecdh_secret, info="WebPush: info"||0||ua_pub||as_pub, 32)
    let mut info = b"WebPush: info\x00".to_vec();
    info.extend_from_slice(&ua_public);
    info.extend_from_slice(eph_public.as_ref());
    let prk_key =
        ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, &auth_secret).extract(&ecdh_secret);
    let ikm = hkdf_expand(&prk_key, &info, 32);

    let mut salt = [0u8; 16];
    ring::rand::SecureRandom::fill(&rng, &mut salt)
        .map_err(|_| "salt generation failed".to_string())?;
    let prk = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, &salt).extract(&ikm);
    let cek = hkdf_expand(&prk, b"Content-Encoding: aes128gcm\x00", 16);
    let nonce = hkdf_expand(&prk, b"Content-Encoding: nonce\x00", 12);

    // Single record: plaintext || 0x02 (last-record delimiter), sealed.
    let mut record = plaintext.to_vec();
    record.push(0x02);
    let key = ring::aead::LessSafeKey::new(
        ring::aead::UnboundKey::new(&ring::aead::AES_128_GCM, &cek)
            .map_err(|_| "content key setup failed".to_string())?,
    );
    let nonce = ring::aead::Nonce::try_assume_unique_for_key(&nonce)
        .map_err(|_| "nonce setup failed".to_string())?;
    key.seal_in_place_append_tag(nonce, ring::aead::Aad::empty(), &mut record)
        .map_err(|_| "payload encryption failed".to_string())?;

    // RFC 8188 header: salt(16) || rs(4) || idlen(1) || keyid(as_public)
    let mut body = Vec::with_capacity(16 + 4 + 1 + 65 + record.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(65);
    body.extend_from_slice(eph_public.as_ref());
    body.extend_from_slice(&record);
    Ok(body)
}

/// RFC 8292 `Authorization: vapid t=<jwt>, k=<pub>` for one endpoint.
pub(crate) fn vapid_authorization(
    keypair: &ring::signature::EcdsaKeyPair,
    endpoint: &str,
    contact: &str,
) -> Result<String, String> {
    use ring::signature::KeyPair as _;
    let endpoint_url =
        url::Url::parse(endpoint).map_err(|_| "subscription endpoint is not a URL".to_string())?;
    let audience = format!(
        "{}://{}",
        endpoint_url.scheme(),
        endpoint_url
            .host_str()
            .map(|host| match endpoint_url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_string(),
            })
            .ok_or_else(|| "subscription endpoint has no host".to_string())?
    );
    let header = b64u(br#"{"typ":"JWT","alg":"ES256"}"#.as_slice());
    let claims = b64u(
        json!({
            "aud": audience,
            "exp": (now_unix_ms() / 1000) + 12 * 3600,
            "sub": contact,
        })
        .to_string()
        .as_bytes(),
    );
    let signing_input = format!("{header}.{claims}");
    let rng = ring::rand::SystemRandom::new();
    let signature = keypair
        .sign(&rng, signing_input.as_bytes())
        .map_err(|_| "VAPID signing failed".to_string())?;
    let public_b64u = b64u(keypair.public_key().as_ref());
    Ok(format!(
        "vapid t={signing_input}.{}, k={public_b64u}",
        b64u(signature.as_ref())
    ))
}

/// Fire one encrypted notification at a subscription. Returns Ok(false)
/// when the push service says the subscription is gone (prune it).
pub(crate) async fn send_web_push(
    http: &reqwest::Client,
    keypair: &ring::signature::EcdsaKeyPair,
    contact: &str,
    subscription: &PushSubscriptionRecord,
    payload: &serde_json::Value,
) -> Result<bool, String> {
    let body = webpush_encrypt(
        &subscription.p256dh,
        &subscription.auth,
        payload.to_string().as_bytes(),
    )?;
    let authorization = vapid_authorization(keypair, &subscription.endpoint, contact)?;
    let response = http
        .post(&subscription.endpoint)
        .header("authorization", authorization)
        .header("content-encoding", "aes128gcm")
        .header("ttl", "86400")
        .header("urgency", "normal")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("push send failed: {e}"))?;
    match response.status().as_u16() {
        200..=299 => Ok(true),
        404 | 410 => Ok(false),
        status => Err(format!("push service returned {status}")),
    }
}

pub(crate) fn load_or_create_vapid_keypair(
    store: &mut Store,
) -> Result<ring::signature::EcdsaKeyPair, String> {
    let rng = ring::rand::SystemRandom::new();
    if store.vapid_private_pk8_b64.is_none() {
        let document = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .map_err(|_| "VAPID key generation failed".to_string())?;
        store.vapid_private_pk8_b64 = Some(b64u(document.as_ref()));
    }
    let der = b64u_decode(store.vapid_private_pk8_b64.as_deref().unwrap_or(""))
        .map_err(|_| "stored VAPID key is not valid base64".to_string())?;
    ring::signature::EcdsaKeyPair::from_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        &der,
        &rng,
    )
    .map_err(|_| "stored VAPID key is invalid".to_string())
}

pub(crate) async fn push_vapid_public_key(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    use ring::signature::KeyPair as _;
    Json(json!({
        "ok": true,
        "public_key": b64u(state.vapid.public_key().as_ref()),
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PushSubscribeRequest {
    endpoint: String,
    #[serde(default)]
    p256dh: String,
    #[serde(default)]
    auth: String,
    #[serde(default)]
    label: String,
    /// Presence (offline/online) alerts. Default on — the block's original
    /// purpose.
    #[serde(default)]
    notify_presence: Option<bool>,
    /// Pending agent-request (approval/question) alerts. Default OFF —
    /// strictly opt-in.
    #[serde(default)]
    notify_requests: Option<bool>,
}

pub(crate) async fn push_subscribe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PushSubscribeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "push_subscribe", 20, 600_000).await?;
    let endpoint = body.endpoint.trim().to_string();
    if !endpoint.starts_with("https://") && !endpoint.starts_with("http://") {
        return Err(ApiError::bad_request("endpoint must be a push service URL"));
    }
    if endpoint.len() > 2048 {
        return Err(ApiError::bad_request("endpoint is too long"));
    }
    let p256dh = body.p256dh.trim().to_string();
    let auth = body.auth.trim().to_string();
    match (b64u_decode(&p256dh), b64u_decode(&auth)) {
        (Ok(point), Ok(secret)) if point.len() == 65 && secret.len() == 16 => {}
        _ => return Err(ApiError::bad_request("subscription keys are malformed")),
    }
    {
        let mut store = state.store.lock().await;
        store
            .push_subscriptions
            .retain(|record| record.endpoint != endpoint);
        let per_user = store
            .push_subscriptions
            .iter()
            .filter(|record| record.user_id == user.id)
            .count();
        if per_user >= 10 {
            return Err(ApiError::bad_request(
                "too many subscriptions on this account",
            ));
        }
        store.push_subscriptions.push(PushSubscriptionRecord {
            user_id: user.id,
            endpoint,
            p256dh,
            auth,
            label: clean_fleet_text(&body.label, FLEET_LABEL_MAX),
            created_unix_ms: now_unix_ms(),
            notify_presence: body.notify_presence.unwrap_or(true),
            notify_requests: body.notify_requests.unwrap_or(false),
        });
        audit(
            &mut store,
            "push_subscribed",
            Some(user.id),
            None,
            json!({}),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PushUnsubscribeRequest {
    #[serde(default)]
    endpoint: String,
}

pub(crate) async fn push_unsubscribe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PushUnsubscribeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    let endpoint = body.endpoint.trim();
    let removed = {
        let mut store = state.store.lock().await;
        let before = store.push_subscriptions.len();
        store.push_subscriptions.retain(|record| {
            !(record.user_id == user.id && (endpoint.is_empty() || record.endpoint == endpoint))
        });
        let removed = before - store.push_subscriptions.len();
        if removed > 0 {
            persist_locked(&state, &store)?;
        }
        removed
    };
    Ok(Json(json!({ "ok": true, "removed": removed })))
}

/// The caller's own subscriptions with their alert flags (no keys) — how
/// the /connect page paints its per-browser toggles.
pub(crate) async fn push_subscriptions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    let subscriptions: Vec<serde_json::Value> = {
        let store = state.store.lock().await;
        store
            .push_subscriptions
            .iter()
            .filter(|record| record.user_id == user.id)
            .map(|record| {
                json!({
                    "endpoint": record.endpoint,
                    "label": record.label,
                    "created_unix_ms": record.created_unix_ms,
                    "notify_presence": record.notify_presence,
                    "notify_requests": record.notify_requests,
                })
            })
            .collect()
    };
    Ok(Json(json!({ "ok": true, "subscriptions": subscriptions })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PushPreferencesRequest {
    endpoint: String,
    #[serde(default)]
    notify_presence: Option<bool>,
    #[serde(default)]
    notify_requests: Option<bool>,
}

/// Flip alert flags on an existing subscription without re-subscribing.
pub(crate) async fn push_preferences(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PushPreferencesRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    let endpoint = body.endpoint.trim();
    let (notify_presence, notify_requests) = {
        let mut store = state.store.lock().await;
        let record = store
            .push_subscriptions
            .iter_mut()
            .find(|record| record.user_id == user.id && record.endpoint == endpoint)
            .ok_or_else(|| ApiError::not_found("no such subscription on this account"))?;
        if let Some(value) = body.notify_presence {
            record.notify_presence = value;
        }
        if let Some(value) = body.notify_requests {
            record.notify_requests = value;
        }
        let flags = (record.notify_presence, record.notify_requests);
        persist_locked(&state, &store)?;
        flags
    };
    Ok(Json(json!({
        "ok": true,
        "notify_presence": notify_presence,
        "notify_requests": notify_requests,
    })))
}

/// Mirrors `notify_signing_payload` in the daemon's `connect_rendezvous.rs`
/// — stable protocol, replicated rather than shared (twin golden test).
pub(crate) fn notify_signing_payload(
    daemon_id: &str,
    daemon_public_key: &str,
    issued_at_unix_ms: u64,
    kind: &str,
    session_label: &str,
) -> String {
    format!(
        "{NOTIFY_PROTOCOL}\n{daemon_id}\n{daemon_public_key}\n{issued_at_unix_ms}\n{kind}\n{session_label}\n"
    )
}

/// Request kinds a nudge may name. A closed vocabulary — extending it is a
/// deliberate act on both binaries (planned: agent notify), never free
/// text from the wire.
const NOTIFY_KINDS: &[&str] = &["approval", "question", "display_request"];

/// Compose the Web Push payload for a pending-request nudge.
///
/// PRIVACY (hard rule): the payload names only the request KIND, the
/// daemon's display label, and the session's display label — NEVER command
/// text, question text, file paths, or any other work content. The nudge
/// wire (`DaemonNotifyRequest`) has no field that could carry those, and
/// this constructor must keep it that way when kinds are added: this
/// service stays zero-knowledge about work content
/// (docs/src/self-hosted-rendezvous.md), and owners read the push on lock
/// screens.
pub(crate) fn attention_push_payload(
    kind: &str,
    daemon_label: &str,
    session_label: &str,
    daemon_id: &str,
) -> serde_json::Value {
    let (title, body) = match kind {
        "question" => (
            format!("{daemon_label}: the agent has a question"),
            format!("Session \u{201c}{session_label}\u{201d} is waiting for your answer."),
        ),
        "display_request" => (
            format!("{daemon_label}: agent asks to view your screen"),
            format!(
                "Session \u{201c}{session_label}\u{201d} is asking for display access. Open the dashboard to allow or deny."
            ),
        ),
        _ => (
            format!("{daemon_label}: approval needed"),
            format!("Session \u{201c}{session_label}\u{201d} is waiting for your approval."),
        ),
    };
    json!({
        "title": title,
        "body": body,
        "url": format!(
            "/app?connect=1&daemon_id={}",
            form_urlencoded::byte_serialize(daemon_id.as_bytes()).collect::<String>()
        ),
        // One stacked notification per daemon: later nudges replace
        // earlier ones instead of piling up.
        "tag": format!("attention-{daemon_id}"),
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct DaemonNotifyRequest {
    protocol: String,
    daemon_id: String,
    daemon_public_key: String,
    issued_at_unix_ms: u64,
    signature: String,
    /// One of [`NOTIFY_KINDS`].
    #[serde(default)]
    kind: String,
    /// The session's display label. Content-free by construction on the
    /// daemon side (id prefix, or the user's explicit rename); sanitized
    /// and capped here like every other fleet label before display.
    #[serde(default)]
    session_label: String,
}

/// Daemon-signed attention nudge: fan a Web Push out to the owner's
/// opted-in (`notify_requests`) browsers. Same authentication discipline as
/// the fleet-DNS publishes: bearer gate, rate limit, freshness window, and
/// the registered-key signature pin.
pub(crate) async fn daemon_notify(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<DaemonNotifyRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let daemon_id = body.daemon_id.trim().to_string();
    // The daemon-side cooldown paces this to once per session per ten
    // minutes; the rate limit is the service-side backstop.
    let daemon = verified_daemon_request(
        &state,
        &headers,
        ("daemon_notify", 12, 600_000),
        (&body.protocol, NOTIFY_PROTOCOL),
        &daemon_id,
        &body.daemon_public_key,
        body.issued_at_unix_ms,
    )
    .await?;
    if !NOTIFY_KINDS.contains(&body.kind.as_str()) {
        return Err(ApiError::bad_request("unsupported notify kind"));
    }
    if body.session_label.len() > FLEET_LABEL_MAX * 4 {
        return Err(ApiError::bad_request("session_label is too long"));
    }
    // The signature covers the kind and label exactly as sent.
    let payload = notify_signing_payload(
        &daemon_id,
        &daemon.daemon_public_key,
        body.issued_at_unix_ms,
        &body.kind,
        &body.session_label,
    );
    if !verify_ed25519_b64u(
        &daemon.daemon_public_key,
        payload.as_bytes(),
        body.signature.trim(),
    ) {
        return Err(ApiError::bad_request("notify signature invalid"));
    }
    let Some(owner) = daemon.owner_user_id else {
        return Err(ApiError::bad_request("daemon is not claimed"));
    };
    let subscriptions: Vec<PushSubscriptionRecord> = {
        let store = state.store.lock().await;
        store
            .push_subscriptions
            .iter()
            .filter(|record| record.notify_requests && record.user_id == owner)
            .cloned()
            .collect()
    };
    let daemon_label = daemon
        .label
        .clone()
        .filter(|label| !label.trim().is_empty())
        .unwrap_or_else(|| daemon.daemon_id.clone());
    let session_label = clean_fleet_text(&body.session_label, FLEET_LABEL_MAX);
    let push_payload = attention_push_payload(&body.kind, &daemon_label, &session_label, &daemon_id);
    let mut sent = 0;
    let mut dead = Vec::new();
    for subscription in &subscriptions {
        match send_web_push(
            &state.push_http,
            &state.vapid,
            &state.config.public_origin,
            subscription,
            &push_payload,
        )
        .await
        {
            Ok(true) => sent += 1,
            Ok(false) => dead.push(subscription.endpoint.clone()),
            Err(e) => eprintln!("[push] attention nudge failed: {e}"),
        }
    }
    {
        let mut store = state.store.lock().await;
        if !dead.is_empty() {
            store
                .push_subscriptions
                .retain(|record| !dead.contains(&record.endpoint));
        }
        // Audit names the kind only — the label is for the push, not the
        // permanent record.
        audit(
            &mut store,
            "daemon_notify",
            Some(owner),
            Some(daemon_id.clone()),
            json!({ "kind": body.kind, "sent": sent }),
        );
        persist_locked(&state, &store)?;
    }
    Ok(Json(
        json!({ "ok": true, "sent": sent, "pruned": dead.len() }),
    ))
}

pub(crate) async fn push_test(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let user = require_user(&state, &headers).await?;
    require_csrf(&state, &headers).await?;
    check_rate_limit(&state, &headers, "push_test", 10, 600_000).await?;
    let subscriptions: Vec<PushSubscriptionRecord> = {
        let store = state.store.lock().await;
        store
            .push_subscriptions
            .iter()
            .filter(|record| record.user_id == user.id)
            .cloned()
            .collect()
    };
    if subscriptions.is_empty() {
        return Err(ApiError::bad_request(
            "no push subscriptions on this account",
        ));
    }
    let payload = json!({
        "title": "Intendant Connect",
        "body": "Test notification — this is what a computer alert will look like.",
        "url": "/connect",
    });
    let mut sent = 0;
    let mut dead = Vec::new();
    for subscription in &subscriptions {
        match send_web_push(
            &state.push_http,
            &state.vapid,
            &state.config.public_origin,
            subscription,
            &payload,
        )
        .await
        {
            Ok(true) => sent += 1,
            Ok(false) => dead.push(subscription.endpoint.clone()),
            Err(e) => eprintln!("[push] test send failed: {e}"),
        }
    }
    if !dead.is_empty() {
        let mut store = state.store.lock().await;
        store
            .push_subscriptions
            .retain(|record| !dead.contains(&record.endpoint));
        persist_locked(&state, &store)?;
    }
    Ok(Json(
        json!({ "ok": true, "sent": sent, "pruned": dead.len() }),
    ))
}

/// Watch claimed daemons for presence transitions and notify their
/// owners' opted-in browsers. The service only narrates facts it already
/// holds (last poll time); payloads are encrypted to each subscription.
pub(crate) async fn presence_alert_monitor(state: Arc<AppState>) {
    let offline_after_ms: u64 = std::env::var("INTENDANT_CONNECT_PRESENCE_OFFLINE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(180_000);
    let poll_ms: u64 = std::env::var("INTENDANT_CONNECT_PRESENCE_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30_000);
    // daemon_id -> last announced state; seeded silently on startup so a
    // service restart never fires a wave of stale alerts.
    let mut announced: HashMap<String, bool> = HashMap::new();
    let mut seeded = false;
    loop {
        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
        let now = now_unix_ms();
        let (transitions, subscriptions) = {
            let store = state.store.lock().await;
            let mut transitions: Vec<(String, String, Option<Uuid>, bool, u64)> = Vec::new();
            for daemon in store.daemons.iter().filter(|d| d.owner_user_id.is_some()) {
                let offline_for = now.saturating_sub(daemon.last_seen_unix_ms);
                let online = offline_for < offline_after_ms;
                let previous = announced.insert(daemon.daemon_id.clone(), online);
                if seeded {
                    if let Some(previous) = previous {
                        if previous != online {
                            let label = daemon
                                .label
                                .clone()
                                .unwrap_or_else(|| daemon.daemon_id.clone());
                            transitions.push((
                                daemon.daemon_id.clone(),
                                label,
                                daemon.owner_user_id,
                                online,
                                offline_for,
                            ));
                        }
                    }
                }
            }
            (transitions, store.push_subscriptions.clone())
        };
        seeded = true;
        if transitions.is_empty() {
            continue;
        }
        let mut dead = Vec::new();
        for (daemon_id, label, owner, online, offline_for) in transitions {
            let payload = json!({
                "title": if online { format!("{label} is back online") } else { format!("{label} went offline") },
                "body": if online {
                    format!("Reconnected after {} offline.", human_duration_ms(offline_for))
                } else {
                    "It stopped polling the rendezvous. The machine may be off, asleep, or disconnected.".to_string()
                },
                "url": format!("/app?connect=1&daemon_id={daemon_id}"),
            });
            for subscription in subscriptions
                .iter()
                .filter(|s| s.notify_presence && Some(s.user_id) == owner)
            {
                match send_web_push(
                    &state.push_http,
                    &state.vapid,
                    &state.config.public_origin,
                    subscription,
                    &payload,
                )
                .await
                {
                    Ok(true) => {}
                    Ok(false) => dead.push(subscription.endpoint.clone()),
                    Err(e) => eprintln!("[push] presence alert failed: {e}"),
                }
            }
        }
        if !dead.is_empty() {
            let mut store = state.store.lock().await;
            store
                .push_subscriptions
                .retain(|record| !dead.contains(&record.endpoint));
            if let Err(err) = persist_locked(&state, &store) {
                eprintln!("[push] failed to persist pruned subscriptions: {err:?}");
            }
        }
    }
}

/// Dormant-handle reclamation (stated policy; enforcement is opt-in via
/// INTENDANT_CONNECT_RECLAIM_AFTER_MS, 0/unset = off): an account with
/// zero claimed daemons and no sign-in past the threshold loses its
/// handle — the account survives, renamed to user-<id-prefix>, and the
/// reclamation is committed to the transparency log. Squatted-but-unused
/// names do not keep.
pub(crate) async fn handle_reclaim_monitor(state: Arc<AppState>) {
    let after_ms: u64 = std::env::var("INTENDANT_CONNECT_RECLAIM_AFTER_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if after_ms == 0 {
        return;
    }
    let poll_ms: u64 = std::env::var("INTENDANT_CONNECT_RECLAIM_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6 * 3600 * 1000);
    loop {
        tokio::time::sleep(Duration::from_millis(poll_ms.max(60_000))).await;
        let now = now_unix_ms();
        let mut store = state.store.lock().await;
        let owners: std::collections::HashSet<Uuid> = store
            .daemons
            .iter()
            .filter_map(|d| d.owner_user_id)
            .collect();
        let mut reclaimed = Vec::new();
        for user in store.users.iter_mut() {
            if user.account_name.starts_with("user-") || owners.contains(&user.id) {
                continue;
            }
            let last_active = user
                .last_login_unix_ms
                .max(user.updated_unix_ms)
                .max(user.created_unix_ms);
            if now.saturating_sub(last_active) < after_ms {
                continue;
            }
            let freed = user.account_name.clone();
            let mut short = user.id.simple().to_string();
            short.truncate(8);
            user.account_name = format!("user-{short}");
            user.updated_unix_ms = now;
            reclaimed.push((freed, user.account_name.clone(), user.id));
        }
        if reclaimed.is_empty() {
            continue;
        }
        for (freed, renamed_to, user_id) in &reclaimed {
            append_log_entry(
                &mut store,
                "handle_reclaimed",
                json!({ "handle": freed, "renamed_to": renamed_to }),
            );
            audit(
                &mut store,
                "handle_reclaimed",
                Some(*user_id),
                None,
                json!({ "handle": freed }),
            );
            eprintln!("[reclaim] freed dormant handle {freed} (account renamed to {renamed_to})");
        }
        if let Err(err) = persist_locked(&state, &store) {
            eprintln!("[reclaim] failed to persist dormant-handle reclamation: {err:?}");
        }
    }
}

pub(crate) fn human_duration_ms(ms: u64) -> String {
    let minutes = ms / 60_000;
    if minutes < 2 {
        return "moments".to_string();
    }
    if minutes < 120 {
        return format!("{minutes} minutes");
    }
    let hours = minutes / 60;
    if hours < 48 {
        return format!("{hours} hours");
    }
    format!("{} days", hours / 24)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webpush_body_has_rfc8188_layout() {
        // A synthetic subscription keypair: any valid P-256 point works
        // for layout checks (ring generates one for us).
        let rng = ring::rand::SystemRandom::new();
        let ua = ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &rng)
            .unwrap();
        let ua_pub = ua.compute_public_key().unwrap();
        let auth = [7u8; 16];
        let plaintext = br#"{"title":"t"}"#;
        let body = webpush_encrypt(&b64u(ua_pub.as_ref()), &b64u(&auth), plaintext).unwrap();
        assert_eq!(&body[16..20], &4096u32.to_be_bytes(), "record size");
        assert_eq!(body[20], 65, "key id length");
        assert_eq!(body[21], 0x04, "uncompressed point marker");
        // salt(16) + rs(4) + idlen(1) + key(65) + ct(pt + delimiter + tag)
        assert_eq!(body.len(), 16 + 4 + 1 + 65 + plaintext.len() + 1 + 16);
        // Two encryptions differ (fresh salt + ephemeral key).
        let again = webpush_encrypt(&b64u(ua_pub.as_ref()), &b64u(&auth), plaintext).unwrap();
        assert_ne!(body, again);
    }

    #[test]
    fn vapid_authorization_signs_a_verifiable_jwt_for_the_endpoint_origin() {
        use ring::signature::KeyPair as _;
        let mut store = Store::default();
        let keypair = load_or_create_vapid_keypair(&mut store).unwrap();
        let auth = vapid_authorization(
            &keypair,
            "https://push.example.net:8443/send/abc123",
            "https://connect.intendant.dev",
        )
        .unwrap();
        let token = auth
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split(", k=").next())
            .unwrap();
        let mut parts = token.split('.');
        let (header, claims, signature) = (
            parts.next().unwrap(),
            parts.next().unwrap(),
            parts.next().unwrap(),
        );
        let claims_json: serde_json::Value =
            serde_json::from_slice(&b64u_decode(claims).unwrap()).unwrap();
        assert_eq!(claims_json["aud"], "https://push.example.net:8443");
        assert_eq!(claims_json["sub"], "https://connect.intendant.dev");
        let signing_input = format!("{header}.{claims}");
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            keypair.public_key().as_ref(),
        )
        .verify(signing_input.as_bytes(), &b64u_decode(signature).unwrap())
        .expect("VAPID JWT must verify against the service public key");
        // And the key survives a reload from the store.
        let reloaded = load_or_create_vapid_keypair(&mut store).unwrap();
        assert_eq!(
            keypair.public_key().as_ref(),
            reloaded.public_key().as_ref()
        );
    }

    /// Twin of the daemon's `claim_and_unclaim_payloads_pin_the_wire_format`
    /// notify case (`connect_rendezvous.rs`) — the two binaries replicate
    /// the format rather than share code, so each pins the same golden
    /// literal.
    #[test]
    fn notify_payload_pins_the_wire_format() {
        assert_eq!(
            notify_signing_payload(
                "daemon-1",
                "PubKey",
                1_700_000_000_000,
                "approval",
                "deploy review"
            ),
            "intendant-connect-daemon-notify-v1\ndaemon-1\nPubKey\n1700000000000\napproval\ndeploy review\n"
        );
    }

    /// PRIVACY pin: the push payload is exactly {title, body, url, tag},
    /// composed from the kind + the two display labels — there is no field
    /// where command text, question text, or paths could ride, and nothing
    /// beyond the inputs appears in the composed strings.
    #[test]
    fn attention_push_payload_carries_no_work_content() {
        let payload = attention_push_payload("approval", "workshop", "deploy review", "daemon-1");
        let object = payload.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["body", "tag", "title", "url"]);
        assert_eq!(payload["title"], "workshop: approval needed");
        assert_eq!(
            payload["body"],
            "Session \u{201c}deploy review\u{201d} is waiting for your approval."
        );
        assert_eq!(payload["url"], "/app?connect=1&daemon_id=daemon-1");
        assert_eq!(payload["tag"], "attention-daemon-1");

        let question = attention_push_payload("question", "workshop", "s-1", "daemon-1");
        assert_eq!(question["title"], "workshop: the agent has a question");
        assert_eq!(
            question["body"],
            "Session \u{201c}s-1\u{201d} is waiting for your answer."
        );

        // Display requests keep the same discipline: kind + labels only —
        // the agent's reason text never rides the push.
        let display = attention_push_payload("display_request", "workshop", "s-1", "daemon-1");
        let display_keys: Vec<&str> = {
            let mut keys: Vec<&str> = display.as_object().unwrap().keys().map(|k| k.as_str()).collect();
            keys.sort_unstable();
            keys
        };
        assert_eq!(display_keys, vec!["body", "tag", "title", "url"]);
        assert_eq!(display["title"], "workshop: agent asks to view your screen");
        assert_eq!(
            display["body"],
            "Session \u{201c}s-1\u{201d} is asking for display access. Open the dashboard to allow or deny."
        );
    }

    /// The nudge request wire shape has no content-bearing fields: an old
    /// or malicious daemon build sending extra fields (command text, ...)
    /// has them ignored by serde, and the vocabulary check refuses free
    /// text in `kind`.
    #[test]
    fn notify_request_kinds_are_a_closed_vocabulary() {
        for kind in NOTIFY_KINDS {
            assert!(matches!(*kind, "approval" | "question" | "display_request"));
        }
        let parsed: DaemonNotifyRequest = serde_json::from_value(json!({
            "protocol": "intendant-connect-daemon-notify-v1",
            "daemon_id": "daemon-1",
            "daemon_public_key": "PubKey",
            "issued_at_unix_ms": 1,
            "signature": "sig",
            "kind": "approval",
            "session_label": "deploy review",
            // Ignored: the shape has nowhere to put work content.
            "command": "rm -rf /",
            "question_text": "which db?"
        }))
        .unwrap();
        assert_eq!(parsed.kind, "approval");
        assert_eq!(parsed.session_label, "deploy review");
    }

    /// Pre-existing subscription records (no `notify_requests` on disk)
    /// deserialize with the flag OFF — the alert stays opt-in across the
    /// upgrade.
    #[test]
    fn notify_requests_defaults_off_for_existing_subscriptions() {
        let record: PushSubscriptionRecord = serde_json::from_value(json!({
            "user_id": Uuid::nil(),
            "endpoint": "https://push.example.net/send/abc",
            "p256dh": "key",
            "auth": "auth",
            "created_unix_ms": 1,
            "notify_presence": true
        }))
        .unwrap();
        assert!(record.notify_presence);
        assert!(!record.notify_requests);
    }
}
