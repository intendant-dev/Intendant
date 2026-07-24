# Codex Cloud workers

Intendant treats a Codex Cloud container as an **ephemeral worker lease**, not
as a permanent federated peer. The provider task and a live network attachment
are separate pieces of state:

- **Provider state** comes from the authenticated local `codex cloud` CLI:
  queued, running, finished, failed, cancelled, or unknown.
- **Attachment state** describes optional live access to the task container:
  not requested, awaiting, connected, disconnected, or expired.

This distinction matters because a task can become `ready` or `error` while a
container is still reachable for a short time. Conversely, the provider can
reclaim a container without producing the orderly disconnect expected from a
durable Intendant daemon. A terminal provider task therefore never proves that
a live attachment still exists.

```text
 provider lane (codex cloud CLI)         attachment lane (broker/operator)
 ───────────────────────────────         ─────────────────────────────────
 queued → running → finished             not requested → awaiting → connected
                  ↘ failed                    │               │        │
                  ↘ cancelled                 │      terminal task      │ TTL lapses or
                                              │      or broker loss     │ terminal + stale
                                              ▼               ▼        ▼
                                          (unchanged)   disconnected → expired
```

## What a worker really is (runtime model)

Empirical testing (2026-07-24) sharpened the model. Three kinds of state must
never be conflated:

| Layer | What it is | What we observed |
|---|---|---|
| Environment/setup cache | Prepared container state from the setup script | Materialized into *separate* workers (different hostname/boot id) with identical dependency content; this is what the documented "up to 12 hours" covers |
| Task workspace state | The repo diff and filesystem artifacts of one task and its follow-ups | A warm same-task follow-up kept a 336 MB ignored cargo `target/` and ran an identical build **68x faster** (189s → 2.8s) |
| Live worker state | CPU, RAM, PIDs, sockets, tunnels | Warm continuity measured across ~8 minutes (same hostname, boot id, PID 1, inodes); allocation beyond a turn is *not* guaranteed |

The first controlled **cold-resume checkpoint** (one run, 2026-07-24; not yet
a measured boundary) sharpened the downside: after ~34 quiet minutes a
same-task follow-up landed on a *replacement* worker — different hostname,
boot id, and PID 1 — and every tested ignored/task-created path was gone
(`target/`, `/root`, `/tmp` probes, heartbeat, the 336 MB cargo tree); only
the selected repository revision survived (`selected_state_only`). One
observation does not establish an eviction timeout, but it proves the loss
mode is real: **an external build cache is a requirement for reuse that must
survive replacement, not an optimization.**

Consequences Intendant encodes:

- **An environment is a template, not a machine.** New tasks — sequential or
  concurrent — get isolated workers; nothing crosses `/root`, `/tmp`, or
  process state between tasks. Cross-task build reuse needs a remote cache,
  not wishful thinking about a shared box.
- **Same-task follow-ups are the warm lever.** `intendant codex-cloud
  followup` (below) reuses the same warm worker and its ignored build
  artifacts while the worker survives. Keep repeated work in one task — and
  treat anything that must outlive a possible replacement as needing `pull`
  or an external cache.
- **Warmth is tracked honestly.** Every lease derives `warm` (actively
  running, or last activity within ~10 minutes — just past the measured
  window), `unknown` (through the 12-hour setup-cache horizon), or `cold`.
  Refreshes detect web-driven follow-ups as terminal → running → terminal
  flaps and count completed turns. The label is a heuristic from measured
  behavior, never a guarantee — the observed ~34-minute eviction sits well
  inside the `unknown` band, which is exactly why that band does not claim
  warmth. (`probe --task` below is the cheap way to *measure* instead of
  guessing; more checkpoints may later tighten these windows.)
- **The 12-hour figure is the setup cache, not the worker.** Put expensive,
  stable toolchain work in `setup.sh` where the cache amortizes it across
  workers; keep task-specific mutable state out of it.

### Worker fingerprints

`intendant codex-cloud probe --env <ENV_ID>` submits a canned diagnostic
task whose only output is one file: a single-line JSON fingerprint
(hostname, boot id, PID 1 start, toolchain, sizes), measured fresh on every
probe turn. The fingerprint travels in the task diff — the one channel the
CLI reliably exposes — and refresh collects it automatically whenever a
probe task finishes a turn. `pull` also parses a fingerprint
opportunistically from any diff that carries one.

