use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::presence::{self, AgentStateSnapshot};
use crate::types::{LogLevel, SessionGoal};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
// Phase 5a.1: the display input authority map is read from a synchronous
// `Fn() -> bool` closure on the WebRTC data-channel input hot path, so
// it can't live behind a `tokio::sync::RwLock` (no `.read().await` from
// sync code).  `StdRwLock` is the local alias to keep that map's type
// distinct at every callsite from the unrelated `tokio::sync::RwLock`
// uses in this file.  All access goes through `unwrap_or_else(|e| e.into_inner())`
// to remain poison-tolerant, matching the rest of the file's std-lock idiom.
use std::sync::RwLock as StdRwLock;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

mod static_assets;
pub(crate) use static_assets::*;

mod http;
pub(crate) use http::*;

mod session_catalog;
pub(crate) use session_catalog::*;

mod routes_files;
pub(crate) use routes_files::*;

mod routes_sessions;
pub(crate) use routes_sessions::*;

mod routes_peers;
pub(crate) use routes_peers::*;

mod routes_access;
pub(crate) use routes_access::*;

mod mcp_gate;
pub(crate) use mcp_gate::*;
mod listener;
pub(crate) use listener::*;
mod dashboard_presence;
pub(crate) use dashboard_presence::*;
mod input_authority;
pub(crate) use input_authority::*;


/// Monotonically increasing counter for assigning unique peer IDs to WebSocket
/// connections.  Used for WebRTC signaling so that each browser tab gets a
/// stable identity within a display session.
static NEXT_PEER_ID: AtomicU64 = AtomicU64::new(1);
static SESSION_LIST_LIMITED_RESPONSE_CACHE: OnceLock<
    Mutex<HashMap<usize, SessionListResponseCacheEntry>>,
> = OnceLock::new();
static SESSION_LIST_ROW_CACHE: OnceLock<Mutex<HashMap<String, SessionListRowCacheEntry>>> =
    OnceLock::new();
static CODEX_SESSION_LIST_CACHE: OnceLock<Mutex<HashMap<String, CodexSessionListCacheEntry>>> =
    OnceLock::new();
static CODEX_PARENT_USAGE_BASELINE_CACHE: OnceLock<
    Mutex<HashMap<String, CodexParentUsageBaselineCacheEntry>>,
> = OnceLock::new();
static INTENDANT_SESSION_LIST_CACHE: OnceLock<
    Mutex<HashMap<String, IntendantSessionListCacheEntry>>,
> = OnceLock::new();

pub const DEFAULT_PORT: u16 = 8765;

/// Mint a short-lived vendor session token server-side so the browser
/// never handles (or stores) a long-lived API key.
pub(crate) async fn mint_session_token(provider: &str, model: &str) -> Result<String, String> {
    match provider {
        "openai" => {
            let api_key = crate::credential_leases::provider_api_key("OPENAI_API_KEY")
                .ok_or_else(|| "OPENAI_API_KEY not set on server".to_string())?;
            let body = serde_json::json!({
                "model": model,
            });
            let resp = reqwest::Client::new()
                .post("https://api.openai.com/v1/realtime/sessions")
                .header("Authorization", format!("Bearer {}", api_key))
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("OpenAI request failed: {}", e))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("OpenAI HTTP {}: {}", status, text));
            }
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("OpenAI parse failed: {}", e))?;
            // Response may have token at top level or nested under client_secret
            let token = data["client_secret"]["value"]
                .as_str()
                .or_else(|| data["value"].as_str())
                .ok_or_else(|| format!("No token in OpenAI response: {}", data))?;
            let expires_at = data["client_secret"]["expires_at"]
                .as_i64()
                .or_else(|| data["expires_at"].as_i64())
                .unwrap_or(0);
            Ok(serde_json::json!({
                "client_secret": { "value": token },
                "expires_at": expires_at
            })
            .to_string())
        }
        "gemini" => {
            let api_key = crate::credential_leases::provider_api_key("GEMINI_API_KEY")
                .ok_or_else(|| "GEMINI_API_KEY not set on server".to_string())?;
            let body = serde_json::json!({
                "uses": 1,
                "bidi_generate_content_setup": {
                    "model": format!("models/{}", model),
                    "generation_config": {
                        "response_modalities": ["AUDIO"],
                        "speech_config": {
                            "voice_config": {
                                "prebuilt_voice_config": {
                                    "voice_name": "Aoede"
                                }
                            }
                        }
                    }
                }
            });
            let url = format!(
                "https://generativelanguage.googleapis.com/v1alpha/auth_tokens?key={}",
                api_key
            );
            let resp = reqwest::Client::new()
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("Gemini request failed: {}", e))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("Gemini HTTP {}: {}", status, text));
            }
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("Gemini parse failed: {}", e))?;
            let token = data["name"]
                .as_str()
                .ok_or("No 'name' in Gemini response")?;
            Ok(serde_json::json!({ "token": token }).to_string())
        }
        _ => Err(format!("Unknown provider: {}", provider)),
    }
}

// Browser-facing external replay is a live UI bootstrap, not an archival export.
// Keep it bounded; native rollout files and session search remain the audit source.

/// Session-specific state that changes when a new agent session starts.
/// Wrapped in `Arc<tokio::sync::RwLock<...>>` so the web gateway can observe
/// session changes without restarting.
pub struct ActiveSessionState {
    /// Stable identity for the long-lived Intendant process. This is distinct
    /// from `session_log`, which may point at a currently active worker session
    /// and may be cleared while the dashboard waits for new tasks.
    pub daemon_session_id: Option<String>,
    pub query_ctx: Option<WebQueryCtx>,
    pub frame_registry: Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    pub session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    pub recording_registry: Option<Arc<tokio::sync::RwLock<crate::recording::RecordingRegistry>>>,
    pub session_registry: Option<crate::display::SharedSessionRegistry>,
    pub snapshot_dir: Option<PathBuf>,
    pub project_root_for_changes: Option<PathBuf>,
    /// Runtime-only daemon settings that may differ from persisted
    /// `intendant.toml` because of CLI flags such as `--agent` or
    /// `--no-presence`.
    pub runtime_settings: RuntimeSettingsState,
    /// Shared handle to the live `FileWatcher`, used to serve the per-round
    /// history endpoints (GET history, POST rollback/redo/prune). The same
    /// mutex guards snapshot creation so concurrent rollback from the web
    /// gateway and snapshot-on-round-complete can't race.
    pub file_watcher: Option<crate::file_watcher::SharedFileWatcher>,
}

impl ActiveSessionState {
    #[allow(dead_code)]
    pub fn empty() -> SharedActiveSession {
        Arc::new(tokio::sync::RwLock::new(Self {
            daemon_session_id: None,
            query_ctx: None,
            frame_registry: None,
            session_log: None,
            recording_registry: None,
            session_registry: None,
            snapshot_dir: None,
            project_root_for_changes: None,
            runtime_settings: RuntimeSettingsState::default(),
            file_watcher: None,
        }))
    }
}

pub type SharedActiveSession = Arc<tokio::sync::RwLock<ActiveSessionState>>;

#[derive(Clone, Default)]
pub struct RuntimeSettingsState {
    pub external_agent:
        Option<Arc<tokio::sync::RwLock<Option<crate::external_agent::AgentBackend>>>>,
    pub presence_enabled: Option<bool>,
}

/// Context for answering presence tool queries from browser-side live models.
/// Shared across all WebSocket connections (read-only for query tools).
#[derive(Clone)]
pub struct WebQueryCtx {
    pub agent_state: Arc<Mutex<AgentStateSnapshot>>,
    pub project_root: PathBuf,
    pub log_dir: PathBuf,
    pub knowledge_path: PathBuf,
    /// Server-authoritative presence session (event window + checkpoint state).
    pub presence_session: Option<Arc<Mutex<crate::presence::PresenceSession>>>,
    /// Shared context injection queue for mid-task interjections.
    pub context_injection: Option<crate::event::ContextInjectionQueue>,
}


/// Debug state for the voice model, tracked server-side from WebSocket messages.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VoiceDebugState {
    pub connected: bool,
    pub voice_log_count: u32,
    pub last_voice_log: String,
}

/// Voice + WebRTC runtime config sent to the web frontend via `/config`.
///
/// Scoped to *runtime config only* — the voice provider, the active
/// model, audio sample rates, and WebRTC ICE servers. Identity-shaped
/// fields (host label, version, git sha) moved out of `/config` and
/// into the Agent Card served at `/.well-known/agent-card.json`: see
/// [`crate::peer::AgentCard`] and [`crate::peer::AgentCard::local_intendant`].
/// That's the single source of truth for who this daemon is and what
/// it can do, and keeping `/config` narrow makes it less likely that
/// future runtime config additions re-blur the boundary.
#[derive(Clone, Debug, Serialize)]
pub struct WebGatewayConfig {
    pub provider: String,
    pub model: String,
    /// Effective server-side presence state for this running daemon. This is
    /// intentionally runtime-scoped, because `--no-presence` can override the
    /// persisted `[presence] enabled` setting.
    #[serde(default)]
    pub presence_enabled: bool,
    /// Effective external-agent backend selected for this daemon at startup.
    /// The voice provider/model above remain scoped to browser live audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_agent: Option<String>,
    pub input_sample_rate: u32,
    pub output_sample_rate: u32,
    /// Whether server-side transcription is enabled (browser should send user_audio).
    #[serde(default)]
    pub transcription_enabled: bool,
    /// ICE servers for WebRTC peer connections (STUN/TURN).
    /// Empty by default (local-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ice_servers: Vec<crate::display::IceServer>,
    /// Whether the *federated* (peer-to-peer) display path may negotiate
    /// H.264. Default false ⇒ the browser pins VP8 for federation (the
    /// safe default for lossy TURN-relayed paths). When true the browser
    /// prefers H.264, allowing the peer's federated H.264 layer
    /// (quarter-resolution, capped bitrate, periodic IDRs, same-SSRC NACK,
    /// small slices) to be selected. Does NOT affect the *local* DisplaySlot
    /// path, which already defaults codec order. Sourced from
    /// `[webrtc].federation_allow_h264` in intendant.toml.
    #[serde(default)]
    pub federation_allow_h264: bool,
    /// Public peer access-request hardening. This is gateway runtime state,
    /// not browser config, so `/config` intentionally omits it.
    #[serde(skip)]
    pub peer_access_requests: crate::project::PeerAccessRequestConfig,
    /// Experimental Connect rendezvous client config. This is daemon runtime
    /// state, not browser config, so `/config` intentionally omits it.
    #[serde(skip)]
    pub connect: crate::project::ConnectConfig,
}

impl Default for WebGatewayConfig {
    fn default() -> Self {
        Self {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash-native-audio-preview-12-2025".to_string(),
            presence_enabled: false,
            external_agent: None,
            input_sample_rate: 16000,
            output_sample_rate: 24000,
            transcription_enabled: false,
            ice_servers: Vec::new(),
            federation_allow_h264: false,
            peer_access_requests: crate::project::PeerAccessRequestConfig::default(),
            connect: crate::project::ConnectConfig::default(),
        }
    }
}


// Deliberately no Access-Control-Allow-Origin here: API responses are
// same-origin by default. Cross-origin readability is opt-in — the fleet
// Access APIs echo allowlisted origins (`with_fleet_cors`) and the public
// bootstrap surfaces use `with_public_cors`. A blanket wildcard would let
// any website read cert-authenticated responses through a visitor's
// browser (see docs/src/trust-architecture.md).


// ── Persistent session-list index ──
// The per-session caches below already carry exact invalidation
// (len/mtime/ctime/dev/ino fingerprints); persisting the entries makes
// that validity survive daemon restarts, so a cold start re-parses only
// sessions that actually changed instead of every log in every store
// (~tens of seconds on a real corpus). One small JSON file per entry,
// written atomically via tempfile+rename: daemons sharing a HOME can only
// race toward equivalent content, never corrupt an entry. External
// stores (~/.codex, ~/.claude, ~/.gemini) are never written — the index
// mirrors them under ~/.intendant/cache/session_index/.
//
// Entries carry a per-namespace schema stamp: when the value shape changes
// in a way serde would accept silently (a removed or defaulted field),
// bumping the namespace schema turns every old entry into a cache miss so
// it is rebuilt in place under the same slot filename. Entries whose
// source path no longer exists are pruned during the preload sweep —
// deleted sessions otherwise accumulate dead index files forever.


// Stale-while-revalidate: within the TTL a cached list is fresh; past it
// (up to the stale ceiling) the cached body is served IMMEDIATELY and one
// background refresh is kicked, so an interactive dashboard never blocks
// on a rescan. Only a very stale (or absent) entry rebuilds inline.


// ---------------------------------------------------------------------------
// Per-round file snapshot history endpoints
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// File upload endpoints
// ---------------------------------------------------------------------------


// Same-origin by default; see `json_response_body` for the CORS rationale.


async fn connect_dashboard_offer_response(
    dashboard_control: &Arc<crate::dashboard_control::DashboardControlRegistry>,
    body_text: &str,
    agent_card: &serde_json::Value,
) -> String {
    let body = match serde_json::from_str::<serde_json::Value>(body_text) {
        Ok(body) => body,
        Err(e) => return json_error("400 Bad Request", format!("invalid JSON: {e}")),
    };
    let sdp = body.get("sdp").and_then(|v| v.as_str()).unwrap_or("");
    if sdp.trim().is_empty() {
        return json_error("400 Bad Request", "missing sdp");
    }
    let client_nonce = body
        .get("client_nonce")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|nonce| !nonce.is_empty())
        .map(str::to_string);
    // Reaching this path already required a trusted transport (mTLS or local
    // loopback), so sessions stay root-compatible. A signed browser identity
    // key refines that: a key with a local IAM grant binds the session to
    // that scoped principal, and an ungranted key keeps root authority but
    // surfaces its fingerprint so the Access UI can offer enrollment. An
    // invalid signature fails closed.
    let client_key_fields: crate::access::client_key::ClientKeyOfferFields =
        serde_json::from_value(body.clone()).unwrap_or_default();
    let verified_client_key = match client_key_fields.verify(
        "",
        client_nonce.as_deref().unwrap_or(""),
        sdp,
        crate::access::client_key::now_unix_ms(),
    ) {
        Ok(verified) => verified,
        Err(e) => {
            return json_error(
                "400 Bad Request",
                format!("client key verification failed: {e}"),
            )
        }
    };
    // Org-grant ride-along (phase 6 step 4), same as the rendezvous path:
    // materialize before grant resolution so a member's first offer binds
    // its scoped principal instead of falling back to trusted-transport
    // root. Failure changes nothing here — the transport already earned
    // its authority — but the error is surfaced for the console.
    let org_grant_error = body
        .get("org_grant")
        .filter(|doc| !doc.is_null())
        .and_then(|doc| {
            crate::access::org::present_org_grant_value(
                doc,
                &org_target_agent_card_ids(agent_card),
                crate::access::client_key::now_unix_ms() as u64,
            )
            .err()
        });
    let grant = match verified_client_key {
        Some(key) => {
            let cert_dir = crate::access::backend::select_backend().cert_dir();
            let loaded = load_local_iam_state_for_request(&cert_dir).ok().flatten();
            let bound = loaded.as_ref().and_then(|state| {
                crate::access::iam::principal_for_client_key(
                    state,
                    &key.fingerprint,
                    "local-dashboard-control",
                )
                .or_else(|| {
                    crate::access::iam::principal_for_client_key_any_status(
                        state,
                        &key.fingerprint,
                        "local-dashboard-control",
                    )
                })
                .map(|principal| (principal, state.clone()))
            });
            match bound {
                Some((principal, iam_state)) => {
                    crate::dashboard_control::DashboardControlGrant::UserClient {
                        principal,
                        iam_state,
                    }
                }
                None => crate::dashboard_control::DashboardControlGrant::UserClientRoot {
                    principal:
                        crate::access::iam::AccessPrincipal::root_dashboard_session_with_client_key(
                            "dashboard-control",
                            "local-dashboard-control",
                            &key.fingerprint,
                            &key.public_key_b64u,
                        ),
                },
            }
        }
        None => crate::dashboard_control::DashboardControlGrant::TrustedLocal,
    };
    match dashboard_control
        .answer_offer_with_grant(sdp.to_string(), None, client_nonce, grant)
        .await
    {
        Ok(answer) => {
            let mut response = serde_json::json!({
                "ok": true,
                "signaling": "connect-bootstrap-local",
                "session_id": answer.session_id,
                "sdp": answer.sdp,
                "binding": answer.binding,
            });
            if let Some(org_error) = org_grant_error {
                response["org_grant_error"] = serde_json::Value::String(org_error);
            }
            json_ok(response)
        }
        Err(e) => json_error("500 Internal Server Error", e),
    }
}

async fn connect_dashboard_ice_response(
    dashboard_control: &Arc<crate::dashboard_control::DashboardControlRegistry>,
    body_text: &str,
) -> String {
    let body = match serde_json::from_str::<serde_json::Value>(body_text) {
        Ok(body) => body,
        Err(e) => return json_error("400 Bad Request", format!("invalid JSON: {e}")),
    };
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if session_id.is_empty() {
        return json_error("400 Bad Request", "missing session_id");
    }
    let candidate = body
        .get("candidate")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    match dashboard_control
        .add_ice_candidate(session_id, &candidate)
        .await
    {
        Ok(true) => json_ok(serde_json::json!({ "ok": true })),
        Ok(false) => json_error("404 Not Found", "dashboard control session not found"),
        Err(e) => json_error("500 Internal Server Error", e),
    }
}

async fn connect_dashboard_close_response(
    dashboard_control: &Arc<crate::dashboard_control::DashboardControlRegistry>,
    body_text: &str,
) -> String {
    let body = match serde_json::from_str::<serde_json::Value>(body_text) {
        Ok(body) => body,
        Err(e) => return json_error("400 Bad Request", format!("invalid JSON: {e}")),
    };
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if session_id.is_empty() {
        return json_error("400 Bad Request", "missing session_id");
    }
    dashboard_control.close(session_id).await;
    json_ok(serde_json::json!({ "ok": true }))
}

