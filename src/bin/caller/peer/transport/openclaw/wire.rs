//! Wire types for the OpenClaw Gateway protocol (v4) — the exact subset
//! slice 1 needs: frame envelopes, the `connect` handshake, structured
//! errors, and the message-relay RPC shapes (`chat.send`,
//! `sessions.list`) plus the `chat` / `session.message` event payloads.
//!
//! ## Machine contract
//!
//! Field names are pinned against the vendored machine contract in
//! `protocol.schema.json` (same directory): the generated schema shipped
//! in the `@openclaw/gateway-protocol` npm package.
//!
//! - Vendored version: **`@openclaw/gateway-protocol@2026.8.1-beta.2`**
//!   (fetched from unpkg 2026-08-19). The `@latest` dist-tag resolved to
//!   a `0.0.0` placeholder release that 404s on `protocol.schema.json`,
//!   so the snapshot comes from `@beta` per the package's rollout notes.
//! - The schema does **not** define the `connect.challenge` event
//!   payload or the structured `PAIRING_REQUIRED` error details; those
//!   shapes are pinned against the upstream sources instead
//!   (`src/gateway/server/ws-connection/ws-connection.ts` emits
//!   `{nonce, ts}`; `packages/gateway-protocol/src/connect-error-details.ts`
//!   defines `PairingConnectErrorDetails`).
//!
//! ## Envelope design: three structs + one internally-tagged demux enum
//!
//! The three frame kinds are separate structs because each direction
//! owns a different subset (we serialize `req`, deserialize `res` and
//! `event`), and [`Frame`] unifies them for inbound demux with a
//! `#[serde(other)] Unknown` fallback per the house wire-enum
//! convention (see `peer/card.rs`): a future frame kind must not kill
//! the connection parse loop. Method-specific payloads ride as raw
//! [`serde_json::Value`] in the envelope and are parsed in a second
//! stage by the typed structs below — the envelope stays stable while
//! the payload vocabulary grows.
//!
//! Receiving structs are permissive (unknown fields ignored, optional
//! fields defaulted) because the gateway evolves additively within a
//! wire version; sending structs emit exactly the schema's fields
//! because the gateway validates `connect` strictly
//! (`additionalProperties: false`).

// actor and mock-gateway seats landing concurrently with this module.
// Remove once the transport impl is wired.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The gateway wire version this module implements. Operator clients
/// must pin the exact current version (`minProtocol == maxProtocol`);
/// only node clients and probes get an N-1 window.
pub(crate) const PROTOCOL_VERSION: u32 = 4;

/// `client.id` we connect as. The schema's `ConnectParams.client.id`
/// is a **closed enum** (not documented on the protocol docs page —
/// recorded as an upstream doc gap): third-party clients must use one
/// of the registered ids, and `"gateway-client"` is the reference
/// third-party/SDK identity. Do not invent an `"intendant"` id — the
/// gateway would reject the connect frame at validation.
pub(crate) const CLIENT_ID: &str = "gateway-client";

/// `client.mode` we connect as: `"backend"` is the reference client's
/// default for headless/server-side operator clients (browser UIs use
/// `"ui"`, terminal UIs `"cli"`).
pub(crate) const CLIENT_MODE: &str = "backend";

/// Method names (subset used by slice 1).
pub(crate) const METHOD_CONNECT: &str = "connect";
pub(crate) const METHOD_CHAT_SEND: &str = "chat.send";
pub(crate) const METHOD_SESSIONS_LIST: &str = "sessions.list";

/// Event names (subset used by slice 1).
pub(crate) const EVENT_CONNECT_CHALLENGE: &str = "connect.challenge";
pub(crate) const EVENT_CHAT: &str = "chat";
pub(crate) const EVENT_SESSION_MESSAGE: &str = "session.message";

/// Structured error detail codes carried in `error.details.code`.
pub(crate) const DETAIL_CODE_PAIRING_REQUIRED: &str = "PAIRING_REQUIRED";
pub(crate) const DETAIL_CODE_MISSING_SCOPE: &str = "MISSING_SCOPE";

/// Top-level `error.code` of the gateway's pairing rejection
/// (`ErrorCodes.NOT_PAIRED` upstream; the `PAIRING_REQUIRED`
/// discriminator rides in `details.code`).
pub(crate) const ERROR_CODE_NOT_PAIRED: &str = "NOT_PAIRED";

// ---------------------------------------------------------------------------
// Frame envelopes
// ---------------------------------------------------------------------------

/// One gateway WebSocket frame, demuxed on the `type` tag.
///
/// Unknown frame kinds parse to [`Frame::Unknown`] instead of erroring
/// so the read loop survives additive protocol growth. (`#[serde(other)]`
/// variants cannot be serialized — fine, we never emit `Unknown`.)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum Frame {
    /// Client → gateway RPC request.
    #[serde(rename = "req")]
    Req(RequestFrame),
    /// Gateway → client RPC response.
    #[serde(rename = "res")]
    Res(ResponseFrame),
    /// Gateway → client push event.
    #[serde(rename = "event")]
    Event(EventFrame),
    /// Forward-compat fallback for frame kinds we don't recognize.
    #[serde(other)]
    Unknown,
}

