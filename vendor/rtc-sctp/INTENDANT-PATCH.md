# Vendored `rtc-sctp` 0.20.3 (carrying our filed upstream MTU fix)

Byte-for-byte copy of the crates.io `rtc-sctp` 0.20.3 package (MIT/Apache-2.0,
webrtc-rs project), wired in via `[patch.crates-io]` in the workspace
`Cargo.toml`, with **one** functional change applied on top: our own fix filed
upstream as **webrtc-rs/rtc#178 → PR #180**, which 0.20.3 predates.

The patch (`src/config.rs`, `src/association/mod.rs`,
`src/association/association_test.rs`):

- `INITIAL_MTU` 1228 → **1191** — the exact TURN-relayed IPv6 budget
  (1280 − 40 IPv6 − 8 UDP − 4 TURN ChannelData − 37 DTLS), the pion/sctp#476
  derivation that webrtc-rs/webrtc#807 adopted and shipped in webrtc v0.17.2;
  replacing that sctp implementation with rtc-sctp in the 0.20 architecture
  reverted the default to 1228.
- `EndpointConfig::default()`'s `max_payload_size` rounded **down to the SCTP
  4-byte padding boundary** so a single maximum-size DATA chunk also marshals
  within `INITIAL_MTU`.
- `bundle_data_chunks_into_packets` and the fast-retransmit sizing loop decide
  on **marshalled wire sizes** (chunk header + payload, padded to the 4-byte
  boundary) instead of raw payload sizes — payload-only accounting admitted
  bundles that serialized past the MTU the association promised (payloads of
  1147 + 16 pass a payload-only check at an MTU of 1191 but marshal to a
  1208-byte packet).
- Three tests bundle real chunks and measure the emitted packets (the
  1147+16 counterexample must split; a single maximum-size chunk must fit;
  a 60-small-chunk burst stays within the MTU), plus a config test pinning
  the 1191 derivation.

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
display rig; clamping the assembled-packet size (originally to 1192 on our
vendored 0.9.1; now 1191, upstream-aligned with the PR #180 derivation)
fixed the delivery end-to-end on the first try. libwebrtc pins its usrsctp
MTU to 1200 for exactly this class of path.

`rtc` 0.20.3 still builds its SCTP `TransportConfig` with `::default()` and
plumbs no public knob for `max_payload_size`/MTU through the setting engine,
so the default itself remains the only place to fix it today.

## Exit criterion

Retire this vendored copy when a **released** `rtc-sctp` containing
webrtc-rs/rtc PR #180 ships — then pin that release and drop the local carry.
Same playbook as the rtc transport-protocol stamping fix (webrtc-rs/rtc#109 →
#110, released in the line we now consume).
