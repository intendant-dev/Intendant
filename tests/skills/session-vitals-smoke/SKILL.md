---
name: session-vitals-smoke
description: Keyless end-to-end smoke for the session-vitals git + cache segments — mock-provider daemon in a scripted git repo, asserts the merged session_vitals event on the control socket (branch/dirty/ahead/parity plus cache hit/TTL)
---

# Session-vitals smoke (git + cache segments)

Verifies the vitals rails end-to-end against real binaries: the
`GitVitalsProber` in `session_vitals.rs` probes the daemon's project root,
the cache listener folds the mock provider's cache-bearing usage (via the
native `UsageSnapshot` derivation in `tui/app.rs`), and the hub emits the
merged `AppEvent::SessionVitals` on change, reaching control-socket
clients with the expected values in both sections. No API keys, no
network, ~10 seconds.

```bash
cargo build --release
node tests/skills/session-vitals-smoke/driver.cjs "$PWD/target/release/intendant"
```

The driver builds a scripted repo (main + feature branch one ahead, one
dirty file), spawns a mock-provider daemon **with the repo as cwd** and an
initial task (idle web startups take the daemon path, which spawns no
control socket), connects, then **dirties a second file** — the producer
emits on change only, and the startup emission always races the socket
client, so the assertion rides the next 5s probe tick. Expects
`branch=feature dirtyFiles=2 ahead=1 behind=0 primaryRef=main
mergeParity=clean` **plus the cache section** from the startup task's
usage: the mock's first request writes cache only (`hitPct=0`,
`ttlSeconds=300`) and the hub's merged snapshot carries it alongside the
git change.

Gotchas encoded:

- Spawn cwd must be the scripted repo — the prober probes the daemon's
  project root, not the driver's temp dirs.
- Change-only emission: never assert on the startup emission; mutate the
  tree after connecting and wait for the re-emission.