/// `{type:"req", id, method, params?, traceparent?}` — the client→server
/// RPC envelope. The first frame on a connection MUST be a `connect`
/// request; pre-auth frames are capped at 64KiB by the gateway.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct RequestFrame {
    pub id: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// W3C trace context passthrough (max 128 chars server-side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
}

impl RequestFrame {
    /// Build a request frame with serialized `params`.
    pub(crate) fn new(
        id: impl Into<String>,
        method: impl Into<String>,
        params: Option<Value>,
    ) -> Self {
        Self {
            id: id.into(),
            method: method.into(),
            params,
            traceparent: None,
        }
    }
}

/// `{type:"res", id, ok, payload?|error?}` — the RPC response envelope.
/// `payload` is method-specific and parsed in a second stage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResponseFrame {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorShape>,
}

/// `{type:"event", event, payload?, seq?, stateVersion?}` — the push
/// envelope. The outer `seq` orders events on the current WebSocket
/// connection only (resets on reconnect); it is distinct from the
/// per-run `seq` inside `chat`/`agent` payloads. `stateVersion`
/// (`{presence, health}` counters) is not consumed in slice 1 and is
/// kept as raw JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct EventFrame {
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(
        default,
        rename = "stateVersion",
        skip_serializing_if = "Option::is_none"
    )]
    pub state_version: Option<Value>,
}

// ---------------------------------------------------------------------------
// Structured errors
// ---------------------------------------------------------------------------

/// `{code, message, details?, retryable?, retryAfterMs?}` — the
/// gateway's structured error shape (`ErrorShape` in the schema).
///
/// Connect-phase failures put the machine-readable discriminator in
/// `details.code` (e.g. `PAIRING_REQUIRED`, `DEVICE_AUTH_*`,
/// `AUTH_TOKEN_MISMATCH`), not the top-level `code` — use
/// [`ErrorShape::detail_code`] / [`ErrorShape::pairing_required`] /
/// [`ErrorShape::missing_scope`] to read them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ErrorShape {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl ErrorShape {
    /// The structured `details.code` discriminator, when present.
    pub(crate) fn detail_code(&self) -> Option<&str> {
        self.details.as_ref()?.get("code")?.as_str()
    }

    /// Parse typed `PAIRING_REQUIRED` details.
    ///
    /// The real gateway sends top-level `code: "NOT_PAIRED"` with
    /// `details.code: "PAIRING_REQUIRED"`
    /// (`src/gateway/server/ws-connection/connect-device-pairing.ts`);
    /// this reader also accepts `PAIRING_REQUIRED` in the top-level
    /// `code` so a sender that hoists the discriminator (our own mock
    /// gateway does) still parses. Returns `None` when the error is
    /// not a pairing rejection or the details don't parse.
    pub(crate) fn pairing_required(&self) -> Option<PairingRequiredDetails> {
        let discriminated = self.detail_code() == Some(DETAIL_CODE_PAIRING_REQUIRED)
            || self.code == ERROR_CODE_NOT_PAIRED
            || self.code == DETAIL_CODE_PAIRING_REQUIRED;
        if !discriminated {
            return None;
        }
        serde_json::from_value(self.details.clone()?).ok()
    }

    /// Parse typed `MISSING_SCOPE` details. Returns `None` unless
    /// `details.code == "MISSING_SCOPE"` and the details parse.
    pub(crate) fn missing_scope(&self) -> Option<MissingScopeDetails> {
        if self.detail_code()? != DETAIL_CODE_MISSING_SCOPE {
            return None;
        }
        serde_json::from_value(self.details.clone()?).ok()
    }
}

/// Typed `PAIRING_REQUIRED` connect-error details
/// (`PairingConnectErrorDetails` in upstream
/// `packages/gateway-protocol/src/connect-error-details.ts`; not present
/// in `protocol.schema.json` — an upstream machine-contract gap).
///
/// Known `reason` values: `not-paired`, `role-upgrade`, `scope-upgrade`,
/// `metadata-upgrade`. Known `recommended_next_step` values:
/// `retry_with_device_token`, `update_auth_configuration`,
/// `update_auth_credentials`, `wait_then_retry`,
/// `review_auth_configuration`. Kept as strings (with the vocabulary
/// documented here) so additive upstream growth can't fail the parse;
/// `request_id` is what the operator feeds to
/// `openclaw devices approve <requestId>` on the gateway host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PairingRequiredDetails {
    /// `"PAIRING_REQUIRED"` on the real gateway; optional here because
    /// the discriminator can also arrive hoisted into the top-level
    /// `error.code` (see [`ErrorShape::pairing_required`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_next_step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// `true` = stop auto-reconnecting until the host approves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_reconnect: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_scopes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_roles: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_scopes: Option<Vec<String>>,
}

/// Typed `MISSING_SCOPE` error details (`MissingScopeErrorDetails` in
/// the schema): the scope the call needed and the full requirement set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MissingScopeDetails {
    /// Always `"MISSING_SCOPE"`.
    pub code: String,
    pub missing_scope: String,
    pub required_scopes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Handshake: connect.challenge → connect → hello-ok
