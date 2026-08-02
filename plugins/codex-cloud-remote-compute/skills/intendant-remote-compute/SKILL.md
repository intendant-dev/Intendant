---
name: intendant-remote-compute
description: Heavy platform-neutral development work — full compiles, broad test suites, workspace lint/clippy sweeps, benchmarks, code generation — MUST run through the remote_command tool instead of loading the local machine. Never silently fall back to heavy local work when the cloud lane fails; report the failure or pick a genuinely cheap alternative. Cheap commands and small platform-specific checks stay local.
compatibility: Requires an Intendant-supervised session with the remote_command tool available (Remote Compute plugin enabled on the daemon; any backend — native, Codex, Claude Code, Kimi Code, Pi).
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
- Request `cache: "durable_sccache"` only when the daemon has configured
  a durable cache relay.

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

## Conduct on failure

- If the lane errors (no worker available, environment missing, enrollment
  broken), say so in your report and continue with cheap local steps only.
  Escalate the lane failure rather than quietly running the expensive thing
  locally.