fn connect_bootstrap_html() -> String {
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Intendant Connect Bootstrap</title>
  <style>
    :root { color-scheme: dark; font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background: #11111b; color: #cdd6f4; }
    body { margin: 0; min-height: 100vh; display: grid; place-items: center; }
    main { width: min(760px, calc(100vw - 32px)); }
    h1 { font-size: 22px; margin: 0 0 16px; }
    pre { white-space: pre-wrap; overflow-wrap: anywhere; padding: 16px; background: #181825; border: 1px solid #45475a; border-radius: 8px; }
    .ok { color: #a6e3a1; }
    .err { color: #f38ba8; }
  </style>
</head>
<body>
  <main>
    <h1>Intendant Connect Bootstrap</h1>
    <pre id="status">starting</pre>
  </main>
  <script>
(() => {
	  const statusEl = document.getElementById('status');
	  const MAX_CHUNKED_RESPONSE_BYTES = 128 * 1024 * 1024;
	  const MAX_BYTE_STREAM_BYTES = 128 * 1024 * 1024;
	  const UPLOAD_CHUNK_BYTES = 16 * 1024;
	  const UPLOAD_BUFFER_HIGH_BYTES = 1024 * 1024;
	  function paint(message, kind = '') {
	    statusEl.textContent = typeof message === 'string' ? message : JSON.stringify(message, null, 2);
	    statusEl.className = kind;
	  }

  function abortError(message = 'dashboard control request aborted') {
    try { return new DOMException(message, 'AbortError'); } catch {
      const err = new Error(message);
      err.name = 'AbortError';
      return err;
    }
  }

  function bytesToBase64Url(bytes) {
    let binary = '';
    for (const b of bytes) binary += String.fromCharCode(b);
    return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/g, '');
  }

  function base64UrlToBytes(value) {
    const padded = String(value || '').replace(/-/g, '+').replace(/_/g, '/').padEnd(Math.ceil(String(value || '').length / 4) * 4, '=');
    const binary = atob(padded);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
  }

	  function base64ToBytes(value) {
	    const binary = atob(String(value || ''));
	    const bytes = new Uint8Array(binary.length);
	    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
	    return bytes;
	  }

	  function bytesToBase64(bytes) {
	    let binary = '';
	    for (let i = 0; i < bytes.byteLength; i++) binary += String.fromCharCode(bytes[i]);
	    return btoa(binary);
	  }

  async function sha256B64u(text) {
    const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(String(text)));
    return bytesToBase64Url(new Uint8Array(digest));
  }

  function randomB64u(byteLength = 32) {
    const bytes = new Uint8Array(byteLength);
    crypto.getRandomValues(bytes);
    return bytesToBase64Url(bytes);
  }

  function bindingPayload(binding) {
    const parts = [
      binding.protocol || '',
      binding.session_id || '',
      binding.daemon_public_key || '',
      String(binding.created_unix_ms || ''),
      String(binding.expires_unix_ms || ''),
      binding.offer_sha256 || '',
      binding.answer_sha256 || '',
    ];
    if (binding.client_nonce) parts.push(binding.client_nonce);
    if (binding.session_grant_sha256) parts.push(binding.session_grant_sha256);
    return parts.join('\n');
  }

  async function verifyEd25519(publicKeyBytes, signatureBytes, payloadBytes) {
    let key;
    try {
      key = await crypto.subtle.importKey('raw', publicKeyBytes, { name: 'Ed25519' }, false, ['verify']);
    } catch (firstErr) {
      try {
        key = await crypto.subtle.importKey('raw', publicKeyBytes, 'Ed25519', false, ['verify']);
      } catch {
        throw firstErr;
      }
    }
    return crypto.subtle.verify({ name: 'Ed25519' }, key, signatureBytes, payloadBytes);
  }

  async function verifyBinding(binding, sessionId, offerSdp, answerSdp, clientNonce = '') {
    if (!binding || typeof binding !== 'object') return { ok: false, error: 'missing binding' };
    if (binding.protocol !== 'intendant-dashboard-control-v1') return { ok: false, error: 'unexpected protocol' };
    if (String(binding.session_id || '') !== String(sessionId || '')) return { ok: false, error: 'session mismatch' };
    if (!crypto?.subtle) return { ok: false, error: 'WebCrypto unavailable' };
    const createdUnixMs = Number(binding.created_unix_ms || 0);
    const expiresUnixMs = Number(binding.expires_unix_ms || 0);
    if (!Number.isFinite(createdUnixMs) || createdUnixMs <= 0) return { ok: false, error: 'missing binding creation time' };
    if (!Number.isFinite(expiresUnixMs) || expiresUnixMs <= 0) return { ok: false, error: 'missing binding expiry' };
    const nowUnixMs = Date.now();
    if (expiresUnixMs + 30000 < nowUnixMs) return { ok: false, error: 'binding expired' };
    if (createdUnixMs - 30000 > nowUnixMs) return { ok: false, error: 'binding timestamp from future' };
    if (binding.offer_sha256 !== await sha256B64u(offerSdp || '')) return { ok: false, error: 'offer hash mismatch' };
    if (binding.answer_sha256 !== await sha256B64u(answerSdp || '')) return { ok: false, error: 'answer hash mismatch' };
    const nonce = String(clientNonce || '');
    if (nonce) {
      if (String(binding.client_nonce || '') !== nonce) return { ok: false, error: 'client nonce mismatch' };
    } else if (binding.client_nonce) {
      return { ok: false, error: 'unexpected client nonce binding' };
    }
    const verified = await verifyEd25519(
      base64UrlToBytes(binding.daemon_public_key || ''),
      base64UrlToBytes(binding.signature || ''),
      new TextEncoder().encode(bindingPayload(binding))
    );
    if (!verified) return { ok: false, error: 'signature invalid' };
    return {
      ok: true,
      daemonPublicKey: binding.daemon_public_key,
      createdUnixMs,
      expiresUnixMs,
      clientNonce: binding.client_nonce || '',
    };
  }

  const connect = {
    pc: null,
    channel: null,
    sessionId: '',
    binding: null,
    verifiedBinding: null,
    clientNonce: '',
    expiresUnixMs: 0,
    localOfferSdp: '',
	    pendingIce: [],
	    pending: new Map(),
	    chunkedResponses: new Map(),
	    byteStreams: new Map(),
	    completedChunkedResponses: 0,
	    completedByteStreams: 0,
	    seq: 0,
	    started: false,

    async start() {
      if (this.started) return this.status();
      this.started = true;
	      this.pc = new RTCPeerConnection({});
	      this.channel = this.pc.createDataChannel('intendant-dashboard-control', { ordered: true });
	      this.channel.onopen = () => {
	        this.sendFrame({ t: 'hello', id: this.nextId(), features: ['response_credit', 'byte_streams', 'upload_frames'] });
	        paint(this.status(), 'ok');
	      };
      this.channel.onmessage = ev => this.handleMessage(ev.data);
      this.channel.onclose = () => paint(this.status());
      this.pc.onconnectionstatechange = () => paint(this.status(), this.pc.connectionState === 'failed' ? 'err' : '');
      this.pc.onicecandidate = ev => {
        if (!ev.candidate) return;
        const candidate = ev.candidate.toJSON ? ev.candidate.toJSON() : ev.candidate;
        if (!this.sessionId) {
          this.pendingIce.push(candidate);
          return;
        }
        this.sendIce(candidate).catch(err => console.warn('connect ice failed', err));
      };
      const offer = await this.pc.createOffer();
      await this.pc.setLocalDescription(offer);
      this.localOfferSdp = offer.sdp || '';
      this.clientNonce = randomB64u(32);
      const answer = await fetch('/connect/dashboard/offer', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ sdp: this.localOfferSdp, client_nonce: this.clientNonce }),
      }).then(async resp => {
        const body = await resp.json().catch(() => ({}));
        if (!resp.ok) throw new Error(body.error || `HTTP ${resp.status}`);
        return body;
      });
      this.sessionId = String(answer.session_id || '');
      this.binding = answer.binding || null;
      const verified = await verifyBinding(this.binding, this.sessionId, this.localOfferSdp, answer.sdp || '', this.clientNonce);
      if (!verified.ok) throw new Error(`binding rejected: ${verified.error || 'unknown'}`);
      this.verifiedBinding = verified;
      this.expiresUnixMs = verified.expiresUnixMs || 0;
      await this.pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
      for (const candidate of this.pendingIce.splice(0)) await this.sendIce(candidate);
      paint(this.status(), 'ok');
      return this.status();
    },

    async sendIce(candidate) {
      await fetch('/connect/dashboard/ice', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ session_id: this.sessionId, candidate }),
      });
    },

    handleMessage(data) {
      let msg;
      try { msg = JSON.parse(String(data)); } catch { return; }
      this.handleFrame(msg);
    },

    handleFrame(msg) {
      if (msg.t === 'hello_ack') {
        paint(this.status(), 'ok');
        return;
      }
      if (msg.t === 'response_start') {
        this.handleResponseStart(msg);
        return;
      }
      if (msg.t === 'response_chunk') {
        this.handleResponseChunk(msg);
        return;
      }
	      if (msg.t === 'response_end') {
	        this.handleResponseEnd(msg);
	        return;
	      }
	      if (msg.t === 'byte_stream_start') {
	        this.handleByteStreamStart(msg);
	        return;
	      }
	      if (msg.t === 'byte_stream_chunk') {
	        this.handleByteStreamChunk(msg);
	        return;
	      }
	      if (msg.t === 'byte_stream_end') {
	        this.handleByteStreamEnd(msg);
	        return;
	      }
	      if (msg.t !== 'pong' && msg.t !== 'response') return;
      const pending = this.pending.get(msg.id);
      if (!pending) return;
      this.pending.delete(msg.id);
      if (msg.cancelled) pending.reject(abortError(msg.error || 'dashboard control request cancelled'));
      else if (msg.t === 'response' && msg.ok === false) pending.reject(new Error(msg.error || 'dashboard control request failed'));
      else pending.resolve(msg.t === 'pong' ? msg : msg.result);
    },

    handleResponseStart(msg) {
      const id = String(msg.id || '');
      if (!id || !this.pending.has(id)) return;
      const totalBytes = Number(msg.total_bytes);
      const expectedChunks = Number(msg.chunks);
      if (
        msg.encoding !== 'base64-json-frame' ||
        !Number.isSafeInteger(totalBytes) ||
        totalBytes < 0 ||
        totalBytes > MAX_CHUNKED_RESPONSE_BYTES ||
        !Number.isSafeInteger(expectedChunks) ||
        expectedChunks < 0
      ) {
        this.rejectChunkedResponse(id, 'invalid connect dashboard chunked response header');
        return;
      }
      this.chunkedResponses.set(id, {
        totalBytes,
        expectedChunks,
        receivedBytes: 0,
        chunks: new Map(),
        ended: false,
      });
      paint(this.status(), 'ok');
    },

    handleResponseChunk(msg) {
      const id = String(msg.id || '');
      const state = this.chunkedResponses.get(id);
      if (!state) return;
      const seq = Number(msg.seq);
      if (!Number.isSafeInteger(seq) || seq < 0 || seq >= state.expectedChunks) {
        this.rejectChunkedResponse(id, 'invalid connect dashboard chunk sequence');
        return;
      }
      if (state.chunks.has(seq)) return;
      let bytes;
      try {
        bytes = base64ToBytes(msg.data);
      } catch {
        this.rejectChunkedResponse(id, 'invalid connect dashboard chunk encoding');
        return;
      }
      state.chunks.set(seq, bytes);
      state.receivedBytes += bytes.byteLength;
      if (state.receivedBytes > state.totalBytes) {
        this.rejectChunkedResponse(id, 'connect dashboard chunked response exceeded declared size');
        return;
      }
      const completed = this.maybeCompleteChunkedResponse(id);
      if (!completed && this.chunkedResponses.has(id)) {
        this.sendChunkCredit(id, 1);
      }
      paint(this.status(), 'ok');
    },

    handleResponseEnd(msg) {
      const id = String(msg.id || '');
      const state = this.chunkedResponses.get(id);
      if (!state) return;
      const finalChunks = Number(msg.chunks);
      if (!Number.isSafeInteger(finalChunks) || finalChunks !== state.expectedChunks) {
        this.rejectChunkedResponse(id, 'invalid connect dashboard chunked response footer');
        return;
      }
      state.ended = true;
      this.maybeCompleteChunkedResponse(id);
      paint(this.status(), 'ok');
    },

    maybeCompleteChunkedResponse(id) {
      const state = this.chunkedResponses.get(id);
      if (!state || !state.ended || state.chunks.size !== state.expectedChunks) return false;
      const merged = new Uint8Array(state.totalBytes);
      let offset = 0;
      for (let seq = 0; seq < state.expectedChunks; seq++) {
        const chunk = state.chunks.get(seq);
        if (!chunk) {
          this.rejectChunkedResponse(id, 'connect dashboard chunked response missed a chunk');
          return false;
        }
        merged.set(chunk, offset);
        offset += chunk.byteLength;
      }
      if (offset !== state.totalBytes) {
        this.rejectChunkedResponse(id, 'connect dashboard chunked response size mismatch');
        return false;
      }
      this.chunkedResponses.delete(id);
      let frame;
      try {
        frame = JSON.parse(new TextDecoder().decode(merged));
      } catch {
        this.rejectChunkedResponse(id, 'connect dashboard chunked response was not valid JSON');
        return false;
      }
      if (frame.t !== 'response' || String(frame.id || '') !== id) {
        this.rejectChunkedResponse(id, 'connect dashboard chunked response id mismatch');
        return false;
      }
      this.completedChunkedResponses += 1;
      this.handleFrame(frame);
      return true;
    },

    rejectChunkedResponse(id, message) {
      this.chunkedResponses.delete(id);
      const pending = this.pending.get(id);
      if (pending) {
        this.pending.delete(id);
        pending.reject(new Error(message));
      }
	      paint(this.status(), 'err');
	    },

	    handleByteStreamStart(msg) {
	      const id = String(msg.id || '');
	      const streamId = String(msg.stream_id || id);
	      if (!id || !streamId || !this.pending.has(id)) return;
	      const totalBytes = Number(msg.total_bytes);
	      const expectedChunks = Number(msg.chunks);
	      if (
	        msg.encoding !== 'base64' ||
	        !Number.isSafeInteger(totalBytes) ||
	        totalBytes < 0 ||
	        totalBytes > MAX_BYTE_STREAM_BYTES ||
	        !Number.isSafeInteger(expectedChunks) ||
	        expectedChunks < 0
	      ) {
	        this.rejectByteStream(streamId, 'invalid connect dashboard byte stream header', id);
	        return;
	      }
	      this.byteStreams.set(streamId, {
	        id,
	        streamId,
	        totalBytes,
	        expectedChunks,
	        receivedBytes: 0,
	        chunks: new Map(),
	        ended: false,
	        result: null,
	        contentType: String(msg.content_type || 'application/octet-stream'),
	        filename: msg.filename ? String(msg.filename) : '',
	      });
	      paint(this.status(), 'ok');
	    },

	    handleByteStreamChunk(msg) {
	      const id = String(msg.id || '');
	      const streamId = String(msg.stream_id || id);
	      const state = this.byteStreams.get(streamId);
	      if (!state) return;
	      const seq = Number(msg.seq);
	      if (!Number.isSafeInteger(seq) || seq < 0 || seq >= state.expectedChunks) {
	        this.rejectByteStream(streamId, 'invalid connect dashboard byte stream chunk sequence');
	        return;
	      }
	      if (state.chunks.has(seq)) return;
	      let bytes;
	      try {
	        bytes = base64ToBytes(msg.data);
	      } catch {
	        this.rejectByteStream(streamId, 'invalid connect dashboard byte stream encoding');
	        return;
	      }
	      state.chunks.set(seq, bytes);
	      state.receivedBytes += bytes.byteLength;
	      if (state.receivedBytes > state.totalBytes) {
	        this.rejectByteStream(streamId, 'connect dashboard byte stream exceeded declared size');
	        return;
	      }
	      const completed = this.maybeCompleteByteStream(streamId);
	      if (!completed && this.byteStreams.has(streamId)) {
	        this.sendChunkCredit(id, 1, streamId === id ? null : streamId);
	      }
	      paint(this.status(), 'ok');
	    },

	    handleByteStreamEnd(msg) {
	      const id = String(msg.id || '');
	      const streamId = String(msg.stream_id || id);
	      const state = this.byteStreams.get(streamId);
	      if (!state) return;
	      if (msg.ok === false) {
	        this.rejectByteStream(streamId, msg.error || 'connect dashboard byte stream failed');
	        return;
	      }
	      const finalChunks = Number(msg.chunks);
	      if (!Number.isSafeInteger(finalChunks) || finalChunks !== state.expectedChunks) {
	        this.rejectByteStream(streamId, 'invalid connect dashboard byte stream footer');
	        return;
	      }
	      state.ended = true;
	      state.result = msg.result || null;
	      this.maybeCompleteByteStream(streamId);
	      paint(this.status(), 'ok');
	    },

	    maybeCompleteByteStream(streamId) {
	      const state = this.byteStreams.get(streamId);
	      if (!state || !state.ended || state.chunks.size !== state.expectedChunks) return false;
	      const merged = new Uint8Array(state.totalBytes);
	      let offset = 0;
	      for (let seq = 0; seq < state.expectedChunks; seq++) {
	        const chunk = state.chunks.get(seq);
	        if (!chunk) {
	          this.rejectByteStream(streamId, 'connect dashboard byte stream missed a chunk');
	          return false;
	        }
	        merged.set(chunk, offset);
	        offset += chunk.byteLength;
	      }
	      if (offset !== state.totalBytes) {
	        this.rejectByteStream(streamId, 'connect dashboard byte stream size mismatch');
	        return false;
	      }
	      this.byteStreams.delete(streamId);
	      const pending = this.pending.get(state.id);
	      if (!pending) return true;
	      const result = state.result && typeof state.result === 'object' && !Array.isArray(state.result)
	        ? { ...state.result }
	        : {};
	      result.ok = result.ok !== false;
	      result.bytes = merged;
	      result.size = state.totalBytes;
	      result.content_type = result.content_type || state.contentType;
	      result.filename = result.filename || state.filename;
	      result.stream_id = state.streamId;
	      this.completedByteStreams += 1;
	      this.pending.delete(state.id);
	      this.chunkedResponses.delete(state.id);
	      pending.resolve(result);
	      paint(this.status(), 'ok');
	      return true;
	    },

	    rejectByteStream(streamId, message, requestId = '') {
	      const state = this.byteStreams.get(streamId);
	      const id = state?.id || requestId || streamId;
	      this.byteStreams.delete(streamId);
	      const pending = this.pending.get(id);
	      if (pending) {
	        this.pending.delete(id);
	        pending.reject(new Error(message));
	      }
	      paint(this.status(), 'err');
	    },

	    request(method, params = {}, options = {}) {
	      if (options.signal?.aborted) return Promise.reject(abortError());
	      if (!this.canUseRpc()) return Promise.reject(new Error('connect dashboard RPC is not connected'));
	      const id = this.nextId();
	      const promise = this.waitFor(id, options);
	      this.sendFrame({ t: 'request', id, method, params });
	      return promise;
	    },

	    requestBytes(method, params = {}, options = {}) {
	      if (options.signal?.aborted) return Promise.reject(abortError());
	      if (!this.canUseRpc()) return Promise.reject(new Error('connect dashboard byte stream is not connected'));
	      const id = this.nextId();
	      const promise = this.waitFor(id, options);
	      this.sendFrame({ t: 'request', id, method, params, bytes: true });
	      return promise;
	    },

	    async uploadBytes(method, params = {}, bytes, options = {}) {
	      if (options.signal?.aborted) return Promise.reject(abortError());
	      if (!this.canUseRpc()) return Promise.reject(new Error('connect dashboard upload is not connected'));
	      const id = this.nextId();
	      const totalBytes = Number(bytes?.size ?? bytes?.byteLength ?? bytes?.length ?? 0);
	      const chunkSize = options.chunkBytes || UPLOAD_CHUNK_BYTES;
	      const chunks = Math.ceil(totalBytes / chunkSize);
	      const promise = this.waitFor(id, options);
	      this.sendFrame({
	        t: 'upload_start',
	        id,
	        method,
	        params,
	        encoding: 'base64',
	        total_bytes: totalBytes,
	        chunks,
	      });
	      try {
	        for (let seq = 0, offset = 0; offset < totalBytes; seq += 1, offset += chunkSize) {
	          if (options.signal?.aborted) throw abortError();
	          if (!this.pending.has(id)) break;
	          const end = Math.min(offset + chunkSize, totalBytes);
	          let chunk;
	          if (bytes instanceof Blob) {
	            chunk = new Uint8Array(await bytes.slice(offset, end).arrayBuffer());
	          } else if (bytes instanceof Uint8Array) {
	            chunk = bytes.subarray(offset, end);
	          } else {
	            chunk = new Uint8Array(bytes.slice(offset, end));
	          }
	          this.sendFrame({
	            t: 'upload_chunk',
	            id,
	            seq,
	            data: bytesToBase64(chunk),
	          });
	          await this.waitForBufferedAmountLow(options.signal);
	        }
	        if (this.pending.has(id)) this.sendFrame({ t: 'upload_end', id, chunks });
	      } catch (err) {
	        if (this.pending.has(id)) this.sendFrame({ t: 'cancel', id });
	        throw err;
	      }
	      return promise;
	    },

	    async waitForBufferedAmountLow(signal = null) {
	      while (
	        this.channel &&
	        this.channel.readyState === 'open' &&
	        this.channel.bufferedAmount > UPLOAD_BUFFER_HIGH_BYTES
	      ) {
	        if (signal?.aborted) throw abortError();
	        await new Promise(resolve => setTimeout(resolve, 10));
	      }
	    },

    waitFor(id, options = {}) {
      return new Promise((resolve, reject) => {
        let settled = false;
        const signal = options.signal || null;
        const fail = (err, cancel = false) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
	          this.pending.delete(id);
	          this.chunkedResponses.delete(id);
	          this.deleteByteStreamsForRequest(id);
	          if (cancel) this.sendFrame({ t: 'cancel', id });
	          reject(err);
	        };
        const abortHandler = signal ? () => fail(abortError(), true) : null;
        const timeoutMs = Number.isFinite(Number(options.timeoutMs)) ? Number(options.timeoutMs) : 10000;
        const timer = setTimeout(() => fail(new Error('connect dashboard request timed out'), true), timeoutMs);
        if (signal && abortHandler) signal.addEventListener('abort', abortHandler, { once: true });
        this.pending.set(id, {
	          resolve: value => {
	            if (settled) return;
	            settled = true;
	            clearTimeout(timer);
	            if (signal && abortHandler) signal.removeEventListener('abort', abortHandler);
	            this.chunkedResponses.delete(id);
	            this.deleteByteStreamsForRequest(id);
	            resolve(value);
	          },
          reject: err => fail(err),
        });
      });
    },

    canUseRpc() {
      return Boolean(this.verifiedBinding && this.pc?.connectionState === 'connected' && this.channel?.readyState === 'open');
    },

    sendFrame(frame) {
      if (this.channel?.readyState === 'open') this.channel.send(JSON.stringify(frame));
    },

	    sendChunkCredit(id, chunks, chunkId = null) {
	      const frame = { t: 'credit', id, chunks };
	      if (chunkId) frame.chunk_id = chunkId;
	      this.sendFrame(frame);
	    },

	    deleteByteStreamsForRequest(id) {
	      for (const [streamId, state] of this.byteStreams) {
	        if (streamId === id || state?.id === id) this.byteStreams.delete(streamId);
	      }
	    },

    status() {
      return {
        connected: this.pc?.connectionState === 'connected',
        pcState: this.pc?.connectionState || '',
	        channelState: this.channel?.readyState || '',
	        sessionId: this.sessionId,
	        verifiedBinding: this.verifiedBinding,
	        clientNonce: this.clientNonce,
	        expiresUnixMs: this.expiresUnixMs,
	        pendingRequests: this.pending.size,
	        pendingChunkedResponses: this.chunkedResponses.size,
	        pendingByteStreams: this.byteStreams.size,
	        completedChunkedResponses: this.completedChunkedResponses,
	        completedByteStreams: this.completedByteStreams,
	      };
	    },

    close() {
      if (this.sessionId) {
        fetch('/connect/dashboard/close', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ session_id: this.sessionId }),
        }).catch(() => {});
	      }
	      this.chunkedResponses.clear();
	      this.byteStreams.clear();
	      try { this.channel?.close(); } catch {}
      try { this.pc?.close(); } catch {}
    },

    nextId() {
      this.seq += 1;
      return `connect-${Date.now()}-${this.seq}`;
    },
  };

  window.intendantConnectDashboard = connect;
  connect.start().catch(err => {
    console.error(err);
    paint(err?.message || String(err), 'err');
  });
})();
  </script>
</body>
</html>"#
        .to_string()
}


/// Check whether it is safe to mutate the project tree (rollback/redo) right
/// now. Returns `Ok(())` if idle, or an `(status_code, body_json)` pair to
/// send back as-is.
fn ensure_idle(
    agent_state: Option<&Arc<Mutex<AgentStateSnapshot>>>,
) -> Result<(), (&'static str, String)> {
    if let Some(state) = agent_state {
        let phase = state.lock().map(|g| g.phase.clone()).unwrap_or_default();
        if !presence::is_agent_idle(&phase) {
            let body = serde_json::json!({
                "error": "agent is busy, stop the turn before rolling back",
                "phase": phase,
            })
            .to_string();
            return Err(("409 Conflict", body));
        }
    }
    Ok(())
}


/// Settings payload for GET/POST /api/settings.
/// Flattened view of intendant.toml sections relevant to the web dashboard.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SettingsPayload {
    // Computer Use
    pub cu_provider: Option<String>,
    pub cu_model: Option<String>,
    pub cu_backend: String,
    /// Read-only: `[experimental] cu_first_routing` from intendant.toml.
    /// The dashboard shows the CU provider/model rows only when the
    /// vaulted routing is enabled; the flag itself is file-only.
    #[serde(default)]
    pub cu_first_routing: bool,
    // Presence
    pub presence_enabled: bool,
    pub presence_provider: Option<String>,
    pub presence_model: Option<String>,
    pub presence_live_provider: Option<String>,
    pub presence_live_model: Option<String>,
    // Transcription
    pub transcription_enabled: bool,
    pub transcription_provider: String,
    pub transcription_model: String,
    pub transcription_endpoint: Option<String>,
    pub transcription_language: Option<String>,
    // Recording
    pub recording_enabled: bool,
    pub recording_framerate: u32,
    pub recording_quality: String,
    // Live Audio
    pub live_audio_enabled: bool,
    pub live_audio_timeout_secs: u64,
    // External agent default (persisted to `[agent] default_backend`).
    // Values: "codex" | "claude-code" | "gemini" | None (internal agent).
    #[serde(default)]
    pub external_agent: Option<String>,
    // Codex runtime config (persisted to `[agent.codex]`). Mirrored here so
    // the Activity → Control sub-tab can load in one fetch.
    #[serde(default)]
    pub codex_command: Option<String>,
    /// Managed-capable (Intendant-aware fork) codex binary; managed
    /// sessions spawn it instead of `codex_command`. Empty string clears.
    #[serde(default)]
    pub codex_managed_command: Option<String>,
    #[serde(default)]
    pub codex_sandbox: Option<String>,
    #[serde(default)]
    pub codex_approval_policy: Option<String>,
    #[serde(default)]
    pub codex_model: Option<String>,
    #[serde(default)]
    pub codex_reasoning_effort: Option<String>,
    // Empty / omitted = inherit Codex config; "standard" sends an explicit
    // normal/clear override for Intendant-managed Codex sessions.
    #[serde(default)]
    pub codex_service_tier: Option<String>,
    #[serde(default)]
    pub codex_web_search: bool,
    #[serde(default)]
    pub codex_network_access: bool,
    #[serde(default)]
    pub codex_writable_roots: Vec<String>,
    #[serde(default, alias = "codex_context_recovery")]
    pub codex_managed_context: Option<String>,
    #[serde(default)]
    pub codex_context_archive: Option<String>,
    // Other external-agent executable commands. The Settings pane does not
    // edit these today, but the New Session pane uses them as per-launch
    // command/path defaults.
    #[serde(default)]
    pub claude_command: Option<String>,
    // Claude Code runtime config (persisted to `[agent.claude_code]`).
    // Mirrors the Codex/Gemini fields for the Activity → Control sub-tab.
    #[serde(default)]
    pub claude_model: Option<String>,
    #[serde(default)]
    pub claude_permission_mode: Option<String>,
    #[serde(default)]
    pub claude_allowed_tools: Option<Vec<String>>,
    // Per-category approval rules (persisted to `[approval]`). Exposed here
    // for the dashboard's "Approval rules" controls to populate the selects.
    // Live edits flow through the `set_approval_rule` ControlMsg, not through
    // `apply_settings_payload`, so these are display/read-only in the payload.
    // Values: "auto" | "ask" | "deny".
    #[serde(default = "default_settings_approval_auto")]
    pub approval_file_read: String,
    #[serde(default = "default_settings_approval_ask")]
    pub approval_file_write: String,
    #[serde(default = "default_settings_approval_ask")]
    pub approval_file_delete: String,
    #[serde(default = "default_settings_approval_auto")]
    pub approval_command_exec: String,
    #[serde(default = "default_settings_approval_auto")]
    pub approval_network: String,
    #[serde(default = "default_settings_approval_ask")]
    pub approval_destructive: String,
    #[serde(default = "default_settings_approval_ask")]
    pub approval_display_control: String,
    #[serde(default = "default_settings_approval_auto")]
    pub approval_tool_call: String,
    // Env var overrides (read-only, shown in UI)
    #[serde(default)]
    pub env_overrides: std::collections::HashMap<String, String>,
}

fn default_settings_approval_auto() -> String {
    crate::autonomy::ApprovalRule::Auto.as_str().to_string()
}

fn default_settings_approval_ask() -> String {
    crate::autonomy::ApprovalRule::Ask.as_str().to_string()
}

fn normalize_settings_codex_command(input: Option<&str>) -> String {
    normalize_settings_agent_command(input, "codex")
}

fn normalize_settings_agent_command(input: Option<&str>, fallback: &str) -> String {
    let trimmed = input.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn settings_payload_from_config(config: &crate::project::ProjectConfig) -> SettingsPayload {
    let mut env_overrides = std::collections::HashMap::new();
    for (key, var) in [
        ("CU_PROVIDER", "CU_PROVIDER"),
        ("CU_MODEL", "CU_MODEL"),
        ("PRESENCE_PROVIDER", "PRESENCE_PROVIDER"),
        ("PRESENCE_MODEL", "PRESENCE_MODEL"),
        ("PROVIDER", "PROVIDER"),
        ("MODEL_NAME", "MODEL_NAME"),
    ] {
        if let Ok(val) = std::env::var(var) {
            env_overrides.insert(key.to_string(), val);
        }
    }
    SettingsPayload {
        cu_provider: config.computer_use.provider.clone(),
        cu_model: config.computer_use.model.clone(),
        cu_backend: config.computer_use.backend.clone(),
        cu_first_routing: config.experimental.cu_first_routing,
        presence_enabled: config.presence.enabled,
        presence_provider: config.presence.provider.clone(),
        presence_model: config.presence.model.clone(),
        presence_live_provider: config.presence.live_provider.clone(),
        presence_live_model: config.presence.live_model.clone(),
        transcription_enabled: config.transcription.enabled,
        transcription_provider: config.transcription.provider.clone(),
        transcription_model: config.transcription.model.clone(),
        transcription_endpoint: config.transcription.endpoint.clone(),
        transcription_language: config.transcription.language.clone(),
        recording_enabled: config.recording.enabled,
        recording_framerate: config.recording.framerate,
        recording_quality: config.recording.quality.clone(),
        live_audio_enabled: config.live_audio.enabled,
        live_audio_timeout_secs: config.live_audio.default_timeout_secs,
        external_agent: config.agent.default_backend.clone(),
        codex_command: Some(config.agent.codex.command.clone()),
        codex_managed_command: config.agent.codex.managed_command.clone(),
        codex_sandbox: Some(crate::project::normalize_sandbox_mode(
            &config.agent.codex.sandbox,
        )),
        codex_approval_policy: Some(crate::project::normalize_approval_policy(
            &config.agent.codex.approval_policy,
        )),
        codex_model: config.agent.codex.model.clone(),
        codex_reasoning_effort: crate::project::normalize_reasoning_effort(
            config.agent.codex.reasoning_effort.as_deref(),
        ),
        codex_service_tier: crate::project::normalize_codex_service_tier(
            config.agent.codex.service_tier.as_deref(),
        ),
        codex_web_search: config.agent.codex.web_search,
        codex_network_access: config.agent.codex.network_access,
        codex_writable_roots: config.agent.codex.writable_roots.clone(),
        codex_managed_context: Some(crate::project::normalize_codex_managed_context(
            &config.agent.codex.managed_context,
        )),
        codex_context_archive: Some(crate::project::normalize_codex_context_archive(
            &config.agent.codex.context_archive,
        )),
        claude_command: Some(config.agent.claude_code.command.clone()),
        claude_model: config.agent.claude_code.model.clone(),
        claude_permission_mode: Some(crate::project::normalize_claude_permission_mode(
            &config.agent.claude_code.permission_mode,
        )),
        claude_allowed_tools: Some(config.agent.claude_code.allowed_tools.clone()),
        approval_file_read: config.approval.file_read.as_str().to_string(),
        approval_file_write: config.approval.file_write.as_str().to_string(),
        approval_file_delete: config.approval.file_delete.as_str().to_string(),
        approval_command_exec: config.approval.command_exec.as_str().to_string(),
        approval_network: config.approval.network.as_str().to_string(),
        approval_destructive: config.approval.destructive.as_str().to_string(),
        approval_display_control: config.approval.display_control.as_str().to_string(),
        approval_tool_call: config.approval.tool_call.as_str().to_string(),
        env_overrides,
    }
}

async fn settings_payload_with_runtime_overrides(
    config: &crate::project::ProjectConfig,
    runtime: &RuntimeSettingsState,
) -> SettingsPayload {
    let mut payload = settings_payload_from_config(config);
    if let Some(presence_enabled) = runtime.presence_enabled {
        payload.presence_enabled = presence_enabled;
    }
    if let Some(shared_external_agent) = &runtime.external_agent {
        payload.external_agent = shared_external_agent
            .read()
            .await
            .as_ref()
            .map(|backend| backend.as_short_str().to_string());
    }
    payload
}

pub(crate) async fn settings_get_response_body(
    project_root: Option<&Path>,
    runtime_settings: &RuntimeSettingsState,
) -> String {
    match project_root {
        Some(root) => match crate::project::Project::from_root(root.to_path_buf()) {
            Ok(proj) => {
                let payload =
                    settings_payload_with_runtime_overrides(&proj.config, runtime_settings).await;
                serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        },
        None => serde_json::json!({"error": "No project root"}).to_string(),
    }
}

fn apply_settings_payload(config: &mut crate::project::ProjectConfig, payload: &SettingsPayload) {
    config.computer_use.provider = payload.cu_provider.clone();
    config.computer_use.model = payload.cu_model.clone();
    config.computer_use.backend = payload.cu_backend.clone();
    config.presence.enabled = payload.presence_enabled;
    config.presence.provider = payload.presence_provider.clone();
    config.presence.model = payload.presence_model.clone();
    config.presence.live_provider = payload.presence_live_provider.clone();
    config.presence.live_model = payload.presence_live_model.clone();
    config.transcription.enabled = payload.transcription_enabled;
    config.transcription.provider = payload.transcription_provider.clone();
    config.transcription.model = payload.transcription_model.clone();
    config.transcription.endpoint = payload.transcription_endpoint.clone();
    config.transcription.language = payload.transcription_language.clone();
    config.recording.enabled = payload.recording_enabled;
    config.recording.framerate = payload.recording_framerate;
    config.recording.quality = payload.recording_quality.clone();
    config.live_audio.enabled = payload.live_audio_enabled;
    config.live_audio.default_timeout_secs = payload.live_audio_timeout_secs;
    // Normalize empty strings to None so the TOML doesn't end up with
    // `default_backend = ""` — the loader treats "" as a valid override
    // and would try to resolve it to a backend.
    config.agent.default_backend =
        payload
            .external_agent
            .as_ref()
            .and_then(|s| if s.is_empty() { None } else { Some(s.clone()) });
    if payload.codex_command.is_some() {
        config.agent.codex.command =
            normalize_settings_codex_command(payload.codex_command.as_deref());
    }
    if payload.codex_managed_command.is_some() {
        // Empty clears the override (managed sessions fall back to
        // `command`); anything else is the fork binary path.
        config.agent.codex.managed_command = payload
            .codex_managed_command
            .as_deref()
            .map(str::trim)
            .filter(|cmd| !cmd.is_empty())
            .map(str::to_string);
    }
    if let Some(mode) = payload.codex_sandbox.as_deref() {
        config.agent.codex.sandbox = crate::project::normalize_sandbox_mode(mode);
    }
    if let Some(policy) = payload.codex_approval_policy.as_deref() {
        config.agent.codex.approval_policy = crate::project::normalize_approval_policy(policy);
    }
    if payload.codex_service_tier.is_some() {
        config.agent.codex.service_tier =
            crate::project::normalize_codex_service_tier(payload.codex_service_tier.as_deref());
    }
    if let Some(mode) = payload.codex_managed_context.as_deref() {
        config.agent.codex.managed_context = crate::project::normalize_codex_managed_context(mode);
    }
    if let Some(mode) = payload.codex_context_archive.as_deref() {
        config.agent.codex.context_archive = crate::project::normalize_codex_context_archive(mode);
    }
    if payload.claude_command.is_some() {
        config.agent.claude_code.command =
            normalize_settings_agent_command(payload.claude_command.as_deref(), "claude");
    }
    if payload.claude_model.is_some() {
        // Empty clears the override (claude picks its configured default).
        config.agent.claude_code.model = payload
            .claude_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
    }
    if let Some(mode) = payload.claude_permission_mode.as_deref() {
        config.agent.claude_code.permission_mode =
            crate::project::normalize_claude_permission_mode(mode);
    }
    if let Some(tools) = payload.claude_allowed_tools.as_ref() {
        config.agent.claude_code.allowed_tools = tools
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
    }
}

pub(crate) fn settings_post_result(
    body_text: &str,
    project_root: Option<&Path>,
    bus: &EventBus,
) -> (&'static str, String) {
    let Some(root) = project_root else {
        return (
            "400 Bad Request",
            serde_json::json!({"error": "No project root"}).to_string(),
        );
    };
    let payload = match serde_json::from_str::<SettingsPayload>(body_text) {
        Ok(payload) => payload,
        Err(e) => {
            return (
                "400 Bad Request",
                serde_json::json!({"error": format!("Invalid settings: {}", e)}).to_string(),
            );
        }
    };
    let mut proj = match crate::project::Project::from_root(root.to_path_buf()) {
        Ok(proj) => proj,
        Err(e) => {
            return (
                "500 Internal Server Error",
                serde_json::json!({"error": e.to_string()}).to_string(),
            );
        }
    };
    apply_settings_payload(&mut proj.config, &payload);
    match proj.save_config() {
        Ok(()) => {
            dispatch_codex_settings_control_msgs(bus, &payload);
            ("200 OK", serde_json::json!({"ok": true}).to_string())
        }
        Err(e) => (
            "500 Internal Server Error",
            serde_json::json!({"error": e.to_string()}).to_string(),
        ),
    }
}

/// Mirror just-persisted `[agent.codex]` settings into the live control plane.
///
/// `apply_settings_payload` + `save_config` update the TOML, but session
/// launches read the live `CodexRuntimeConfig`, which OVERLAYS the TOML
/// (`project_with_runtime_config`). Without this, an API client that POSTs
/// `codex_managed_context: "managed"` sees /api/settings echo the new value
/// while sessions keep launching with the stale live value until a daemon
/// restart. Frontends stay display-only, so we don't write shared state here:
/// we emit the same `ControlMsg`s a dashboard would, and the control plane
/// (the single writer) updates shared state, broadcasts `CodexConfigChanged`,
/// and re-persists the normalized value. That second persist is intentional
/// and idempotent — both paths run the same normalizers, and the gateway's
/// own synchronous TOML write (kept above) is what makes an immediate
/// GET /api/settings read back the saved values.
///
/// Only fields actually present in the payload are dispatched, mirroring
/// `apply_settings_payload`'s conditional writes; only codex fields with a
/// live control-plane setter are covered.
fn dispatch_codex_settings_control_msgs(bus: &EventBus, payload: &SettingsPayload) {
    if payload.codex_command.is_some() {
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexCommand {
            command: payload.codex_command.clone(),
        }));
    }
    if payload.codex_managed_command.is_some() {
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexManagedCommand {
                command: payload.codex_managed_command.clone(),
            },
        ));
    }
    if let Some(mode) = payload.codex_sandbox.clone() {
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexSandbox {
            mode,
        }));
    }
    if let Some(policy) = payload.codex_approval_policy.clone() {
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexApprovalPolicy { policy },
        ));
    }
    if payload.codex_service_tier.is_some() {
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexServiceTier {
            service_tier: payload.codex_service_tier.clone(),
        }));
    }
    if let Some(mode) = payload.codex_managed_context.clone() {
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexManagedContext { mode },
        ));
    }
    if let Some(mode) = payload.codex_context_archive.clone() {
        bus.send(AppEvent::ControlCommand(
            ControlMsg::SetCodexContextArchive { mode },
        ));
    }
}

