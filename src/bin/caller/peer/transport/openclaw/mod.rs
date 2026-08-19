//! OpenClaw Gateway transport (slice 1) — Intendant connects to an
//! OpenClaw Gateway's WebSocket control plane as an `operator` client
//! and relays messages ([`TransportSpec::OpenClawWs`]).
//!
//! Protocol v4: JSON req/res/event frames over a single multiplexed
//! port (default 18789); `connect.challenge` → signed `connect` →
//! `hello-ok`; Ed25519 device identity with host-approved pairing and
//! a persistent device token. Seam map and slice plan:
//! `~/openclaw-transport-next.md`; upstream spec:
//! <https://docs.openclaw.ai/gateway/protocol>.
//!
//! [`TransportSpec::OpenClawWs`]: crate::peer::card::TransportSpec::OpenClawWs
//!
//! Module layout (filled in by the slice-1 seats):
//! - [`wire`] — frame/handshake/RPC serde types, pinned to the
//!   vendored `protocol.schema.json` snapshot.
//! - [`identity`] — Ed25519 device identity, challenge signing, and
//!   the persisted device token.
//! - `mock_gateway` (test-only) — hermetic in-process v4 gateway for
//!   integration tests.

pub(crate) mod identity;
pub(crate) mod wire;

#[cfg(test)]
pub(crate) mod mock_gateway;
