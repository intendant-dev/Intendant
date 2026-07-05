# Peer-sessions federation smoke

End-to-end proof of the peer sessions rail (task #38): two real daemons
federate, a task is delegated through the primary, and the primary's
`/api/peers` (plus optionally the Station scene) must carry the peer's
folded per-session state.

Keyless — both daemons run the scripted mock provider (`PROVIDER=mock`).
Not in CI because it binds fixed local ports (18777/18778), spawns real
daemons, and the `--browser` leg drives a real headless Chrome.

## Run

```bash
cargo build --release          # the rig runs target/release/intendant
python3 tests/skills/peer-sessions-smoke/peer-rig.py             # API proof
python3 tests/skills/peer-sessions-smoke/peer-rig.py --browser   # + Station probe
```

`--browser` needs Chrome at the standard macOS path and a CDP helper at
`~/projects/smoke-agent-unification/cdp.py` (eval/nav/shot against port
9333); it also stretches the delegated task's sleep step to hold a probe
window open, and drops a screenshot next to the rig's scratch dir
(`PEER_RIG_SCRATCH` overrides the default temp dir).

## What it asserts

- Daemon B federates to daemon A via `POST /api/peers` (card URL only,
  `--no-tls` localhost) and delegates a task via
  `POST /api/peers/{id}/task` — the instructions select A's mock profile.
- B's `PeerSnapshot.sessions` grows the delegated child: label from the
  instructions, `started_at`, phase `working` folded from `TurnStarted`,
  then `done` from `TaskComplete`.
- The completed child **lingers as done** (persistent-daemon parity —
  no `SessionEnded` on natural completion).
- A's own primary session arrives stamped `is_primary` **with git
  vitals** (proj-a is a dirty git repo): this exercises the
  connect-time per-session state replay, since the prober's only
  emission predates B's connection.
- `--browser`: the Station scene snapshot contains the
  `peer-session-<host>-<sid>` node (phase `running`, task label) and the
  peer node carries the primary session's git chip.
- Stopping the child on A (`{"action":"stop_session"}` over A's `/ws`)
  retires the folded entry on B.

## Diagnostics when it fails

- `<scratch>/home-{a,b}/daemon.log` — daemon stderr/stdout.
- `<scratch>/home-b/.intendant/logs/*/peers.jsonl` — every
  `TaggedPeerEvent` B's transport upcast (the durable record of exactly
  which peer events crossed; if `session_updated` is missing here, the
  fold never received the wire events).
- `<scratch>/home-a/.intendant/logs/<sid>/session.jsonl` — what the
  delegated session actually did on A.
