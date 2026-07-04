---
name: files-ide-peer-e2e
description: >
  Live two-daemon test of the Files-tab editor against a federated peer: pair
  two isolated daemons on localhost, scope the inbound peer identity to
  file-operator with a single write root, then drive a real browser through
  daemon A's dashboard to browse/edit/save files on daemon B — including
  cross-daemon conflict detection and both IAM denial paths (write outside
  write_roots, read outside all roots), verified on disk and in B's
  [peer-fs] audit trail.
compatibility: Requires playwright (npm) with its chromium download, a release
  build of intendant, and two free localhost ports. No model calls, no API
  keys — both daemons run idle.
allowed-tools: Bash Read
disable-model-invocation: false
---

# Files-Tab Editor: Federated Peer E2E

## Purpose

Unit tests prove the enforcement functions; this proves the *product*: the
full chain browser → daemon A (signaling relay) → WebRTC dashboard-control
tunnel → daemon B, with B alone deciding what the peer identity may touch.
It exists because the first live run of exactly this scenario caught a real
gap unit tests missed: `PeerDashboardControlSignal` was classified
`SessionInspect`, so file-scoped peer profiles could never open the tunnel
their `api_fs_*` methods ride on (fleet peers all ran operator/admin
profiles, hiding it).

## What It Verifies

- Peer pairing via `intendant peer invite` / `peer join` between two daemons
  with fully isolated identities (separate `HOME`s).
- The dashboard-control tunnel door opens for a `file-operator` profile
  (`profile_allows_dashboard_control_tunnel`) and the editor lists, opens,
  and saves files on the peer through `api_fs_list/stat/read` +
  `api_fs_write` upload frames.
- Optimistic concurrency across daemons: a save with a stale sha256 after
  B's file changed on disk returns `409 code:"conflict"`, surfaced as the
  Reload/Overwrite banner; reload recovers.
- Enforcement on the receiving daemon: a write outside B's `write_roots`
  and a read outside all roots are refused (nothing lands on disk), the UI
  shows B's reason verbatim, and B's session log carries matching
  `[peer-fs] denied` audit lines (plus `allowed` lines for the good ops).

## Procedure

All paths below live under a scratch `RIG` directory. Ports: A=18800
(plain HTTP for the browser), B=18801 (normal TLS/mTLS).

1. **Rig layout**
   ```bash
   export RIG=/tmp/files-ide-peer-rig BIN=$PWD/target/release/intendant
   mkdir -p $RIG/{home-a,home-b,proj-a,proj-b,peer-files/sub,outside}
   printf '# Peer note\n\nEdited from another daemon soon.\n' > $RIG/peer-files/peer-note.md
   printf 'port = 18801\n' > $RIG/peer-files/sub/settings.toml
   printf 'secret outside the grant\n' > $RIG/outside/secret.txt
   ```
2. **B: access CA + invite** (isolated `HOME` = isolated daemon identity)
   ```bash
   HOME=$RIG/home-b $BIN access setup --name peer-b --port 18801 --ip 127.0.0.1 --no-serve-certs
   HOME=$RIG/home-b $BIN peer invite --card-url https://127.0.0.1:18801 --label rig-a > $RIG/invite.txt
   ```
3. **A: join** (writes `[[peer]]` into `proj-a/intendant.toml`)
   ```bash
   cd $RIG/proj-a && HOME=$RIG/home-a $BIN peer join "$(tail -1 $RIG/invite.txt)" --label peer-b
   ```
4. **Scope B's inbound identity** — invites default to `peer-operator`;
   tighten to the profile under test. Edit the single JSON under
   `$RIG/home-b/.intendant/access-certs/peer-access-identities/`:
   set `"profile": "file-operator"` and
   `"filesystem": {"read_roots": [], "write_roots": ["$RIG/peer-files"]}`.
5. **Launch both** (idle daemons; keyless boot is fine)
   ```bash
   cd $RIG/proj-b && HOME=$RIG/home-b $BIN --web 18801 --bind 127.0.0.1 &
   cd $RIG/proj-a && HOME=$RIG/home-a $BIN --web 18800 --bind 127.0.0.1 --no-tls &
   ```
   Wait until `http://127.0.0.1:18800/api/peers` shows
   `"state": "connected"`.
6. **Drive the browser**
   ```bash
   NODE_PATH=<repo>/node_modules RIG=$RIG node tests/skills/files-ide-peer-e2e/peer-ide-smoke.cjs
   ```
   Expect `PEER-IDE-SMOKE PASS: 12 steps` and screenshots under `$RIG/`.
   (The three newest steps rename and delete on the peer through
   `api_fs_rename`/`api_fs_delete`, including the cross-root rename denial
   — B must refuse a destination outside `write_roots` even though the
   source is inside.)
7. **Audit trail** — B's session log must show the tunnel-lane trail:
   ```bash
   grep -rh 'peer-fs' $RIG/home-b/.intendant/logs/*/session.jsonl | tail
   ```
   Expect `allowed` lines for the list/read/write ops and `denied` lines
   for the outside-roots write and read.
8. **Cleanup**: kill both daemons (by listener PID, never pkill), delete
   `$RIG`.

## Notes

- The single-daemon (local target) variant of this smoke needs no rig:
  launch one daemon `--no-tls --bind 127.0.0.1` and run
  `node scripts/validate-dashboard.cjs --port <p> --path /app
  --wait-for-function "$(cat local-ide-smoke-expr.js)"` — the expression
  drives open/edit/save/conflict/reload/create through
  `window.intendantDashboardFilesIde._debug*` and verifies in-page.
- Keep `--no-tls` off daemon B: the peer lane must exercise real mTLS.
- If the tunnel times out at "peer target selected", check B's log for
  `[ws] denied peer dashboard-control signaling` — that is the tunnel door
  refusing the profile, which is exactly the regression this scenario
  exists to catch.