/// Return JSON with boolean flags indicating which API keys are usable —
/// an active credential lease counts the same as a configured .env key.
fn get_api_key_status_json() -> String {
    let openai = crate::credential_leases::provider_api_key("OPENAI_API_KEY").is_some();
    let anthropic = crate::credential_leases::provider_api_key("ANTHROPIC_API_KEY").is_some();
    let gemini = crate::credential_leases::provider_api_key("GEMINI_API_KEY").is_some();
    serde_json::json!({
        "openai": openai,
        "anthropic": anthropic,
        "gemini": gemini,
    })
    .to_string()
}

pub(crate) fn api_key_status_response_body() -> String {
    get_api_key_status_json()
}

/// Whether any provider credential is usable at all — the aggregate of
/// [`get_api_key_status_json`], safe to expose at presence level.
pub(crate) fn any_provider_credential_usable() -> bool {
    ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"]
        .iter()
        .any(|name| crate::credential_leases::provider_api_key(name).is_some())
}

pub(crate) fn project_root_response_body(project_root: Option<&Path>) -> String {
    serde_json::json!({
        "project_root": project_root.map(|root| root.to_string_lossy().to_string())
    })
    .to_string()
}

/// Availability of the external-agent backends (Codex, Claude Code):
/// the configured command, whether it resolves to an executable, and
/// when this daemon last ran a session with it. Deliberately independent
/// of provider fueling — external agents bring their own credentials, so
/// the dashboard pairs this with the `fueled` flag instead of letting the
/// first-run nudge claim an unfueled daemon can't do anything.
pub(crate) fn external_agents_response_body(project_root: Option<&Path>) -> String {
    let agent_config = project_root
        .and_then(|root| crate::project::Project::from_root(root.to_path_buf()).ok())
        .map(|project| project.config.agent)
        .unwrap_or_default();
    let home = crate::platform::home_dir();
    serde_json::json!({
        "external_agents":
            crate::external_agent::backend_availability_json(&agent_config, &home),
    })
    .to_string()
}

pub(crate) async fn displays_response_body(
    session_registry: &Option<crate::display::SharedSessionRegistry>,
) -> String {
    let displays = crate::display::enumerate_displays_with_sessions(session_registry).await;
    serde_json::to_string(&displays).unwrap_or_else(|_| "[]".to_string())
}

/// Payload for POST /api/api-keys.
#[derive(serde::Deserialize)]
struct SetApiKeysPayload {
    keys: std::collections::HashMap<String, String>,
}

/// Handle POST /api/api-keys: persist keys to ~/.config/intendant/.env and
/// set them in the current process.
pub(crate) fn handle_set_api_keys(body: &str) -> String {
    let payload: SetApiKeysPayload = match serde_json::from_str(body) {
        Ok(p) => p,
        Err(e) => {
            return serde_json::json!({"error": format!("Invalid payload: {}", e)}).to_string();
        }
    };

    // Only allow known key names.
    const ALLOWED: &[&str] = &["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"];
    for key in payload.keys.keys() {
        if !ALLOWED.contains(&key.as_str()) {
            return serde_json::json!({"error": format!("Unknown key: {}", key)}).to_string();
        }
    }

    // Resolve config dir.
    let config_dir = match dirs::config_dir() {
        Some(d) => d.join("intendant"),
        None => {
            return serde_json::json!({"error": "Cannot determine config directory"}).to_string();
        }
    };

    // Ensure the directory exists.
    if let Err(e) = std::fs::create_dir_all(&config_dir) {
        return serde_json::json!({"error": format!("Cannot create config dir: {}", e)})
            .to_string();
    }

    let env_path = config_dir.join(".env");

    // Read existing content (may not exist yet).
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();

    // Build updated content: replace existing lines, append new ones.
    let mut lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();
    let mut written_keys = std::collections::HashSet::new();

    for line in &mut lines {
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq_pos) = trimmed.find('=') {
            let var_name = trimmed[..eq_pos].trim().to_string();
            if let Some(new_val) = payload.keys.get(&var_name) {
                *line = format!("{}={}", var_name, new_val);
                written_keys.insert(var_name);
            }
        }
    }

    // Append keys that weren't already in the file.
    for (key, val) in &payload.keys {
        if !written_keys.contains(key.as_str()) {
            lines.push(format!("{}={}", key, val));
        }
    }

    let new_content = lines.join("\n") + "\n";

    if let Err(e) = crate::file_watcher::atomic_write(&env_path, new_content.as_bytes()) {
        return serde_json::json!({"error": format!("Write failed: {}", e)}).to_string();
    }

    // Set env vars in the current process so future provider instantiations
    // pick them up without requiring a restart.
    for (key, val) in &payload.keys {
        std::env::set_var(key, val);
    }

    serde_json::json!({"ok": true}).to_string()
}

// ---------------------------------------------------------------------------
// MCP-over-HTTP (Streamable HTTP) types
// ---------------------------------------------------------------------------
//
// rmcp's Streamable HTTP transport expects:
//   - Requests (with `id`):   200 OK + application/json body
//   - Notifications (no `id`): 202 Accepted + empty body
//
// Returning 200+JSON for notifications causes rmcp to try deserializing the
// body as ServerJsonRpcMessage, which fails because there's no valid `id`.


async fn handle_project_root(mut stream: DemuxStream, project_root: Option<PathBuf>) {
    use tokio::io::AsyncWriteExt;
    let body = project_root_response_body(project_root.as_deref());
    let response = HttpResponse::with_content("200 OK", "application/json", body)
        .header("Cache-Control", "no-cache")
        .header("Connection", "close")
        .into_string();
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

async fn handle_settings_post(
    mut stream: DemuxStream,
    body_text: String,
    bus: EventBus,
    project_root: Option<PathBuf>,
) {
    use tokio::io::AsyncWriteExt;
    let (status, result) =
        settings_post_result(&body_text, project_root.as_deref(), &bus);
    let response = json_response(status, result);
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

async fn handle_settings_get(
    mut stream: DemuxStream,
    project_root: Option<PathBuf>,
    runtime_settings: RuntimeSettingsState,
) {
    use tokio::io::AsyncWriteExt;
    let body =
        settings_get_response_body(project_root.as_deref(), &runtime_settings)
            .await;
    let response = HttpResponse::with_content("200 OK", "application/json", body)
        .header("Cache-Control", "no-cache")
        .header("Connection", "close")
        .into_string();
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

async fn handle_api_keys_post(mut stream: DemuxStream, body_text: String) {
    use tokio::io::AsyncWriteExt;
    let result = handle_set_api_keys(&body_text);
    let response = HttpResponse::with_content("200 OK", "application/json", result)
        .header("Access-Control-Allow-Origin", "*")
        .header("Connection", "close")
        .into_string();
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

async fn handle_api_key_status(mut stream: DemuxStream) {
    use tokio::io::AsyncWriteExt;
    let body = api_key_status_response_body();
    let response = HttpResponse::with_content("200 OK", "application/json", body)
        .header("Cache-Control", "no-cache")
        .header("Connection", "close")
        .into_string();
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

async fn handle_external_agents(mut stream: DemuxStream, project_root: Option<PathBuf>) {
    use tokio::io::AsyncWriteExt;
    let body = external_agents_response_body(project_root.as_deref());
    let response = HttpResponse::with_content("200 OK", "application/json", body)
        .header("Cache-Control", "no-cache")
        .header("Connection", "close")
        .into_string();
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

async fn handle_diagnostics_visual_freshness(
    mut stream: DemuxStream,
    body_text: String,
    request_line: &str,
) {
    // **Phase 0 visual-freshness transcript sink** (task #83).
    // Body is browser-emitted NDJSON (one JSON record per
    // `\n`-terminated line); server appends verbatim to
    // `~/.intendant/diagnostics/visual-freshness/<session>.ndjson`.
    // No parsing or schema validation here — that's
    // browser-side or post-hoc analysis on the
    // transcript. Session id arrives via `?session_id=…`
    // query param; we sanitize aggressively (alnum + `-`
    // + `_` only) and reject anything that collapses
    // empty so a missing param can't accidentally
    // produce a bare-`.ndjson` write.
    use tokio::io::AsyncWriteExt;
    let session_id_raw: String = request_line
        .split('?')
        .nth(1)
        .and_then(|qs| qs.split_whitespace().next())
        .map(|qs| {
            qs.split('&')
                .find_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    if k == "session_id" {
                        Some(v.to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let (status, body) =
        match crate::diagnostics::append_visual_freshness_record(
            &session_id_raw,
            body_text.as_bytes(),
        ) {
            Ok(written) => (
                "200 OK",
                serde_json::json!({"ok": true, "written": written}).to_string(),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => (
                "400 Bad Request",
                serde_json::json!({"error": e.to_string()}).to_string(),
            ),
            Err(e) => (
                "500 Internal Server Error",
                serde_json::json!({"error": e.to_string()}).to_string(),
            ),
        };
    let response = HttpResponse::with_content(status, "application/json", body)
        .header("Access-Control-Allow-Origin", "*")
        .header("Connection", "close")
        .into_string();
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}


/// Build a `WebGatewayConfig` from the presence config's live fields,
/// falling back to environment variable detection.
///
/// Returns voice/runtime fields only. Daemon identity (host label,
/// version, git sha) lives on the Agent Card at
/// `/.well-known/agent-card.json` and is assembled at gateway spawn
/// time via [`build_local_agent_card`].
pub fn build_config(
    live_provider: Option<&str>,
    live_model: Option<&str>,
    transcription_enabled: bool,
    ice_config: crate::display::IceConfig,
    federation_allow_h264: bool,
) -> WebGatewayConfig {
    build_config_inner(
        live_provider,
        live_model,
        transcription_enabled,
        ice_config.ice_servers,
        federation_allow_h264,
    )
}

// ---------------------------------------------------------------------------
// /api/peers helpers
// ---------------------------------------------------------------------------


#[derive(Deserialize)]
struct AddPeerRequest {
    card_url: String,
    /// Optional display label override for this peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// Persist this manual add into `intendant.toml` after the live
    /// registration succeeds. Unchecked manual adds remain runtime-only.
    #[serde(default)]
    persist: bool,
    /// Optional connecting-side override for the peer's transport
    /// URLs. When non-empty, the card's `transports` field is
    /// replaced with one `IntendantWs` entry per URL. Lets the
    /// operator route around topologies the advertising peer's card
    /// doesn't know about (port-forwards, proxies, named tunnels).
    /// `#[serde(default)]` so older clients without this field
    /// continue to work.
    #[serde(default)]
    via_urls: Vec<String>,
    /// Optional outbound bearer token sent to this peer (the
    /// `[[peer]] bearer_token` equivalent for dashboard-added
    /// peers). When set, sent on the agent-card fetch and the
    /// WebSocket upgrade. Required when the peer's card declares
    /// `auth.application = Some(Bearer)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bearer_token: Option<String>,
    /// Optional operator-supplied pinned cert fingerprints. When
    /// non-empty, REPLACES whatever the peer's card declares for
    /// `auth.transport` — eliminates the TOFU window when the
    /// operator got the fingerprint out-of-band. Same wire format
    /// as the card's: lowercase hex with optional `:` separators.
    #[serde(default)]
    pinned_fingerprints: Vec<String>,
    /// Explicit URL the **browser** uses to reach this peer's HTTP
    /// port for WebRTC ICE-TCP. When set, the dashboard uses this
    /// (not `d.ws_url`) as the `advertise_tcp_via_url` hint in the
    /// federated WebRTC offer. Decouples the browser-side URL from
    /// the via URL the primary uses for federation, which matters
    /// when the two network positions differ (primary-side localhost
    /// tunnel, browser on a different machine, etc.). `None` falls
    /// back to the slice 3a.2 behavior of using the primary's via URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    browser_tcp_via_url: Option<String>,
}


#[derive(Deserialize)]
struct RemovePeerRequest {
    peer_id: String,
}


fn trimmed_nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn persist_manual_peer(
    project_root: &Path,
    req: &AddPeerRequest,
    label: Option<String>,
) -> Result<PathBuf, crate::error::CallerError> {
    let mut project = crate::project::Project::from_root(project_root.to_path_buf())?;
    let existing = project
        .config
        .peers
        .iter_mut()
        .find(|peer| peer.card_url == req.card_url);
    match existing {
        Some(peer) => {
            if label.is_some() {
                peer.label = label;
            }
            peer.bearer_token = req.bearer_token.clone();
            peer.via_urls = req.via_urls.clone();
            peer.pinned_fingerprints = req.pinned_fingerprints.clone();
            peer.browser_tcp_via_url = req.browser_tcp_via_url.clone();
        }
        None => {
            project.config.peers.push(crate::project::PeerConfig {
                card_url: req.card_url.clone(),
                label,
                bearer_token: req.bearer_token.clone(),
                via_urls: req.via_urls.clone(),
                client_cert: None,
                client_key: None,
                pinned_fingerprints: req.pinned_fingerprints.clone(),
                browser_tcp_via_url: req.browser_tcp_via_url.clone(),
            });
        }
    }
    project.save_config()?;
    Ok(project.root.join("intendant.toml"))
}


fn target_card_url_from_request(header_text: &str, is_tls: bool) -> Option<String> {
    let host = header_text
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("host"))
        })
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())?;
    let scheme = if is_tls { "https" } else { "http" };
    Some(format!(
        "{scheme}://{}{}",
        host.trim_end_matches('/'),
        crate::peer::pairing::AGENT_CARD_PATH
    ))
}


fn identity_summary_json(
    record: crate::peer::access_policy::PeerIdentityRecord,
) -> serde_json::Value {
    // `active` mirrors the gateway auth gate (approved AND unexpired), so
    // an org-materialized identity past its expiry reads as inert here —
    // the raw status/expiry/provenance fields let the UI say why.
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    serde_json::json!({
        "fingerprint": record.fingerprint,
        "label": record.label,
        "profile": record.profile,
        "status": record.status,
        "active": record.is_active(now_unix),
        "card_url": record.card_url,
        "request_id": record.request_id,
        "created_at_unix": record.created_at_unix,
        "revoked_at_unix": record.revoked_at_unix,
        "expires_at_unix": record.expires_at_unix,
        "source": record.source,
        "org_grant_id": record.org_grant_id,
        "issued_via": record.issued_via,
    })
}


// ---------------------------------------------------------------------------
// Per-peer outbound op handlers
// ---------------------------------------------------------------------------
//
// These three endpoints let the dashboard drive the read-write peer
// transport directly. Each maps a JSON body to the matching
// [`crate::peer::PeerHandle`] method:
//
//   POST /api/peers/{id}/message  →  PeerHandle::send_message
//   POST /api/peers/{id}/task     →  PeerHandle::delegate_task
//   POST /api/peers/{id}/approval →  PeerHandle::resolve_approval
//
// Error model (uniform across the three):
//
//   400  bad JSON / missing required field
//   404  peer not in registry
//   405  peer's transport doesn't support this op (UnsupportedCapability)
//   502  transport-level failure (NotConnected, Transport, Auth, …)
//   500  catch-all for unexpected errors
//
// Status codes pick a meaningful HTTP semantic per [`PeerError`] variant
// rather than collapsing everything to 502 — the dashboard renders a
// different message for "wrong peer kind" vs "peer is offline".

/// Shared body for `POST /api/peers/{id}/message`.
///
/// Two equivalent shapes accepted:
///
/// 1. Shorthand: `{"text": "hello"}` — implicit user role + Text content.
/// 2. Full:     `{"role": "user", "content": {"type": "text", "text": "hello"}, "session": null}`.
///
/// The `content` field, when present, wins over `text`. Either `text`
/// or `content` is required; everything else is optional.
#[derive(Deserialize)]
struct SendMessageRequest {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    role: Option<crate::peer::MessageRole>,
    #[serde(default)]
    content: Option<crate::peer::MessageContent>,
    #[serde(default)]
    session: Option<String>,
}

impl SendMessageRequest {
    fn into_message(self) -> Result<crate::peer::PeerMessage, String> {
        let role = self.role.unwrap_or(crate::peer::MessageRole::User);
        let content = match (self.content, self.text) {
            (Some(c), _) => c,
            (None, Some(t)) => crate::peer::MessageContent::Text { text: t },
            (None, None) => {
                return Err("either 'text' or 'content' is required".to_string());
            }
        };
        Ok(crate::peer::PeerMessage {
            session: self.session,
            role,
            content,
        })
    }
}

#[derive(Deserialize)]
struct DelegateTaskRequest {
    instructions: String,
    #[serde(default)]
    context: serde_json::Value,
    #[serde(default)]
    client_correlation_id: Option<String>,
}

#[derive(Deserialize)]
struct ResolveApprovalRequest {
    request_id: String,
    decision: crate::peer::ApprovalDecision,
}


/// Slice 3b: rewrite an outgoing federated `WebRtcSignal::Answer` to
/// (a) register the peer's ICE ufrag in the relay registry and
/// (b) inject a TCP candidate pointing at the primary's own address
/// alongside the peer's direct candidate.
///
/// After the rewrite, a browser receiving this Answer has two TCP
/// candidates: the peer's direct TCP candidate (if the peer provided
/// one via `advertise_tcp_via_url`) and the primary's relay
/// candidate. Browser ICE tries both and uses whichever forms first.
/// Because the relay candidate is emitted with a lower priority
/// (see `inject_relay_tcp_candidate`), direct wins on reachable
/// topologies and relay is the fallback.
///
/// Non-Answer events pass through verbatim. Events with malformed
/// SDPs, missing ufrags, or a peer URL that can't be resolved fall
/// through without rewriting — the browser still sees the original
/// Answer, just without the relay candidate.
async fn maybe_rewrite_federated_answer(
    peer: &crate::peer::PeerId,
    event: crate::peer::PeerEvent,
    registry: &crate::peer::PeerRegistry,
    relay_registry: &Arc<crate::display::webrtc::TcpRelayRegistry>,
    relay_advertise_url: Option<&str>,
    bus: &EventBus,
) -> crate::peer::PeerEvent {
    const LOG_SOURCE: &str = "webrtc-peer";

    // Match only the specific variant that carries an Answer SDP; all
    // other event variants (Log, Usage, ActivityStarted, IceCandidate,
    // etc.) pass through unchanged.
    let (display_id, session_id, sdp) = match &event {
        crate::peer::PeerEvent::WebRtcSignal {
            display_id,
            session_id,
            signal: crate::peer::WebRtcSignal::Answer { sdp, .. },
        } => (*display_id, session_id.clone(), sdp.clone()),
        _ => return event,
    };

    // Extract the peer's ICE ufrag from the Answer SDP. Without it we
    // can't key the relay registry, so we skip rewriting and let the
    // browser try whatever direct candidate the peer advertised.
    let ufrag = match crate::display::webrtc::parse_sdp_ice_ufrag(&sdp) {
        Some(u) => u,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     Answer SDP missing a=ice-ufrag attribute"
                ),
                turn: None,
            });
            return event;
        }
    };

    // Resolve the peer's outbound TCP address — where the primary
    // will dial when it sees a relay-destined TCP connection. Prefer
    // `browser_tcp_via_url` (operator's split-browser-side URL) then
    // fall back to `ws_url` (primary-side via URL). In the typical
    // co-located case the two are the same; in split topologies the
    // operator uses browser_tcp_via_url to point at where they'd
    // like the BROWSER to reach the peer. Here we're dialing FROM
    // the primary, but the primary typically shares the LAN position
    // of the operator's browser-reachable URL when one is set.
    let outbound_url = registry.get(peer).and_then(|h| {
        let snap = h.snapshot();
        snap.browser_tcp_via_url.or(snap.ws_url)
    });
    let outbound_url = match outbound_url {
        Some(u) => u,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     no outbound URL on the peer's snapshot (peer removed mid-Answer?)"
                ),
                turn: None,
            });
            return event;
        }
    };
    let outbound_addr = match resolve_url_to_socket_addr(&outbound_url).await {
        Some(addr) => addr,
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "skipping relay registration for peer={peer} session={session_id}: \
                     outbound URL {outbound_url:?} didn't resolve to a SocketAddr"
                ),
                turn: None,
            });
            return event;
        }
    };
    relay_registry.register(ufrag.clone(), outbound_addr);

    // Resolve the primary's own relay URL into a SocketAddr we can
    // put in an SDP candidate line. When the primary has no
    // advertised URL we can work with (local_addr() was None at
    // spawn, headless mode, etc), skip injection and just forward
    // the Answer unchanged — the browser still has the peer's
    // direct candidate to try.
    let primary_relay_addr = match relay_advertise_url {
        Some(url) => match resolve_url_to_socket_addr(url).await {
            Some(addr) => addr,
            None => {
                bus.send(AppEvent::LogEntry {
                    session_id: None,
                    level: "warn".to_string(),
                    source: LOG_SOURCE.to_string(),
                    content: format!(
                        "registered ufrag={ufrag} outbound={outbound_addr} for peer={peer} session={session_id} \
                         but can't inject relay candidate — primary's own URL {url:?} doesn't resolve"
                    ),
                    turn: None,
                });
                return event;
            }
        },
        None => {
            bus.send(AppEvent::LogEntry {
                session_id: None,
                level: "warn".to_string(),
                source: LOG_SOURCE.to_string(),
                content: format!(
                    "registered ufrag={ufrag} outbound={outbound_addr} for peer={peer} session={session_id} \
                     but no primary relay URL configured — skipping candidate injection"
                ),
                turn: None,
            });
            return event;
        }
    };

    let rewritten_sdp =
        crate::display::webrtc::inject_relay_tcp_candidate(&sdp, primary_relay_addr);
    bus.send(AppEvent::LogEntry {
        session_id: None,
        level: "info".to_string(),
        source: LOG_SOURCE.to_string(),
        content: format!(
            "relay registered ufrag={ufrag} peer={peer} session={session_id} \
             primary_relay={primary_relay_addr} outbound={outbound_addr}"
        ),
        turn: None,
    });

    crate::peer::PeerEvent::WebRtcSignal {
        display_id,
        session_id,
        signal: crate::peer::WebRtcSignal::Answer {
            sdp: rewritten_sdp,
            binding: None,
        },
    }
}