// ---------------------------------------------------------------------------

/// Payload of the `connect.challenge` event the gateway pushes
/// immediately after the WebSocket opens: `{nonce, ts}`, where `nonce`
/// is an opaque string (a UUID upstream) and `ts` is server
/// `Date.now()` milliseconds.
///
/// `ts` is typed `u64` so a non-integer or negative value fails
/// deserialization by construction — mirroring the reference client,
/// which treats a challenge without a non-negative safe-integer `ts`
/// as unusable for device auth. `ts` becomes the device proof's
/// `signedAt` verbatim (the gateway enforces a ±2 minute skew window
/// against its own clock, so client clock error is irrelevant).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ConnectChallenge {
    pub nonce: String,
    pub ts: u64,
}

impl ConnectChallenge {
    /// The trimmed nonce, or `None` when it is empty — the reference
    /// client rejects a challenge whose nonce trims to nothing.
    pub(crate) fn nonce_trimmed(&self) -> Option<&str> {
        let trimmed = self.nonce.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }
}

/// Params of the `connect` request (first frame on the socket).
///
/// Serialized strictly to the schema subset slice 1 uses — the gateway
/// validates `connect` with `additionalProperties: false`, so nothing
/// extra may ride along. `caps` is always emitted (schema default
/// `[]`); slice 1 advertises none because nothing capability-gated is
/// rendered yet, and absent caps silently gate delivery (e.g.
/// `tool-events`) rather than erroring.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectParams {
    /// Operators pin the exact version: both bounds [`PROTOCOL_VERSION`].
    pub min_protocol: u32,
    pub max_protocol: u32,
    pub client: ConnectClient,
    #[serde(default)]
    pub caps: Vec<String>,
    /// `"operator"` for slice 1 (see `card::OpenClawRole::as_str`).
    pub role: String,
    /// Requested scopes; slice 1 asks for `operator.read` +
    /// `operator.write` only. These exact strings (with this exact
    /// ordering) are bound into the device signature.
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ConnectAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<ConnectDevice>,
}

/// `connect.params.client` — who is connecting. `id` and `mode` are
/// closed enums server-side ([`CLIENT_ID`] / [`CLIENT_MODE`]);
/// `display_name` is what the pairing approval UI shows.
///
/// `platform` and `device_family` are bound into the v3 device
/// signature after lowercase/trim normalization — keep them ASCII.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectClient {
    pub id: String,
    pub version: String,
    pub platform: String,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
}

/// `connect.params.auth` — credential lanes. Slice 1 uses `token` (or
/// `password`) for the bootstrap connect and `device_token` for
/// reconnects; `bootstrap_token` is the single-use setup-link lane.
/// (The schema also has `approvalRuntimeToken` /
/// `agentRuntimeIdentityToken` lanes — out of slice-1 scope, omitted.)
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectAuth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl ConnectAuth {
    /// The credential bound into the device signature, mirroring the
    /// gateway's `resolveSignatureToken` exactly
    /// (`src/gateway/server/ws-connection/handshake-auth-helpers.ts`):
    /// `token ?? deviceToken ?? bootstrapToken ?? null` — **password is
    /// never part of the signed payload**, and the precedence order
    /// matters because the server rebuilds the payload from the same
    /// rule. `None` serializes into the payload as the empty string.
    pub(crate) fn signature_token(&self) -> Option<&str> {
        self.token
            .as_deref()
            .or(self.device_token.as_deref())
            .or(self.bootstrap_token.as_deref())
    }
}

/// `connect.params.device` — the Ed25519 device proof. Built by
/// [`super::identity::DeviceIdentity::connect_proof`]; see that module
/// for the exact signed byte layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectDevice {
    /// Device fingerprint: lowercase hex SHA-256 of the raw 32-byte
    /// public key. The gateway re-derives this from `public_key` and
    /// rejects on mismatch.
    pub id: String,
    /// Unpadded base64url of the raw 32-byte Ed25519 public key.
    pub public_key: String,
    /// Unpadded base64url of the 64-byte Ed25519 signature.
    pub signature: String,
    /// The challenge's `ts`, echoed verbatim (milliseconds).
    pub signed_at: u64,
    /// The challenge's `nonce`, echoed verbatim.
    pub nonce: String,
}

/// `hello-ok` — the successful `connect` response payload.
///
/// Permissive subset: the schema also carries required `type`,
/// `features` (method/event lists), and `snapshot` (presence/health)
/// fields that the docs summary omits and slice 1 does not consume —
/// unknown fields are ignored here, not round-tripped.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct HelloOk {
    pub protocol: u32,
    pub server: HelloServer,
    pub auth: HelloAuth,
    pub policy: HelloPolicy,
}

/// `hello-ok.server` — gateway build identity + this connection's id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HelloServer {
    pub version: String,
    pub conn_id: String,
}

