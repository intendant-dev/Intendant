---
name: landing-shepherd-rig
description: Prove the landing shepherd live — a DIRTY armed PR wakes its owning session within one poll interval, and an ownerless DIRTY armed PR parks one needs-you agenda item. Scratch repo + stub gh + mock provider; keyless, network-free, never touches the real box state.
---

# Landing-shepherd rig

The unit tests pin the transition classifier table and the wake/park
fallback order (`landing_shepherd.rs` inline tests). This rig proves the
LIVE half on real binaries: the bounded `gh` poll notices a DIRTY armed
PR and delivers within one poll interval, through the real lanes — the
task lane into a supervised session's transcript, and the agenda store
for the seat-gone fallback.

## Run

```bash
cargo build --bin intendant --bin intendant-runtime
node tests/skills/landing-shepherd-rig/driver.cjs target/debug/intendant
```

Expect `LANDING SHEPHERD RIG PASS` and exit 0 in ~15 s.

## What the driver builds

- A scratch `HOME` (so `~/.intendant` state, logs, and the agenda store
  are all rig-local) and a scratch git repo checked out on
  `rig-armed-seat`, with a ref-only `rig-ghost-seat` branch nobody owns.
- A stub `gh` first on `PATH`: `repo view` → `rig/landing`, `pr list` →
  a state file the driver flips mid-run, `api graphql` → `mergeQueue:
  null`. The shepherd's whole GitHub view is this stub; no network.
- A mock-provider daemon (`PROVIDER=mock`) started in the repo with
  `INTENDANT_LANDING_SHEPHERD_POLL_MS=1000`, plus one supervised seat
  session created over the control socket (`create_session`) — the
  session records + `git worktree list` join makes it the owner of
  `rig-armed-seat`.

## The proof

1. With the fixture empty, the shepherd ticks quietly.
2. The driver flips the fixture: PR #101 (`rig-armed-seat`) and PR #102
   (`rig-ghost-seat`), both `DIRTY` + `CONFLICTING` with auto-merge
   armed and settled checks.
3. Within one poll interval (+3 s subprocess slack) the daemon logs
   `woke session … PR #101` and `parked needs-you item … PR #102`.
4. Delivery is then verified beyond the log lines: the wake text
   (`[landing-shepherd] Your PR #101 …` with the merge-never-rebase
   ritual) appears in the seat's transcript and the seat runs the
   follow-up turn (the mock's second step carries
   `expect_transcript_contains` for the same text); the
   `Landing needs you: PR #102` item appears in the agenda store; and
   PR #101 was NOT parked (wake beats park while the seat lives).

## Boundaries this rig respects

- Observe-and-wake only: nothing in the rig merges, resolves, or
  re-arms; the stub never sees a mutating `gh` verb (`gh-calls.log` in
  the scratch HOME records every invocation for inspection).
- Keyless (`PROVIDER=mock`), display-free, and network-free; safe on a
  fleet box. The scratch HOME strips `INTENDANT_HOME` /
  `INTENDANT_COORDINATION_DIR` from the environment so a rig run inside
  a supervised session cannot write the real daemon's state.
