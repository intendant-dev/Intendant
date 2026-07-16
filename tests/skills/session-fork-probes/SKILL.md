---
name: session-fork-probes
description: >
  Validation probes for the fork-session-from-anchor feature: prove the
  external-backend primitives the fork engines depend on still behave as
  built. Spike 1 drives a vanilla codex app-server through
  thread/fork{path} + thread/rollback{numTurns} + resume of the forked
  child. Spike 2 proves Claude Code copied-transcript forking (chain-slice
  surgery) and pins the resume-leaf semantics of the installed CC version.
  Run when bumping the pinned codex/CC versions or when forked sessions
  misbehave.
compatibility: Requires vanilla codex on PATH with auth in ~/.codex, Claude Code on PATH with auth, and at least one existing codex rollout under ~/.codex/sessions. Costs a few haiku -p calls (pennies) plus zero-turn codex RPC.
allowed-tools: Bash Read
disable-model-invocation: false
---

# Session-fork validation probes

The fork-from-anchor engines (`src/bin/caller/session_fork/`) rest on two
externally-owned behaviors that no unit test can pin. These probes are the
regression harness for them, first run 2026-07-16 against codex-cli 0.144.4
and Claude Code 2.1.211 (both PASS).

## Spike 1 — vanilla codex fork + rollback + resume

```bash
SRC=$(find ~/.codex/sessions -name 'rollout-*.jsonl' -mmin +60 -size -2000k \
      -print0 | xargs -0 ls -t | head -1)
python3 tests/skills/session-fork-probes/spike1_codex_fork.py "$SRC" 1
```

Proves, against the **vanilla** binary (no managed fork required):
- `thread/fork {threadId:"", path:<staged copy>}` mints a new thread whose
  metadata carries `forkedFromId` (the source rollout's session id — the
  session catalog derives fork lineage from this for free).
- `thread/rollback {numTurns}` is accepted on the fresh forked child
  *before any turn*; the child rollout records an appended
  `thread_rolled_back` event_msg (append-only, same convention the anchor
  scanner already resolves).
- The child resumes on a fresh app-server via `thread/resume {threadId}`.
- The parent rollout is byte-identical afterwards (hash-checked); the
  child materializes as a normal rollout under `~/.codex/sessions`.

Known caveats observed on 0.144.4:
- `thread/rollback` emits a **deprecation notice** ("will be removed
  soon"). If a version bump removes it, the vanilla fork path needs the
  successor API — this probe failing on rollback is the early warning.
- `thread/read {includeTurns:true}` does not return a top-level `turns`
  list; verify trim effects from the child rollout file instead.

The probe prints the child thread id + rollout path it created; delete
that rollout afterwards if you don't want the artifact in the session list.

## Spike 2 — Claude Code chain-slice fork surgery

```bash
python3 tests/skills/session-fork-probes/spike2_cc_fork.py
```

Builds a scratch two-turn haiku session in /tmp/fork-spike-proj, then runs
four copy-only fork probes (parent hash-verified untouched):

| probe | construction | expected |
|---|---|---|
| A | prefix-truncated copy, sessionId rewritten, new uuid filename | knows only turn 1 |
| B | full copy + legacy `last-prompt` pin at turn-1 tail | knows BOTH turns (pins-dead sentinel) |
| C | synthetic `compact_boundary` spliced before turn 2 | knows only turn 2 |
| D | chain-slice at the pre-boundary anchor from C's file | knows only turn 1 |

What these pin (CC 2.1.211 semantics the surgery engine assumes):
- Resume resolves a session by the `<uuid>.jsonl` **filename stem**; a
  copied file with per-line `sessionId` rewritten resumes first-class.
- **`last-prompt` leaf pins are dead** — resume walks the message chain
  back from the tail. Fork-from-anchor therefore means **chain-slice**:
  emit only the anchor's ancestor chain (plus uuid-less meta lines like
  `ai-title`/`agent-name`), which makes the anchor the tail. If probe B
  ever reports turn-1-only, CC honors pins again — revisit
  `session_fork/claude_surgery.rs`.
- A `compact_boundary` line with `parentUuid: null` severs the chain walk;
  chain-slicing at a **pre-boundary** anchor simply never includes the
  boundary, so pre-compaction anchors fork with no extra surgery and the
  child is a normal session (auto-compaction intact). The old
  `compact_boundary_disabled` trio is dead and unnecessary.

The script exits non-zero on any unexpected verdict and prints the exact
cleanup command for the scratch project dir.