/// `hello-ok.auth` — the granted (not requested) role/scopes, plus the
/// minted device token to persist for reconnects. A missing
/// `device_token` on a device-identity connect means the token was
/// already established (or the gateway declined to mint one).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HelloAuth {
    pub role: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_token: Option<String>,
}

/// `hello-ok.policy` — connection-time limits snapshot. Defaults
/// upstream: 25MiB `max_payload`, 50MiB `max_buffered_bytes`, 15s
/// `tick_interval_ms` (server closes with code 4000 at 2× tick
/// silence). Attachment ceilings are operator-tunable and re-read on
/// every reconnect; older gateways omit them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HelloPolicy {
    pub max_payload: u64,
    pub max_buffered_bytes: u64,
    pub tick_interval_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<AttachmentPolicy>,
}

/// `hello-ok.policy.attachments` — per-attachment decoded-byte ceilings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AttachmentPolicy {
    pub max_bytes: u64,
    pub max_image_bytes: u64,
}

// ---------------------------------------------------------------------------
// RPC: chat.send / sessions.list
// ---------------------------------------------------------------------------

/// `chat.send` params — the required subset (`operator.write`).
///
/// `idempotency_key` is **required** by the schema (the docs summary
/// omits it): a client-minted unique key the gateway uses to dedupe
/// retries of the same logical send.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatSendParams {
    pub session_key: String,
    pub message: String,
    pub idempotency_key: String,
}

/// `chat.send` ack payload: `{runId, status:"started", ...}` upstream.
/// Permissive — both fields optional so alternate/queued ack shapes
/// can't fail the parse; `run_id` is the handle for correlating
/// subsequent `chat` events.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatSendResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// `sessions.list` params (`operator.read`). Slice 1 sends `{}` (or a
/// small `limit`) as the connect-time liveness/capability probe.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionsListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `sessions.list` result — probe subset of upstream
/// `SessionsListResultBase` (`ts`/`path`/`count`/`defaults` etc. exist
/// on the wire; only what the probe reads is typed).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionsListResult {
    #[serde(default)]
    pub sessions: Vec<SessionRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

/// One `sessions.list` row — liveness-probe subset. The schema requires
/// only `key` + `kind`; everything else here is display sugar. `kind`
/// vocabulary: `direct` / `group` / `global` / `unknown` (kept as a
/// string — the probe never branches on it, and the vocabulary is
/// upstream's to grow).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionRow {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_title: Option<String>,
    /// Milliseconds; `null` on the wire maps to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<f64>,
}

// ---------------------------------------------------------------------------
// Event payloads: chat / session.message
// ---------------------------------------------------------------------------

/// Run lifecycle state of a `chat` event (`payload.state`). Bare-string
/// wire form with hand-rolled fallback per the house convention
/// (`peer/card.rs`): unknown states parse to [`ChatEventState::Unknown`]
/// instead of dropping the event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatEventState {
    /// Pre-model phases (`payload.phase`: `preparing_workspace`, …).
    Status,
    /// Streaming text delta (`payload.delta_text`).
    Delta,
    /// Run finished; `payload.message` is the final assistant message.
    Final,
    /// Run aborted (`chat.abort` or interruption).
    Aborted,
    /// Run failed (`payload.error_message` / `payload.error_kind`).
    Error,
    /// Forward-compat fallback for states we don't recognize.
    Unknown,
}

impl ChatEventState {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Delta => "delta",
            Self::Final => "final",
            Self::Aborted => "aborted",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn from_wire(s: &str) -> Self {
        match s {
            "status" => Self::Status,
            "delta" => Self::Delta,
            "final" => Self::Final,
            "aborted" => Self::Aborted,
            "error" => Self::Error,
            _ => Self::Unknown,
        }
    }
}

impl Serialize for ChatEventState {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ChatEventState {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <String>::deserialize(d)?;
        Ok(Self::from_wire(&s))
    }
}

/// `chat` event payload — one flat permissive struct instead of a
/// per-state union: the schema's five `state` variants (`status`,
/// `delta`, `final`, `aborted`, `error`) share `runId`/`sessionKey`/
/// `seq` and differ only in which optional fields are present, and
/// field presence (not variant identity) is what the relay acts on.
/// `seq` orders events *within one run*; a forward gap means events
/// were missed and authoritative history should be reloaded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatEventPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub state: ChatEventState,
    /// `status`-state phase (`preparing_workspace`, `starting_model`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_text: Option<String>,
    /// `true` on a delta that replaces (not appends to) buffered text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace: Option<bool>,
    /// Message object (role/content blocks) — kept raw for the relay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
}