`intendant codex-cloud probe --task <TASK_ID>` is the cross-turn instrument:
it re-probes an *existing* task with a follow-up turn that rewrites the
fingerprint file. Matching `hostname` + `boot_id` + `pid1_start` confirm the
same booted worker; a mismatch against the recorded fingerprint is a
**detected cold replacement** — the displaced fingerprint moves into the
lease's `worker_history` and `cold_replacements_observed` increments (shown
by `status` and the dashboard card). This turns the runtime findings'
cold-resume methodology into a one-command check.

## Controller commands

The commands below use the user's existing Codex CLI authentication. Intendant
does not copy Codex credentials into the cloud container, and every provider
subprocess runs in a disposable working directory (the upstream CLI writes an
account-bearing `error.log` into its cwd).

```bash
# Verify the CLI and Cloud authentication.
intendant codex-cloud doctor

# Submit a task. Use -- to keep the task prompt separate from wrapper flags.
intendant codex-cloud exec \
  --env <environment-id> \
  --branch feature/example \
  -- "Run the requested checks"

# Refresh the provider-owned lease store.
intendant codex-cloud list
intendant codex-cloud list --json

# Inspect a tracked task or its diff.
intendant codex-cloud status task_e_...
intendant codex-cloud diff task_e_...

# Bring a finished task home (see below).
intendant codex-cloud pull task_e_...

# Fingerprint a worker; re-probe a task to detect cold replacement.
intendant codex-cloud probe --env <environment-id>
intendant codex-cloud probe --task task_e_...

# Continue a finished task with a new turn (see "Follow-ups" below).
intendant codex-cloud followup task_e_... -m "Now also fix the tests"

# Drop terminal leases with no live attachment (default: older than 7 days).
intendant codex-cloud prune
intendant codex-cloud prune --all
```

`list` shows the provider's current window **plus** any tracked lease with a
live attachment (`awaiting`/`connected`) that has fallen out of that window —
liveness outlives the provider's list. Each row carries the derived warmth
label (`warm`/`unknown`/`cold`; `--json` and the daemon lanes serialize it as
`warmth`). When the provider returns a pagination cursor, `list` prints the
ready-made `--cursor` invocation for the next page, and `--json` carries it
as `cursor`.

The lease store defaults to
`$XDG_DATA_HOME/intendant/codex-cloud/leases.json` (or the platform data
directory). `INTENDANT_CODEX_CLOUD_STATE` overrides the exact path, and
`INTENDANT_CODEX_COMMAND` overrides the Codex executable. Every
read-modify-write of the store takes a sidecar advisory file lock
(`leases.json.lock`), so concurrent CLI invocations, the daemon's MCP tools,
and the dashboard route cannot clobber each other's updates — and each
terminal transition is observed by exactly one refresher.

## Pulling results home

`pull` closes the loop: it fetches the task's unified diff through the Codex
CLI (in the disposable directory, never inside your repository) and applies it
with `git apply --3way` onto a fresh branch in a new git worktree under
`.intendant/worktrees/`:

```bash
intendant codex-cloud pull task_e_...              # branch codex-cloud/task_e_...
intendant codex-cloud pull task_e_... --attempt 2  # best-of-N: pick an attempt
intendant codex-cloud pull task_e_... --branch fix/cloud-result --dir ../review
```

Nothing is committed: the worktree is left for review, and a conflicted
three-way apply leaves standard conflict markers with the conflicting paths
listed. A diff that applies nowhere removes the branch and worktree again. The
upstream `codex cloud apply` command is deliberately not wrapped — it would
either run in the disposable cwd (a no-op) or inside your repository (the
`error.log` hazard); piping `diff` into our own `git` sidesteps both.

## Follow-ups: continuing a task's warm worker

A follow-up appends a new user turn to an existing task, and it is *the* warm
lever: an active turn holds the task's worker, and a worker that kept its
ignored build artifacts rebuilt an identical tree 68× faster in the 2026-07-24
measurements. The product supports follow-ups, but the public Codex CLI has no
verb for them (its Cloud surface is `exec`/`status`/`list`/`apply`/`diff`;
upstream issue [#24777](https://github.com/openai/codex/issues/24777) is an
unassigned proposal). `followup` therefore rides the same private backend the
web UI uses — empirically validated end to end — under a deliberately
conservative contract:

```bash
intendant codex-cloud followup task_e_... -m "Now also fix the tests"
intendant codex-cloud followup task_e_... --json < prompt.txt   # stdin keeps prompts out of shell history
```

- **Auth is the Codex CLI's own ChatGPT login** (`auth.json` under
  `$CODEX_HOME`, default `~/.codex`) — no browser, no cookies, no separate
  credential. The bearer token and account id are read into process memory
  for the two requests and are never printed, logged, or serialized into
  receipts. API-key-only Codex auth cannot drive Cloud follow-ups.
