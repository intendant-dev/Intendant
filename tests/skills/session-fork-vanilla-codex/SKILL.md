---
name: session-fork-vanilla-codex
description: >
  Live gate for the codex anchor-fork engine on the VANILLA codex binary:
  fork a real codex session from a mid-history turn boundary through the
  daemon (fork-points catalog → ForkSessionAtAnchor → spawned child),
  verify the child's history is the trimmed prefix, the parent rollout is
  untouched, and the anchor-fork lineage edge appears at the child's
  identity announce. Run before shipping changes to
  session_fork/codex_stage.rs, the codex start_thread fork branch, or when
  bumping the pinned codex version.
compatibility: Requires vanilla codex on PATH with auth in ~/.codex, a built daemon from this worktree, and an existing multi-turn codex session to fork (create a scratch one if needed). Not CI — real backend, real tokens.
allowed-tools: Bash Read
disable-model-invocation: false
---

# Vanilla-codex anchor-fork live gate

The CI e2e proves the native lane; the codex lane's backend interplay
(`thread/fork{path}` on a staged copy + pre-first-turn `thread/rollback`)
can only be proven against a real app-server. The wire primitives
themselves are pinned by `tests/skills/session-fork-probes/spike1`.

## Procedure

1. Build and start a scratch daemon from this worktree (own port):
   `cargo build --bin intendant --bin intendant-runtime` then
   `./target/debug/intendant --no-web` is NOT enough — use
   `./target/debug/intendant "idle" --web <throwaway port>` or an existing
   dev daemon you own.
2. Create (or pick) a codex session with ≥3 turns; note its backend id
   from the Sessions tab or `~/.codex/sessions` (`session_meta.payload.id`).
3. Catalog: `curl -s localhost:<port>/api/session/<backend-id>/fork-points | jq
   '.fork_points[] | select(.kind=="turn-boundary")'` — pick a mid-history
   turn (e.g. `turn:1` of 3).
4. Fork over the ws intent lane (or the dashboard once PR F lands):
   ```json
   { "action": "fork_session_at_anchor", "source": "codex",
     "session_id": "<backend-id>",
     "anchor": { "kind": "turn-boundary", "turn": 1 },
     "task": "summarize where we are", "request_id": "skill-1" }
   ```
5. Verify, in order:
   - `session_fork_result` arrives with `error: null` (child id is null —
     codex children announce later).
   - A new wrapper session spawns and its first turn answers with ONLY
     turn-1 knowledge (the summarize prompt is the probe).
   - `~/.codex/sessions` gains a new rollout whose `forkedFromId` is the
     staged copy's id; the PARENT rollout's bytes are unchanged (hash it
     before/after).
   - The Sessions list shows the child with an `anchor-fork` relationship
     to the parent once the child announces (the overlay's
     `fork_anchor` records the chosen anchor).
   - `~/.intendant/fork_staging/` holds the staged copy (swept after 7d).
6. Managed-binary variant (optional, needs the patched fork configured as
   `[agent.codex] managed_command` + `managed_context = "managed"`): fork
   from an `item-anchor` point and verify the child's cut is item-exact
   rather than turn-rounded.

## Known caveats

- `thread/rollback` is deprecated upstream (0.144.4 notice): if a codex
  bump removes it, this skill fails at step 5's trimmed-history check —
  see the probes skill for the tripwire and note the successor API.
- Vanilla forks do not inherit the prompt-cache lineage key (managed-fork
  bonus): the child's first turn is cache-cold. Cost, not correctness.
