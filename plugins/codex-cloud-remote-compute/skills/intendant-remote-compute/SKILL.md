---
name: intendant-remote-compute
description: In any Intendant-supervised native, Codex, Claude Code, Kimi Code, or Pi session where the remote_command tool is available, heavy platform-neutral development work — full compiles, broad test suites, workspace lint/clippy sweeps, benchmarks, code generation — MUST run through that tool instead of loading the local machine. Never silently fall back to heavy local work when the remote lane fails; report the failure or pick a genuinely cheap alternative. Cheap commands and small platform-specific checks stay local.
---

> Applicability check first: if the `remote_command` tool is not available
> to you — as a native tool, or through `"$INTENDANT" ctl tools call
> remote_command` under Intendant supervision — this skill does not apply:
> you are not running under an Intendant daemon with the Remote Compute
> plugin enabled. Say so if the task assumed otherwise, and work locally at
> normal discretion.

# Remote Compute: offload heavy work

## The rule

- Heavy, platform-neutral work — full compiles, broad test runs, workspace
  clippy, benchmarks, code generation — runs remotely via `remote_command`.
- Do NOT silently fall back to heavy local work if the cloud lane fails.
  Report the failure, or pick a genuinely cheap alternative (for example,
  `cargo check` on the one crate you touched).
- Cheap commands (focused unit tests, single-crate checks, quick greps and
  small builds) and small platform-specific checks stay local. Platform CI
  remains authoritative for platform-only behavior and for final
  cross-platform confidence.

## How to invoke

- Native Intendant sessions call the `remote_command` tool directly;
  external backends under supervision reach the same lane with
  `"$INTENDANT" ctl tools call remote_command`.
- Always run from an isolated git worktree.
- Omit the `host` argument so scheduling stays provider-neutral —
  reusing or acquiring a matching worker is the daemon's job.
- Iterate with `source: "working_tree"` (an explicit content-addressed
  snapshot of your local changes); run final, authoritative validation
  with `source: "git_revision"` plus a pushed `expected_revision`.
- When the daemon cannot infer the supervised worktree's provider branch,
  pass the pushed branch explicitly as `branch`. The worker must still report
  the requested `expected_revision`; a branch name never weakens that guard.
- For Rust work that should reuse compile outputs after worker replacement,
  request `cache: "durable_sccache"`. The default authenticated relay needs no
  cloud credentials, but it namespaces its cache repository by the supervised
  session's project root — an unsupervised call (e.g. an owner-shell acceptance
  run) has none and fails early with "durable_sccache through home requires a
  supervised project root". Run such calls from a supervised session, or omit
  `cache` (the `none` default) and accept a cold build. The job also fails
  early if sccache or the relay is unavailable.

## Waiting for an acquired worker

- `start` returns immediately. Keep its `job_id`, then use `status` or repeated
  `wait` calls (at most 60 seconds each) until the same job is terminal.
- A job in `acquiring` may still be creating a cold worker. Read
  `job.acquisition`: it names the stage, pushed branch, provider task id and
  URL, provider and attachment states, deadline, coalescing, and the latest
  provider-refresh error. Do not submit a duplicate merely because setup is
  slow. Matching environment/revision/branch requests already coalesce.
- Automatic acquisition allows one hour by default because a small cold worker
  can take tens of minutes to prepare. A terminal provider task fails early. An
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
