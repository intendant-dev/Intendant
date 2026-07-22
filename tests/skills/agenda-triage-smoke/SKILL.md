---
name: agenda-triage-smoke
description: >
  Operator scenario for the G3 triage mandate: against a scratch daemon
  with a seeded frontier (items parked from distinct project-rooted
  sessions plus existing hubs), one owner-approved triage run places N
  items under the right hubs, writes ONE triage:summary item with the
  ranked attention list, performs ZERO disposals, and an immediate re-run
  is a no-op that says so; the standing weekly manifest fires on cadence
  without re-approval. Verified from the durable op history and
  live-verified on the dashboard via the validate-dashboard rig recipe.
compatibility: Operator hardware, never CI (spawns a daemon and drives it
  end to end; the real-provider variant needs API keys). Keyless in the
  default mock form.
allowed-tools: Bash Read
---

# Agenda triage — live write-pattern smoke (G3)

The mandate under test is documented in `docs/src/agenda-and-memory.md`
("The triage mandate"): a standing item + G3-pre standing manifest whose
goal embeds the frontier-scoped placement + attention-curation mandate.

## What the run must demonstrate (the acceptance pattern)

Over a seeded agenda (two hubs for distinct projects; three frontier
items parked from sessions recorded under those projects; one item with
no project provenance; one open question), a single triage pass produces
exactly:

1. **Placements**: each project-provenance frontier item gains
   `part_of` under its matching hub (attributed to the triage session,
   `--source triage`); the provenance-less singleton stays unplaced with
   a `triage: no placement — standalone` annotation.
2. **Exactly one** new open item tagged `triage:summary`, its body
   listing every placement plus the ranked attention list (the open
   question ranks first).
3. **Zero disposals**: no complete/retire/clear-blocker/answer ops from
   the triage session; reminder policy untouched.
4. **Frontier drains**: `ctl agenda list --frontier` is empty after the
   run (the summary item is self-excluded by definition); an immediate
   re-run writes nothing and its occurrence write-back says
   "frontier empty, no action".
5. **Standing cadence**: the manifest's digest and approval are
   untouched by the runs; a second cadence instant fires without any
   re-approval (mock the wait or use a 15m cadence and `Run now` for the
   ad-hoc instant — `request_occurrence` leaves the approval intact).

## Recipe

```bash
WT=<this worktree>; BIN=$WT/target/debug/intendant
SCRATCH=$(mktemp -d); PROJA=$(mktemp -d); PROJB=$(mktemp -d)

# 1. Mock script: the triage profile executes the mandate verbatim —
#    read `list --all --json`, place the two project-A items under the A
#    hub and the B item under the B hub (ctl agenda place), annotate the
#    provenance-less item "triage: no placement — standalone", add ONE
#    summary item tagged triage:summary with the ranked list, exit done.
#    Match key: the mandate goal's first line ("Agenda triage pass.").
#    A second profile (matched on the same key, second occurrence) exits
#    immediately with "frontier empty, no action".

# 2. Boot the scratch daemon keyless (PROVIDER=mock,
#    INTENDANT_MOCK_DISPLAY=synthetic — the F4 smoke's incantation).

# 3. Seed: park items from sessions whose session_meta.json carries
#    project_root=$PROJA/$PROJB (the e2e create_session lane, or ctl
#    from within supervised sessions), plus:
#      CTL agenda add "Project A" --note   # hub by convention
#      CTL agenda add "Project B" --note
#      CTL agenda ask "Which palette for the relaunch?"
#      CTL agenda add "orphan idea: unify the pumps" --note
#      CTL agenda add "Triage" --body "<mandate text from the docs>"
#      CTL agenda schedule <triage-id> --goal "<mandate text>" \
#        --at +1m --every 15m --suspend-after 3
#    Owner shell: CTL agenda approve <triage-id> --digest <printed>

# 4. First instant fires on cadence (≤1m). Verify the five-point pattern
#    from `CTL --json agenda list --all` + `CTL agenda list --frontier`.
#    "Triage now": CTL agenda start <triage-id> — fires the SAME approved
#    digest via request_occurrence; assert digest/approval unchanged and
#    the second run's write-back says frontier empty.

# 5. Dashboard live-verify (the Track J rig recipe): open
#    /app?token=$(cat $SCRATCH/.intendant/loopback-tokens/<port>.token),
#    Agenda tab — hub roll-up chips show the placements, the summary item
#    renders, the Attention lens leads with the open question, and the
#    standing chip shows "every 15m". validate-dashboard drives it as a
#    polled state-machine via --wait-for-function.
```

The mock form proves machinery + markers end to end (real binaries, real
daemon, real op log, real frontier lens). To judge a real model's
obedience to the mandate — the never-list above all — re-run with a real
provider and the documented template goal verbatim; the op history is
the audit either way: a correct run is placements + annotations + one
summary + zero disposals.