/// Parse a WebSocket / HTTP URL and resolve it to a [`SocketAddr`].
///
/// Used to convert the browser's view of a peer's HTTP port (the
/// `advertise_tcp_via_url` hint in a federated
/// [`crate::peer::WebRtcSignal::Offer`]) into the concrete address
/// the peer advertises in its ICE-TCP candidate.
///
/// Accepts `ws://` / `wss://` / `http://` / `https://` schemes (all
/// produce the same authority shape). The host can be an IPv4
/// literal, a bracketed IPv6 literal, or a hostname — hostnames are
/// resolved via [`tokio::net::lookup_host`] and the first returned
/// address is used. The port must be explicit; there's no default-
/// port fallback, because we can't know what the peer's HTTP
/// listener bound to without being told.
///
/// Returns `None` on any parse or resolution failure. Callers treat
/// that as "no TCP candidate, UDP-only path" — the same behavior as
/// slice 3a's pre-3a.2 baseline.
async fn resolve_url_to_socket_addr(url: &str) -> Option<std::net::SocketAddr> {
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("http://"))?;
    // Strip any path / query that follows the authority. Authority
    // for an IPv6 literal is `[::1]:8766`, which contains neither
    // `/` nor `?` inside the brackets, so split-on-first is safe.
    let authority = rest.split(['/', '?']).next()?;
    // Fast path for `ipv4:port` or `[ipv6]:port`: parse directly.
    if let Ok(addr) = authority.parse::<std::net::SocketAddr>() {
        return Some(addr);
    }
    // Hostname:port — needs DNS. `lookup_host` accepts `host:port`
    // and returns the resolved SocketAddrs in OS-chosen order; first
    // is the winner (matches what the kernel would pick for a
    // regular connect()).
    tokio::net::lookup_host(authority).await.ok()?.next()
}


// ---------------------------------------------------------------------------
// Coordinator endpoints — capability-based discovery + delegation
// ---------------------------------------------------------------------------

/// Parse `?capability=display&capability=custom:foo` into a typed
/// `Vec<Capability>` plus a list of unknown strings (for diagnostics).
/// Empty input returns `(vec![], vec![])` — empty-required-capabilities
/// matches every peer, which the handler rejects upstream.
fn parse_capability_query(query: &str) -> (Vec<crate::peer::Capability>, Vec<String>) {
    let mut caps = Vec::new();
    let mut unknown = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if k != "capability" {
            continue;
        }
        match crate::peer::Capability::from_query_string(v) {
            Some(cap) => caps.push(cap),
            None => unknown.push(v.to_string()),
        }
    }
    (caps, unknown)
}


/// True for HTTP requests that hit the federation REST surface:
/// `/api/peers*`, `/api/coordinator/*`, `/api/sessions`, and
/// `/api/worktrees`. These
/// are the endpoints the bearer-token enforcement layer protects
/// when `[server.auth] bearer_token` is set. Discovery
/// (`/.well-known/agent-card.json`), browser bootstrap (`/config`,
/// `/`, `/static/*`), and `/ws` are exempt — see
/// `spawn_web_gateway::inbound_bearer_token` docs for why.
fn is_federation_path(request_line: &str) -> bool {
    let (_, path, _) = parse_request_target(request_line);
    path_is_or_under(path, "/api/peers")
        || path.starts_with("/api/coordinator/")
        || path_is_or_under(path, "/api/sessions")
        || path_is_or_under(path, "/api/worktrees")
}


fn dashboard_http_operation(
    req_method: &str,
    req_path: &str,
) -> Option<crate::peer::access_policy::PeerOperation> {
    // Pure table lookup: every dispatched route is declared in
    // gateway_routes::ROUTES with its IAM operation, and an undeclared
    // (method, path) is not a route — nothing to gate. (The hand-written
    // match this function used to be lived and died with the route-table
    // migration; the invariants in gateway_routes.rs hold in its place.)
    match crate::gateway_routes::classify(req_method, req_path) {
        crate::gateway_routes::TableClassification::Matched(op) => op,
        crate::gateway_routes::TableClassification::NoMatch => None,
    }
}

fn http_access_forbidden_response(
    access: &HttpAccessContext,
    decision: crate::access::iam::AccessDecision,
) -> String {
    json_response(
        "403 Forbidden",
        serde_json::json!({
            "error": "principal does not allow this operation",
            "principal": access.principal.as_value(),
            "permission": decision.permission,
            "reason": decision.reason,
        })
        .to_string(),
    )
}


fn is_public_connect_bootstrap_path(request_line: &str) -> bool {
    let Some(path) = request_line.split_whitespace().nth(1) else {
        return false;
    };
    let path = path.split('?').next().unwrap_or(path);
    matches!(
        path,
        "/connect/bootstrap"
            | "/connect/status"
            | "/connect/dashboard/offer"
            | "/connect/dashboard/ice"
            | "/connect/dashboard/close"
    )
}


fn peer_identity_allows_ws_control(
    identity: Option<&PeerConnectionIdentity>,
    ctrl: &ControlMsg,
    bus: &EventBus,
) -> bool {
    let Some(identity) = identity else {
        return true;
    };
    // The dashboard-control tunnel is multi-capability; its signaling relay
    // opens for any profile that can use something inside it, and every
    // method/frame is then individually authorized on this same identity.
    if matches!(ctrl, ControlMsg::PeerDashboardControlSignal { .. }) {
        if crate::peer::access_policy::profile_allows_dashboard_control_tunnel(&identity.profile) {
            return true;
        }
        bus.send(AppEvent::PresenceLog {
            message: format!(
                "[ws] denied peer dashboard-control signaling from {}: profile={} allows no tunnel capability",
                identity.label, identity.profile,
            ),
            level: Some(LogLevel::Warn),
            turn: None,
        });
        return false;
    }
    let op = crate::peer::access_policy::control_msg_operation(ctrl);
    let decision = crate::access::iam::evaluate_principal_operation(
        &peer_identity_access_principal(identity, "peer-ws"),
        op,
    );
    if decision.allowed {
        return true;
    }
    bus.send(AppEvent::PresenceLog {
        message: format!(
            "[ws] denied peer control frame from {}: profile={} permission={} reason={}",
            identity.label, identity.profile, decision.permission, decision.reason,
        ),
        level: Some(LogLevel::Warn),
        turn: None,
    });
    false
}

/// Map a typed `/ws` frame to the `PeerOperation` it exercises — the
/// direct-WebSocket mirror of `dashboard_control_frame_operation` and
/// the `CONTROL_METHODS` table (dashboard_control.rs), so the same
/// IAM grant answers the same way whichever transport a client speaks.
/// `None` means the frame carries no authority of its own: replies, pings,
/// and the `dashboard_control_*` signaling frames (the tunnel they establish
/// enforces this very grant per-frame itself, and scoped clients must be
/// able to reach their allowed surface through it).
fn ws_frame_operation(frame_type: &str) -> Option<crate::peer::access_policy::PeerOperation> {
    use crate::peer::access_policy::PeerOperation;
    match frame_type {
        // Same frame names as the dashboard-control tunnel table. Floor
        // operations: terminal_open may additionally require shell.spawn
        // (when the session doesn't exist yet) and every terminal frame is
        // scoped to sessions the actor can see — both enforced statefully
        // in the frame handlers.
        "terminal_open" => Some(PeerOperation::TerminalView),
        "terminal_input" | "terminal_resize" | "terminal_close" | "terminal_share" => {
            Some(PeerOperation::TerminalWrite)
        }
        "display_input" => Some(PeerOperation::DisplayInput),
        // Parity: api_diagnostics_visual_freshness → DisplayInput. The
        // marker is stamped pre-encoder and lands in every viewer's stream,
        // so it is display mutation, not viewing.
        "set_diagnostics_visual_marker" => Some(PeerOperation::DisplayInput),
        // Parity: api_display_bootstrap / api_display_webrtc_signal.
        "display_offer" | "display_ice" => Some(PeerOperation::DisplayView),
        // The embedded web TUI drives the daemon's own runtime — the direct
        // twins of the tunnel's tui_* frames.
        "key" | "resize" | "term_subscribe" | "term_unsubscribe" => {
            Some(PeerOperation::RuntimeControl)
        }
        // Live voice/media session machinery. Parity: api_voice_session,
        // api_presence_video_frame, api_media_annotation_*, api_media_clip_*.
        "presence_connect"
        | "presence_disconnect"
        | "make_active"
        | "user_audio"
        | "video_frame"
        | "voice_log"
        | "voice_diagnostic"
        | "presence_checkpoint"
        | "live_usage_update"
        | "annotation_attach"
        | "annotation_submit"
        | "clip_start"
        | "clip_frame"
        | "clip_end" => Some(PeerOperation::RuntimeControl),
        // Presence tool dispatch. Parity: api_mcp_tool_call → Message.
        "tool_request" | "async_query" => Some(PeerOperation::Message),
        _ => None,
    }
}

/// Per-frame IAM gate for the direct `/ws` path. Returns `true` when the
/// frame was denied and fully handled — a denial frame has been sent (plus
/// the pane-visible `terminal_error` shape for terminal frames) and a
/// once-per-frame-type warning logged — so the caller drops the frame.
/// Root-equivalent grants (plain local dashboards, unbound mTLS root
/// certificates) short-circuit to allow inside the evaluator; the check is
/// pure in-memory, safe at keystroke/audio-frame rates.
fn deny_ws_frame_if_unauthorized(
    grant: &crate::dashboard_control::DashboardControlGrant,
    json: &serde_json::Value,
    direct_tx: &mpsc::UnboundedSender<String>,
    bus: &EventBus,
    logged_denials: &mut std::collections::HashSet<String>,
) -> bool {
    let Some(frame_type) = json.get("t").and_then(|v| v.as_str()) else {
        return false;
    };
    let Some(op) = ws_frame_operation(frame_type) else {
        return false;
    };
    let decision = grant.access_decision(op);
    if decision.allowed {
        return false;
    }
    if frame_type.starts_with("terminal_") {
        let err = serde_json::json!({
            "t": "terminal_error",
            "host_id": json.get("host_id").and_then(|v| v.as_str()).unwrap_or("local"),
            "terminal_id": json.get("terminal_id").and_then(|v| v.as_str()).unwrap_or(""),
            "error": format!("not allowed: {}", decision.reason),
        });
        let _ = direct_tx.send(err.to_string());
    }
    let denied = serde_json::json!({
        "t": "ws_denied",
        "frame": frame_type,
        "permission": decision.permission,
        "reason": decision.reason,
    });
    let _ = direct_tx.send(denied.to_string());
    if logged_denials.insert(frame_type.to_string()) {
        bus.send(AppEvent::PresenceLog {
            message: format!(
                "[ws] denied {frame_type} frame for {}: permission={} reason={}",
                grant.wire_kind(),
                decision.permission,
                decision.reason,
            ),
            level: Some(LogLevel::Warn),
            turn: None,
        });
    }
    true
}

/// Grant-lane twin of `peer_identity_allows_ws_control` for the ControlMsg
/// fall-through on the direct `/ws` path: peer connections keep their
/// identity-based gate (which already ran in the preceding match guard),
/// every other connection answers to its dashboard-control grant through
/// the same ControlMsg→operation table the peer lane uses.
fn ws_grant_allows_control(
    grant: &crate::dashboard_control::DashboardControlGrant,
    peer_identity: Option<&PeerConnectionIdentity>,
    ctrl: &ControlMsg,
    bus: &EventBus,
) -> bool {
    if peer_identity.is_some() {
        return true;
    }
    // Relaying signaling to a connected peer delegates THIS daemon's peer
    // identity — the receiving peer authorizes the tunnel against its
    // grants for this daemon, not against the human grant that asked for
    // the relay. That delegation is its own named permission (peer.use),
    // never inferred from local capabilities.
    if matches!(
        ctrl,
        ControlMsg::PeerDashboardControlSignal { .. } | ControlMsg::PeerFileTransferSignal { .. }
    ) {
        let decision = grant.access_decision(crate::peer::access_policy::PeerOperation::PeerUse);
        if decision.allowed {
            return true;
        }
        bus.send(AppEvent::PresenceLog {
            message: format!(
                "[ws] denied {} peer signaling relay: permission={} reason={}",
                grant.wire_kind(),
                decision.permission,
                decision.reason,
            ),
            level: Some(LogLevel::Warn),
            turn: None,
        });
        return false;
    }
    let op = crate::peer::access_policy::control_msg_operation(ctrl);
    let decision = grant.access_decision(op);
    if decision.allowed {
        return true;
    }
    bus.send(AppEvent::PresenceLog {
        message: format!(
            "[ws] denied {} control frame: permission={} reason={}",
            grant.wire_kind(),
            decision.permission,
            decision.reason,
        ),
        level: Some(LogLevel::Warn),
        turn: None,
    });
    false
}


/// Verify a WebSocket upgrade request carries the expected bearer
/// token. Browser WebSocket clients cannot natively set custom
/// headers on `WebSocket` opens, so this accepts the token in EITHER
/// an `Authorization: Bearer <token>` header (sent by
/// `IntendantWsTransport` from the daemon side) OR a `?token=...`
/// URL query parameter (sent by the browser dashboard). The dual
/// path is the standard pragmatic workaround for the browser
/// limitation.
///
/// Returns `Ok(())` when no token is required (no
/// `inbound_bearer_token` configured) or when the request presents
/// the matching token via either method. Returns `Err((401, body))`
/// otherwise — the caller writes a plain HTTP 401 response *before*
/// the WebSocket handshake and returns, so the rejected client never
/// sees a successful upgrade.
pub(crate) fn verify_bearer_for_ws(
    header_text: &str,
    expected_token: Option<&str>,
) -> Result<(), (u16, String)> {
    let Some(expected) = expected_token else {
        return Ok(());
    };

    // Try the Authorization header first (cheaper and the daemon-to-
    // daemon path uses it). On miss, fall back to the URL query.
    if verify_bearer_token(header_text, Some(expected)).is_ok() {
        return Ok(());
    }

    let request_line = header_text.lines().next().unwrap_or("");
    if extract_token_query_param(request_line).as_deref() == Some(expected) {
        return Ok(());
    }

    Err((
        401,
        serde_json::json!({
            "error": "missing or invalid bearer token (Authorization header or ?token=)"
        })
        .to_string(),
    ))
}

/// Verify a federation HTTP request carries the expected bearer
/// token in the `Authorization` header. Header name lookup is
/// case-insensitive per the HTTP spec; the `Bearer` scheme prefix
/// match accepts either case.
///
/// Returns `Ok(())` when no token is required (no
/// `inbound_bearer_token` configured) or when the request presents
/// the matching token. Returns `Err((401, body_json))` otherwise —
/// the caller writes that response and returns.
pub(crate) fn verify_bearer_token(
    header_text: &str,
    expected_token: Option<&str>,
) -> Result<(), (u16, String)> {
    let Some(expected) = expected_token else {
        return Ok(());
    };
    let auth_header = header_text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("authorization") {
            Some(value.trim().to_string())
        } else {
            None
        }
    });
    let auth = match auth_header {
        Some(v) => v,
        None => {
            return Err((
                401,
                serde_json::json!({"error": "missing Authorization header"}).to_string(),
            ));
        }
    };
    let token = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "));
    let token = match token {
        Some(t) => t.trim(),
        None => {
            return Err((
                401,
                serde_json::json!({
                    "error": "Authorization header must use Bearer scheme"
                })
                .to_string(),
            ));
        }
    };
    if token == expected {
        Ok(())
    } else {
        Err((
            401,
            serde_json::json!({"error": "invalid bearer token"}).to_string(),
        ))
    }
}

/// Resolve the list of WebSocket URLs to advertise in the Agent
/// Card for this daemon, in preference order.
///
/// **Additive auto-detection.** Mirrors WebRTC's host-candidate
/// gathering pattern: the daemon enumerates its own routable
/// interfaces via [`crate::access::routable_local_addrs`] and emits one
/// URL per address by default, so the operator doesn't need to type
/// their own LAN IP into `--advertise-url`. The operator's overrides
/// (CLI `--advertise-url` or `[server.advertise]` in intendant.toml)
/// are *prepended* — they win on preference order, but the auto-
/// detected entries still ride along as fallbacks. The connecting
/// peer's `MultiTransport::connect` walks the merged list top-down
/// and picks the first that succeeds.
///
/// ## Bind-address rules
///
/// - **Specific bind** (e.g. `192.168.1.42:8765`): only that one IP
///   is auto-detected. The operator narrowed the listener for a
///   reason; we don't second-guess by also enumerating other
///   interfaces.
/// - **Wildcard bind** (`0.0.0.0` / `::`): every routable interface
///   becomes one URL. Loopback is excluded — advertising loopback to
///   remote peers is useless. If the operator wants to expose
///   loopback (e.g. for self-peering tests), they can pass it via
///   `--advertise-url`.
///
/// ## Fallbacks (in order, when auto-detection finds nothing)
///
/// 1. Resolved host label ([`crate::access::resolve_host_label`]) —
///    works on a trusted LAN with mDNS, fragile elsewhere. Last-
///    ditch best-effort.
/// 2. `ws://localhost:0/ws` if there's no listener at all
///    (shouldn't happen in practice; the listener is always bound by
///    the time spawn is called). Card stays valid; URL won't work.
///
/// Dedup: exact-string match. If the operator's override happens to
/// match an auto-detected URL, only the operator's copy is kept.
///
/// ## Scheme
///
/// `tls_enabled` selects the auto-detected URL scheme: `wss://` when the
/// dashboard is served over TLS (`--tls` / `[server.tls]`), `ws://`
/// otherwise. This keeps advertised peer URLs honest — a TLS daemon is
/// HTTPS/WSS-only (see the strict-TLS demux in `spawn_web_gateway`), so a
/// peer handed a `ws://` URL would be refused. Operator overrides are
/// taken verbatim (the operator owns their scheme) and the final
/// no-listener fallback tracks the flag too.
pub(crate) fn resolve_advertise_urls(
    local_addr: Option<std::net::SocketAddr>,
    overrides: &[String],
    tls_enabled: bool,
) -> Vec<String> {
    let port = local_addr.map(|a| a.port()).unwrap_or(0);

    // Auto-detect. Operator overrides come first; auto entries append.
    let auto = auto_detect_advertise_urls(local_addr, port, tls_enabled);

    let mut out: Vec<String> = Vec::with_capacity(overrides.len() + auto.len());
    for url in overrides {
        if !out.contains(url) {
            out.push(url.clone());
        }
    }
    for url in auto {
        if !out.contains(&url) {
            out.push(url);
        }
    }

    if out.is_empty() {
        // No bind, no overrides, no interfaces. Card stays valid;
        // URL just won't work until the next daemon restart. Match the
        // TLS scheme so even this degenerate fallback is scheme-honest.
        out.push(format_ws_url("localhost", 0, tls_enabled));
    }
    out
}

