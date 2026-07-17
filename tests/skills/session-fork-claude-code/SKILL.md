---
name: session-fork-claude-code
description: >
  Live gate for the claude-code anchor-fork engine: fork a real CC session
  from a mid-history anchor and from an inactive sibling branch tip
  through the daemon (fork-points catalog → ForkSessionAtAnchor → spawned
  child), verify the child resumes with exactly the sliced history, the
  parent transcript is byte-identical, and the anchor-fork lineage edge
  lands. Run before shipping changes to session_fork/claude_surgery.rs or
  claude_tree.rs, or when bumping the pinned Claude Code version.
compatibility: Requires Claude Code on PATH with auth, a built daemon from this worktree, and a scratch CC project dir. Costs a few haiku turns. Not CI — real backend.
allowed-tools: Bash Read
disable-model-invocation: false
---

# Claude-code anchor-fork live gate

The surgery semantics (chain-slice, filename-stem resume, dead pins,
boundary behavior) are pinned by `tests/skills/session-fork-probes/spike2`;
this skill proves the daemon-integrated path end to end.

## Procedure

1. Build + start a scratch daemon from this worktree on its own port.
2. Seed a scratch CC session with distinguishable rounds (the probes'
   codeword pattern works: round one BLUEFIN, round two MARIGOLD via the
   dashboard composer or `claude -p` + `--resume` in a scratch project),
   supervised by the daemon so the overlay records its project root.
3. Catalog: `curl -s localhost:<port>/api/session/<cc-uuid>/fork-points | jq
   '.fork_points[]'` — expect `head`, `msg:*` turn boundaries, and
   `tip:*` rows for any abandoned sibling branches.
4. Fork before round two:
   ```json
   { "action": "fork_session_at_anchor", "source": "claude-code",
     "session_id": "<cc-uuid>",
     "anchor": { "kind": "message", "message_uuid": "<turn-1 tail uuid>" },
     "task": "Which codewords do you know? Answer with just the codewords.",
     "request_id": "skill-cc-1" }
   ```
5. Verify, in order:
   - `session_fork_result` carries the child uuid immediately (the surgery
     mints it) with `error: null`.
   - The child's answer names ONLY the round-one codeword.
   - The parent transcript file is byte-identical (hash before/after);
     the child `<uuid>.jsonl` sits in the SAME project dir with every
     line's `sessionId` rewritten.
   - The Sessions list shows the `anchor-fork` edge parent→child; the
     child's overlay records `forked_from` + `fork_anchor`.
6. Branch-tip variant: pick a `tip:*` fork point (create one by editing a
   message in the parent first if none exists) and verify the child
   resumes that abandoned branch's content.
7. Pre-compaction variant (when a compacted parent is at hand): fork a
   `pre_compaction: true` anchor and verify the child loads the full
   pre-compact history (the slice omits the boundary) and still
   auto-compacts normally later.

## Known caveats

- The child spawns in the parent's recorded project root; a parent that
  never ran under Intendant has no overlay root and falls back to the
  daemon default — if that differs from the transcript's project dir, CC
  cannot resolve the resumed id (structured spawn failure, no data risk).
- Subagent sidecar dirs (`<uuid>/subagents/`) are not copied in v1;
  sidechain content referenced only there is absent from the child.
