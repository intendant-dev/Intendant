---
name: agenda-housekeeping-smoke
description: >
  Operator scenario for the F4 agenda-housekeeping recipe: against a
  scratch daemon with a seeded agenda, an owner-approved housekeeping
  session executes the propose-don't-dispose mandate and the run is
  verified from the durable op history — N annotations + exactly ONE
  summary item + one next-run proposal + ZERO disposals (no
  complete/retire of other actors' items, no blocker clears). Runs
  keyless under the mock provider (the mock script IS the mandate-obedient
  session); swap in a real provider to also judge model obedience.
compatibility: Operator hardware, never CI (spawns a daemon and drives it
  end to end; the real-provider variant needs API keys). Keyless in the
  default mock form.
allowed-tools: Bash Read
---

# Agenda housekeeping — live write-pattern smoke (F4)

The recipe under test is documented in `docs/src/agenda-and-memory.md`
("The housekeeping recipe"): one ordinary item carries the scheduled
session whose goal embeds the propose-don't-dispose mandate; recurrence is
each run re-proposing the next pass for one-click owner approval.

## What the run must demonstrate (the acceptance pattern)

Over a seeded agenda (a stale task, a blocked task with an uncleared
blocker, an unanswered question), one housekeeping pass produces exactly:

1. **N ≥ 2 annotations** on existing items (staleness/evidence notes),
   attributed to the housekeeping session (`agent_session` + its sid);
2. **exactly one** new open item titled `Housekeeping summary …`;
3. **one** fresh `propose_effect` revision on the housekeeping item
   (the next pass, awaiting owner approval — nothing armed);
4. **zero disposals**: every pre-existing item keeps its status; the
   blocker stays uncleared (evidence arrives as an annotation instead).

## Recipe

```bash
WT=<this worktree>; BIN=$WT/target/debug/intendant
SCRATCH=$(mktemp -d); PROJ=$(mktemp -d); : > "$PROJ/intendant.toml"

# 1. Mock script: the housekeeping profile executes the mandate verbatim
#    (annotate two items via id prefixes it reads from `list --json`,
#    add ONE summary, re-propose the next pass, done). See
#    scripts snippet in the F4 PR description, or write your own — the
#    match key is the mandate goal's first line.

# 2. Boot: env -i HOME=$SCRATCH PATH=$PATH PROVIDER=mock \
#      INTENDANT_MOCK_SCRIPT=$SCRATCH/mock.json INTENDANT_MOCK_DISPLAY=synthetic \
#      $BIN --web 0 --bind 127.0.0.1 --no-tui --no-tls --autonomy full &
#    (port from the Dashboard: line; CTL() = env -i HOME=$SCRATCH PATH=$PATH \
#      $BIN ctl --url http://127.0.0.1:$PORT/mcp)

# 3. Seed: CTL agenda add "stale: rotate the fleet certs" --task
#          CTL agenda add "blocked: enable gpt-live-1 path" --task
#          CTL agenda block <blocked-id> "gpt-live-1 available on the API"
#          CTL agenda ask "Keep the v1 shim past the soak?"
#          CTL agenda add "Agenda housekeeping" --task

# 4. Fire: CTL agenda start <housekeeping-id>   # owner shell = owner surface
#    (or: schedule --at +1m, review, approve — the timed path)

# 5. Verify from the DURABLE history once last_run.state == completed:
#    CTL --json agenda list --all | <assert the four-point pattern above>
#    plus: the housekeeping item's effect has approval == null on the NEW
#    digest (next pass proposed, not armed) and last_run on the OLD one.
```

The mock form proves the machinery end to end (real binaries, real
daemon, real op log). To judge a real model's obedience to the mandate,
re-run with a real provider and the documented template goal verbatim,
then apply the same four-point verification — the op history is the
audit either way.

## Reference run

2026-07-20, this worktree, mock form: 2 annotations (attributed to the
spawned session), 1 summary item, 1 next-pass proposal awaiting
approval, 0 disposals, blocker uncleared. Verification script:
`f4-housekeeping-smoke.sh` in the session scratchpad (reproduced in the
F4 PR description).