- **Idle tasks only, serialized per task.** A fresh provider refresh (or, for
  tasks outside the list window, the upstream status verb) must show the task
  terminal before anything is sent; a per-task sidecar lock serializes
  concurrent invocations machine-wide.
- **The parent turn is resolved immediately before sending** from the task
  detail's `current_turn_id`, and must be an assistant turn — the validated
  behavior is HTTP 404 for anything else.
- **Fail closed on drift.** HTTP 404/409/422, a missing `current_turn_id`, or
  a 200 response that no longer references the task are reported as
  compatibility breaks of the private schema — never retried around. When
  upstream ships an official follow-up command, prefer it and retire this
  lane.
- **The lease learns immediately:** an accepted follow-up records a running
  edge (warmth stays warm, and the next refresh's terminal edge counts the
  turn), and the receipt carries the new turn ids the response referenced.

`INTENDANT_CODEX_CLOUD_BACKEND` overrides the backend base URL (tests point
it at a local stub; the default is the production web backend).

## Attachment lifecycle

An attachment broker or operator records the independent attachment state:

```bash
intendant codex-cloud attachment task_e_... awaiting
intendant codex-cloud attachment task_e_... connected
intendant codex-cloud attachment task_e_... disconnected
```

Refreshes age attachments by three rules:

- `awaiting` or `disconnected` on a terminal task becomes `expired` — the
  broker is gone or will never arrive.
- `connected` carries `attached_at_unix_ms` and expires after a staleness TTL
  (`INTENDANT_CODEX_CLOUD_ATTACH_TTL_S`, default 3600) unless re-asserted:
  recording `connected` again restarts the clock. This is the stopgap until a
  broker owns liveness — a crashed broker cannot leave a lease `connected`
  forever.
- `connected` within the TTL survives even a terminal provider task, because
  reachability must be checked independently of provider state.

## Terminal transitions land on the Agenda

Whoever refreshes the store and observes a task leave the live states —
`queued`/`running` → `finished`/`failed`/`cancelled` — parks a note on the
daemon's [Agenda](./agenda-and-memory.md): the task title, its URL, and the
ready-made `pull` command. The store lock guarantees each edge is observed
exactly once, so one finished task produces one note, whichever lane (CLI,
MCP tool, dashboard) happened to see it first. The bare CLI parks through the
local daemon's lane when a daemon is up; without one, the printed notice is
the whole delivery. A task first seen already-terminal is history, not an
edge, and is never parked.

## Dashboard card

The dashboard's **Sessions → Cloud** subtab renders the lease store: provider
chip and attachment chip per lease (independent, like everything else here),
the provider's task link, and the ready-made `pull` and `followup` commands
for terminal tasks. The default paint is a cached read; **Sync with provider** hits
`GET /api/codex-cloud/workers?refresh=1`, which re-syncs through the daemon
host's Codex CLI and parks agenda notes for any transitions it observes. A
failed sync degrades to the cached view with the error shown — the card never
goes blank because the provider CLI is missing.

## MCP tools

The daemon's full MCP tool profile exposes `list_codex_cloud_workers`,
`submit_codex_cloud_task`, and `follow_up_codex_cloud_task`. This lets an
Intendant agent refresh worker state, delegate a task, or continue one —
the full warm-builder loop (submit → probe → follow up → pull) is drivable
end to end by an agent using the daemon host's authenticated Codex CLI.
These tools are intentionally omitted from the compact/core tool profile;
agents can discover and invoke them through `intendant ctl tools list` and
`intendant ctl tools call`. The list tool reports the same shape as
`list --json` (window, tracked-active, cursor, transitions) plus how many
agenda notes it parked; the follow-up tool returns the acceptance receipt
(parent turn, new turn ids) under the same fail-closed contract as the CLI
verb.

## Environment bootstrap

Generate the bundle from the same Intendant revision used by the controller:

```bash
intendant codex-cloud bootstrap --output ./intendant-codex-cloud
```

Paste `setup.sh` and `maintenance.sh` into the matching fields in the Codex
Cloud environment settings. They are intentionally split by lifecycle:

