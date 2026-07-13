# Vendored `rtc-sctp` 0.9.1 (single-constant patch)

Byte-for-byte copy of the crates.io `rtc-sctp` 0.9.1 package (MIT/Apache-2.0,
webrtc-rs project), wired in via `[patch.crates-io]` in the workspace
`Cargo.toml`, with **one** functional change:

- `src/config.rs`: `INITIAL_MTU` 1228 → **1192**.

## Why

`INITIAL_MTU` caps how large an SCTP packet the association assembles
(bundling DATA chunks up to this size). Each SCTP packet becomes one DTLS
application-data record (~37 bytes of record overhead), carried in one UDP
datagram. With upstream's 1228:

```
1228 SCTP + ~37 DTLS = ~1265-byte record + 48 IPv6/UDP headers = ~1313 wire bytes
```

That exceeds the 1280-byte IPv6 minimum MTU used by common overlay paths
(Tailscale/WireGuard tunnels advertise 1280). The oversized datagram is
dropped by the path, SCTP retransmits **rebundle to the same oversized
packet**, and the flight is lost forever — no error surfaces anywhere
(`send_text` returns `Ok`). Any small message bundled into such a flight
(for us: `display_input_authority_state` after a federated Take Control,
alongside a large tile-snapshot chunk) silently never arrives, while RTP
media (independently sized well under the MTU) keeps flowing on the same
candidate pair. Diagnosed live 2026-07-13 on the Mac ↔ dell federated
display rig; clamping to 1192 (record ≤ ~1229, wire ≤ ~1277) fixed the
delivery end-to-end on the first try. libwebrtc pins its usrsctp MTU to
1200 for exactly this class of path.

`rtc` 0.9.1 builds its SCTP `TransportConfig` with `::default()` and
plumbs no public knob for `max_payload_size`/MTU through `SettingEngine`,
so the default itself is the only place to fix it today.

## Exit criteria

Retire this vendored copy when either lands upstream (webrtc-rs/rtc):

1. a safe default (≤1200-byte SCTP packets), or
2. a `SettingEngine`/`TransportConfig` knob reachable from
   `RTCPeerConnection` construction — then set it from
   `crates/intendant-display/src/webrtc/offer.rs` instead.

Same playbook as the rtc 0.9.1 transport-protocol stamping fix
(webrtc-rs/rtc#109 → #110): upstream the change, pin the release, drop the
local carry.