/// Build the auto-detected URL list from the listener bind address.
/// See [`resolve_advertise_urls`] for the full resolution rules.
/// `tls_enabled` selects `wss://` vs `ws://` (see that fn's docstring).
fn auto_detect_advertise_urls(
    local_addr: Option<std::net::SocketAddr>,
    port: u16,
    tls_enabled: bool,
) -> Vec<String> {
    use std::net::IpAddr;
    let Some(addr) = local_addr else {
        return Vec::new();
    };

    // Specific bind: that one IP wins, no enumeration.
    match addr.ip() {
        IpAddr::V4(v4) if !v4.is_unspecified() => {
            return vec![format_ws_url(&v4.to_string(), port, tls_enabled)];
        }
        IpAddr::V6(v6) if !v6.is_unspecified() => {
            return vec![format_ws_url(&format!("[{v6}]"), port, tls_enabled)];
        }
        _ => {}
    }

    // Wildcard bind: enumerate every non-loopback routable interface.
    // IPv4 entries sort before IPv6 — WebRTC ICE-TCP in WebKit/WKWebView
    // silently drops IPv6 ULA candidates (seen empirically against
    // fdc2::/8 addresses on macOS 15), so the *first* URL in the list
    // — which slice 3b's `maybe_rewrite_federated_answer` takes as the
    // relay candidate verbatim — needs to be the one browsers actually
    // dial. Within each address family we preserve `getifaddrs` order
    // (`stable_sort_by`), so a multi-NIC host that already had a
    // preferred primary interface keeps it.
    let mut ips = crate::access::routable_local_addrs(false);
    ips.sort_by(|a, b| match (a, b) {
        (IpAddr::V4(_), IpAddr::V6(_)) => std::cmp::Ordering::Less,
        (IpAddr::V6(_), IpAddr::V4(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    let mut urls: Vec<String> = ips
        .into_iter()
        .map(|ip| match ip {
            IpAddr::V6(v6) => format_ws_url(&format!("[{v6}]"), port, tls_enabled),
            ip => format_ws_url(&ip.to_string(), port, tls_enabled),
        })
        .collect();

    // No interfaces found (unusual — host with no networking?). Fall
    // back to the resolved host label so the card carries *something*
    // dialable on a trusted LAN with mDNS.
    if urls.is_empty() {
        urls.push(format_ws_url(
            &crate::access::resolve_host_label(),
            port,
            tls_enabled,
        ));
    }
    urls
}

/// Format one advertised WebSocket URL. `tls_enabled` picks the secure
/// scheme (`wss://`) so a TLS daemon never advertises a `ws://` URL a peer
/// would be refused on.
fn format_ws_url(host: &str, port: u16, tls_enabled: bool) -> String {
    let scheme = if tls_enabled { "wss" } else { "ws" };
    format!("{scheme}://{host}:{port}/ws")
}

/// Assemble the [`crate::peer::AgentCard`] for this daemon from live
/// runtime state.
///
/// Called once per `spawn_web_gateway` invocation, right after the
/// config is serialized — the result is cached as `agent_card_json`
/// and cloned into each per-connection handler, matching the pattern
/// used for `/config`.
///
/// Capabilities:
/// - `ComputerUse`, `Knowledge`, `Display` are always-on subsystems
///   compiled into every build and always able to service a federation
///   request (for `Display`, that's `DisplaySession::handle_offer`
///   against whatever the local dashboard has activated — returns
///   "no such display" if nothing is active, which is the correct
///   semantics for a peer trying to view a display the operator
///   hasn't opened yet).
/// - `Voice` / `Phone` / `Recording` are gated on runtime configuration
///   that isn't plumbed through here yet. Those become additive as
///   each subsystem teaches itself to advertise, likely via dynamic
///   `PeerEvent::CapabilityEngaged` once slice 3a.2 lands.
///
/// `advertise_urls` is the preference-ordered list of WebSocket URLs
/// peers should try when dialing this daemon. Each becomes a
/// [`crate::peer::TransportSpec::IntendantWs`] entry in the card.
/// Built by [`resolve_advertise_urls`], which merges operator
/// overrides (`--advertise-url`, `[server.advertise]`) with auto-
/// detected fallback. The list is non-empty by construction.
///
/// `auth` is the [`crate::peer::AuthRequirements`] to advertise —
/// what connecting peers should send. Built by
/// `crate::main::build_local_advertised_auth` from
/// `[server.auth]` (advertised_transport + bearer_token) and the
/// access cert dir (for `pin-self-cert` fingerprint). Phase 1 of slice
/// 2c always passed `AuthRequirements::none()`; this signature
/// change lets the operator advertise mTLS / pinned-mTLS / bearer
/// in the card so connecting peers know what to send.
pub fn build_local_agent_card(
    advertise_urls: Vec<String>,
    auth: crate::peer::AuthRequirements,
) -> crate::peer::AgentCard {
    use crate::peer::{Capability, TransportSpec};
    let transports: Vec<TransportSpec> = advertise_urls
        .into_iter()
        .map(|url| TransportSpec::IntendantWs { url })
        .collect();
    crate::peer::AgentCard::local_intendant(
        crate::access::resolve_host_label(),
        env!("CARGO_PKG_VERSION").to_string(),
        Some(env!("INTENDANT_GIT_SHA").to_string()),
        transports,
        vec![
            Capability::ComputerUse,
            Capability::Knowledge,
            Capability::Display,
        ],
        auth,
    )
}

fn build_config_inner(
    live_provider: Option<&str>,
    live_model: Option<&str>,
    transcription_enabled: bool,
    ice_servers: Vec<crate::display::IceServer>,
    federation_allow_h264: bool,
) -> WebGatewayConfig {
    // If an explicit provider is given, use it directly.
    if let Some(provider) = live_provider {
        let model = live_model.unwrap_or(match provider {
            "openai" => "gpt-4o-realtime-preview",
            _ => "gemini-2.5-flash-native-audio-preview-12-2025",
        });
        let (input_rate, output_rate) = if provider == "openai" {
            (24000, 24000)
        } else {
            (16000, 24000)
        };
        return WebGatewayConfig {
            provider: provider.to_string(),
            model: model.to_string(),
            input_sample_rate: input_rate,
            output_sample_rate: output_rate,
            transcription_enabled,
            ice_servers,
            federation_allow_h264,
            ..Default::default()
        };
    }

    // If an explicit live model is given, detect provider from the model name.
    if let Some(model) = live_model {
        if model.starts_with("gpt")
            || model.starts_with("o1")
            || model.starts_with("o3")
            || model.starts_with("o4")
        {
            return WebGatewayConfig {
                provider: "openai".to_string(),
                model: model.to_string(),
                input_sample_rate: 24000,
                output_sample_rate: 24000,
                transcription_enabled,
                ice_servers,
                federation_allow_h264,
                ..Default::default()
            };
        }
        return WebGatewayConfig {
            provider: "gemini".to_string(),
            model: model.to_string(),
            input_sample_rate: 16000,
            output_sample_rate: 24000,
            transcription_enabled,
            ice_servers,
            federation_allow_h264,
            ..Default::default()
        };
    }

    // Fall back to usable-key detection (leases shadow env vars).
    if crate::credential_leases::provider_api_key("OPENAI_API_KEY").is_some()
        && crate::credential_leases::provider_api_key("GEMINI_API_KEY").is_none()
    {
        WebGatewayConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-realtime-preview".to_string(),
            input_sample_rate: 24000,
            output_sample_rate: 24000,
            transcription_enabled,
            ice_servers,
            federation_allow_h264,
            ..Default::default()
        }
    } else {
        WebGatewayConfig {
            transcription_enabled,
            ice_servers,
            federation_allow_h264,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OutboundEvent;
    use tokio::io::AsyncWriteExt;

    // Crate-wide (not module-local): tests in other modules mutate the same
    // process environment, so a per-module lock would still race them.
    use crate::test_support::TEST_ENV_LOCK;


    pub(crate) struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        pub(crate) fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }


    pub(crate) async fn next_ws_json_matching<S, F>(ws_rx: &mut S, mut matches: F) -> serde_json::Value
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
        F: FnMut(&serde_json::Value) -> bool,
    {
        let mut seen = Vec::new();
        for _ in 0..20 {
            let msg = tokio::time::timeout(tokio::time::Duration::from_secs(2), ws_rx.next())
                .await
                .expect("timeout")
                .expect("websocket closed")
                .expect("websocket error");
            let Message::Text(text) = msg else {
                continue;
            };
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            if matches(&json) {
                return json;
            }
            seen.push(json);
        }
        panic!("expected websocket message not found; seen: {seen:?}");
    }

    #[test]
    fn test_default_port() {
        assert_eq!(DEFAULT_PORT, 8765);
    }


    #[test]
    fn list_sessions_joins_external_context_from_debug_thread_log() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        let command_cwd = repo.join(".worktrees").join("feature");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&command_cwd).unwrap();

        let intendant_id = "intendant-wrapper-session";
        let log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join(intendant_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": intendant_id,
                "created_at": "2026-05-17T20:44:00",
                "project_root": repo.to_string_lossy(),
                "task": "Dashboard-started Codex task",
                "status": "running"
            })
            .to_string(),
        )
        .unwrap();

        let codex_id = "019e37ae-dashboard-started";
        let intendant_lines = [
            serde_json::json!({
                "ts": "2026-05-17T20:44:01",
                "event": "debug",
                "message": "Mode: external agent (Codex)"
            }),
            serde_json::json!({
                "ts": "2026-05-17T20:44:01.500Z",
                "event": "session_capabilities",
                "data": {
                    "session_id": intendant_id,
                    "capabilities": {
                        "follow_up": true,
                        "steer": true,
                        "interrupt": true,
                        "codex_thread_actions": ["compact", "fork", "side"],
                        "codex_managed_context": "managed",
                        "codex_command": "/tmp/codex-managed"
                    }
                }
            }),
            serde_json::json!({
                "ts": "2026-05-17T20:44:02",
                "event": "debug",
                "message": format!("External agent thread: {codex_id}")
            }),
        ];
        std::fs::write(
            log_dir.join("session.jsonl"),
            intendant_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let codex_lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": codex_id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": repo.to_string_lossy()
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {
                    "type": "exec_command_end",
                    "cwd": command_cwd.to_string_lossy()
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{codex_id}.jsonl")),
            codex_lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions: Vec<serde_json::Value> =
            serde_json::from_str(&list_sessions_from_home(home.path())).unwrap();
        assert!(
            sessions.iter().all(|s| {
                !(s.get("source").and_then(|v| v.as_str()) == Some("intendant")
                    && s.get("session_id").and_then(|v| v.as_str()) == Some(intendant_id))
            }),
            "intendant wrapper should be merged into the native external session row"
        );
        let wrapped = sessions
            .iter()
            .find(|s| {
                s.get("source").and_then(|v| v.as_str()) == Some("codex")
                    && s.get("session_id").and_then(|v| v.as_str()) == Some(codex_id)
            })
            .expect("native Codex session should be listed");
        let expected_project_root = repo.to_string_lossy().to_string();
        let expected_cwd = command_cwd.to_string_lossy().to_string();
        assert_eq!(
            wrapped.get("project_root").and_then(|v| v.as_str()),
            Some(expected_project_root.as_str())
        );
        assert_eq!(
            wrapped.get("cwd").and_then(|v| v.as_str()),
            Some(expected_cwd.as_str())
        );
        assert_eq!(
            wrapped.get("backend_source").and_then(|v| v.as_str()),
            Some("codex")
        );
        assert_eq!(
            wrapped.get("backend_source_label").and_then(|v| v.as_str()),
            Some("Codex")
        );
        assert_eq!(
            wrapped.get("backend_session_id").and_then(|v| v.as_str()),
            Some(codex_id)
        );
        assert_eq!(
            wrapped.get("intendant_session_id").and_then(|v| v.as_str()),
            Some(intendant_id)
        );
        let capabilities = wrapped
            .get("capabilities")
            .and_then(|v| v.as_object())
            .expect("capabilities should be merged from wrapper session");
        assert_eq!(
            capabilities
                .get("codex_managed_context")
                .and_then(|v| v.as_str()),
            Some("managed")
        );
        assert_eq!(
            capabilities.get("codex_command").and_then(|v| v.as_str()),
            Some("/tmp/codex-managed")
        );
        assert_eq!(
            wrapped
                .get("codex_managed_context")
                .and_then(|v| v.as_str()),
            Some("managed")
        );
        assert_eq!(
            wrapped.get("agent_command").and_then(|v| v.as_str()),
            Some("/tmp/codex-managed")
        );
    }


    #[test]
    fn list_codex_sessions_exposes_usage_limited_goal() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("07");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e5c7a-4d05-78d3-a98a-29999cb9898e";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-06-07T15:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-06-07T15:00:00Z",
                    "cwd": "/repo"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-06-07T15:01:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "thread_goal_updated",
                    "threadId": id,
                    "goal": {
                        "threadId": id,
                        "objective": "Keep the Station goal moving",
                        "status": "usageLimited",
                        "tokensUsed": 39449760,
                        "timeUsedSeconds": 93019
                    }
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-06-07T15-00-00-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(
            session.pointer("/goal/objective").and_then(|v| v.as_str()),
            Some("Keep the Station goal moving")
        );
        assert_eq!(
            session.pointer("/goal/status").and_then(|v| v.as_str()),
            Some("usageLimited")
        );
        assert_eq!(
            session
                .pointer("/session_goal/tokens_used")
                .and_then(|v| v.as_u64()),
            Some(39449760)
        );
    }

    #[test]
    fn filtered_codex_sessions_hydrates_goal_outside_list_scan_window() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("07");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e5c7a-4d05-78d3-a98a-29999cb9898e";
        let filler = "x".repeat(4096);
        let mut lines = vec![serde_json::json!({
            "timestamp": "2026-06-07T15:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "timestamp": "2026-06-07T15:00:00Z",
                "cwd": "/repo"
            }
        })];
        for idx in 0..160 {
            lines.push(serde_json::json!({
                "timestamp": format!("2026-06-07T15:00:{idx:02}Z"),
                "type": "noop",
                "payload": { "blob": filler }
            }));
        }
        lines.push(serde_json::json!({
            "timestamp": "2026-06-07T15:05:00Z",
            "type": "event_msg",
            "payload": {
                "type": "thread_goal_updated",
                "threadId": id,
                "goal": {
                    "threadId": id,
                    "objective": "Keep the Station goal moving",
                    "status": "usageLimited",
                    "tokensUsed": 39449760,
                    "timeUsedSeconds": 93019
                }
            }
        }));
        for idx in 0..160 {
            lines.push(serde_json::json!({
                "timestamp": format!("2026-06-07T15:10:{idx:02}Z"),
                "type": "noop",
                "payload": { "blob": filler }
            }));
        }
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-06-07T15-00-00-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let body = list_sessions_from_home(home.path());
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(session.get("goal"), None);

        let filtered = filter_session_list_by_ids_with_codex_goal_hydration(
            home.path(),
            &body,
            &[id.to_string()],
        );
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&filtered).unwrap();
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should still be listed");
        assert_eq!(
            session.pointer("/goal/objective").and_then(|v| v.as_str()),
            Some("Keep the Station goal moving")
        );
        assert_eq!(
            session.pointer("/goal/status").and_then(|v| v.as_str()),
            Some("usageLimited")
        );
    }

    #[test]
    fn targeted_codex_session_list_accepts_prefix_and_hydrates_goal() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("07");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e5c7a-4d05-78d3-a98a-29999cb9898e";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-06-07T15:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-06-07T15:00:00Z",
                    "cwd": "/repo"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-06-07T15:01:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "thread_goal_updated",
                    "threadId": id,
                    "goal": {
                        "threadId": id,
                        "objective": "Keep the Station goal moving",
                        "status": "usageLimited"
                    }
                }
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-06-07T15-00-00-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let body = cached_list_sessions_for_ids_from_home(home.path(), &["019e5c7a".to_string()]);
        let sessions: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.get("session_id").and_then(|v| v.as_str()), Some(id));
        assert_eq!(
            session.pointer("/goal/objective").and_then(|v| v.as_str()),
            Some("Keep the Station goal moving")
        );
    }


    #[test]
    fn external_codex_detail_limit_keeps_usage_limited_goal() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("07");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e5c7a-4d05-78d3-a98a-29999cb9898e";
        let mut lines = vec![
            serde_json::json!({
                "timestamp": "2026-06-07T15:00:00Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-06-07T15:00:00Z",
                    "cwd": "/repo"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-06-07T15:01:00Z",
                "type": "event_msg",
                "payload": {
                    "type": "thread_goal_updated",
                    "threadId": id,
                    "goal": {
                        "threadId": id,
                        "objective": "Keep the Station goal moving",
                        "status": "usageLimited",
                        "tokensUsed": 39449760,
                        "timeUsedSeconds": 93019
                    }
                }
            }),
        ];
        for idx in 0..6 {
            lines.push(serde_json::json!({
                "timestamp": format!("2026-06-07T15:02:{idx:02}Z"),
                "type": "event_msg",
                "payload": {
                    "type": "agent_message",
                    "message": format!("later output {idx}")
                }
            }));
        }
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-06-07T15-00-00-{id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let detail =
            external_session_detail_from_home_with_limit(home.path(), "codex", id, Some(2))
                .expect("external detail should parse");
        let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
        let entries = detail["entries"].as_array().unwrap();
        let goal = entries
            .iter()
            .find(|entry| entry["event"] == "session_goal")
            .expect("latest goal metadata should survive detail limiting");
        assert_eq!(
            goal.pointer("/data/goal/objective")
                .and_then(|v| v.as_str()),
            Some("Keep the Station goal moving")
        );
        assert_eq!(
            goal.pointer("/data/goal/status").and_then(|v| v.as_str()),
            Some("usageLimited")
        );

        let replay = external_session_activity_replay_from_home(home.path(), "codex", id, 2)
            .expect("external replay should parse");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        assert!(replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["event"] == "session_goal"
                && entry.pointer("/data/goal/status").and_then(|v| v.as_str())
                    == Some("usageLimited")));
    }


    #[test]
    fn list_codex_sessions_exposes_thread_name_separately_from_task() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let codex_dir = home.path().join(".codex");
        let sessions_dir = codex_dir
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37ae-thread-name";
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            serde_json::json!({
                "id": id,
                "updated_at": "2026-05-17T20:44:33Z",
                "thread_name": "Rehydration fix"
            })
            .to_string()
                + "\n",
        )
        .unwrap();

        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T20:44:33Z",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T20:44:33Z",
                    "cwd": "/Users/vm/projects/intendant"
                }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T20:45:21Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Fix activity replay"}
            }),
        ];
        let contents = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T20-44-33-{id}.jsonl")),
            contents,
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(
            session.get("name").and_then(|v| v.as_str()),
            Some("Rehydration fix")
        );
        assert_eq!(
            session.get("task").and_then(|v| v.as_str()),
            Some("Fix activity replay")
        );
    }

    #[test]
    fn codex_index_skeleton_reads_bounded_tail() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let codex_dir = home.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let old_id = "019ef734-old-index-row";
        let recent_id = "019ef734-recent-index-row";
        let old_line = serde_json::json!({
            "id": old_id,
            "updated_at": "2026-06-24T01:00:00Z",
            "thread_name": "Old index row"
        })
        .to_string();
        let recent_line = serde_json::json!({
            "id": recent_id,
            "updated_at": "2026-06-24T02:00:00Z",
            "thread_name": "Recent index row"
        })
        .to_string();
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            format!(
                "{old_line}\n{}\n{recent_line}\n",
                "x".repeat(CODEX_SESSION_INDEX_TAIL_READ_LIMIT as usize + 16)
            ),
        )
        .unwrap();

        let rows = list_codex_index_skeleton_sessions_with_limit(home.path(), 10);
        let ids = rows
            .iter()
            .filter_map(|row| value_str(row, "session_id"))
            .collect::<Vec<_>>();
        assert!(ids.contains(&recent_id.to_string()));
        assert!(!ids.contains(&old_id.to_string()));
    }

    #[test]
    fn quick_session_rows_use_external_wrapper_shape() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let codex_dir = home.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let backend_id = "019ef734-be3f-7882-b1f5-a8ed1dfe12be";
        let wrapper_id = "wrapper-session-for-codex";
        std::fs::write(
            codex_dir.join("session_index.jsonl"),
            serde_json::json!({
                "id": backend_id,
                "updated_at": "2026-06-24T02:00:00Z",
                "thread_name": "Wrapped Codex row"
            })
            .to_string()
                + "\n",
        )
        .unwrap();
        let wrapper_dir = home.path().join(".intendant").join("logs").join(wrapper_id);
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        std::fs::write(
            wrapper_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": wrapper_id,
                "created_at": "2026-06-24T01:59:00",
                "task": "Run through Intendant"
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home.path(),
            "codex",
            backend_id,
            wrapper_id,
            &wrapper_dir,
            None,
        )
        .unwrap();

        let mut rows = list_intendant_skeleton_sessions_with_limit(home.path(), 10);
        rows.extend(list_codex_index_skeleton_sessions_with_limit(
            home.path(),
            10,
        ));
        merge_quick_session_rows_with_wrapper_index(home.path(), &mut rows);

        assert_eq!(rows.len(), 1);
        assert_eq!(value_str(&rows[0], "source").as_deref(), Some("codex"));
        assert_eq!(
            value_str(&rows[0], "intendant_session_id").as_deref(),
            Some(wrapper_id)
        );
        assert_eq!(
            value_str(&rows[0], "session_id").as_deref(),
            Some(backend_id)
        );
        assert_eq!(rows[0]["total_bytes"].as_u64(), Some(0));
        assert!(value_str(&rows[0], "path").is_none());
    }

    #[test]
    fn list_codex_sessions_marks_subagent_lineage() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("06")
            .join("24");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let parent_id = "019ef734-parent-thread";
        let child_id = "019ef734-child-thread";
        let lines = [
            serde_json::json!({
                "timestamp": "2026-06-24T01:18:11Z",
                "type": "session_meta",
                "payload": {
                    "id": child_id,
                    "timestamp": "2026-06-24T01:18:09Z",
                    "cwd": "/repo",
                    "source": {
                        "subagent": {
                            "thread_spawn": {
                                "parent_thread_id": parent_id,
                                "agent_nickname": "Zeno"
                            }
                        }
                    }
                }
            }),
            serde_json::json!({
                "timestamp": "2026-06-24T01:18:12Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "Run child lane"}
            }),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-06-24T01-18-09-{child_id}.jsonl")),
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(child_id))
            .expect("codex subagent session should be listed");
        assert_eq!(
            session.get("parent_session_id").and_then(|v| v.as_str()),
            Some(parent_id)
        );
        assert_eq!(
            session.get("relationship_kind").and_then(|v| v.as_str()),
            Some("subagent")
        );
        assert_eq!(
            session.get("thread_source").and_then(|v| v.as_str()),
            Some("subagent")
        );
        assert_eq!(
            session.get("agent_nickname").and_then(|v| v.as_str()),
            Some("Zeno")
        );
    }


    #[test]
    fn list_codex_sessions_parses_large_prefix_and_daily_usage() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        let sessions_dir = home
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("05")
            .join("17");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let id = "019e37c5-large-prefix-daily";
        let large_prompt = "x".repeat(EXTERNAL_SESSION_READ_LIMIT as usize + 1024);
        let filler = "y".repeat(EXTERNAL_SESSION_READ_LIMIT as usize + 1024);
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T10:00:00",
                "type": "session_meta",
                "payload": {
                    "id": id,
                    "timestamp": "2026-05-17T10:00:00",
                    "cwd": "/Users/vm/projects/intendant",
                    "model_provider": "openai",
                    "base_instructions": {"text": large_prompt}
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-05-17T10:00:01",
                "type": "turn_context",
                "payload": {
                    "cwd": "/Users/vm/projects/intendant",
                    "model": "gpt-5.4"
                }
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-05-17T10:00:02",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 80,
                            "cached_input_tokens": 20,
                            "output_tokens": 20,
                            "total_tokens": 100
                        }
                    }
                }
            })
            .to_string(),
            filler,
            serde_json::json!({
                "timestamp": "2026-05-18T10:00:02",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 200,
                            "cached_input_tokens": 50,
                            "output_tokens": 50,
                            "total_tokens": 250
                        }
                    }
                }
            })
            .to_string(),
        ];
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T10-00-00-{id}.jsonl")),
            lines.join("\n"),
        )
        .unwrap();

        let sessions = list_codex_sessions(home.path());
        let session = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(id))
            .expect("codex session should be listed");
        assert_eq!(session["model"].as_str(), Some("gpt-5.4"));
        assert_eq!(session["prompt_tokens"].as_u64(), Some(200));
        assert_eq!(session["completion_tokens"].as_u64(), Some(50));
        assert_eq!(session["cached_tokens"].as_u64(), Some(50));
        assert_eq!(session["total_tokens"].as_u64(), Some(250));

        let daily = session["daily_usage"].as_array().expect("daily usage");
        let by_day = daily
            .iter()
            .map(|row| {
                (
                    row["day"].as_str().unwrap().to_string(),
                    row["total_tokens"].as_u64().unwrap(),
                )
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(by_day.get("2026-05-17"), Some(&100));
        assert_eq!(by_day.get("2026-05-18"), Some(&150));
    }


    #[test]
    fn codex_transcript_imports_function_call_output() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-function-output";
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_empty",
                        "output": ""
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00.500Z",
                    "type": "response_item",
                    "payload": {
                        "id": "call-item-output",
                        "type": "function_call",
                        "call_id": "call_output",
                        "name": "exec_command",
                        "arguments": "{\"cmd\":\"echo actual\",\"workdir\":\"/tmp\"}"
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:01Z",
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "call_output",
                        "output": "Chunk ID: abc123\nWall time: 0.0001 seconds\nProcess exited with code 0\nOriginal token count: 8\nOutput:\nTotal output lines: 1\n\nactual command output\n"
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let entries = external_session_entries_from_home(dir.path(), "codex", session_id)
            .expect("codex session should parse");
        let outputs: Vec<_> = entries
            .iter()
            .filter(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("agent_output"))
            .collect();

        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0]["session_id"], session_id);
        assert_eq!(outputs[0]["source"], "codex");
        assert_eq!(outputs[0]["kind"], "agent_output");
        assert_eq!(outputs[0]["output_id"], "call_output");
        assert_eq!(outputs[0]["stdout"], "actual command output\n");
        assert_eq!(outputs[0]["item_id"], "call-item-output");
        assert_eq!(outputs[0]["item_type"], "command_execution");
        assert_eq!(outputs[0]["command_item_id"], "call-item-output");
        assert_eq!(outputs[0]["turn_id"], "turn-unknown");
        assert_eq!(outputs[0]["delivery"], "lossless");
        assert!(outputs[0]["ts_ms"].as_i64().is_some());
        assert_eq!(outputs[0]["command_execution"]["status"], "completed");
        assert_eq!(outputs[0]["command_execution"]["command"], "echo actual");
        assert_eq!(outputs[0]["command_execution"]["cwd"], "/tmp");
        assert_eq!(outputs[0]["thread_item"]["type"], "command_execution");
        assert_eq!(
            outputs[0]["thread_history_change"]["changed_items"][0]["id"],
            "call-item-output"
        );

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let replay_output = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["event"] == "log_entry" && entry["kind"] == "agent_output")
            .expect("command output should replay as a log entry");
        assert_eq!(replay_output["content"], "actual command output\n");
        assert_eq!(replay_output["output_id"], "call_output");
        assert_eq!(
            replay_output["event_id"],
            format!("external:codex:{session_id}:item:call-item-output")
        );
        assert_eq!(replay_output["delivery"], "lossless");
        assert!(replay_output["ts_ms"].as_i64().is_some());
        assert_eq!(replay_output["item_type"], "command_execution");
        assert_eq!(replay_output["command_execution"]["id"], "call-item-output");
        assert_eq!(
            replay_output["thread_history_change"]["changed_items"][0]["id"],
            "call-item-output"
        );
    }


    #[test]
    fn external_activity_replay_uses_compact_session_transcript() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let dir = tempfile::tempdir().unwrap();
        let sessions_dir = dir.path().join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let session_id = "019e37b2-e756-7461-9946-34b639448717";
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "id": "msg-user-refresh",
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "What happens on refresh?" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:04Z",
                    "type": "response_item",
                    "payload": {
                        "id": "msg-agent-refresh",
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "The task keeps running." }]
                    }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let normalized = external_session_entries_from_home(dir.path(), "codex", session_id)
            .expect("codex session should parse");
        let normalized_user = normalized
            .iter()
            .find(|entry| entry["content"] == "What happens on refresh?")
            .expect("user entry should parse");
        assert_eq!(normalized_user["item_id"], "msg-user-refresh");
        let normalized_agent = normalized
            .iter()
            .find(|entry| entry["content"] == "The task keeps running.")
            .expect("agent entry should parse");
        assert_eq!(normalized_agent["item_id"], "msg-agent-refresh");

        let replay =
            external_session_activity_replay_from_home(dir.path(), "codex", session_id, 80)
                .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        assert_eq!(replay["t"], "log_replay");
        assert_eq!(replay["replay_semantics"], EXTERNAL_TRANSCRIPT_SEMANTICS);

        let entries = replay["entries"].as_array().unwrap();
        assert_eq!(entries[0]["event"], "replay_start");
        assert_eq!(
            entries[0]["replay_semantics"],
            EXTERNAL_TRANSCRIPT_SEMANTICS
        );
        assert_eq!(entries[1]["event"], "session_attached");
        assert_eq!(entries[1]["session_id"], session_id);
        assert_eq!(entries[1]["source"], "codex");

        assert!(entries.iter().any(|entry| {
            entry["event"] == "log_entry"
                && entry["session_id"] == session_id
                && entry["level"] == "info"
                && entry["source"] == "user"
                && entry["content"] == "What happens on refresh?"
                && entry["user_turn_index"] == 1
                && entry["item_id"] == "msg-user-refresh"
        }));
        assert!(entries.iter().any(|entry| {
            entry["event"] == "log_entry"
                && entry["session_id"] == session_id
                && entry["level"] == "model"
                && entry["source"] == "codex"
                && entry["content"] == "The task keeps running."
                && entry["item_id"] == "msg-agent-refresh"
        }));
    }


    #[test]
    fn settings_payload_accepts_settings_tab_save_without_agent_runtime_fields() {
        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex"
        })
        .to_string();

        let payload: SettingsPayload = serde_json::from_str(&body).unwrap();
        assert_eq!(payload.external_agent.as_deref(), Some("codex"));
        assert_eq!(payload.codex_sandbox, None);
        assert_eq!(payload.codex_approval_policy, None);
        assert_eq!(payload.codex_managed_context, None);

        let mut config = crate::project::ProjectConfig::default();
        config.agent.codex.command = "/opt/codex/bin/codex".to_string();
        config.agent.codex.sandbox = "danger-full-access".to_string();
        config.agent.codex.approval_policy = "never".to_string();
        config.agent.codex.managed_context = "managed".to_string();
        config.agent.codex.service_tier = Some("priority".to_string());
        apply_settings_payload(&mut config, &payload);

        assert_eq!(config.agent.default_backend.as_deref(), Some("codex"));
        assert_eq!(config.agent.codex.command, "/opt/codex/bin/codex");
        assert_eq!(config.agent.codex.sandbox, "danger-full-access");
        assert_eq!(config.agent.codex.approval_policy, "never");
        assert_eq!(config.agent.codex.managed_context, "managed");
        assert_eq!(config.agent.codex.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn settings_payload_round_trips_codex_command() {
        let mut config = crate::project::ProjectConfig::default();
        config.agent.codex.command = "/usr/local/bin/codex".to_string();
        config.agent.codex.managed_context = "managed".to_string();
        config.agent.codex.service_tier = Some("priority".to_string());
        config.agent.claude_code.command = "/usr/local/bin/claude".to_string();

        let payload = settings_payload_from_config(&config);
        assert_eq!(
            payload.codex_command.as_deref(),
            Some("/usr/local/bin/codex")
        );
        assert_eq!(payload.codex_sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(payload.codex_approval_policy.as_deref(), Some("on-request"));
        assert_eq!(payload.codex_managed_context.as_deref(), Some("managed"));
        assert_eq!(payload.codex_service_tier.as_deref(), Some("priority"));
        assert_eq!(
            payload.claude_command.as_deref(),
            Some("/usr/local/bin/claude")
        );

        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex",
            "codex_command": "  /opt/homebrew/bin/codex  ",
            "codex_sandbox": "danger-full-access",
            "codex_approval_policy": "never",
            "codex_service_tier": "normal",
            "codex_managed_context": "true",
            "claude_command": "  /opt/claude/bin/claude  "
        })
        .to_string();

        let payload: SettingsPayload = serde_json::from_str(&body).unwrap();
        apply_settings_payload(&mut config, &payload);

        assert_eq!(config.agent.codex.command, "/opt/homebrew/bin/codex");
        assert_eq!(config.agent.codex.sandbox, "danger-full-access");
        assert_eq!(config.agent.codex.approval_policy, "never");
        assert_eq!(config.agent.codex.service_tier.as_deref(), Some("standard"));
        assert_eq!(config.agent.codex.managed_context, "managed");
        assert_eq!(config.agent.claude_code.command, "/opt/claude/bin/claude");
    }

    #[test]
    fn settings_post_result_rejects_invalid_json_with_bad_request() {
        let (status, body) = settings_post_result(
            "{\"external_agent\":",
            Some(Path::new(".")),
            &EventBus::new(),
        );

        assert_eq!(status, "400 Bad Request");
        assert!(body.contains("Invalid settings"));
    }

    #[test]
    fn settings_post_result_rejects_missing_project_root_with_bad_request() {
        let (status, body) = settings_post_result("{}", None, &EventBus::new());

        assert_eq!(status, "400 Bad Request");
        assert!(body.contains("No project root"));
    }

    /// POST /api/settings must keep the LIVE codex runtime config coherent,
    /// not just the TOML: launches read the shared `CodexRuntimeConfig`,
    /// which overrides the file. The gateway does that by re-dispatching
    /// the codex fields as control-plane intents after a successful save.
    #[test]
    fn settings_post_dispatches_codex_control_msgs_for_live_state() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex",
            "codex_command": "/opt/codex/bin/codex",
            "codex_sandbox": "danger-full-access",
            "codex_approval_policy": "never",
            "codex_service_tier": "priority",
            "codex_managed_context": "managed",
            "codex_context_archive": "exact"
        })
        .to_string();

        let (status, _) = settings_post_result(&body, Some(dir.path()), &bus);
        assert_eq!(status, "200 OK");

        let mut saw_command = false;
        let mut saw_sandbox = false;
        let mut saw_approval = false;
        let mut saw_service_tier = false;
        let mut saw_managed = false;
        let mut saw_archive = false;
        while let Ok(event) = rx.try_recv() {
            let AppEvent::ControlCommand(msg) = event else {
                continue;
            };
            match msg {
                ControlMsg::SetCodexCommand { command } => {
                    assert_eq!(command.as_deref(), Some("/opt/codex/bin/codex"));
                    saw_command = true;
                }
                ControlMsg::SetCodexSandbox { mode } => {
                    assert_eq!(mode, "danger-full-access");
                    saw_sandbox = true;
                }
                ControlMsg::SetCodexApprovalPolicy { policy } => {
                    assert_eq!(policy, "never");
                    saw_approval = true;
                }
                ControlMsg::SetCodexServiceTier { service_tier } => {
                    assert_eq!(service_tier.as_deref(), Some("priority"));
                    saw_service_tier = true;
                }
                ControlMsg::SetCodexManagedContext { mode } => {
                    assert_eq!(mode, "managed");
                    saw_managed = true;
                }
                ControlMsg::SetCodexContextArchive { mode } => {
                    assert_eq!(mode, "exact");
                    saw_archive = true;
                }
                _ => {}
            }
        }
        assert!(saw_command, "SetCodexCommand was not dispatched");
        assert!(saw_sandbox, "SetCodexSandbox was not dispatched");
        assert!(saw_approval, "SetCodexApprovalPolicy was not dispatched");
        assert!(saw_service_tier, "SetCodexServiceTier was not dispatched");
        assert!(saw_managed, "SetCodexManagedContext was not dispatched");
        assert!(saw_archive, "SetCodexContextArchive was not dispatched");

        // The synchronous TOML write still happened (read-after-write
        // consistency for an immediate GET /api/settings).
        let saved = std::fs::read_to_string(dir.path().join("intendant.toml")).unwrap();
        assert!(saved.contains("managed_context = \"managed\""));
    }

    /// Codex fields absent from the payload must not be re-dispatched —
    /// a partial settings save must not clobber live state with defaults.
    #[test]
    fn settings_post_skips_codex_control_msgs_for_absent_fields() {
        let dir = tempfile::tempdir().unwrap();
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let body = serde_json::json!({
            "cu_provider": null,
            "cu_model": null,
            "cu_backend": "auto",
            "presence_enabled": true,
            "presence_provider": null,
            "presence_model": null,
            "presence_live_provider": null,
            "presence_live_model": null,
            "transcription_enabled": false,
            "transcription_provider": "openai",
            "transcription_model": "whisper-1",
            "transcription_endpoint": null,
            "transcription_language": null,
            "recording_enabled": false,
            "recording_framerate": 15,
            "recording_quality": "medium",
            "live_audio_enabled": false,
            "live_audio_timeout_secs": 300,
            "external_agent": "codex"
        })
        .to_string();

        let (status, _) = settings_post_result(&body, Some(dir.path()), &bus);
        assert_eq!(status, "200 OK");

        while let Ok(event) = rx.try_recv() {
            if let AppEvent::ControlCommand(msg) = event {
                assert!(
                    !matches!(
                        msg,
                        ControlMsg::SetCodexCommand { .. }
                            | ControlMsg::SetCodexSandbox { .. }
                            | ControlMsg::SetCodexApprovalPolicy { .. }
                            | ControlMsg::SetCodexServiceTier { .. }
                            | ControlMsg::SetCodexManagedContext { .. }
                            | ControlMsg::SetCodexContextArchive { .. }
                    ),
                    "unexpected codex control msg for absent payload field: {msg:?}"
                );
            }
        }
    }


    /// A specific bind address is preserved verbatim in the
    /// advertised URL. The operator chose it; we trust them.
    #[test]
    fn advertise_url_preserves_specific_bind_address() {
        use std::net::{Ipv4Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(specific), &[], false),
            vec!["ws://127.0.0.1:8765/ws".to_string()]
        );
        let lan_ip = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(lan_ip), &[], false),
            vec!["ws://192.168.1.42:8765/ws".to_string()]
        );
    }

    /// With TLS enabled the auto-detected scheme is `wss://`, not `ws://`
    /// — a TLS daemon is HTTPS/WSS-only, so advertising `ws://` would hand
    /// peers a URL they'd be refused on. Operator overrides are still
    /// taken verbatim (they own their scheme).
    #[test]
    fn advertise_url_uses_wss_when_tls_enabled() {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        assert_eq!(
            resolve_advertise_urls(Some(specific), &[], true),
            vec!["wss://192.168.1.42:8765/ws".to_string()]
        );
        let v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 8765);
        let urls = resolve_advertise_urls(Some(v6), &[], true);
        assert_eq!(urls, vec!["wss://[::1]:8765/ws".to_string()]);
        // Wildcard bind with TLS: every auto-detected URL is wss://.
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        for url in resolve_advertise_urls(Some(wildcard), &[], true) {
            assert!(url.starts_with("wss://"), "tls scheme on every URL: {url}");
        }
        // Operator override is verbatim — its scheme is not rewritten.
        let overrides = vec!["ws://operator.example:9000/ws".to_string()];
        let urls = resolve_advertise_urls(Some(specific), &overrides, true);
        assert_eq!(urls[0], "ws://operator.example:9000/ws");
    }

    /// Wildcard bind (0.0.0.0) gets replaced with one URL per routable
    /// interface (auto-detection), never the literal wildcard. This
    /// is the guard against the production case where main.rs binds
    /// to 0.0.0.0:8765 and an earlier implementation was handing out
    /// `ws://0.0.0.0:8765/ws` in the Agent Card — an unusable URL
    /// that the transport-url-is-the-listener-addr assumption let
    /// slip through localhost-only tests.
    ///
    /// The exact set of interfaces is environment-dependent so we
    /// can't pin specific addresses; we only assert that no entry is
    /// the wildcard literal and the port is preserved everywhere.
    #[test]
    fn advertise_url_replaces_ipv4_wildcard_with_interface_urls() {
        use std::net::{Ipv4Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[], false);
        assert!(
            !urls.is_empty(),
            "auto-detect should produce at least one URL"
        );
        for url in &urls {
            assert!(
                !url.contains("0.0.0.0"),
                "wildcard must not appear in any auto-detected URL: {url}"
            );
            assert!(url.starts_with("ws://"), "scheme preserved: {url}");
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
            let host = url
                .strip_prefix("ws://")
                .and_then(|rest| rest.strip_suffix(":8765/ws"))
                .expect("url has expected prefix/suffix");
            assert!(
                !host.is_empty(),
                "host must resolve to something non-empty: {url}"
            );
        }
    }

    /// Same guard for IPv6 wildcards (::), which have the same
    /// unreachability problem as 0.0.0.0. Auto-detected v6 entries
    /// are bracketed per RFC 3986; we don't pin which interfaces are
    /// found because that's environment-dependent.
    #[test]
    fn advertise_url_replaces_ipv6_wildcard_with_interface_urls() {
        use std::net::{Ipv6Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[], false);
        assert!(
            !urls.is_empty(),
            "wildcard v6 bind should still produce some auto-detected URLs"
        );
        for url in &urls {
            assert!(
                !url.contains("[::]"),
                "ipv6 wildcard must not appear in any auto-detected URL: {url}"
            );
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
        }
    }

    /// IPv6 specific addresses are bracketed in the URL per RFC 3986
    /// so a literal address like `::1` doesn't collide with the
    /// `:port` separator.
    #[test]
    fn advertise_url_brackets_specific_ipv6_address() {
        use std::net::{Ipv6Addr, SocketAddr};
        let specific = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 8765);
        let urls = resolve_advertise_urls(Some(specific), &[], false);
        assert_eq!(urls.len(), 1);
        assert!(
            urls[0].contains("[::1]"),
            "ipv6 literal must be bracketed: {}",
            urls[0]
        );
    }

    // -----------------------------------------------------------------
    // resolve_url_to_socket_addr (slice 3a.2 — URL hint parsing)
    // -----------------------------------------------------------------

    /// Directly-parseable `ipv4:port` authorities are returned
    /// without any DNS round-trip.
    #[tokio::test]
    async fn resolve_url_parses_ipv4_literal_url() {
        let addr = resolve_url_to_socket_addr("ws://127.0.0.1:8766/ws")
            .await
            .expect("parses");
        assert_eq!(addr.to_string(), "127.0.0.1:8766");
    }

    /// Bracketed IPv6 literals round-trip through the parser; the
    /// `/ws` path suffix is stripped before the SocketAddr parse.
    #[tokio::test]
    async fn resolve_url_parses_ipv6_literal_url() {
        let addr = resolve_url_to_socket_addr("wss://[::1]:8443/ws")
            .await
            .expect("parses");
        assert_eq!(addr.port(), 8443);
        assert!(addr.is_ipv6(), "expected IPv6, got {addr}");
    }

    /// `http://` and `https://` are accepted alongside the WebSocket
    /// schemes so the same URL form works whether the operator types
    /// the dashboard URL or the /ws URL.
    #[tokio::test]
    async fn resolve_url_accepts_http_and_https_schemes() {
        let a = resolve_url_to_socket_addr("http://127.0.0.1:8000/")
            .await
            .expect("http parses");
        assert_eq!(a.port(), 8000);
        let b = resolve_url_to_socket_addr("https://127.0.0.1:8443")
            .await
            .expect("https parses");
        assert_eq!(b.port(), 8443);
    }

    /// Hostnames route through `tokio::net::lookup_host`. `localhost`
    /// is the one name we can rely on across every test environment.
    #[tokio::test]
    async fn resolve_url_resolves_localhost_via_dns() {
        let addr = resolve_url_to_socket_addr("ws://localhost:8766/ws")
            .await
            .expect("resolves");
        assert_eq!(addr.port(), 8766);
        assert!(
            addr.ip().is_loopback(),
            "localhost must resolve to a loopback address: {addr}"
        );
    }

    /// URLs with a path + query string strip cleanly: the authority
    /// is everything up to the first `/` or `?`.
    #[tokio::test]
    async fn resolve_url_strips_path_and_query() {
        let a = resolve_url_to_socket_addr("ws://127.0.0.1:8766/ws/path?foo=bar")
            .await
            .expect("parses");
        assert_eq!(a.to_string(), "127.0.0.1:8766");
    }

    /// Unknown schemes, missing ports, and unresolvable hostnames
    /// all return `None` — caller falls back to UDP-only path.
    #[tokio::test]
    async fn resolve_url_returns_none_on_malformed_inputs() {
        // Unknown scheme
        assert!(resolve_url_to_socket_addr("foo://127.0.0.1:8766")
            .await
            .is_none());
        // Empty authority
        assert!(resolve_url_to_socket_addr("ws:///path").await.is_none());
        // No port (authority parses as IP but not SocketAddr; lookup_host
        // rejects a bare host with no port).
        assert!(resolve_url_to_socket_addr("ws://127.0.0.1/ws")
            .await
            .is_none());
    }

    /// Operator overrides come first in the merged list (preference
    /// order), but auto-detected entries are appended as fallbacks.
    /// The connecting peer's `MultiTransport::connect` walks the list
    /// top-down and uses the first that succeeds, so overrides win on
    /// preference while auto entries provide redundancy.
    #[test]
    fn advertise_overrides_prepend_to_auto_detected() {
        use std::net::{Ipv4Addr, SocketAddr};
        // Specific bind so we can assert exactly one auto-detected entry
        // (wildcard bind would enumerate every host interface — non-
        // deterministic in CI). Specific-bind also covers the
        // intentionally-narrowed-listener case.
        let bind = SocketAddr::new(Ipv4Addr::new(127, 0, 0, 1).into(), 8765);
        let overrides = vec![
            "ws://192.168.1.42:8765/ws".to_string(),
            "wss://laptop.tail-abcd.ts.net:8443/ws".to_string(),
        ];
        let urls = resolve_advertise_urls(Some(bind), &overrides, false);
        // Overrides come first, auto-detected entry appended.
        assert_eq!(urls.len(), 3, "got: {urls:?}");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
        assert_eq!(urls[1], "wss://laptop.tail-abcd.ts.net:8443/ws");
        assert_eq!(urls[2], "ws://127.0.0.1:8765/ws");
    }

    /// An empty overrides list relies entirely on auto-detection.
    /// With a specific bind the result is exactly that one URL.
    #[test]
    fn empty_overrides_use_only_auto_detected_url() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let urls = resolve_advertise_urls(Some(lan), &[], false);
        assert_eq!(urls, vec!["ws://192.168.1.42:8765/ws".to_string()]);
    }

    /// Dedup: an operator URL that happens to match an auto-detected
    /// entry is kept exactly once (in operator position, since
    /// overrides are processed first). Avoids advertising the same
    /// URL twice when the operator types out their LAN IP that the
    /// daemon would have auto-detected anyway.
    #[test]
    fn advertise_dedupes_overrides_matching_auto_detected() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let overrides = vec!["ws://192.168.1.42:8765/ws".to_string()];
        let urls = resolve_advertise_urls(Some(lan), &overrides, false);
        assert_eq!(urls.len(), 1, "duplicate suppressed: {urls:?}");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
    }

    /// A wildcard bind enumerates every routable non-loopback
    /// interface. We can't pin exact addresses (CI hosts vary) but
    /// can assert: (a) at least one URL is produced, (b) loopback is
    /// excluded (advertising loopback to remote peers is useless),
    /// (c) the port matches the bind port.
    #[test]
    fn advertise_wildcard_bind_enumerates_interfaces_excluding_loopback() {
        use std::net::{Ipv4Addr, SocketAddr};
        let wildcard = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 8765);
        let urls = resolve_advertise_urls(Some(wildcard), &[], false);
        assert!(
            !urls.is_empty(),
            "expected at least one auto-detected URL, got: {urls:?}"
        );
        for url in &urls {
            assert!(
                !url.contains("127.0.0.1"),
                "loopback must not appear in auto-detected federation URLs: {url}"
            );
            assert!(
                !url.contains("0.0.0.0"),
                "wildcard must not appear in auto-detected URLs: {url}"
            );
            assert!(url.ends_with(":8765/ws"), "port preserved: {url}");
        }
    }

    /// When operator wants to override completely (e.g. for security
    /// reasons — only advertise the Tailscale URL even though the
    /// daemon binds wildcard), they bind to a specific interface
    /// instead of wildcard. Specific bind narrows auto-detection to
    /// just that interface, so combined with operator override the
    /// effective list is `[override..., that_one_interface]`.
    #[test]
    fn specific_bind_narrows_auto_detection_to_one_interface() {
        use std::net::{Ipv4Addr, SocketAddr};
        let lan_only = SocketAddr::new(Ipv4Addr::new(192, 168, 1, 42).into(), 8765);
        let urls = resolve_advertise_urls(Some(lan_only), &[], false);
        assert_eq!(urls.len(), 1, "specific bind = exactly one auto entry");
        assert_eq!(urls[0], "ws://192.168.1.42:8765/ws");
    }


    #[test]
    fn is_federation_path_uses_parsed_routes() {
        assert!(is_federation_path("GET /api/peers HTTP/1.1"));
        assert!(is_federation_path("POST /api/peers/p-1/task HTTP/1.1"));
        assert!(is_federation_path("GET /api/sessions?limit=5 HTTP/1.1"));
        // Look-alike paths and query mentions are not federation routes.
        assert!(!is_federation_path("GET /api/peersonal HTTP/1.1"));
        assert!(!is_federation_path(
            "GET /api/fs/stat?path=/api/sessions HTTP/1.1"
        ));
    }


    #[test]
    fn test_web_gateway_config_default() {
        let config = WebGatewayConfig::default();
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
        assert_eq!(config.output_sample_rate, 24000);
    }

    #[test]
    fn test_web_gateway_config_serialize() {
        let config = WebGatewayConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"provider\":\"gemini\""));
        assert!(json.contains("\"input_sample_rate\":16000"));
    }

    #[test]
    fn test_build_config_gemini_model() {
        let config = build_config(
            None,
            Some("gemini-2.5-flash-native-audio-preview-12-2025"),
            false,
            crate::display::IceConfig::default(),
            false,
        );
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.input_sample_rate, 16000);
    }

    #[test]
    fn test_build_config_openai_model() {
        let config = build_config(
            None,
            Some("gpt-4o-realtime-preview"),
            false,
            crate::display::IceConfig::default(),
            false,
        );
        assert_eq!(config.provider, "openai");
        assert_eq!(config.input_sample_rate, 24000);
    }

    #[test]
    fn test_build_config_explicit_provider() {
        let config = build_config(
            Some("openai"),
            None,
            false,
            crate::display::IceConfig::default(),
            false,
        );
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "gpt-4o-realtime-preview");
    }

    #[test]
    fn test_build_config_no_model() {
        // With no model and no env vars set in a predictable way,
        // this should default to gemini
        let config = build_config(
            None,
            None,
            false,
            crate::display::IceConfig::default(),
            false,
        );
        // Either gemini or openai depending on env, but it shouldn't panic
        assert!(!config.provider.is_empty());
    }


    #[test]
    fn session_detail_limit_keeps_latest_goal_per_nested_session_id() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let entries = vec![
            serde_json::json!({
                "event": "session_goal",
                "data": {
                    "session_id": "target-session",
                    "goal": {
                        "objective": "old target goal",
                        "status": "active"
                    }
                }
            }),
            serde_json::json!({
                "event": "session_goal",
                "data": {
                    "session_id": "target-session",
                    "goal": {
                        "objective": "latest target goal",
                        "status": "active"
                    }
                }
            }),
            serde_json::json!({
                "event": "session_goal",
                "data": {
                    "session_id": "other-session",
                    "goal": {
                        "objective": "other goal",
                        "status": "active"
                    }
                }
            }),
            serde_json::json!({"event": "model_response", "summary": "tail 1"}),
            serde_json::json!({"event": "model_response", "summary": "tail 2"}),
        ];

        let limited = limited_session_detail_entries(entries, Some(2));
        let goals: Vec<_> = limited
            .iter()
            .filter(|entry| entry["event"] == "session_goal")
            .filter_map(|entry| {
                entry
                    .pointer("/data/goal/objective")
                    .and_then(|v| v.as_str())
            })
            .collect();
        assert_eq!(goals, vec!["latest target goal", "other goal"]);
    }

    #[test]
    fn websocket_bootstrap_replay_omits_context_and_caps_history() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let mut entries = vec![
            serde_json::json!({"event": "replay_start"}),
            serde_json::json!({
                "event": "context_snapshot",
                "raw": {"instructions": "large historical context"}
            }),
            serde_json::json!({
                "event": "session_goal",
                "data": {
                    "session_id": "target-session",
                    "goal": {
                        "objective": "latest goal",
                        "status": "active"
                    }
                }
            }),
        ];
        for n in 0..40 {
            entries.push(serde_json::json!({
                "event": "model_response",
                "summary": format!("tail {n}")
            }));
        }
        entries.push(serde_json::json!({
            "event": "model_response",
            "summary": "x".repeat(WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES + 1024)
        }));

        let limited = prepare_websocket_bootstrap_replay_entries(entries, 10);

        assert!(!limited.iter().any(|entry| {
            entry.get("event").and_then(|v| v.as_str()) == Some("context_snapshot")
        }));
        assert!(limited.len() <= 12);
        assert!(limited.iter().any(|entry| {
            entry
                .pointer("/data/goal/objective")
                .and_then(|v| v.as_str())
                == Some("latest goal")
        }));
        let oversized_summary = limited
            .last()
            .and_then(|entry| entry.get("summary"))
            .and_then(|v| v.as_str())
            .expect("tail summary should remain");
        assert!(oversized_summary.ends_with("..."));
        assert!(oversized_summary.len() < WEBSOCKET_BOOTSTRAP_REPLAY_TEXT_LIMIT_BYTES + 16);
    }


    #[test]
    fn external_activity_replay_uses_wrapper_index_for_multiple_codex_attaches() {
        let _codex_home = EnvVarGuard::unset("CODEX_HOME");
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let session_id = "019e37b2-multiple-wrapper-index";
        let sessions_dir = home.join(".codex").join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-05-17T16-48-52-{session_id}.jsonl")),
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "continue indexed thread" }]
                    }
                }),
            ]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        for (wrapper_id, request_id, request_index) in [
            ("wrapper-before-daemon-restart", "req-before", 1_u64),
            ("wrapper-after-daemon-restart", "req-after", 2_u64),
        ] {
            let wrapper_log_dir = home.join(".intendant").join("logs").join(wrapper_id);
            let mut log = crate::session_log::SessionLog::open(wrapper_log_dir).unwrap();
            log.session_identity(wrapper_id, "codex", session_id);
            log.context_snapshot_for_session(
                Some(session_id),
                "codex",
                "Codex resolved request payload",
                Some(request_id),
                Some(request_index),
                Some(request_index as usize),
                "openai.responses.resolved_request.v1",
                Some(1200 + request_index),
                Some("backend_reported"),
                Some(128_000),
                Some(272_000),
                Some(1),
                &serde_json::json!({
                    "_intendant_context": {
                        "thread_id": session_id,
                        "request_id": request_id,
                        "request_index": request_index
                    },
                    "input": [{"role": "user", "content": request_id}]
                }),
            );
            drop(log);
        }

        let indexed = crate::external_wrapper_index::wrappers_for(home, "codex", session_id);
        assert_eq!(indexed.len(), 2);

        let replay = external_session_activity_replay_from_home(home, "codex", session_id, 80)
            .expect("codex session should replay");
        let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
        let request_ids: HashSet<_> = replay["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["event"] == "context_snapshot")
            .filter_map(|entry| entry["request_id"].as_str())
            .collect();
        assert!(request_ids.contains("req-before"));
        assert!(request_ids.contains("req-after"));

        let sessions: Vec<serde_json::Value> =
            serde_json::from_str(&list_sessions_from_home(home)).unwrap();
        let row = sessions
            .iter()
            .find(|session| session["session_id"] == session_id)
            .expect("codex session row should be present");
        assert_eq!(row["intendant_wrappers"].as_array().map(Vec::len), Some(2));
    }


    // ---- /api/peers endpoint tests ----

    /// Spawn a test gateway with the given peer registry option and
    /// return (port, gateway handle). Condensed helper to keep the
    /// /api/peers tests below compact.
    async fn spawn_test_gateway_with_registry(
        peer_registry: Option<crate::peer::PeerRegistry>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            peer_registry,
            Vec::new(),
            None,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        (port, handle)
    }

    /// Fire a raw HTTP request and read the response bytes.
    async fn http_request_bytes(port: u16, request: &str) -> Vec<u8> {
        use tokio::io::AsyncWriteExt;
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
        )
        .await;
        response
    }

    /// Fire a raw HTTP request and read the response. Small helper
    /// because the /api/peers tests all make a handful of these.
    async fn http_request(port: u16, request: &str) -> String {
        String::from_utf8_lossy(&http_request_bytes(port, request).await).into_owned()
    }

    #[tokio::test]
    async fn test_api_dashboard_targets_lists_local_root_target() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let resp = http_request(
            port,
            "GET /api/dashboard/targets HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let (head, body) = resp
            .split_once("\r\n\r\n")
            .expect("HTTP response has header/body split");
        assert!(
            head.starts_with("HTTP/1.1 200 OK\r\n"),
            "dashboard targets should return 200: {head}"
        );
        assert!(
            head.contains("Content-Type: application/json"),
            "dashboard targets should return JSON: {head}"
        );

        let payload: serde_json::Value =
            serde_json::from_str(body).expect("dashboard targets body is JSON");
        let targets = payload["targets"].as_array().expect("targets array");
        assert_eq!(targets.len(), 1, "test gateway has only the local target");
        let local = &targets[0];
        assert_eq!(local["local"], true);
        assert_eq!(local["access_domain"], "user_client");
        assert_eq!(local["route"], "current_dashboard");
        assert_eq!(local["effective_role"], "root");
        assert_eq!(local["connected"], true);

        handle.abort();
    }


    #[tokio::test]
    async fn test_api_origin_gate_refuses_foreign_pages() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        // Foreign origin on a fleet path: refused (not in the allowlist).
        let resp = http_request(
            port,
            "GET /api/access/overview HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.example\r\n\r\n",
        )
        .await;
        assert!(
            resp.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "foreign origin should be refused on fleet APIs: {}",
            resp.lines().next().unwrap_or("")
        );
        // Foreign origin on a non-fleet API: also refused.
        let resp = http_request(
            port,
            "GET /api/dashboard/targets HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.example\r\n\r\n",
        )
        .await;
        assert!(
            resp.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "foreign origin should be refused on non-fleet APIs: {}",
            resp.lines().next().unwrap_or("")
        );
        // The daemon's own origin sails through and is echoed on fleet paths.
        let resp = http_request(
            port,
            "GET /api/access/overview HTTP/1.1\r\nHost: localhost\r\nOrigin: http://localhost\r\n\r\n",
        )
        .await;
        assert!(
            resp.starts_with("HTTP/1.1 200 OK\r\n"),
            "own origin should be allowed: {}",
            resp.lines().next().unwrap_or("")
        );
        assert!(
            !resp.contains("Access-Control-Allow-Origin: *"),
            "fleet APIs must never be wildcard-readable"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn test_api_access_overview_lists_current_browser_root_grant() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let resp = http_request(
            port,
            "GET /api/access/overview HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let (head, body) = resp
            .split_once("\r\n\r\n")
            .expect("HTTP response has header/body split");
        assert!(
            head.starts_with("HTTP/1.1 200 OK\r\n"),
            "access overview should return 200: {head}"
        );
        assert!(
            head.contains("Content-Type: application/json"),
            "access overview should return JSON: {head}"
        );

        let payload: serde_json::Value =
            serde_json::from_str(body).expect("access overview body is JSON");
        assert_eq!(payload["schema_version"], 1);
        assert_eq!(payload["scope"]["kind"], "local_daemon");
        assert_eq!(payload["targets"].as_array().expect("targets").len(), 1);

        // Trusted local requests resolve through the shared IAM evaluator to
        // an actual root-session principal; the overview must report that
        // real subject rather than a synthetic "current browser" placeholder.
        let principals = payload["principals"].as_array().expect("principals");
        assert!(
            principals
                .iter()
                .any(|p| p["id"] == "principal:root:dashboard" && p["kind"] == "root_session"),
            "current root session principal should be present: {principals:?}"
        );
        let grants = payload["grants"].as_array().expect("grants");
        assert!(
            grants
                .iter()
                .any(|grant| grant["kind"] == "user_client_root"
                    && grant["role"] == "root"
                    && grant["policy_id"] == "policy:root"),
            "current browser root grant should be present"
        );
        let policies = payload["policies"].as_array().expect("policies");
        assert!(
            policies
                .iter()
                .any(|policy| policy["id"] == "policy:peer-profile"
                    && policy["status"] == "enforced"),
            "peer profile policy should be visible in the overview"
        );
        let permissions = payload["permissions"].as_array().expect("permissions");
        for expected in [
            "access.inspect",
            "access.manage",
            "peer.inspect",
            "peer.manage",
        ] {
            assert!(
                permissions
                    .iter()
                    .any(|permission| permission["id"].as_str() == Some(expected)),
                "{expected} permission should be visible in the overview"
            );
        }
        assert_eq!(
            payload["iam"]["capabilities"]["enforce_user_client_grants"],
            true
        );
        assert_eq!(
            payload["iam"]["capabilities"]["enforce_root_and_peer_grants"],
            true
        );
        assert_eq!(
            payload["iam"]["enforcement"]["principal_binding"],
            "root_peer_and_local_user_client"
        );

        handle.abort();
    }


    /// End-to-end exercise of the static-asset arms through a real
    /// gateway socket: exact-path routing (the `/api/...?path=<asset>`
    /// shadowing regression), conditional requests, gzip negotiation,
    /// the `?v=` cache policy, and HEAD.
    #[tokio::test]
    async fn test_static_asset_serving_end_to_end() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;

        // Exact path serves the wasm with ETag + CORS + revalidation
        // caching (no current `?v=` buster on the request).
        let resp = http_request_bytes(
            port,
            "GET /wasm-station/station_web_bg.wasm HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "got: {head}");
        assert!(head.contains("Content-Type: application/wasm\r\n"));
        assert!(head.contains("Access-Control-Allow-Origin: *\r\n"));
        assert!(head.contains("Cache-Control: no-cache, must-revalidate\r\n"));
        assert_eq!(&resp[split..], WASM_STATION_BIN);
        let etag_line = head
            .lines()
            .find(|l| l.starts_with("ETag: "))
            .expect("ETag header on asset response")
            .to_string();

        // The same asset path mentioned inside an API query parameter is
        // no longer shadowed by the asset arm: /api/fs/stat answers JSON.
        let resp = http_request(
            port,
            "GET /api/fs/stat?path=/wasm-station/station_web_bg.wasm HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let head = resp.split("\r\n\r\n").next().unwrap_or("");
        assert!(
            head.contains("Content-Type: application/json"),
            "fs API must answer JSON, not the wasm asset; got: {head}"
        );

        // Conditional revalidation: matching If-None-Match → 304, no body.
        let etag = etag_line.trim_start_matches("ETag: ").trim();
        let req = format!(
            "GET /wasm-station/station_web_bg.wasm HTTP/1.1\r\nHost: localhost\r\nIf-None-Match: {etag}\r\n\r\n"
        );
        let resp = http_request_bytes(port, &req).await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();
        assert!(
            head.starts_with("HTTP/1.1 304 Not Modified\r\n"),
            "got: {head}"
        );
        assert!(head.contains(&etag_line));
        assert!(head.contains("Access-Control-Allow-Origin: *\r\n"));
        assert_eq!(resp.len(), split, "304 must carry no body");

        // Current-version buster + gzip: immutable caching and a gzip
        // body that round-trips to the original bytes.
        let req = format!(
            "GET /wasm-station/station_web_bg.wasm?v={} HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip, deflate\r\n\r\n",
            asset_version()
        );
        let resp = http_request_bytes(port, &req).await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();
        assert!(head.contains("Cache-Control: public, max-age=31536000, immutable\r\n"));
        assert!(head.contains("Content-Encoding: gzip\r\n"));
        assert!(head.contains("Vary: Accept-Encoding\r\n"));
        use std::io::Read as _;
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(&resp[split..])
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, WASM_STATION_BIN);

        // HEAD: status + headers only.
        let resp = http_request_bytes(
            port,
            "HEAD /wasm-station/station_web_bg.wasm HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "got: {head}");
        assert_eq!(resp.len(), split, "HEAD must carry no body");

        handle.abort();
    }

    #[tokio::test]
    async fn test_dashboard_fs_read_serves_file_bytes() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("download.txt");
        std::fs::write(&file, b"download through dashboard").unwrap();
        let req = format!(
            "GET /api/fs/read?path={} HTTP/1.1\r\nHost: localhost\r\n\r\n",
            file.display()
        );

        let resp = http_request_bytes(port, &req).await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();

        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "got: {head}");
        assert!(
            head.contains("Content-Type: text/plain; charset=utf-8\r\n"),
            "got: {head}"
        );
        assert!(head.contains("Accept-Ranges: bytes\r\n"), "got: {head}");
        assert!(
            head.contains("Content-Disposition: attachment; filename=\"download.txt\"\r\n"),
            "got: {head}"
        );
        assert_eq!(&resp[split..], b"download through dashboard");

        handle.abort();
    }

    #[tokio::test]
    async fn test_dashboard_fs_read_serves_byte_ranges() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("download.txt");
        std::fs::write(&file, b"download through dashboard").unwrap();
        let req = format!(
            "GET /api/fs/read?path={} HTTP/1.1\r\nHost: localhost\r\nRange: bytes=9-15\r\n\r\n",
            file.display()
        );

        let resp = http_request_bytes(port, &req).await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();

        assert!(
            head.starts_with("HTTP/1.1 206 Partial Content\r\n"),
            "got: {head}"
        );
        assert!(
            head.contains("Content-Range: bytes 9-15/26\r\n"),
            "got: {head}"
        );
        assert!(head.contains("Accept-Ranges: bytes\r\n"), "got: {head}");
        assert_eq!(&resp[split..], b"through");

        handle.abort();
    }

    #[tokio::test]
    async fn test_dashboard_fs_read_rejects_unsatisfiable_byte_range() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("download.txt");
        std::fs::write(&file, b"download").unwrap();
        let req = format!(
            "GET /api/fs/read?path={} HTTP/1.1\r\nHost: localhost\r\nRange: bytes=99-100\r\n\r\n",
            file.display()
        );

        let resp = http_request_bytes(port, &req).await;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&resp[..split]).into_owned();
        let body = String::from_utf8_lossy(&resp[split..]).into_owned();

        assert!(
            head.starts_with("HTTP/1.1 416 Range Not Satisfiable\r\n"),
            "got: {head}"
        );
        assert!(head.contains("Content-Range: bytes */8\r\n"), "got: {head}");
        assert!(body.contains("range is not satisfiable"), "body: {body}");

        handle.abort();
    }

    /// Same as `spawn_test_gateway_with_registry` but also wires an
    /// inbound bearer token. Used by the federation auth tests.
    async fn spawn_test_gateway_with_auth(
        peer_registry: Option<crate::peer::PeerRegistry>,
        bearer_token: Option<String>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            peer_registry,
            Vec::new(),
            bearer_token,
            crate::peer::AuthRequirements::none(),
            false,
            None,
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        (port, handle)
    }

    /// Spawn a gateway with a self-signed TLS acceptor wired in (strict
    /// HTTPS/WSS mode) plus an optional inbound bearer token. Used by the
    /// strict-TLS demux tests and the TLS variant of the /ws bearer test
    /// (audit F2), which only manifests over TLS — rustls buffers the
    /// response ciphertext, so a missing flush truncates it to empty.
    async fn spawn_test_gateway_tls(
        bearer_token: Option<String>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        spawn_test_gateway_tls_with_client_cert_requirement(bearer_token, false).await
    }

    async fn spawn_test_gateway_tls_with_client_cert_requirement(
        bearer_token: Option<String>,
        tls_client_cert_required: bool,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let bus = EventBus::new();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Self-signed cert with localhost / 127.0.0.1 in the SAN list, the
        // same construction the production `--tls` self-signed path uses.
        let acceptor = crate::web_tls::build_acceptor(&crate::web_tls::TlsCertSource::SelfSigned {
            bind_ip: Some("127.0.0.1".parse().unwrap()),
            hostname: None,
        })
        .expect("self-signed acceptor builds");
        let handle = spawn_web_gateway(
            listener,
            bus,
            broadcast_tx,
            WebGatewayConfig::default(),
            ActiveSessionState::empty(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            bearer_token,
            crate::peer::AuthRequirements::none(),
            tls_client_cert_required,
            Some(acceptor),
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        (port, handle)
    }

    /// Test-only `ServerCertVerifier` that accepts any certificate. The
    /// gateway serves a self-signed cert with no chain to a trust anchor,
    /// so a real verifier would reject it; tests only care that the bytes
    /// flow over an encrypted channel, not that the cert is trusted.
    /// Signature verification still delegates to the ring provider so the
    /// handshake's signed-transcript check is genuine.
    #[derive(Debug)]
    struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

    impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    /// Fire a raw request over a TLS client connection to a `--tls` gateway
    /// and read the full decrypted response. The TLS analogue of
    /// `http_request`: connects with `AcceptAnyCert` (the gateway's cert is
    /// self-signed), writes the request as cleartext into the TLS session,
    /// and reads to EOF. Returns the decrypted bytes as a lossy string.
    async fn https_request(port: u16, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let tcp = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let mut tls = connector.connect(server_name, tcp).await.unwrap();
        tls.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            tls.read_to_end(&mut response),
        )
        .await;
        String::from_utf8_lossy(&response).into_owned()
    }

    // -----------------------------------------------------------------
    // verify_bearer_token + is_federation_path unit tests
    // -----------------------------------------------------------------

    #[test]
    fn verify_bearer_token_passes_when_no_token_configured() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(verify_bearer_token(header, None).is_ok());
    }

    #[test]
    fn verify_bearer_token_rejects_missing_header_when_required() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_token(header, Some("expected-token")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("missing Authorization"));
    }

    #[test]
    fn verify_bearer_token_rejects_wrong_token() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n";
        let err = verify_bearer_token(header, Some("expected-token")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("invalid bearer"));
    }

    #[test]
    fn verify_bearer_token_accepts_correct_token() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_header_name_case_insensitive() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nauthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_scheme_case_insensitive() {
        let header = "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: bearer right\r\n\r\n";
        assert!(verify_bearer_token(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_token_rejects_non_bearer_scheme() {
        let header =
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Basic Zm9vOmJhcg==\r\n\r\n";
        let err = verify_bearer_token(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
        assert!(err.1.contains("Bearer scheme"));
    }

    #[test]
    fn is_federation_path_recognizes_federation_endpoints() {
        assert!(is_federation_path("GET /api/peers HTTP/1.1"));
        assert!(is_federation_path("POST /api/peers HTTP/1.1"));
        assert!(is_federation_path("DELETE /api/peers HTTP/1.1"));
        assert!(is_federation_path("GET /api/peers/eligible HTTP/1.1"));
        assert!(is_federation_path(
            "POST /api/peers/intendant:foo/message HTTP/1.1"
        ));
        assert!(is_federation_path("POST /api/coordinator/route HTTP/1.1"));
        assert!(is_federation_path("GET /api/sessions HTTP/1.1"));
    }

    #[test]
    fn is_federation_path_excludes_unauthenticated_endpoints() {
        // Discovery, dashboard bootstrap, and `/ws` must NOT be
        // mistaken for federation paths — they're intentionally
        // exempt from bearer enforcement.
        assert!(!is_federation_path(
            "GET /.well-known/agent-card.json HTTP/1.1"
        ));
        assert!(!is_federation_path("GET /config HTTP/1.1"));
        assert!(!is_federation_path("GET / HTTP/1.1"));
        assert!(!is_federation_path("GET /static/app.js HTTP/1.1"));
        assert!(!is_federation_path(
            "GET /ws HTTP/1.1\r\nUpgrade: websocket"
        ));
        assert!(!is_federation_path("GET /api/settings HTTP/1.1"));
        assert!(!is_federation_path("POST /api/api-keys HTTP/1.1"));
    }


    #[test]
    fn public_connect_bootstrap_path_is_narrow() {
        assert!(is_public_connect_bootstrap_path(
            "GET /connect/bootstrap HTTP/1.1"
        ));
        assert!(is_public_connect_bootstrap_path(
            "GET /connect/status?poll=1 HTTP/1.1"
        ));
        assert!(is_public_connect_bootstrap_path(
            "POST /connect/dashboard/offer HTTP/1.1"
        ));
        assert!(is_public_connect_bootstrap_path(
            "POST /connect/dashboard/ice HTTP/1.1"
        ));
        assert!(is_public_connect_bootstrap_path(
            "POST /connect/dashboard/close HTTP/1.1"
        ));

        assert!(!is_public_connect_bootstrap_path("GET / HTTP/1.1"));
        assert!(!is_public_connect_bootstrap_path("GET /config HTTP/1.1"));
        assert!(!is_public_connect_bootstrap_path(
            "GET /connect/dashboard HTTP/1.1"
        ));
        assert!(!is_public_connect_bootstrap_path(
            "GET /connect/dashboard/offers HTTP/1.1"
        ));
        assert!(!is_public_connect_bootstrap_path(
            "POST /api/peers HTTP/1.1"
        ));
    }

    #[test]
    fn connect_bootstrap_html_exposes_debug_api() {
        let html = connect_bootstrap_html();
        assert!(html.contains("Intendant Connect Bootstrap"));
        assert!(html.contains("window.intendantConnectDashboard"));
        assert!(html.contains("/connect/dashboard/offer"));
        assert!(html.contains("intendant-dashboard-control-v1"));
    }


    #[test]
    fn dashboard_http_operation_maps_access_and_dashboard_routes() {
        use crate::peer::access_policy::PeerOperation;

        assert_eq!(
            dashboard_http_operation("GET", "/api/access/overview"),
            Some(PeerOperation::AccessInspect)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/access/iam/grants/update"),
            Some(PeerOperation::AccessManage)
        );
        assert_eq!(
            dashboard_http_operation("GET", "/api/dashboard/targets"),
            Some(PeerOperation::AccessInspect)
        );
        assert_eq!(
            dashboard_http_operation("GET", "/api/session/current/uploads"),
            Some(PeerOperation::SessionManage)
        );
        assert_eq!(
            dashboard_http_operation("GET", "/api/fs/read"),
            Some(PeerOperation::FilesystemRead)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/fs/write"),
            Some(PeerOperation::FilesystemWrite)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/fs/rename"),
            Some(PeerOperation::FilesystemWrite)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/fs/delete"),
            Some(PeerOperation::FilesystemWrite)
        );
        // GET must not inherit the write classification, and look-alike
        // paths must not classify at all.
        assert_eq!(dashboard_http_operation("GET", "/api/fs/write"), None);
        assert_eq!(dashboard_http_operation("GET", "/api/fs/rename"), None);
        assert_eq!(dashboard_http_operation("GET", "/api/fs/delete"), None);
        assert_eq!(dashboard_http_operation("POST", "/api/fs/writeable"), None);
        assert_eq!(dashboard_http_operation("POST", "/api/fs/deleted"), None);
        // Historically unclassified (browsers ungated); the table row
        // delegates to federation_http_operation, closing the gap the
        // federation bearer gate already closed for peers.
        assert_eq!(
            dashboard_http_operation("POST", "/api/coordinator/route"),
            Some(PeerOperation::Task)
        );
        assert_eq!(dashboard_http_operation("GET", "/config"), None);
        // The prefix families use the same boundary rule as dispatch:
        // exact or a real `/` segment — dispatch's look-alike non-routes
        // must be non-routes for the classifier too.
        assert_eq!(
            dashboard_http_operation("GET", "/api/sessions"),
            Some(PeerOperation::SessionInspect)
        );
        assert_eq!(dashboard_http_operation("GET", "/api/sessionsfoo"), None);
        assert_eq!(
            dashboard_http_operation("POST", "/api/worktrees/inspect"),
            Some(PeerOperation::SessionInspect)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/worktrees/inspect-old"),
            None
        );
        assert_eq!(dashboard_http_operation("GET", "/api/peersfoo"), None);
        assert_eq!(
            dashboard_http_operation("GET", "/api/session/current/changes/src/main.rs"),
            Some(PeerOperation::SessionManage)
        );
        // Methods a route does not declare are not routes and carry no
        // operation (the retired hand classifier used to gate some of
        // these method-blind; dispatch never served them).
        assert_eq!(dashboard_http_operation("GET", "/api/worktrees/inspect"), None);
        assert_eq!(
            dashboard_http_operation("PUT", "/api/session/current/history"),
            None
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/managed-context/anchors"),
            None
        );
        // Deliberately public routes classify as no operation: the
        // payload's own signature/shape is the authority.
        assert_eq!(
            dashboard_http_operation("POST", "/api/peer-pairing/requests"),
            None
        );
        assert_eq!(
            dashboard_http_operation("GET", "/api/peer-pairing/requests/req1"),
            None
        );
        assert_eq!(dashboard_http_operation("POST", "/api/access/org-grants"), None);
        assert_eq!(
            dashboard_http_operation("POST", "/api/access/orgs/revocations/apply"),
            None
        );
        // The federation surface delegates to federation_http_operation —
        // the same ladder the federation bearer gate enforces.
        assert_eq!(
            dashboard_http_operation("GET", "/api/peers"),
            Some(PeerOperation::PeerInspect)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/peers/p1/message"),
            Some(PeerOperation::PeerUse)
        );
        assert_eq!(
            dashboard_http_operation("POST", "/api/peers/pairing/invite"),
            Some(PeerOperation::AccessManage)
        );
        // /mcp is token-bound inside the handler, not operation-gated.
        assert_eq!(dashboard_http_operation("POST", "/mcp"), None);
        // Method tightening (phase 4d) superseded the Any-era gate on
        // DELETE /api/settings: the method matches no row, so it never
        // classifies — and never reaches a handler; dispatch answers the
        // miss with 405 + the Allow union derived from the table.
        assert_eq!(dashboard_http_operation("DELETE", "/api/settings"), None);
        assert_eq!(
            crate::gateway_routes::allowed_methods_for_path("/api/settings").as_deref(),
            Some("GET, POST, OPTIONS")
        );
    }

    /// Routes served by the legacy dispatch chain (the non-API surface:
    /// connect bootstrap, recordings, frames, debug, config, static/SPA)
    /// must never match the table — a route is served by exactly one of
    /// the two.
    #[test]
    fn route_table_does_not_shadow_legacy_chain_routes() {
        let legacy_served: &[(&str, &str)] = &[
            ("GET", "/connect/bootstrap"),
            ("GET", "/connect/status"),
            ("POST", "/connect/dashboard/offer"),
            ("POST", "/connect/dashboard/ice"),
            ("POST", "/connect/dashboard/close"),
            ("GET", "/frames/f1"),
            ("POST", "/session"),
            ("GET", "/recordings"),
            ("GET", "/recordings/stream1/meta"),
            ("GET", "/debug"),
            ("GET", "/config"),
            ("GET", "/.well-known/agent-card.json"),
            ("GET", "/index.html"),
            ("GET", "/"),
        ];
        for (method, path) in legacy_served {
            assert!(
                crate::gateway_routes::match_route(method, path).is_none(),
                "{method} {path} is still served by the legacy chain but \
                 matches the route table — port the family (removing its \
                 chain arm) before declaring it, then move it out of this \
                 list",
            );
        }
    }


    // -----------------------------------------------------------------
    // End-to-end: federation REST auth enforcement
    // -----------------------------------------------------------------

    /// With `inbound_bearer_token` configured, a federation request
    /// without an Authorization header is rejected 401.
    #[tokio::test]
    async fn test_federation_endpoint_rejects_missing_bearer() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        // Request without auth — should 401, NOT pass through to the
        // 503-no-registry response that would happen otherwise.
        let resp = http_request(port, "GET /api/peers HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(resp.contains("missing Authorization"));
        assert!(
            resp.contains("WWW-Authenticate: Bearer"),
            "WWW-Authenticate header signals the auth scheme"
        );
        handle.abort();
    }

    /// Wrong bearer token → 401 with "invalid bearer token".
    #[tokio::test]
    async fn test_federation_endpoint_rejects_wrong_bearer() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(
            port,
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n",
        )
        .await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(resp.contains("invalid bearer"));
        handle.abort();
    }

    /// Correct bearer token → request flows through to the normal
    /// handler (which then returns 503 because no registry was
    /// configured — proves auth passed and dispatch ran).
    #[tokio::test]
    async fn test_federation_endpoint_accepts_correct_bearer() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(
            port,
            "GET /api/peers HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer test-token\r\n\r\n",
        )
        .await;
        // Auth passed; handler returned its 503 (no registry).
        assert!(
            resp.contains("503"),
            "expected 503 (auth passed, registry missing), got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    /// /config is exempt — even when bearer is required for
    /// federation endpoints, the dashboard bootstrap continues to work
    /// without auth. This is how the dashboard remains usable on the
    /// loopback / trusted-network case where the operator has set a
    /// bearer for WAN federation.
    #[tokio::test]
    async fn test_config_endpoint_unauthenticated_when_bearer_set() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(port, "GET /config HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(
            resp.contains("200 OK"),
            "config should serve unauthenticated, got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    #[tokio::test]
    async fn test_favicon_routes_serve_png() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;

        for path in ["/icon-128.png", "/favicon.ico"] {
            let request = format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n");
            let resp = http_request_bytes(port, &request).await;
            let response_str = String::from_utf8_lossy(&resp);
            assert!(
                response_str.starts_with("HTTP/1.1 200 OK"),
                "expected 200 for {path}, got: {response_str}"
            );
            assert!(
                response_str.contains("Content-Type: image/png"),
                "expected PNG content type for {path}, got: {response_str}"
            );
            assert!(
                !response_str.contains("<!DOCTYPE html>"),
                "{path} should not fall through to app HTML"
            );

            let body_offset = resp
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|idx| idx + 4)
                .expect("HTTP response should contain a body separator");
            assert!(
                resp[body_offset..].starts_with(b"\x89PNG\r\n\x1a\n"),
                "{path} should serve PNG bytes"
            );
        }

        handle.abort();
    }

    // -----------------------------------------------------------------
    // /ws bearer enforcement (slice 2d)
    // -----------------------------------------------------------------


    #[test]
    fn ws_frame_operation_mirrors_dashboard_control_tables() {
        use crate::peer::access_policy::PeerOperation;
        assert_eq!(
            ws_frame_operation("terminal_open"),
            Some(PeerOperation::TerminalView)
        );
        assert_eq!(
            ws_frame_operation("terminal_input"),
            Some(PeerOperation::TerminalWrite)
        );
        assert_eq!(
            ws_frame_operation("terminal_share"),
            Some(PeerOperation::TerminalWrite)
        );
        assert_eq!(
            ws_frame_operation("display_input"),
            Some(PeerOperation::DisplayInput)
        );
        assert_eq!(
            ws_frame_operation("set_diagnostics_visual_marker"),
            Some(PeerOperation::DisplayInput)
        );
        assert_eq!(
            ws_frame_operation("display_offer"),
            Some(PeerOperation::DisplayView)
        );
        assert_eq!(
            ws_frame_operation("key"),
            Some(PeerOperation::RuntimeControl)
        );
        assert_eq!(
            ws_frame_operation("term_subscribe"),
            Some(PeerOperation::RuntimeControl)
        );
        assert_eq!(
            ws_frame_operation("presence_connect"),
            Some(PeerOperation::RuntimeControl)
        );
        assert_eq!(
            ws_frame_operation("user_audio"),
            Some(PeerOperation::RuntimeControl)
        );
        assert_eq!(
            ws_frame_operation("tool_request"),
            Some(PeerOperation::Message)
        );
        assert_eq!(
            ws_frame_operation("async_query"),
            Some(PeerOperation::Message)
        );
        // Tunnel signaling stays open: the tunnel enforces the same grant
        // per-frame itself, and scoped clients must be able to establish it.
        assert_eq!(ws_frame_operation("dashboard_control_offer"), None);
        assert_eq!(ws_frame_operation("dashboard_control_ice"), None);
        assert_eq!(ws_frame_operation("dashboard_control_close"), None);
        assert_eq!(ws_frame_operation("ping"), None);
    }


    #[test]
    fn verify_bearer_for_ws_passes_when_no_token_configured() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n";
        assert!(verify_bearer_for_ws(header, None).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_accepts_authorization_header() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer right\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_accepts_token_query_param() {
        // The dashboard browser path: no Authorization header (browsers
        // can't easily set headers on WebSocket opens), token rides on
        // the URL.
        let header = "GET /ws?token=right HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    #[test]
    fn verify_bearer_for_ws_rejects_when_neither_present() {
        let header = "GET /ws HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_for_ws(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
    }

    #[test]
    fn verify_bearer_for_ws_rejects_wrong_query_token() {
        let header = "GET /ws?token=wrong HTTP/1.1\r\nHost: x\r\n\r\n";
        let err = verify_bearer_for_ws(header, Some("right")).unwrap_err();
        assert_eq!(err.0, 401);
    }

    /// Header AND query both present — header wins (matches first).
    /// Mismatched header with matching query: header check fails, query
    /// check passes, overall accepted. Documents the fallback behavior.
    #[test]
    fn verify_bearer_for_ws_header_wrong_falls_back_to_query() {
        let header = "GET /ws?token=right HTTP/1.1\r\n\
                      Host: x\r\n\
                      Authorization: Bearer wrong\r\n\r\n";
        assert!(verify_bearer_for_ws(header, Some("right")).is_ok());
    }

    /// Real /ws upgrade through `spawn_test_gateway_with_auth`:
    /// connecting without a token gets a plain HTTP 401 *before* the
    /// WebSocket handshake completes — the dashboard sees a 401 page,
    /// not a successful upgrade then immediate close.
    #[tokio::test]
    async fn test_ws_upgrade_rejects_missing_bearer() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("ws-token".into())).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(resp.contains("401"), "expected 401, got: {resp}");
        assert!(
            resp.contains("WWW-Authenticate: Bearer"),
            "WWW-Authenticate signals scheme"
        );
        // Critically, the upgrade did NOT complete.
        assert!(
            !resp.contains("101 Switching Protocols"),
            "must reject before WS handshake completes"
        );
        handle.abort();
    }

    /// The same /ws-without-token rejection, but over a real TLS connection
    /// (audit F2). The /ws bearer-reject arm writes the 401 then returns,
    /// dropping the stream; over TLS the 401's ciphertext can sit in the
    /// rustls session buffer and be discarded on an abortive close, so the
    /// client reads an *empty* response instead of the 401. The
    /// `finalize_http_stream` flush+shutdown on that arm closes the session
    /// cleanly (close_notify + FIN) so the response always lands. This test
    /// exercises the TLS path end to end; it's the cross-platform companion
    /// to the plain-TCP `test_ws_upgrade_rejects_missing_bearer` and is the
    /// regression net for the empty-response symptom on platforms whose
    /// socket close discards queued TX (e.g. Windows).
    #[tokio::test]
    async fn test_ws_upgrade_rejects_missing_bearer_over_tls() {
        let (port, handle) = spawn_test_gateway_tls(Some("ws-token".into())).await;
        let resp = https_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(
            !resp.is_empty(),
            "TLS 401 must not be truncated to empty (audit F2)"
        );
        assert!(resp.contains("401"), "expected 401 over TLS, got: {resp}");
        assert!(
            resp.contains("WWW-Authenticate: Bearer"),
            "WWW-Authenticate signals scheme"
        );
        assert!(
            !resp.contains("101 Switching Protocols"),
            "must reject before WS handshake completes"
        );
        handle.abort();
    }

    /// Strict TLS: with a TLS acceptor configured, a *cleartext* HTTP
    /// connection to the secure port is refused with a 426 Upgrade Required
    /// hint and closed — never served over plain HTTP (audit F3 "no
    /// unencrypted traffic"). Uses a plain `http_request` (no TLS) against
    /// the TLS gateway.
    #[tokio::test]
    async fn test_strict_tls_rejects_cleartext_http() {
        let (port, handle) = spawn_test_gateway_tls(None).await;
        let resp = http_request(port, "GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(
            resp.contains("426"),
            "cleartext HTTP to a --tls gateway must get 426, got: {resp}"
        );
        assert!(
            !resp.contains("200 OK"),
            "must not serve the dashboard over cleartext, got: {resp}"
        );
        handle.abort();
    }

    /// Strict TLS: a cleartext WebSocket upgrade to the secure port is
    /// likewise refused (426) and never upgraded — the WS-over-cleartext
    /// path is closed off the same way as plain HTTP.
    #[tokio::test]
    async fn test_strict_tls_rejects_cleartext_ws() {
        let (port, handle) = spawn_test_gateway_tls(None).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("426"),
            "cleartext WS to a --tls gateway must get 426, got: {resp}"
        );
        assert!(
            !resp.contains("101 Switching Protocols"),
            "must not upgrade a cleartext WS on the secure port, got: {resp}"
        );
        handle.abort();
    }

    /// Managed child agents connect to Intendant's Streamable HTTP MCP
    /// endpoint over loopback. That local control channel must continue to
    /// work when the operator enables TLS/mTLS for the dashboard; otherwise
    /// child CLIs cannot report tool calls or receive session-scoped MCP
    /// context. The exception is path- and peer-scoped to loopback `/mcp`.
    #[tokio::test]
    async fn test_strict_tls_allows_loopback_cleartext_mcp_when_mtls_required() {
        let (port, handle) = spawn_test_gateway_tls_with_client_cert_requirement(None, true).await;
        let token = loopback_mcp_auth_token();
        let resp = http_request(
            port,
            &format!(
                "POST /mcp?session_id=child&mcp_token={token} HTTP/1.1\r\n\
                 Host: localhost\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: 2\r\n\
                 \r\n\
                 {{}}"
            ),
        )
        .await;
        assert!(
            resp.contains("503 Service Unavailable"),
            "loopback cleartext /mcp should reach the MCP handler, got: {resp}"
        );
        assert!(
            !resp.contains("426"),
            "loopback cleartext /mcp must not be rejected by strict TLS, got: {resp}"
        );
        assert!(
            !resp.contains("mTLS client certificate required"),
            "loopback cleartext /mcp must not be rejected by dashboard mTLS, got: {resp}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn test_strict_tls_rejects_loopback_cleartext_mcp_without_token() {
        let (port, handle) = spawn_test_gateway_tls_with_client_cert_requirement(None, true).await;
        let resp = http_request(
            port,
            "POST /mcp?session_id=child HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Type: application/json\r\n\
             Content-Length: 2\r\n\
             \r\n\
             {}",
        )
        .await;
        assert!(
            resp.contains("426"),
            "loopback cleartext /mcp without token should stay on the strict TLS reject path, got: {resp}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn test_strict_tls_rejects_browser_origin_cleartext_mcp() {
        let (port, handle) = spawn_test_gateway_tls_with_client_cert_requirement(None, true).await;
        let token = loopback_mcp_auth_token();
        let resp = http_request(
            port,
            &format!(
                "POST /mcp?session_id=child&mcp_token={token} HTTP/1.1\r\n\
                 Host: localhost\r\n\
                 Origin: https://example.test\r\n\
                 Content-Type: text/plain\r\n\
                 Content-Length: 2\r\n\
                 \r\n\
                 {{}}"
            ),
        )
        .await;
        assert!(
            resp.contains("426"),
            "browser-origin cleartext /mcp should stay on the strict TLS reject path, got: {resp}"
        );
        assert!(
            !resp.contains("503 Service Unavailable"),
            "browser-origin cleartext /mcp must not reach the MCP handler, got: {resp}"
        );
        handle.abort();
    }

    /// Strict TLS sanity + truncation guard: a *real* TLS request to the
    /// secure port serves the full dashboard (the rejection above is
    /// specific to cleartext, not a blanket closure). The body-length
    /// assertion guards the audit-F2 truncation class: the ~871 KB
    /// `app.html` far exceeds one synchronous rustls record, so a missing
    /// `finalize_http_stream` flush+shutdown can drop the buffered tail and
    /// truncate the body. Whether that manifests is platform-dependent
    /// (Windows' abortive socket close discards queued TX; macOS loopback
    /// happens to drain it), so this is a cross-platform regression net —
    /// strongest on the Windows build. We assert the decrypted body length
    /// matches `Content-Length` and that the closing `</html>` survived.
    #[tokio::test]
    async fn test_strict_tls_serves_https() {
        let (port, handle) = spawn_test_gateway_tls(None).await;
        let resp = https_request(port, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(
            resp.contains("200 OK"),
            "HTTPS request to a --tls gateway should serve the dashboard, got first 200 bytes: {}",
            &resp.chars().take(200).collect::<String>()
        );
        // Body must arrive intact, not truncated mid-buffer (audit F2).
        let content_length: usize = resp
            .split("\r\n")
            .find_map(|line| {
                line.strip_prefix("Content-Length: ")
                    .and_then(|v| v.trim().parse().ok())
            })
            .expect("response carries a Content-Length");
        let body = resp
            .split_once("\r\n\r\n")
            .map(|(_, b)| b)
            .expect("response has a header/body separator");
        assert_eq!(
            body.len(),
            content_length,
            "TLS body truncated: got {} bytes, Content-Length promised {content_length}",
            body.len()
        );
        assert!(
            body.contains("</html>"),
            "TLS body must include the closing </html> (not cut off mid-record)"
        );
        handle.abort();
    }

    /// The dispatch chain routes on parsed `(method, path)`, matching the
    /// IAM/Origin gates. The old `request_line.contains(...)` dispatch let
    /// a request whose path merely EMBEDDED an API route reach its handler
    /// while the gates — which classify by parsed path — never saw it as
    /// that route (`POST /x/api/api-keys` reached the API-key writer).
    #[tokio::test]
    async fn dispatch_routes_on_parsed_paths_not_substrings() {
        let (port, handle) = spawn_test_gateway_with_auth(None, None).await;

        // Path embedding /api/api-keys must NOT reach the API-key writer;
        // it falls through to the SPA shell like any unknown route.
        let resp = http_request(
            port,
            "POST /x/api/api-keys HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert!(
            resp.contains("Content-Type: text/html"),
            "off-path api-keys request must fall through, got: {}",
            &resp.chars().take(120).collect::<String>()
        );

        // The exact route still reaches the writer (a JSON responder).
        let resp = http_request(
            port,
            "POST /api/api-keys HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert!(
            resp.contains("Content-Type: application/json"),
            "exact api-keys route must reach the writer, got: {}",
            &resp.chars().take(120).collect::<String>()
        );

        // A query string mentioning another route must not shadow the
        // requested one. /api/project-root dispatches well before /debug,
        // so under substring routing this query would have answered as
        // project-root; parsed routing serves the /debug state object.
        // (Both are cheap and machine-independent, unlike the session
        // list, which scans the real home directory.)
        let resp = http_request(
            port,
            "GET /debug?note=/api/project-root HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("\"agent_state\"") && !resp.contains("\"project_root\""),
            "query mention must not shadow the routed path, got: {}",
            &resp.chars().take(200).collect::<String>()
        );

        // Look-alike longer paths are not the route.
        let resp = http_request(port, "GET /api/sessionsfoo HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(
            resp.contains("Content-Type: text/html"),
            "look-alike path must fall through, got: {}",
            &resp.chars().take(120).collect::<String>()
        );

        // Per-file diff subpaths ARE the route (regression: the parsed-path
        // refactor briefly matched only the exact list endpoint, dropping
        // /api/session/current/changes/{path}).
        let resp = http_request(
            port,
            "GET /api/session/current/changes/src/main.rs HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(
            !resp.contains("Content-Type: text/html"),
            "per-file changes subpath must hit the changes handler, got: {}",
            &resp.chars().take(200).collect::<String>()
        );

        handle.abort();
    }

    /// /ws with a matching Authorization header completes the upgrade
    /// (101 Switching Protocols). This is the daemon-to-daemon path
    /// that IntendantWsTransport uses.
    #[tokio::test]
    async fn test_ws_upgrade_accepts_matching_authorization_header() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("ws-token".into())).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Authorization: Bearer ws-token\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("101 Switching Protocols"),
            "expected upgrade success, got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    /// /ws with `?token=` query parameter completes the upgrade. This
    /// is the dashboard-browser path (browsers can't set arbitrary
    /// headers on `WebSocket` opens, so the token rides on the URL).
    #[tokio::test]
    async fn test_ws_upgrade_accepts_matching_query_token() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("ws-token".into())).await;
        let resp = http_request(
            port,
            "GET /ws?token=ws-token HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("101 Switching Protocols"),
            "expected upgrade success, got: {resp}"
        );
        handle.abort();
    }

    /// /ws with no token still works when the gateway has no bearer
    /// configured (the common case for trusted-LAN deployments).
    #[tokio::test]
    async fn test_ws_upgrade_accepts_when_no_bearer_configured() {
        let (port, handle) = spawn_test_gateway_with_auth(None, None).await;
        let resp = http_request(
            port,
            "GET /ws HTTP/1.1\r\n\
             Host: x\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGVzdA==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("101 Switching Protocols"),
            "expected upgrade success, got: {resp}"
        );
        handle.abort();
    }

    /// /.well-known/agent-card.json is exempt — discovery must work
    /// before any auth handshake. Connecting peers fetch the card to
    /// see what auth they need to satisfy.
    #[tokio::test]
    async fn test_agent_card_endpoint_unauthenticated_when_bearer_set() {
        let (port, handle) = spawn_test_gateway_with_auth(None, Some("test-token".into())).await;
        let resp = http_request(
            port,
            "GET /.well-known/agent-card.json HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("200 OK"),
            "agent card should serve unauthenticated, got: {resp}"
        );
        assert!(!resp.contains("401"));
        handle.abort();
    }

    /// `GET /api/peers` returns 503 when the web gateway was spawned
    /// without a peer registry. This lets the dashboard distinguish
    /// "peers not configured" from "no peers yet" and render
    /// differently.
    #[tokio::test]
    async fn test_api_peers_returns_503_without_registry() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let resp = http_request(port, "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(resp.contains("503"), "expected 503, got: {resp}");
        assert!(resp.contains("peer registry not configured"));
        handle.abort();
    }


    #[test]
    fn test_persist_manual_peer_writes_outbound_peer_config() {
        let root = tempfile::TempDir::new().unwrap();
        let req = AddPeerRequest {
            card_url: "https://peer.example:8765/.well-known/agent-card.json".into(),
            label: Some("Ignored Raw Label".into()),
            persist: true,
            via_urls: vec!["wss://tailnet-peer.example:8765/ws".into()],
            bearer_token: Some("legacy-token".into()),
            pinned_fingerprints: vec![
                "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899".into(),
            ],
            browser_tcp_via_url: Some("wss://browser-peer.example:8765/ws".into()),
        };

        let path = persist_manual_peer(root.path(), &req, Some("Peer Display".into())).unwrap();

        assert_eq!(path, root.path().join("intendant.toml"));
        let project = crate::project::Project::from_root(root.path().to_path_buf()).unwrap();
        assert_eq!(project.config.peers.len(), 1);
        let peer = &project.config.peers[0];
        assert_eq!(peer.card_url, req.card_url);
        assert_eq!(peer.label.as_deref(), Some("Peer Display"));
        assert_eq!(peer.via_urls, req.via_urls);
        assert_eq!(peer.bearer_token, req.bearer_token);
        assert_eq!(peer.pinned_fingerprints, req.pinned_fingerprints);
        assert_eq!(peer.browser_tcp_via_url, req.browser_tcp_via_url);
        assert!(peer.client_cert.is_none());
        assert!(peer.client_key.is_none());
    }


    /// `GET /api/peers` on a registry with no peers returns
    /// `{"peers":[]}`. Baseline for the list endpoint shape.
    #[tokio::test]
    async fn test_api_peers_list_empty_registry() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;
        let resp = http_request(port, "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(resp.contains("200 OK"));
        // Split body from headers.
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert_eq!(body.trim(), r#"{"peers":[]}"#);
        handle.abort();
    }

    /// End-to-end: spawn a "target" gateway (gateway A) and a
    /// "dashboard" gateway (gateway B) with a peer registry. POST
    /// A's card URL to B's /api/peers. Assert the peer is added,
    /// GET /api/peers shows it, DELETE removes it. This exercises
    /// the full path from HTTP request through PeerRegistry,
    /// IntendantWsTransport, the Agent Card fetch, WebSocket
    /// connect, and event drain.
    #[tokio::test]
    async fn test_api_peers_add_list_remove_end_to_end() {
        // Gateway A: the target peer this dashboard will federate with.
        let (target_port, target_handle) = spawn_test_gateway_with_registry(None).await;

        // Gateway B: the dashboard, with its own peer registry.
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(64);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (dash_port, dash_handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        // POST A's Agent Card URL to B's /api/peers.
        let card_url = format!("http://127.0.0.1:{target_port}/.well-known/agent-card.json");
        let body = serde_json::json!({"card_url": card_url}).to_string();
        let req = format!(
            "POST /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "add failed: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        let peer_id = parsed["peer_id"]
            .as_str()
            .expect("peer_id missing")
            .to_string();
        assert!(peer_id.starts_with("intendant:"));

        // GET /api/peers should now show the added peer.
        let list_resp = http_request(
            dash_port,
            "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(list_resp.contains("200 OK"));
        let list_body = list_resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let list: serde_json::Value = serde_json::from_str(list_body).unwrap();
        let peers_arr = list["peers"].as_array().unwrap();
        assert_eq!(peers_arr.len(), 1);
        assert_eq!(peers_arr[0]["id"].as_str().unwrap(), peer_id);
        // The "id" field should match the peer_id returned from POST.
        // The "version" should be the local build's version.
        assert_eq!(
            peers_arr[0]["version"].as_str().unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        // The dashboard panel rebuild relies on `ws_url` being
        // present so the browser can open a secondary WASM
        // connection without re-fetching the card. Guard against
        // the field being dropped or renamed.
        let ws_url = peers_arr[0]["ws_url"]
            .as_str()
            .expect("ws_url field must be present in the API response");
        assert!(
            ws_url.starts_with("ws://") && ws_url.ends_with("/ws"),
            "ws_url should be a native Intendant WebSocket URL: {ws_url}"
        );
        // The dashboard renders capability badges from this list,
        // so it must be present and contain the always-on phase 1
        // capabilities the test peer advertises.
        let caps = peers_arr[0]["capabilities"]
            .as_array()
            .expect("capabilities must be a JSON array");
        assert!(!caps.is_empty(), "expected at least one capability");

        // DELETE /api/peers with the peer_id.
        let del_body = serde_json::json!({"peer_id": peer_id}).to_string();
        let del_req = format!(
            "DELETE /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            del_body.len(),
            del_body
        );
        let del_resp = http_request(dash_port, &del_req).await;
        assert!(del_resp.contains("200 OK"), "delete failed: {del_resp}");

        // GET should now be empty.
        let empty_resp = http_request(
            dash_port,
            "GET /api/peers HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        let empty_body = empty_resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert_eq!(empty_body.trim(), r#"{"peers":[]}"#);

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers` with an invalid body returns 400 with a
    /// diagnostic error message.
    #[tokio::test]
    async fn test_api_peers_post_invalid_body() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "not json";
        let req = format!(
            "POST /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        handle.abort();
    }

    /// `DELETE /api/peers` for an unknown peer id returns 404.
    #[tokio::test]
    async fn test_api_peers_delete_unknown_returns_404() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = r#"{"peer_id":"intendant:ghost"}"#;
        let req = format!(
            "DELETE /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        handle.abort();
    }

    // -----------------------------------------------------------------
    // Per-peer outbound op endpoints — `/api/peers/{id}/{op}`
    // -----------------------------------------------------------------

    /// Poll the registry until the peer transitions to
    /// `ConnectionState::Connected`, or `timeout` elapses. Returns
    /// whether the peer connected in time. Used by the routing tests
    /// below to avoid sending ops at a peer whose transport is still
    /// in handshake (which would bounce off as `NotConnected` → 502
    /// and obscure the actual code path under test).
    async fn wait_for_connected(
        registry: &crate::peer::PeerRegistry,
        peer_id: &crate::peer::PeerId,
        timeout: tokio::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if let Some(h) = registry.get(peer_id) {
                if h.is_connected() {
                    return true;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
        }
        false
    }

    /// Boilerplate: spawn target gateway A, register it as a peer on
    /// dashboard gateway B via HTTP, wait for the transport to connect,
    /// return everything the per-peer op tests need: the dashboard's
    /// port (where ops are POSTed) plus the peer id (the path
    /// parameter for every op endpoint) plus all four task handles to
    /// abort at end of test. Cuts ~30 lines of setup per test.
    async fn setup_peer_op_test() -> (
        u16,
        String,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        // Gateway A: the target peer this dashboard will federate with.
        let (target_port, target_handle) = spawn_test_gateway_with_registry(None).await;

        // Gateway B: the dashboard, with its own peer registry.
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(64);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let registry_for_wait = registry.clone();
        let (dash_port, dash_handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        // POST A's Agent Card URL to B's /api/peers.
        let card_url = format!("http://127.0.0.1:{target_port}/.well-known/agent-card.json");
        let body = serde_json::json!({"card_url": card_url}).to_string();
        let req = format!(
            "POST /api/peers HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "register failed: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        let peer_id = parsed["peer_id"].as_str().unwrap().to_string();

        // Wait for the IntendantWsTransport to finish its handshake so
        // the op ack distinguishes "handler+routing works" from
        // "transport not ready yet".
        let pid = crate::peer::PeerId(peer_id.clone());
        assert!(
            wait_for_connected(
                &registry_for_wait,
                &pid,
                tokio::time::Duration::from_secs(3),
            )
            .await,
            "peer never reached Connected"
        );

        (dash_port, peer_id, target_handle, dash_handle)
    }

    /// `POST /api/peers/{id}/message` with a `{text}` shorthand body
    /// returns 200 + a `message_id`. Verifies the path-parameter
    /// routing, the JSON shorthand parsing, and the dispatch into
    /// `PeerHandle::send_message`. The wire-level encoding (this
    /// becomes a `ControlMsg::FollowUp` over the WebSocket) is covered
    /// by `peer::transport::intendant::tests::send_message_writes_followup_control_msg`.
    #[tokio::test]
    async fn test_api_peers_send_message_text_shorthand_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({"text": "hello peer"}).to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "send_message failed: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert!(
            parsed["message_id"].as_str().is_some(),
            "expected message_id in response: {resp_body}"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// Peer ids contain `:` and browsers commonly percent-encode
    /// path segments (`intendant:e2e` -> `intendant%3Ae2e`). The
    /// shared `/api/peers/{id}/{op}` route must decode the id before
    /// looking it up in the registry.
    #[tokio::test]
    async fn test_api_peers_encoded_peer_id_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let encoded_peer_id = peer_id.replace(':', "%3A");
        let body = serde_json::json!({"text": "hello encoded peer"}).to_string();
        let req = format!(
            "POST /api/peers/{encoded_peer_id}/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "encoded peer id failed: {resp}");

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{id}/message` with a full `{role, content,
    /// session}` body works the same. Verifies the full-control shape
    /// path through `SendMessageRequest::into_message` (where `content`
    /// wins over `text` when both are present).
    #[tokio::test]
    async fn test_api_peers_send_message_full_shape_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "role": "user",
            "content": {"type": "text", "text": "hello"},
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "send_message failed: {resp}");

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{id}/task` with `{instructions}` returns 200 +
    /// `task_id`. Wire-level encoding covered by
    /// `peer::transport::intendant::tests::delegate_task_writes_start_task_control_msg`.
    #[tokio::test]
    async fn test_api_peers_delegate_task_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "instructions": "do the thing",
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/task HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "delegate_task failed: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert!(
            parsed["task_id"].as_str().is_some(),
            "expected task_id in response: {resp_body}"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{id}/approval` with `{request_id, decision}`
    /// returns 200. Wire-level encoding covered by
    /// `peer::transport::intendant::tests::resolve_approval_maps_each_decision_to_its_control_msg`.
    #[tokio::test]
    async fn test_api_peers_resolve_approval_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "request_id": "42",
            "decision": "accept",
        })
        .to_string();
        let req = format!(
            "POST /api/peers/{peer_id}/approval HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "resolve_approval failed: {resp}");

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/peers/{unknown}/message` returns 404 with a
    /// diagnostic body. Doesn't require setup — exercises only the
    /// peer lookup path before any transport interaction.
    #[tokio::test]
    async fn test_api_peers_op_unknown_peer_returns_404() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = serde_json::json!({"text": "hi"}).to_string();
        let req = format!(
            "POST /api/peers/intendant:ghost/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("intendant:ghost"),
            "404 body should mention the missing id: {resp_body}"
        );
        handle.abort();
    }

    /// `POST /api/peers/{id}/message` with malformed JSON returns 400.
    #[tokio::test]
    async fn test_api_peers_send_message_invalid_body_returns_400() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "not json";
        let req = format!(
            "POST /api/peers/intendant:any/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        handle.abort();
    }

    /// `POST /api/peers/{id}/message` with neither `text` nor
    /// `content` returns 400. Verifies the `into_message` validation
    /// rejects empty bodies before the peer lookup runs.
    #[tokio::test]
    async fn test_api_peers_send_message_requires_text_or_content() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = serde_json::json!({"session": "scratch"}).to_string();
        let req = format!(
            "POST /api/peers/intendant:any/message HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("text") && resp_body.contains("content"),
            "error body should mention the missing fields: {resp_body}"
        );
        handle.abort();
    }

    /// Unknown sub-op (e.g. `/api/peers/{id}/bogus`) returns 404 with
    /// a diagnostic body. Guards the dispatch arm that distinguishes
    /// "supported op" from "unrecognized verb".
    #[tokio::test]
    async fn test_api_peers_unknown_op_returns_404() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "{}";
        let req = format!(
            "POST /api/peers/intendant:any/bogus HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("bogus"),
            "404 body should name the unknown op: {resp_body}"
        );
        handle.abort();
    }

    // -----------------------------------------------------------------
    // Coordinator endpoints — capability discovery + delegation
    // -----------------------------------------------------------------

    /// `GET /api/peers/eligible` returns 503 with no registry,
    /// matching the rest of /api/peers.
    #[tokio::test]
    async fn test_api_peers_eligible_returns_503_without_registry() {
        let (port, handle) = spawn_test_gateway_with_registry(None).await;
        let resp = http_request(
            port,
            "GET /api/peers/eligible?capability=display HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("503"), "expected 503, got: {resp}");
        handle.abort();
    }

    /// Missing `?capability=...` query param returns 400 with a
    /// hint that at least one is required.
    #[tokio::test]
    async fn test_api_peers_eligible_requires_capability_param() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let resp = http_request(
            port,
            "GET /api/peers/eligible HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("capability"),
            "400 body should mention capability: {resp_body}"
        );
        handle.abort();
    }

    /// Unknown capability strings return 400 with the offending
    /// values surfaced (not silently dropped, which would let an
    /// /api/peers/eligible?capability=typo through and return all
    /// peers).
    #[tokio::test]
    async fn test_api_peers_eligible_rejects_unknown_capability() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let resp = http_request(
            port,
            "GET /api/peers/eligible?capability=display&capability=nope HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("nope"),
            "400 body should name the unknown capability: {resp_body}"
        );
        handle.abort();
    }

    /// With one connected peer that advertises both ComputerUse and
    /// Knowledge (the test fixture's defaults), `?capability=computer-use`
    /// returns the peer; `?capability=display` returns an empty list
    /// (the fixture doesn't advertise Display).
    #[tokio::test]
    async fn test_api_peers_eligible_returns_matching_peers() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        // Hits: the test peer's card advertises ComputerUse.
        let resp = http_request(
            dash_port,
            "GET /api/peers/eligible?capability=computer-use HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        let peers = parsed["peers"].as_array().expect("peers array");
        assert_eq!(peers.len(), 1, "expected one matching peer");
        assert_eq!(peers[0]["id"].as_str().unwrap(), peer_id);

        // Misses: the fixture doesn't advertise Voice (build_local_agent_card
        // advertises ComputerUse + Knowledge + Display; Voice / Phone /
        // Recording are gated on runtime configuration that isn't plumbed
        // through yet).
        let resp = http_request(
            dash_port,
            "GET /api/peers/eligible?capability=voice HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["peers"].as_array().unwrap().len(), 0);

        target_handle.abort();
        dash_handle.abort();
    }

    /// `POST /api/coordinator/route` with required_capabilities the
    /// connected peer satisfies returns 200 + peer_id + task_id.
    /// Wire encoding to ControlMsg::StartTask is covered by
    /// peer::transport::intendant::tests.
    #[tokio::test]
    async fn test_api_coordinator_route_returns_200() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        let body = serde_json::json!({
            "required_capabilities": ["computer-use"],
            "task": {
                "instructions": "do the thing",
                "context": {"file": "src/main.rs"},
            },
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert_eq!(
            parsed["peer_id"].as_str().expect("peer_id present"),
            peer_id
        );
        assert!(
            parsed["task_id"].as_str().is_some(),
            "task_id should be present in response: {resp_body}"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// Routing a capability no connected peer satisfies returns 404
    /// with the considered peer ids surfaced for diagnostics.
    #[tokio::test]
    async fn test_api_coordinator_route_no_match_returns_404() {
        let (dash_port, peer_id, target_handle, dash_handle) = setup_peer_op_test().await;

        // Voice is the "gated, not-advertised-by-default" capability
        // that the stock build_local_agent_card fixture doesn't claim
        // — so routing by it hits no-route and surfaces the considered
        // list. Display moved to always-on in the 3a.1 fix, so it can
        // no longer serve as the deliberately-unsatisfied capability.
        let body = serde_json::json!({
            "required_capabilities": ["voice"],
            "task": {"instructions": "needs voice"},
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(dash_port, &req).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert_eq!(parsed["error"].as_str().unwrap(), "no route");
        let considered = parsed["considered"].as_array().expect("considered array");
        assert!(
            considered.iter().any(|v| v.as_str() == Some(&peer_id)),
            "considered list should include the peer that didn't match"
        );

        target_handle.abort();
        dash_handle.abort();
    }

    /// Bad JSON body returns 400.
    #[tokio::test]
    async fn test_api_coordinator_route_invalid_body_returns_400() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = "not json";
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        handle.abort();
    }

    /// Empty `required_capabilities` returns 400 — would otherwise
    /// match every peer and route to the first lexicographically,
    /// which is almost never what the caller meant.
    #[tokio::test]
    async fn test_api_coordinator_route_rejects_empty_capabilities() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let body = serde_json::json!({
            "required_capabilities": [],
            "task": {"instructions": "anything"},
        })
        .to_string();
        let req = format!(
            "POST /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let resp = http_request(port, &req).await;
        assert!(resp.contains("400"), "expected 400, got: {resp}");
        let resp_body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            resp_body.contains("required_capabilities"),
            "400 body should mention required_capabilities: {resp_body}"
        );
        handle.abort();
    }

    /// GET on the route endpoint returns 405 — only POST is allowed.
    #[tokio::test]
    async fn test_api_coordinator_route_get_returns_405() {
        let (log_tx, _log_rx) =
            tokio::sync::mpsc::channel::<crate::peer::event::TaggedPeerEvent>(16);
        let registry = crate::peer::PeerRegistry::new(log_tx);
        let (port, handle) = spawn_test_gateway_with_registry(Some(registry)).await;

        let resp = http_request(
            port,
            "GET /api/coordinator/route HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(resp.contains("405"), "expected 405, got: {resp}");
        handle.abort();
    }

    /// Insert a `DisplayInputHolder` directly into the map for tests
    /// that need to seed a holder without going through the full
    /// `apply_grant_input_authority` flow.  The inserted holder owns
    /// a fresh dummy `direct_tx` whose receiver is dropped — sends to
    /// it return `Err`, which the production code already tolerates
    /// (the WS-close path would have cleared this entry in real life).
    pub(crate) fn seed_holder(
        map: &Arc<StdRwLock<HashMap<u32, DisplayInputHolder>>>,
        display_id: u32,
        connection_id: &str,
    ) {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        map.write().unwrap_or_else(|e| e.into_inner()).insert(
            display_id,
            DisplayInputHolder::LocalWs {
                connection_id: connection_id.to_string(),
                direct_tx: tx,
            },
        );
    }


}
