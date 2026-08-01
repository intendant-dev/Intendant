---
name: credential-watch-rig
description: Keyless rig for the out-of-band credential watch — scratch auth file + stub claude CLI under a fast poll; proves an account switch made outside Intendant is detected within one poll interval and delivered as the one info notification, secret-free
---

# Credential-watch rig (out-of-band change detection)

Card 01KYT12BZ1G8V4XKVCYFCSQ5JT's rig leg: the daemon's credential
watch (`credential_watch.rs`) must notice a backend re-authentication
made directly at the CLI — no ceremony, no backend announce — within
one poll interval, and deliver exactly the surface the card names (era
mint + reload candidates ride the same fire; those are pinned by unit
tests, this rig proves the live detection loop end to end). No API
keys, no network, no real credential store touched, ~10 seconds. Unix
(the stub CLI is a `/bin/sh` script).

```bash
cargo build --release --bin intendant --bin intendant-runtime
node tests/skills/credential-watch-rig/driver.cjs "$PWD/target/release/intendant"
```

The driver builds a temp `$HOME` holding a scratch
`~/.claude/.credentials.json` (fake bytes — the watch must never read
them) and a stub `claude` whose `auth status` answers with whatever
identity a state file names; the project's `intendant.toml` points
`[agent.claude_code] command` at the stub, and
`INTENDANT_CREDENTIAL_POLL_MS=400` makes the interval rig-fast. It
spawns a mock-provider daemon with a trivial task, waits for the
watch's baseline tick to adopt account A, then — the out-of-band act —
flips the stub to account B and rewrites the scratch auth file,
exactly what a direct `claude` re-login does. It asserts on the
control socket:

- a `user_notification` with id `credential-change-claude-code`
  arrives within one poll interval (+ probe/delivery slack) of the
  rewrite;
- the copy names the switch (`now signed in as rig-b@example.com`,
  `(was rig-a@example.com)`) and the reload story, at `info` urgency;
- no scratch-file bytes appear in the notification (labels only).

Gotchas encoded:

- `HOME` is the temp dir, so the default `~/.claude` resolution is
  exercised without touching the real box; `CLAUDE_CONFIG_DIR` /
  `CODEX_HOME` are stripped from the spawn env so an operator shell
  can't redirect the rig.
- The stub bakes the state-file path in absolutely: the identity
  probe runs under the external-child env policy (base allowlist
  only), so env-var plumbing into the stub would be silently dropped.
- The baseline wait matters: the watch's first tick adopts the
  starting identity from Unknown (the temp state root has no
  persisted era), and only a probe-confirmed CONFLICT fires — flip
  the stub before the baseline lands and the rig sees no change at
  all.