/// `session.message` event payload — permissive mirror of upstream's
/// own tolerant reader (`SessionMessagePayload` in
/// `src/mcp/channel-shared.ts`): every field optional, unknown fields
/// ignored. `message` is `{role?, content?, …}` kept raw; `content`
/// can be a plain string or a block array, and the relay decides how
/// to render it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionMessagePayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_is_owner: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The vendored machine contract this module's field names are
    /// pinned against (see the module docs for version provenance).
    const SCHEMA: &str = include_str!("protocol.schema.json");

    fn schema() -> Value {
        serde_json::from_str(SCHEMA).expect("vendored protocol.schema.json parses")
    }

    fn definition(root: &Value, name: &str) -> Value {
        root.pointer(&format!("/definitions/{name}"))
            .unwrap_or_else(|| panic!("schema definition {name} exists"))
            .clone()
    }

    fn required_set(def: &Value) -> Vec<String> {
        def.get("required")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn property_names(def: &Value) -> Vec<String> {
        def.get("properties")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn assert_has_props(def: &Value, names: &[&str]) {
        let props = property_names(def);
        for name in names {
            assert!(
                props.iter().any(|p| p == name),
                "schema property {name} missing; have {props:?}"
            );
        }
    }

    // -- schema pins: frames + errors --------------------------------------

    #[test]
    fn schema_pins_frame_envelopes() {
        let root = schema();
        let req = definition(&root, "RequestFrame");
        assert_eq!(required_set(&req), ["type", "id", "method"]);
        assert_has_props(&req, &["type", "id", "method", "params", "traceparent"]);

        let res = definition(&root, "ResponseFrame");
        assert_eq!(required_set(&res), ["type", "id", "ok"]);
        assert_has_props(&res, &["payload", "error"]);

        let event = definition(&root, "EventFrame");
        assert_eq!(required_set(&event), ["type", "event"]);
        assert_has_props(&event, &["payload", "seq", "stateVersion"]);
    }

    #[test]
    fn schema_pins_error_shapes() {
        let root = schema();
        let err = definition(&root, "ErrorShape");
        assert_eq!(required_set(&err), ["code", "message"]);
        assert_has_props(
            &err,
            &["code", "message", "details", "retryable", "retryAfterMs"],
        );

        let missing = definition(&root, "MissingScopeErrorDetails");
        assert_eq!(
            required_set(&missing),
            ["code", "missingScope", "requiredScopes"]
        );
        assert_eq!(
            missing
                .pointer("/properties/code/const")
                .and_then(Value::as_str),
            Some(DETAIL_CODE_MISSING_SCOPE)
        );
    }

    // -- schema pins: connect + hello-ok ------------------------------------

    #[test]
    fn schema_pins_connect_params() {
        let root = schema();
        let connect = definition(&root, "ConnectParams");
        assert_eq!(
            required_set(&connect),
            ["minProtocol", "maxProtocol", "client"]
        );
        assert_has_props(
            &connect,
            &["caps", "role", "scopes", "auth", "device", "client"],
        );

        // client.id is a CLOSED enum; our id and mode must be members.
        let client = connect.pointer("/properties/client").unwrap();
        assert_eq!(required_set(client), ["id", "version", "platform", "mode"]);
        let ids: Vec<&str> = client
            .pointer("/properties/id/enum")
            .and_then(Value::as_array)
            .expect("client.id is a closed enum")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(ids.contains(&CLIENT_ID), "client.id enum lost {CLIENT_ID}");
        let modes: Vec<&str> = client
            .pointer("/properties/mode/enum")
            .and_then(Value::as_array)
            .expect("client.mode is a closed enum")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(modes.contains(&CLIENT_MODE), "mode enum lost {CLIENT_MODE}");
        assert_has_props(client, &["displayName", "deviceFamily", "instanceId"]);

        let device = connect.pointer("/properties/device").unwrap();
        assert_eq!(
            required_set(device),
            ["id", "publicKey", "signature", "signedAt", "nonce"]
        );

        let auth = connect.pointer("/properties/auth").unwrap();
        assert_has_props(
            auth,
            &["token", "bootstrapToken", "deviceToken", "password"],
        );
    }

    #[test]
    fn schema_pins_hello_ok() {
        let root = schema();
        let hello = definition(&root, "HelloOk");
        let required = required_set(&hello);
        for field in ["protocol", "server", "auth", "policy"] {
            assert!(
                required.iter().any(|r| r == field),
                "hello-ok requires {field}"
            );
        }

        let server = hello.pointer("/properties/server").unwrap();
        assert_eq!(required_set(server), ["version", "connId"]);

        let auth = hello.pointer("/properties/auth").unwrap();
        assert_eq!(required_set(auth), ["role", "scopes"]);
        assert_has_props(auth, &["deviceToken"]);

        let policy = hello.pointer("/properties/policy").unwrap();
        assert_eq!(
            required_set(policy),
            ["maxPayload", "maxBufferedBytes", "tickIntervalMs"]
        );
        let attachments = policy.pointer("/properties/attachments").unwrap();
        assert_eq!(required_set(attachments), ["maxBytes", "maxImageBytes"]);
    }

    // -- schema pins: RPC subset --------------------------------------------

    #[test]
    fn schema_pins_chat_send() {
        let root = schema();
        let params = definition(&root, "ChatSendParams");
        assert_eq!(
            required_set(&params),
            ["sessionKey", "message", "idempotencyKey"],
            "chat.send required set changed (idempotencyKey is load-bearing)"
        );
        // Scope metadata: the methods map is part of the machine contract.
        assert_eq!(
            root.pointer("/methods/chat.send/scope")
                .and_then(Value::as_str),
            Some("operator.write")
        );
    }

    #[test]
    fn schema_pins_sessions_list() {
        let root = schema();
        let params = definition(&root, "SessionsListParams");
        assert!(required_set(&params).is_empty());
        assert_has_props(&params, &["limit"]);

        let row = definition(&root, "SessionRow");
        assert_eq!(required_set(&row), ["key", "kind"]);
        assert_has_props(&row, &["label", "displayName", "derivedTitle", "updatedAt"]);

        assert_eq!(
            root.pointer("/methods/sessions.list/scope")
                .and_then(Value::as_str),
            Some("operator.read")
        );
    }

    #[test]
    fn schema_pins_chat_event_states() {
        let root = schema();
        let chat = definition(&root, "ChatEvent");
        let states: Vec<String> = chat
            .get("anyOf")
            .and_then(Value::as_array)
            .expect("ChatEvent is a state union")
            .iter()
            .filter_map(|v| v.pointer("/properties/state/const"))
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        assert_eq!(states, ["status", "delta", "final", "aborted", "error"]);
        for state in &states {
            assert_ne!(
                ChatEventState::from_wire(state),
                ChatEventState::Unknown,
                "schema state {state} must map to a known variant"
            );
        }
    }

    // -- envelope round-trips -----------------------------------------------

    #[test]
    fn request_frame_serializes_to_pinned_json() {
        let frame = Frame::Req(RequestFrame::new(
            "r1",
            METHOD_CHAT_SEND,
            Some(json!({"sessionKey": "main", "message": "hi", "idempotencyKey": "k1"})),
        ));
        let encoded = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            encoded,
            json!({
                "type": "req",
                "id": "r1",
                "method": "chat.send",
                "params": {"sessionKey": "main", "message": "hi", "idempotencyKey": "k1"},
            })
        );
    }

    #[test]
    fn frame_demux_parses_all_kinds_and_tolerates_unknown() {
        let res: Frame =
            serde_json::from_str(r#"{"type":"res","id":"r1","ok":true,"payload":{"x":1}}"#)
                .unwrap();
        match res {
            Frame::Res(r) => {
                assert!(r.ok);
                assert_eq!(r.id, "r1");
                assert!(r.error.is_none());
            }
            other => panic!("expected res frame, got {other:?}"),
        }

        let event: Frame = serde_json::from_str(
            r#"{"type":"event","event":"tick","seq":7,"stateVersion":{"presence":1,"health":2}}"#,
        )
        .unwrap();
        match event {
            Frame::Event(e) => {
                assert_eq!(e.event, "tick");
                assert_eq!(e.seq, Some(7));
                assert!(e.state_version.is_some());
            }
            other => panic!("expected event frame, got {other:?}"),
        }

        // Future frame kinds must not error the read loop.
        let unknown: Frame = serde_json::from_str(r#"{"type":"ping","whenever":1}"#).unwrap();
        assert_eq!(unknown, Frame::Unknown);
    }

    #[test]
    fn error_response_parses_with_structured_details() {
        // Real gateway shape: top-level NOT_PAIRED, discriminator in
        // details.code (connect-device-pairing.ts).
        let frame: Frame = serde_json::from_str(
            r#"{"type":"res","id":"c1","ok":false,
                "error":{"code":"NOT_PAIRED","message":"device pairing required",
                         "retryable":false,"retryAfterMs":2500,
                         "details":{"code":"PAIRING_REQUIRED","reason":"not-paired",
                                    "requestId":"req_123",
                                    "recommendedNextStep":"wait_then_retry",
                                    "pauseReconnect":true,
                                    "futureField":"ignored"}}}"#,
        )
        .unwrap();
        let Frame::Res(res) = frame else {
            panic!("expected res frame");
        };
        let error = res.error.expect("error present");
        assert_eq!(error.retry_after_ms, Some(2500));
        assert_eq!(error.detail_code(), Some(DETAIL_CODE_PAIRING_REQUIRED));

        let pairing = error.pairing_required().expect("typed pairing details");
        assert_eq!(pairing.request_id.as_deref(), Some("req_123"));
        assert_eq!(pairing.reason.as_deref(), Some("not-paired"));
        assert_eq!(
            pairing.recommended_next_step.as_deref(),
            Some("wait_then_retry")
        );
        assert_eq!(pairing.pause_reconnect, Some(true));
        // Wrong-code reads return None instead of misparsing.
        assert!(error.missing_scope().is_none());
    }

    #[test]
    fn pairing_required_also_reads_hoisted_top_level_code() {
        // Mock-gateway shape: discriminator hoisted to error.code, no
        // details.code member.
        let error = ErrorShape {
            code: DETAIL_CODE_PAIRING_REQUIRED.into(),
            message: "pairing required".into(),
            details: Some(json!({
                "requestId": "req_9",
                "recommendedNextStep": "wait_then_retry",
            })),
            retryable: Some(true),
            retry_after_ms: None,
        };
        let pairing = error.pairing_required().expect("hoisted code parses");
        assert_eq!(pairing.request_id.as_deref(), Some("req_9"));
        assert_eq!(pairing.code, None);

        // An unrelated error with a details object stays None.
        let unrelated = ErrorShape {
            code: "UNAUTHORIZED".into(),
            message: "no".into(),
            details: Some(json!({"reason": "token-mismatch"})),
            retryable: None,
            retry_after_ms: None,
        };
        assert!(unrelated.pairing_required().is_none());
    }

    #[test]
    fn missing_scope_details_parse() {
        let error = ErrorShape {
            code: "FORBIDDEN".into(),
            message: "missing scope".into(),
            details: Some(json!({
                "code": "MISSING_SCOPE",
                "missingScope": "operator.write",
                "requiredScopes": ["operator.write"],
            })),
            retryable: None,
            retry_after_ms: None,
        };
        let details = error.missing_scope().expect("typed missing-scope details");
        assert_eq!(details.missing_scope, "operator.write");
        assert_eq!(details.required_scopes, ["operator.write"]);
        assert!(error.pairing_required().is_none());
    }

    // -- handshake payloads --------------------------------------------------

    #[test]
    fn connect_challenge_rejects_bad_ts_and_trims_nonce() {
        let ok: ConnectChallenge =
            serde_json::from_str(r#"{"nonce":" abc ","ts":1755600000000}"#).unwrap();
        assert_eq!(ok.ts, 1_755_600_000_000);
        assert_eq!(ok.nonce_trimmed(), Some("abc"));

        let empty: ConnectChallenge = serde_json::from_str(r#"{"nonce":"  ","ts":1}"#).unwrap();
        assert_eq!(empty.nonce_trimmed(), None);

        for bad in [
            r#"{"nonce":"n","ts":-1}"#,
            r#"{"nonce":"n","ts":12.5}"#,
            r#"{"nonce":"n","ts":"12"}"#,
            r#"{"nonce":"n"}"#,
        ] {
            assert!(
                serde_json::from_str::<ConnectChallenge>(bad).is_err(),
                "challenge {bad} must be rejected"
            );
        }
    }

    #[test]
    fn connect_params_serialize_to_pinned_wire_names() {
        let params = ConnectParams {
            min_protocol: PROTOCOL_VERSION,
            max_protocol: PROTOCOL_VERSION,
            client: ConnectClient {
                id: CLIENT_ID.into(),
                version: "0.2.0".into(),
                platform: "macos".into(),
                mode: CLIENT_MODE.into(),
                display_name: Some("Intendant (macbook)".into()),
                device_family: None,
                instance_id: None,
            },
            caps: vec![],
            role: "operator".into(),
            scopes: vec!["operator.read".into(), "operator.write".into()],
            auth: Some(ConnectAuth {
                token: Some("tok".into()),
                ..ConnectAuth::default()
            }),
            device: Some(ConnectDevice {
                id: "ab".into(),
                public_key: "pk".into(),
                signature: "sig".into(),
                signed_at: 5,
                nonce: "n".into(),
            }),
        };
        let encoded = serde_json::to_value(&params).unwrap();
        assert_eq!(
            encoded,
            json!({
                "minProtocol": 4,
                "maxProtocol": 4,
                "client": {
                    "id": "gateway-client",
                    "version": "0.2.0",
                    "platform": "macos",
                    "mode": "backend",
                    "displayName": "Intendant (macbook)",
                },
                "caps": [],
                "role": "operator",
                "scopes": ["operator.read", "operator.write"],
                "auth": {"token": "tok"},
                "device": {
                    "id": "ab",
                    "publicKey": "pk",
                    "signature": "sig",
                    "signedAt": 5,
                    "nonce": "n",
                },
            })
        );
    }

    #[test]
    fn signature_token_precedence_mirrors_gateway() {
        let all = ConnectAuth {
            token: Some("t".into()),
            bootstrap_token: Some("b".into()),
            device_token: Some("d".into()),
            password: Some("p".into()),
        };
        assert_eq!(all.signature_token(), Some("t"));

        let device_first = ConnectAuth {
            device_token: Some("d".into()),
            bootstrap_token: Some("b".into()),
            ..ConnectAuth::default()
        };
        assert_eq!(device_first.signature_token(), Some("d"));

        let bootstrap_only = ConnectAuth {
            bootstrap_token: Some("b".into()),
            ..ConnectAuth::default()
        };
        assert_eq!(bootstrap_only.signature_token(), Some("b"));

        // Password is never part of the signed payload.
        let password_only = ConnectAuth {
            password: Some("p".into()),
            ..ConnectAuth::default()
        };
        assert_eq!(password_only.signature_token(), None);
    }

    #[test]
    fn hello_ok_parses_permissively() {
        // Includes the schema-required fields slice 1 does not consume
        // (type/features/snapshot) to prove unknown-field tolerance.
        let hello: HelloOk = serde_json::from_str(
            r#"{"type":"hello-ok","protocol":4,
                "server":{"version":"2026.8.1","connId":"c-1","buildId":"g123"},
                "features":{"methods":["chat.send"],"events":["chat"]},
                "snapshot":{"presence":[],"health":{},"stateVersion":{"presence":0,"health":0},"uptimeMs":1},
                "auth":{"role":"operator","scopes":["operator.read"],"deviceToken":"dt-1"},
                "policy":{"maxPayload":26214400,"maxBufferedBytes":52428800,
                          "tickIntervalMs":15000,
                          "attachments":{"maxBytes":1048576,"maxImageBytes":2097152}}}"#,
        )
        .unwrap();
        assert_eq!(hello.protocol, 4);
        assert_eq!(hello.server.conn_id, "c-1");
        assert_eq!(hello.auth.device_token.as_deref(), Some("dt-1"));
        assert_eq!(hello.policy.tick_interval_ms, 15_000);
        let attachments = hello.policy.attachments.expect("attachment policy");
        assert_eq!(attachments.max_image_bytes, 2_097_152);

        // Older gateways omit attachments entirely.
        let minimal: HelloOk = serde_json::from_str(
            r#"{"protocol":4,"server":{"version":"1","connId":"c"},
                "auth":{"role":"operator","scopes":[]},
                "policy":{"maxPayload":1,"maxBufferedBytes":1,"tickIntervalMs":1}}"#,
        )
        .unwrap();
        assert!(minimal.policy.attachments.is_none());
        assert!(minimal.auth.device_token.is_none());
    }

    // -- RPC + event payloads ------------------------------------------------

    #[test]
    fn chat_send_round_trip() {
        let params = ChatSendParams {
            session_key: "main".into(),
            message: "hello".into(),
            idempotency_key: "idem-1".into(),
        };
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            json!({"sessionKey": "main", "message": "hello", "idempotencyKey": "idem-1"})
        );

        let ack: ChatSendResult = serde_json::from_str(
            r#"{"runId":"run-1","status":"started","serverTiming":{"totalMs":3}}"#,
        )
        .unwrap();
        assert_eq!(ack.run_id.as_deref(), Some("run-1"));
        assert_eq!(ack.status.as_deref(), Some("started"));
    }

    #[test]
    fn sessions_list_result_parses_probe_subset() {
        let result: SessionsListResult = serde_json::from_str(
            r#"{"ts":1755,"path":"/x","count":2,"defaults":{"model":null},
                "sessions":[
                  {"key":"main","kind":"direct","displayName":"Main",
                   "updatedAt":1755600000000.0,"unknownRowField":true},
                  {"key":"g:ops","kind":"group","updatedAt":null}
                ]}"#,
        )
        .unwrap();
        assert_eq!(result.count, Some(2));
        assert_eq!(result.sessions.len(), 2);
        assert_eq!(result.sessions[0].key, "main");
        assert_eq!(result.sessions[0].kind.as_deref(), Some("direct"));
        assert_eq!(result.sessions[0].display_name.as_deref(), Some("Main"));
        assert_eq!(result.sessions[1].updated_at, None);

        // Empty params serialize to the bare probe `{}`.
        assert_eq!(
            serde_json::to_value(SessionsListParams::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn chat_event_payload_parses_each_state_and_unknown() {
        let delta: ChatEventPayload = serde_json::from_str(
            r#"{"runId":"r","sessionKey":"main","seq":3,"state":"delta",
                "deltaText":"hel","replace":false}"#,
        )
        .unwrap();
        assert_eq!(delta.state, ChatEventState::Delta);
        assert_eq!(delta.delta_text.as_deref(), Some("hel"));

        let final_event: ChatEventPayload = serde_json::from_str(
            r#"{"runId":"r","sessionKey":"main","seq":9,"state":"final",
                "message":{"role":"assistant","content":"done"},
                "usage":{"input":10},"stopReason":"end_turn"}"#,
        )
        .unwrap();
        assert_eq!(final_event.state, ChatEventState::Final);
        assert_eq!(final_event.stop_reason.as_deref(), Some("end_turn"));
        assert!(final_event.message.is_some());

        let error_event: ChatEventPayload = serde_json::from_str(
            r#"{"runId":"r","sessionKey":"main","seq":2,"state":"error",
                "errorMessage":"boom","errorKind":"provider"}"#,
        )
        .unwrap();
        assert_eq!(error_event.state, ChatEventState::Error);
        assert_eq!(error_event.error_message.as_deref(), Some("boom"));

        // Future states degrade to Unknown, not a parse failure.
        let future: ChatEventPayload = serde_json::from_str(
            r#"{"runId":"r","sessionKey":"main","seq":1,"state":"paused","novel":1}"#,
        )
        .unwrap();
        assert_eq!(future.state, ChatEventState::Unknown);
    }

    #[test]
    fn session_message_payload_parses_permissively() {
        let payload: SessionMessagePayload = serde_json::from_str(
            r#"{"sessionKey":"main","messageId":"m1","messageSeq":12,
                "senderIsOwner":true,
                "message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},
                "lastChannel":"webchat","futureField":{"deep":true}}"#,
        )
        .unwrap();
        assert_eq!(payload.session_key.as_deref(), Some("main"));
        assert_eq!(payload.message_seq, Some(12));
        assert_eq!(payload.sender_is_owner, Some(true));
        assert!(payload.message.is_some());
    }
}
