---
name: scoped-human-mtls-e2e
description: >
  Live single-daemon proof that browser mTLS certificates bind to scoped IAM
  principals end-to-end: mint extra client certs from the rig CA, bind them
  to role:files-write (with fs roots) and role:operator via the IAM API from
  the root cert, then hit the HTTPS dashboard with Playwright client
  certificates and watch the daemon enforce role ceilings, fs scoping on
  write/rename/delete (both rename legs), and the peer.use gate on peer
  signaling relays and quick controls.
compatibility: Requires playwright >= 1.46 (clientCertificates), openssl, a
  release build of intendant, one free localhost port. No model calls, no
  API keys — the daemon runs idle.
allowed-tools: Bash Read
disable-model-invocation: false
---

# Scoped-Human mTLS E2E

## Purpose

Unit tests prove `http_access_context` + the gates in isolation; this proves
the wiring: TLS handshake → certificate fingerprint → IAM grant lookup →
scoped principal → per-operation and per-path enforcement, over the same
transport a real browser uses. The unbound owner cert from `access setup`
stays root; every other cert is exactly what its grant says.

## Procedure

1. **Rig + CA + extra client certs**
   ```bash
   export RIG=/tmp/scoped-human-rig BIN=$PWD/target/release/intendant
   bash tests/skills/scoped-human-mtls-e2e/rig.sh   # builds $RIG, mints certs
   ```
2. **Launch the idle daemon (TLS + client certs required by default)**
   ```bash
   cd $RIG/proj && HOME=$RIG/home $BIN --web 18820 --bind 127.0.0.1 &
   ```
3. **Drive it**
   ```bash
   NODE_PATH=<repo>/node_modules RIG=$RIG node tests/skills/scoped-human-mtls-e2e/scoped-human-smoke.cjs
   ```
   Expect `SCOPED-HUMAN-MTLS PASS`.
4. **Cleanup**: kill the daemon by listener PID, delete `$RIG`.

## What It Verifies

- The root cert (from `access setup`) can manage IAM
  (`POST /api/access/iam/user-client-grants` binds the two new
  fingerprints).
- `role:files-write` + fs roots: list/write/rename/delete allowed inside
  the roots; write outside, rename *destination* outside (source inside!),
  and delete outside all 403; `access.inspect` 403 (role ceiling).
- `role:files-write` has no `peer.use`: peer signaling relays *and* the
  message/task/approval quick controls 403.
- `role:operator` carries `peer.use`: the same peer routes clear the
  permission gate (404/503 for a ghost peer, never 401/403).
