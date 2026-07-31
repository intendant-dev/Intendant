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

# Attach the live worker to this daemon over mTLS (see "Attaching" below).
intendant codex-cloud attach task_e_... --home-url wss://your-host:8765 --send

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

## Attaching a worker to home (mTLS enrollment ceremony)

A worker has no inbound reachability and no durable identity, so attaching
it inverts the usual peer pairing. Home runs the whole ceremony:

```bash
# Mint a single-use token bound to the task and deliver the attach prompt
# as a follow-up turn into the warm worker:
intendant codex-cloud attach task_e_... --home-url wss://your-host:8765 --send

# Or print the prompt to deliver by hand (task page, initial submit):
intendant codex-cloud attach task_e_... --home-url wss://your-host:8765
```

The composed prompt tells the worker to run `intendant codex-cloud agent`
through `run-worker.sh` (task-local state), with the token on stdin — never
argv. The agent then:

1. generates a keypair in its task-local state root (the private key never
   leaves the worker; the daemon signs a public key, never mints one);
2. redeems `{token, public key}` at home's public
   `POST /api/codex-cloud/enroll` route and receives a client certificate
   whose identity record carries the system-issued **`cloud-worker`**
   profile and a hard expiry (`--identity-ttl-s`, default 3600);
3. dials home's gateway over mTLS, pinned to the fingerprint baked into
   the prompt, and holds the socket in the foreground. The accepted socket
   *is* the attachment: the lease flips `connected` while it lives and
   `disconnected` when it dies.

Security shape: the enrollment token is stored hashed, minutes-TTL
(`--token-ttl-s`, default 900), bound to one task, and burned atomically on
first redemption — an unknown, used, or expired token refuses identically,
and the public doorbell is rate-limited. The `cloud-worker` profile is
recognized by every enforcement path but grants **no** operation at all
(strictly less than presence-only) and is never operator-assignable; the
gateway routes an attaching cloud-worker socket to the attachment lane
before any dashboard grant exists, so the certificate authenticates exactly
one thing: this heartbeat. Authority over the worker flows home→worker in
later slices; the worker's inbound authority on home stays nothing.

The worker's egress must be able to reach `--home-url`: in a
network-restricted Codex Cloud environment, add the home host to the
environment's allowlist or the dial is refused before TLS.

## Terminal on a live worker

