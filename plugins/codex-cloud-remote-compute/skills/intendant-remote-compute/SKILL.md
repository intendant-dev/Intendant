---
name: intendant-remote-compute
description: In ANY session on a machine run by an Intendant daemon — a supervised native, Codex, Claude Code, Kimi Code, or Pi session, an unsupervised coding-harness session, or a subagent — heavy platform-neutral development work (full compiles, workspace builds, broad test suites, workspace lint/clippy sweeps, benchmarks, code generation) MUST be offloaded through Intendant's remote-compute lane instead of loading the local machine. The trigger is the WORK, not tool possession — if you are about to run a heavy build/check/clippy/test sweep locally, read this first; `intendant ctl remote` reaches the lane from every session kind. Never silently fall back to heavy local work when the remote lane fails; report the failure or pick a genuinely cheap alternative. Cheap commands, small platform-specific checks, and validation batteries that assert non-root permission behavior stay local.
---

# Remote Compute: offload heavy work

## The rule

- Heavy, platform-neutral work — full compiles, workspace builds, broad test
  runs, workspace clippy, benchmarks, code generation — runs remotely
  through the daemon's provider-neutral remote-compute lane.
- Do NOT silently fall back to heavy local work if the cloud lane fails.
  Report the failure, or pick a genuinely cheap alternative (for example,
  `cargo check` on the one crate you touched).
- Cheap commands (focused unit tests, single-crate checks, quick greps and
  small builds) and small platform-specific checks stay local. Platform CI
  remains authoritative for platform-only behavior and for final
  cross-platform confidence.
- Remote workers run as root: test suites that assert non-root permission
  or sandbox behavior (permission-injection tests, sandbox-denial
  assertions) fail there, so a validation battery containing such tests
  stays local — offload the platform-neutral compile/check/clippy volume
  around it instead.

## How to invoke — `intendant ctl remote` (every session kind)

The primary lane is the CLI verb family; it works from supervised sessions
and plain harness/owner shells alike, with real flags instead of hand-built
JSON:

```bash
git push origin my-branch    # the worker checks out a PUSHED revision
"$INTENDANT" ctl remote start --branch my-branch --revision <sha> \
    -- cargo clippy --workspace -- -D warnings
"$INTENDANT" ctl remote wait remote-<id> --for 1800
```

- Resolve the CLI: supervised sessions receive `INTENDANT` (absolute
  controller path) in their environment — use `"$INTENDANT" ctl remote …`.
  Unsupervised shells use `intendant ctl remote …` against the local
  daemon. `ctl remote --help` teaches every flag.
- `start` returns the job id immediately; `wait JOB --for SECONDS` blocks
  until the job is terminal (exit 0 only for success, remote output
  printed), `status` polls once, `cancel` stops it. Refusals and failures
  surface the daemon's words verbatim.
- Identity rides the normal ctl lanes: inside a supervised session the
  injected `INTENDANT_MCP_URL` binds your session, so the daemon resolves
  your recorded project root (this is what `--cache durable_sccache`
  namespaces by). Unsupervised callers run without a project root — omit
  `--cache` and accept a cold build, and pass `--branch` explicitly.
- Always run from an isolated git worktree, and omit `--host` so
  scheduling stays provider-neutral — reusing or acquiring a matching
  worker is the daemon's job.
- Iterate with `--source working_tree` (an explicit content-addressed
  snapshot of your local changes; needs a supervised session's project
  root); run final, authoritative validation with the default
  `git_revision` source plus a pushed `--revision`.
- When the daemon cannot infer the supervised worktree's provider branch,
  pass the pushed branch explicitly as `--branch`. The worker must still
  report the requested revision; a branch name never weakens that guard.

Native Intendant sessions can call the built-in `remote_command` tool
directly instead — the same job vocabulary (`op: start|status|wait|cancel`)
behind the ctl verbs. External supervised backends no longer carry the MCP
schema in their toolsets; `"$INTENDANT" ctl remote` is their lane.

## What `--cache durable_sccache` honestly does

- For Rust work that should reuse compile outputs after worker replacement,
  request `cache: "durable_sccache"`. The default authenticated relay needs no
  cloud credentials, but it namespaces its cache repository by the supervised
  session's project root — an unsupervised call (e.g. an owner-shell acceptance
  run) has none and fails early with "durable_sccache through home requires a
  supervised project root". Run such calls from a supervised session, or omit
  `cache` (the `none` default) and accept a cold build. The job also fails
  early if sccache or the relay is unavailable.

## Waiting for an acquired worker

- A job in `acquiring` may still be creating a cold worker. Read
  `job.acquisition` (printed by `ctl remote status`/`wait`): it names the
  stage, pushed branch, provider task id and URL, provider and attachment
  states, deadline, coalescing, and the latest provider-refresh error. Do
  not submit a duplicate merely because setup is slow. Matching
  environment/revision/branch requests already coalesce.
- Automatic acquisition allows one hour by default because a small cold worker
  can take tens of minutes to prepare. Prefer one `ctl remote wait JOB
  --for 3600` over resubmitting. A terminal provider task fails early. An
  acquisition timeout leaves the provider task running and reports its URL; do
  not claim that it was cancelled.

## What to expect from caching (the honest version)

- An exact repeat of the same command on the same worker/task can reuse its
  `target/` and finish in seconds.
- A different snapshot/revision, or a replacement worker, may be cold. A
  durable compiler cache can recover many identical compile outputs, but not
  every build script, workspace/incremental artifact, test binary, crate
  type, or final link.
- Remote compute is primarily a capacity and isolation win — it removes
  contention and crash pressure from the local machine. It is NOT a promise
  that one cold command is faster; cold full builds can take tens of minutes
  on small workers.
- Keep provider/cache warmth separate from connectivity. `warmth` estimates
  cache or worker continuity; only `remote_compute_usable: true` proves this
  daemon currently has a live command channel. A task can look warm while its
  attachment is offline.

## Conduct on failure

- If the lane errors (no worker available, environment missing, enrollment
  broken), report the acquisition stage, task URL, provider status, and last
  provider error when present, then continue with cheap local steps only.
  Escalate the lane failure rather than quietly running the expensive thing
  locally.
- A "Not signed in" / signed-out failure means the provider lease died —
  leases do not survive daemon restarts, so the lane is down for everyone
  until it is re-armed from the Vault tab. The daemon parks one lane-down
  agenda note on the first such failure per boot; say so in your report
  rather than treating it as your session's private problem.

## Provider plumbing (rarely needed)

The lane's first host adapter is Codex Cloud. Worker-lease diagnosis and
operator-side task management live on the `intendant codex-cloud` CLI
family (`doctor`, `list`, `status`, `attach`, `pull`, …) and the
`docs/src/codex-cloud-workers.md` chapter — reach for them only when the
lane itself misbehaves; submitting and scheduling workers is the daemon's
job, not yours.
