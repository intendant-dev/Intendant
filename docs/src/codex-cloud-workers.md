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

## Controller commands

The commands below use the user's existing Codex CLI authentication. Intendant
does not copy Codex credentials into the cloud container.

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
```

The lease store defaults to
`$XDG_DATA_HOME/intendant/codex-cloud/leases.json` (or the platform data
directory). `INTENDANT_CODEX_CLOUD_STATE` overrides the exact path, and
`INTENDANT_CODEX_COMMAND` overrides the Codex executable.

An attachment broker or operator can update the independent attachment state:

```bash
intendant codex-cloud attachment task_e_... awaiting
intendant codex-cloud attachment task_e_... connected
intendant codex-cloud attachment task_e_... disconnected
```

The daemon's full MCP tool profile also exposes
`list_codex_cloud_workers` and `submit_codex_cloud_task`. This lets an
Intendant agent refresh worker state or delegate a task using the daemon
host's authenticated Codex CLI. These tools are intentionally omitted from
the compact/core tool profile; agents can discover and invoke them through
`intendant ctl tools list` and `intendant ctl tools call`.

When a disconnected attachment's provider task is terminal, the next refresh
changes it to `expired`. Connected attachments on terminal tasks remain visible
with a warning because reachability must be checked independently.

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
environment caches may be reused for up to 12 hours, and Business/Enterprise
caches can be shared by users with access to the environment. Secrets are
available to setup scripts but are removed before the agent phase, so cached
setup state is the wrong place for a per-task identity or enrollment secret.
See the official [Codex Cloud environments
documentation](https://learn.chatgpt.com/docs/environments/cloud-environment).

## Current boundary

This integration covers the reliable job/control plane and the safe
setup/maintenance contract. The attachment state is ready for a broker to own,
but Intendant does not yet mint one-time enrollment credentials or allocate
multi-tenant relay routes. Until that broker exists, live attachment remains an
explicit deployment-specific command rather than pretending each Cloud task is
a static `[[peer]]`.