A `connected` lease renders a **Terminal** button on its Cloud-card row (and
appears in the Terminal tab's host picker as `cloud:<task_id>`). The shell
runs **inside the worker**: the dashboard's ordinary terminal frames, after
clearing the same per-frame IAM gate a local shell demands (opening also
requires `shell.spawn`), are bridged by this daemon over the task's
attachment socket; the worker serves them from its own PTY registry and
streams output frames back. The browser only ever talks to home — a worker
behind a cloud egress allowlist can never terminate a direct browser
connection, so the peer-terminal WebRTC path deliberately does not apply.

Fail-closed rules: the bridge forwards only terminal *request* kinds to the
worker, accepts only terminal *reply* kinds back (anything else off the
socket is dropped — the worker's inbound authority on home stays nothing),
and routes replies solely to dashboard subscribers of that `cloud:` host.
Worker shells are unscoped (`ShellSpawnPolicy.scope = None`): the container
is the sandbox and home's principal owns the worker, so the Landlock/
Seatbelt scoped-shell machinery never engages. Sessions survive a socket
reconnect (the worker keeps its registry across redials) and die with the
task turn or the identity expiry. Sharing is refused on cloud hosts, and
the bridge rides the dashboard tunnel only — the legacy `/ws` fallback
serves local terminals exclusively. Because the tunnel is the only lane,
opening a cloud terminal starts the dashboard-control transport **on
demand** when it is not already up (the default browser posture keeps the
legacy `/ws` as the event lane and no tunnel): the transport comes up as a
data lane, the event-lane preference is untouched, and the queued open
replays automatically once it connects. On the worker, each terminal key
keeps exactly one output forwarder — a re-open (browser reload, host
round-trip) replaces the listener on the surviving PTY rather than
stacking a second one, which would double every output chunk.

## Live view of a worker (display)

A `connected` lease also renders a **View** button: the worker starts (or
attaches) a virtual display — Xvfb on a Linux container (installed by
`setup.sh`; the synthetic test card under the mock rig pair) — captures it
with the standard X11 backend, and streams it through the real tile
pipeline into a **single-subscriber socket stream** over the attachment
(`DisplaySession::spawn_tile_socket_stream`: tile mode only, no video
fallback, whole-snapshot delivery, 10 s re-anchor). Home bridges the
frames onto the dashboard tunnel (`display_open`/`display_close` under
`display.view`; tile frames arrive as `display_tiles`), and the Cloud
card's viewer paints them with the same transport-agnostic tile
compositor the peer display path ships. Pointer and keyboard input ride
the existing `display_input` frame (`display.input`) with a
`cloud:<task_id>` host, delivered to the worker's ordered input queue and
injected via XTEST.

WebRTC is deliberately absent from this path: a cloud worker can neither
accept nor dial a peer connection (no inbound reachability; passive-only
ICE-TCP; UDP-only TURN), so the attachment socket is the one lane —
which also means the worker never runs video encoders at all
(`disable_video_bank`): tiles are pure-Rust encodes, and capture idles to
keepalive cadence when no viewer is subscribed. The same fail-closed
rules as the terminal apply: only display reply kinds cross back from
the worker, routed solely to the viewing connection.

## Computer use on a live worker

`execute_cu_actions` accepts `display_target: "cloud:<task_id>"`: the whole
batch inverts over the task's attachment as one `cu_execute` frame, the
worker runs the standard CU executor against its own display session
(screenshots and input ride the session — the same virtual display the
View button streams — with an unscoped actor, since the container is the
sandbox), and the correlated `cu_result` carries the reduced outcome
home: per-action status lines in the usual `ok`/`injected`/`failed`
vocabulary, the observation description, and the trailing screenshot.
Normalized `coordinate_space` batches are denormalized on the worker
against its own display size. The MCP caller is gated on home exactly
like a local CU call; the worker applies no further gates (home's
authority over the worker is total), and `cu_result` joins the closed
reply-kind allowlist like every other worker frame. A worker without a
display capability answers with the named error from the display
resolver rather than guessing.

## Provider-neutral remote commands

An acquired or explicitly attached worker can run a bounded, non-interactive
command for any supervised agent backend. The public tool is named
`remote_command`, not
`codex_cloud_shell`: Codex, Claude Code, Kimi Code, Pi, and native Intendant
sessions all receive the same job vocabulary in their compact/core tool
profile. Codex Cloud is only the first host adapter.

The tool has four operations: `start`, `status`, `wait`, and `cancel`.
`start` returns immediately with a daemon-local job id; `status` polls it,
`wait` waits for at most 60 seconds, and `cancel` terminates the worker
process tree. Omitted `host` (or `host: "auto"`) reuses a live worker whose
environment and exact Git revision match, or submits and enrolls one. An
operator can still select an already-connected lease with
`host: "cloud:<task-id>"`:

```bash
intendant ctl tools call remote_command --args '{
  "op": "start",
  "argv": ["cargo", "test", "-p", "intendant-core"],
  "expected_revision": "0123456789abcdef",
  "require_clean": true,
  "timeout_s": 900
}'

intendant ctl tools call remote_command \
  --args '{"op":"wait","job_id":"remote-...","wait_s":30}'
```

Uncommitted or not-yet-pushed source uses an explicit working-tree snapshot:

```bash
intendant ctl tools call remote_command --args '{
  "op": "start",
  "source": "working_tree",
  "argv": ["cargo", "check", "--workspace"],
  "cache": "durable_sccache",
  "timeout_s": 900
}'
```

The contract is intentionally stricter than an interactive terminal:

- Commands are transported as an argv array and are not implicitly parsed by
  a shell. `cwd`, when present, is repository-relative and cannot escape the
  selected checkout. Explicit environment entries are additions to the
  worker's agent-phase environment.
- `source: "git_revision"` is the default and requires
  `expected_revision`. The worker refuses a different checkout; abbreviated
  object ids are accepted from 7 hexadecimal characters.
  `source: "working_tree"` captures a binary Git patch plus non-ignored
  untracked regular files relative to `expected_revision`, or
  `INTENDANT_REMOTE_COMPUTE_BASE_REF` (default `origin/main`) when omitted.
  Capture runs twice and proceeds only when the content-addressed bytes match.
  Ignored files — notably `.env` and `target/` — do not cross the attachment.
  The compressed transfer is capped at 64 MiB (128 MiB expanded, 16 MiB per
  untracked file, 4096 untracked files), chunked over mTLS, and verified by
  digest before the worker materializes an isolated Git worktree.
- `require_clean` defaults to true. For a pushed revision it means a clean
  checkout; for a snapshot it means unchanged from the exact selected
  snapshot. Commands using one snapshot are serialized. A command that
  mutates selected source invalidates that prepared workspace; ignored build
  outputs such as `target/` remain warm and reusable.
- `cache: "durable_sccache"` is explicit and rejects a local backend at
  setup. Home maps only
  dedicated `INTENDANT_REMOTE_CACHE_` settings for sccache and its documented
  backend credential families (`SCCACHE_*`, `AWS_*`, `ACTIONS_*`,
  `ALIBABA_CLOUD_*`, and `TENCENTCLOUD_*`) into the mTLS command, requires an
  external backend (a task-local `SCCACHE_DIR` is refused), configures
  `RUSTC_WRAPPER=sccache`, `CARGO_INCREMENTAL=0`, and a stable
  `SCCACHE_BASEDIRS`, then reports hit/miss/write/error deltas. Backend
  configuration is inherited by the requested build/test child because a
  sccache client automatically restarts a missing server; carrying the same
  configuration prevents that restart from quietly selecting the default
  local-disk cache. Startup also rejects a server whose stats identify a
  local-disk backend. The default is `cache: "none"`. The Cloud environment
  must provide sccache 0.14 or newer; explicit durable-cache jobs fail before
  the requested command when it is absent or the server/backend cannot start.
  During a running build, sccache can still compile uncached after a backend
  read/write error; the result reports those error counters rather than
  pretending every compilation was cached.
- Stdout and stderr are each bounded to 128 KiB. On overflow the result keeps
  the first 32 KiB and the latest tail and marks that stream truncated.
  Timeout, cancellation, and attachment loss terminate the owned process
  tree. The result reports the exact terminal state, exit code when one
  exists, worker revision, duration, and whether the checkout became dirty.
- Jobs are owned by the supervised session that started them. Another
  session receives the same not-found response as an unknown id; an
  unrestricted local owner surface may inspect them. Jobs are in daemon
  memory and do not survive a home-daemon restart.
- The worker accepts command start/cancel frames only from its authenticated
  home attachment. Home accepts only the correlated result frame back; the
  cloud-worker identity still has zero authority over home. MCP call-time IAM
  additionally requires `shell.spawn`.

Automatic acquisition requires `INTENDANT_CODEX_CLOUD_ENVIRONMENT` and the
reachable `INTENDANT_CODEX_CLOUD_HOME_URL`; the environment bootstrap must
install the matching Intendant binary and allow egress to home. Optional
`INTENDANT_REMOTE_COMPUTE_BRANCH` selects the provider checkout,
`INTENDANT_REMOTE_COMPUTE_ACQUIRE_TIMEOUT_S` bounds attachment wait, and
`INTENDANT_REMOTE_COMPUTE_IDLE_TIMEOUT_S` controls retirement. Concurrent
requests for the same environment/revision/branch coalesce into one
acquisition. Only workers created by this daemon process are auto-retired;
manually attached workers are never retired behind their operator's back.
Acquisition state is process-local, so a daemon restart may leave an acquired
task until its expiring cloud-worker identity and provider turn end.

## Attachment lifecycle

The enrollment broker above records the attachment state for its workers;
an external broker or operator can also record it manually:

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
  recording `connected` again restarts the clock — a crashed broker cannot
  leave a lease `connected` forever.
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

The subtab also carries the **submit form** — prompt, environment id,
optional branch/attempts/title, exactly the `exec` verb's parameters.
Submitting posts `POST /api/codex-cloud/submit` (tunnel twin
`api_codex_cloud_submit`, Task-classed like every start-agent-work
surface), which rides the same `submit_task` lane as the CLI verb and the
MCP tool: the daemon host's authenticated Codex CLI creates the task and
the worker lease is recorded before the response returns, so the form's
immediate follow-up sync lists the new task even while the provider window
lags. Environment suggestions derive from the tracked leases (there is no
provider env-list verb); the last environment submitted from that browser
is remembered locally.

## MCP tools

The daemon's full MCP tool profile exposes the provider operations
`list_codex_cloud_workers`, `submit_codex_cloud_task`, and
`follow_up_codex_cloud_task`. This lets an Intendant agent refresh worker
state, delegate a Codex task, or continue one — the full warm-builder loop
(submit → probe → follow up → pull) is drivable end to end by an agent using
the daemon host's authenticated Codex CLI. These provider operations are
intentionally omitted from the compact/core tool profile; agents can discover
and invoke them through `intendant ctl tools list` and `intendant ctl tools
call`. The list tool reports the same shape as `list --json` (window,
tracked-active, cursor, transitions) plus how many agenda notes it parked; the
follow-up tool returns the acceptance receipt (parent turn, new turn ids)
under the same fail-closed contract as the CLI verb.

`remote_command` is different: it does not create provider work or ask Codex
to reason. It spends shell authority on an attached compute host, so it is in
the compact/core profile used by every supervised backend and is IAM-classed
as `shell.spawn`. Claude Code, Kimi Code, Pi, and Codex therefore use the same
attached Linux worker without changing which model is doing the reasoning.

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

## Cache strategy on ephemeral workers

The 189 s → 2.8 s warm result above came from a surviving task-local
`target/`; it is valuable but disposable. The cold-resume observation proved
that a replacement worker can lose the entire ignored tree. Treat this like
an ephemeral hosted CI runner:

- Keep repeated commands on the same attached task while it remains warm.
- Put stable toolchain/package preparation in the environment setup cache.
- Use `cache: "durable_sccache"` with a separately scoped external backend
  when cold replacement performance matters. A task-local sccache directory
  is refused because it is lost with the same worker as `target/`.
- Do not promise a fully warm Rust build from sccache alone. Existing
  cross-worktree measurements show it can repopulate identical dependency
  outputs, but local incremental workspace crates, build scripts,
  non-cacheable crate types, test binaries, and final links still need a
  fresh target tree. Correctness never depends on cache hits, and the job
  result exposes the measured cache deltas instead of claiming warmth.

The daemon-side prefixes are a custody boundary, not new sccache option
names: for example, `INTENDANT_REMOTE_CACHE_SCCACHE_BUCKET` becomes
`SCCACHE_BUCKET`, and `INTENDANT_REMOTE_CACHE_AWS_ACCESS_KEY_ID` becomes
`AWS_ACCESS_KEY_ID` only inside the authenticated remote command. Consult
sccache's upstream [configuration reference](https://github.com/mozilla/sccache/blob/main/docs/Configuration.md)
for backend-specific variables and its [Rust caveats](https://github.com/mozilla/sccache/blob/main/docs/Rust.md)
for what can and cannot be cached.

The worker container is still a shell-authority boundary, not a credential
enclave: the requested command inherits the cache configuration, and another
same-UID process could inspect it too. Use a cache-only principal restricted
to the one cache namespace, never a general AWS/account credential or any
daemon/provider credential.

The practical split is therefore: Linux workers run platform-neutral
`cargo check`, tests, clippy, code generation, and other heavy computation;
the platform CI matrix remains authoritative, including the macOS runner.
Agents should still run small, targeted macOS checks when a change directly
touches Apple APIs, entitlements, the app bundle, or the repository's
macOS-only deterministic WASM artifact path. Remote Linux success reduces
feedback time; it does not replace CI.

## Current boundary

This integration covers automatic reuse/acquisition, explicit working-tree
snapshots, durable compiler-cache configuration, the job/control plane, the
safe setup/maintenance contract, and the enrollment ceremony that attaches a
live worker to home over mTLS. The attachment carries terminal, tile display,
computer-use, bounded source-transfer, and provider-neutral remote-command
frames in addition to liveness; each direction has a closed allowlist, and
worker replies are never dispatched as authority on home. Workers are
ephemeral enrollments with zero-authority expiring identities, not static
`[[peer]]` registry entries, and home must be reachable from the worker's
egress allowlist (there is no relay tier).

Two boundaries remain deliberate. Linux results do not replace the
cross-platform CI matrix or a small macOS-specific check when Apple APIs,
entitlements, bundles, or deterministic macOS-generated WASM are touched.
And remote compilation does not reduce the resident RAM used by Claude,
Codex, Kimi, or Pi themselves on the supervisor; it moves their heavy child
build/test processes, not their reasoning process.