1. `setup.sh` installs Intendant and the task-time launcher. It either builds
   the checked-out repository with Cargo or downloads a binary when both
   `INTENDANT_CLOUD_BINARY_URL` and its mandatory
   `INTENDANT_CLOUD_BINARY_SHA256` are configured.
2. `maintenance.sh` refreshes the installation after a cached container resumes,
   clears the per-user task-runtime directory, and creates a new boot nonce.
3. `run-worker.sh` runs only during the agent phase. It creates fresh XDG and
   Intendant state roots under `$XDG_RUNTIME_DIR` (or a per-user `/tmp`
   directory), then `exec`s the supplied foreground command without shell
   re-parsing.

The scripts can also be printed for direct pasting:

```bash
intendant codex-cloud bootstrap --print setup
intendant codex-cloud bootstrap --print maintenance
intendant codex-cloud bootstrap --print worker
```

Codex Cloud runs setup and maintenance in shells which finish before the agent
phase. Do not start the Intendant daemon, Chisel, or another attachment
supervisor there. Start the worker launcher in a task-owned background terminal
and keep the supervisor in the foreground of that terminal:

```bash
~/.local/libexec/intendant-cloud/run-worker.sh -- <supervisor> <args...>
```

The supervisor is deployment-specific. It may start an Intendant daemon and an
outbound tunnel or connect an edge transport to a broker, but it must:

- use a one-time, short-lived enrollment credential;
- keep its identity and certificates inside the launcher's task-local runtime
  state;
- keep the public relay/domain allowlist exact;
- remain in the foreground so task cancellation tears it down;
- report connection loss so the controller expires the attachment.

The bootstrap scripts deliberately do not embed relay credentials, private
keys, static peer identity, AWS details, or a fixed reverse port. Codex Cloud
environment caches may be reused for up to 12 hours — that figure covers
*prepared container state*, which is materialized into fresh workers, not a
promise that any particular worker stays allocated — and Business/Enterprise
caches can be shared by users with access to the environment. Secrets are
available to setup scripts but are removed before the agent phase, so cached
setup state is the wrong place for a per-task identity or enrollment secret.
See the official [Codex Cloud environments
documentation](https://learn.chatgpt.com/docs/environments/cloud-environment).

## Toward visual workers: display streaming and computer use

Nothing about a cloud worker changes Intendant's display architecture — it
only moves the daemon to the far side of an attachment. Once a supervisor
inside the container starts an Intendant daemon and connects it out through
the enrollment broker, that daemon is an ordinary (if short-lived) federated
peer: the [peer display pipeline](./peer-federation.md) can stream a display
it owns, and [computer use](./computer-use-and-audio.md) can drive that
display, exactly as on any headless Linux box.

```text
 Codex Cloud container                     your machine
 ┌───────────────────────────────┐        ┌───────────────────────────┐
 │ run-worker.sh (task identity) │        │ Intendant daemon          │
 │  └─ supervisor (foreground)   │        │  ├─ worker lease store    │
 │      ├─ intendant daemon      │◄──────►│  ├─ Agenda (transitions)  │
 │      │   ├─ virtual display   │ tunnel │  └─ dashboard             │
 │      │   │   (Xvfb + CU)      │ (one-  │      └─ Sessions → Cloud  │
 │      │   └─ WebRTC encoder    │  time  │          live tile view   │
 │      └─ outbound transport    │  cred) │          + input          │
 └───────────────────────────────┘        └───────────────────────────┘
```

The pieces already exist per-box: virtual display management lives in
`crates/intendant-platform` (`vision.rs`), capture/encode in
`intendant-display`, and the cross-machine tile stream plus remote input in
the peer federation layer. What gates it is the same thing that gates any
live attachment — the enrollment broker minting one-time credentials and a
relay route, because a container behind the provider's egress proxy can only
dial out (agent-phase network is allowlisted, so expect forced-TCP transport
rather than UDP ICE). Until the broker exists, the lease store, the agenda
notes, and the dashboard card above are deliberately the shipped surface: the
job/control plane is reliable today, and the display lane composes onto the
attachment lane instead of pretending each Cloud task is a static `[[peer]]`.

## Current boundary

This integration covers the reliable job/control plane and the safe
setup/maintenance contract. The attachment state is ready for a broker to own,
but Intendant does not yet mint one-time enrollment credentials or allocate
multi-tenant relay routes. Until that broker exists, live attachment remains an
explicit deployment-specific command rather than pretending each Cloud task is
a static `[[peer]]`.
