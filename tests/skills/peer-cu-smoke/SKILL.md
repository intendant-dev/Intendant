# Peer direct-CU fleet smoke

Live proof of the direct-computer-use-on-peers path: `intendant ctl
--peer` drives a **real federated peer box** — display inventory,
screenshot, input injection — over the peer's `/mcp` with mTLS, with
**no daemon running on either the operator side or locally at all**
(the target's daemon is the peer; nothing runs here but `ctl`).

The headless twin lives in CI
(`ctl_peer_mtls_pairing_binds_scoped_profile_and_gates_display_input`
in `tests/e2e/`): it pins the pairing ceremony and the profile gates on
loopback rigs. This smoke covers what that can't: a real box across the
network, a real user graphical session, standing grants that survived
daemon restarts, and screenshots a human actually looks at.

Not in CI because it needs a paired fleet peer with a live graphical
session and persistent grants.

## Prerequisites

- A release controller built from **your own worktree** (never the
  repo root): `cargo build --release`.
- A `[[peer]]` entry for the target box, produced by the pairing
  ceremony (`intendant peer request/approve/complete` — see
  `docs/src/peer-federation.md`): `card_url`, `client_cert` /
  `client_key` (absolute paths), ideally `pinned_fingerprints`.
- On the peer, a profile grant for this daemon's identity:
  `read-only-display` or better for the view legs,
  `peer-operator` / `peer-root` for the input leg.

## Setup — scratch-dir technique

`ctl --peer` resolves the `[[peer]]` entry from the working
directory's `intendant.toml`. Run from a scratch dir so the smoke
never touches a live project's config and never runs from the repo
root:

```bash
mkdir -p /tmp/peer-cu-smoke && cd /tmp/peer-cu-smoke
# Copy the target box's [[peer]] block out of the live project's
# intendant.toml — cert/key paths are absolute, so it ports verbatim.
cat > intendant.toml <<'EOF'
[[peer]]
card_url = "https://<peer-host>:<port>/.well-known/agent-card.json"
label = "<peer-label>"
client_cert = "/abs/path/to/peers/<slug>/client.crt"
client_key = "/abs/path/to/peers/<slug>/client.key"
pinned_fingerprints = ["<sha256-hex>"]
EOF
# Scrub supervised-session env so ctl can't route to a local daemon:
ctl() { env -u INTENDANT_MCP_URL -u INTENDANT_PORT \
            -u INTENDANT_SESSION_ID -u INTENDANT_MANAGED_CONTEXT \
        <your-worktree>/target/release/intendant ctl "$@"; }
```

## The legs

```bash
ctl --peer <peer-label> display list
# → the peer's real monitors with geometry (e.g. "eDP-1 1920x1080")

ctl --peer <peer-label> display screenshot --target user_session --output peer.png
# → PNG of the box's live desktop. Open it and look — the pass
#   criterion is visual, not exit-code-only.

ctl --peer <peer-label> cu actions --target user_session \
    --actions '[{"type":"move_mouse","x":400,"y":300}]'
# Grant holds display input (peer-operator/peer-root) → ok.
# Grant is read-only-display → non-zero exit with
#   "Permission denied for tool 'execute_cu_actions' ...
#    (principal principal:peer-daemon:<fingerprint>, ...)"
# — that denial IS the pass for the deny leg; run whichever
# matches the standing grant, or both against two peers.
#
# THIRD outcome class on user_session legs: peers are never owner
# surfaces, so if the TARGET daemon's user-display grant is not held
# (`ctl display grant-user` on the box), screenshots/actions/elements
# against user_session return the explicit opt-in refusal ("Access to
# the user's real display (user_session) is an explicit opt-in ...").
# That is the grant gate passing, not a transport or profile failure —
# distinct from the IAM "Permission denied" above. The fleet boxes with
# a persistent grant never show it; report it if one does.

ctl --peer <peer-label> cu elements
# OPTIONAL leg — the peer's frontmost UI element tree (read_screen:
# display-view class, same grant as screenshots; the cheap textual
# first look before pulling pixels). Accessibility availability
# varies by box (Linux AT-SPI, macOS AX trust, Windows UIA), so a
# "read_screen error: ..." from a missing/disabled stack — or a
# denial under a sub-display-view grant — is a finding to report,
# not a smoke failure.
```

## What it proves

- The `[[peer]]` mTLS identity + pinned fingerprints carry a one-shot
  JSON-RPC `tools/call` to the peer's `/mcp` — no local daemon, no
  WebRTC, no card-advertised auth fallback.
- `display list` reflects the peer's real hardware.
- `user_session` screenshots show the live desktop, and the image
  content survives the whole MCP → ctl → PNG path.
- Input is gated by the **peer-granted profile**, and the denial
  diagnostic names the peer-daemon principal.
- Peer-side diagnostics travel verbatim; `ctl` exits non-zero when the
  tool reply is an error.

## Traps

- **Pass `--target user_session` explicitly.** Current daemons
  auto-detect an omitted target availability-aware (a live virtual
  display when one exists, else the user session), but the explicit
  flag is deterministic: on a peer running an older daemon, an omitted
  target blindly assumes `:99` and a box whose only session is `:0`
  fails with "cannot connect to X display :99" — and on a box that
  *does* have an agent virtual display, the omitted form captures that
  display, not the desktop this smoke wants to look at.
- A denial means the peer's owner has not granted the capability —
  report it, don't retry or work around it.
- `--peer` resolves by label (case-insensitive), `card_url` host,
  exact `card_url`, or `intendant:<label>`.
- `client_cert` / `client_key` must come together — a half-set pair
  errors loudly rather than silently falling back.
- Failures arrive as the peer's own diagnostic text; if it's opaque,
  the next stop is the peer box's daemon log, not this side.
