#!/usr/bin/env python3
"""Spike 2: prove a Claude Code session can be forked from a mid-history
anchor by copy-only file surgery, and pin the resume-leaf semantics of the
installed CC version.

Findings this probe encodes (first proven on CC 2.1.211, 2026-07-16):
  - resume resolves the session purely by <uuid>.jsonl filename stem in the
    project dir; a copied file with per-line sessionId rewritten to the new
    uuid resumes as a first-class session.
  - legacy `last-prompt` leaf pins are IGNORED (files no longer contain
    them); resume walks the message chain back from the tail. Forking from
    an arbitrary anchor therefore means CHAIN-SLICE: emit only the anchor's
    ancestor chain (+ uuid-less meta lines), making the anchor the tail.
  - a `compact_boundary` system line with parentUuid:null severs the chain
    walk (pre-boundary history is not loaded).
  - chain-slicing at a PRE-boundary anchor naturally omits the boundary, so
    pre-compaction anchors fork with no extra surgery (the old
    compact_boundary_disabled trio is dead and unnecessary).

Probes (scratch 2-turn session, codewords BLUEFIN then MARIGOLD):
  A. truncation fork  -> expect BLUEFIN only          (core mechanism)
  B. full copy + legacy pin -> expect BOTH            (pins-dead sentinel:
     if this ever reports BLUEFIN-only, CC honors pins again — revisit
     session_fork/claude_surgery.rs assumptions)
  C. synthetic boundary before turn 2 -> expect MARIGOLD only (gating)
  D. chain-slice at pre-boundary anchor on C's file -> expect BLUEFIN only
     (the production fork mechanism, across a boundary)

Every fork is a NEW file; the parent is hash-verified untouched.
Cost: ~6 haiku -p calls. Cleanup of the scratch project dir is printed.
"""

import hashlib
import json
import subprocess
import uuid as uuidlib
from datetime import datetime, timezone
from pathlib import Path

MODEL = "claude-haiku-4-5-20251001"
SCRATCH = Path("/tmp/fork-spike-proj")
CLAUDE_PROJECTS = Path.home() / ".claude" / "projects"


def sha256(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


def run_claude(prompt: str, resume: str | None = None) -> dict:
    cmd = ["claude", "-p", prompt, "--model", MODEL, "--output-format", "json"]
    if resume:
        cmd += ["--resume", resume]
    print(f"$ claude -p {'--resume ' + resume + ' ' if resume else ''}{prompt[:60]!r}")
    r = subprocess.run(cmd, cwd=SCRATCH, capture_output=True, text=True, timeout=240)
    if r.returncode != 0:
        print(f"  FAILED rc={r.returncode} stderr={r.stderr[:400]}")
        return {"__failed": True, "stderr": r.stderr}
    out = json.loads(r.stdout)
    print(f"  session={out.get('session_id')} result={str(out.get('result'))[:120]!r}")
    return out


def find_transcript(session_id: str) -> Path:
    hits = list(CLAUDE_PROJECTS.glob(f"*/{session_id}.jsonl"))
    if not hits:
        raise SystemExit(f"transcript for {session_id} not found")
    return hits[0]


def lines_of(p: Path) -> list[dict]:
    return [json.loads(l) for l in p.read_text().splitlines() if l.strip()]


def write_fork(lines: list[dict], dest: Path, new_id: str):
    with open(dest, "w") as f:
        for line in lines:
            d = dict(line)
            if "sessionId" in d:
                d["sessionId"] = new_id
            f.write(json.dumps(d) + "\n")


def chain_slice(lines: list[dict], anchor_uuid: str) -> list[dict]:
    """Ancestor chain of the anchor + uuid-less meta lines, original order."""
    by_uuid = {l["uuid"]: l for l in lines if l.get("uuid")}
    chain: set[str] = set()
    cur = anchor_uuid
    while cur and cur in by_uuid:
        chain.add(cur)
        cur = by_uuid[cur].get("parentUuid")
    return [l for l in lines if not l.get("uuid") or l["uuid"] in chain]


def probe(name: str, new_id: str, expect_bluefin: bool, expect_marigold: bool) -> tuple[bool, str]:
    out = run_claude("Which codewords do you know? Answer with just the codewords.", resume=new_id)
    if out.get("__failed"):
        return False, f"{name}: RESUME FAILED ({out['stderr'][:160]})"
    text = str(out.get("result", "")).upper()
    has_b, has_m = "BLUEFIN" in text, "MARIGOLD" in text
    ok = has_b == expect_bluefin and has_m == expect_marigold
    return ok, (
        f"{name}: {'PASS' if ok else 'UNEXPECTED'} "
        f"(bluefin={has_b} want {expect_bluefin}, marigold={has_m} want {expect_marigold})"
    )


def main():
    SCRATCH.mkdir(exist_ok=True)
    results: list[tuple[bool, str]] = []

    t1 = run_claude("Remember this codeword: BLUEFIN. Reply with exactly: OK")
    sid = t1["session_id"]
    run_claude("Remember a second codeword: MARIGOLD. Reply with exactly: OK", resume=sid)
    parent = find_transcript(sid)
    parent_hash = sha256(parent)
    lines = lines_of(parent)
    print(f"parent transcript: {parent} ({len(lines)} lines)")

    marigold_idx = next(
        i for i, l in enumerate(lines)
        if l.get("type") == "user" and "MARIGOLD" in json.dumps(l.get("message", {}))
    )
    anchor_uuid = lines[marigold_idx].get("parentUuid")
    anchor_idx = next(i for i, l in enumerate(lines) if l.get("uuid") == anchor_uuid)
    proj_dir = parent.parent
    fork_ids: list[str] = []

    # A: truncation fork at the turn-1 tail
    a_id = str(uuidlib.uuid4())
    fork_ids.append(a_id)
    write_fork(lines[: anchor_idx + 1], proj_dir / f"{a_id}.jsonl", a_id)
    results.append(probe("A truncation", a_id, expect_bluefin=True, expect_marigold=False))

    # B: full copy + legacy last-prompt pin (pins-dead sentinel)
    b_id = str(uuidlib.uuid4())
    fork_ids.append(b_id)
    pin = {
        "type": "system", "subtype": "last-prompt",
        "uuid": str(uuidlib.uuid4()), "parentUuid": None, "isSidechain": False,
        "sessionId": b_id,
        "timestamp": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ"),
        "leafUuid": anchor_uuid,
    }
    write_fork(lines + [pin], proj_dir / f"{b_id}.jsonl", b_id)
    results.append(probe("B pins-dead sentinel", b_id, expect_bluefin=True, expect_marigold=True))

    # C: synthetic compact_boundary spliced before turn 2 (chain re-parented)
    c_id = str(uuidlib.uuid4())
    fork_ids.append(c_id)
    boundary = {
        "parentUuid": None, "logicalParentUuid": anchor_uuid, "isSidechain": False,
        "type": "system", "subtype": "compact_boundary",
        "content": "Conversation compacted", "level": "info",
        "uuid": str(uuidlib.uuid4()),
        "timestamp": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ"),
        "sessionId": c_id,
        "compactMetadata": {"trigger": "auto", "preTokens": 1000, "postTokens": 100},
    }
    c_lines = []
    for l in lines[: anchor_idx + 1]:
        c_lines.append(l)
    c_lines.append(boundary)
    for l in lines[anchor_idx + 1 :]:
        if l.get("uuid") == lines[marigold_idx]["uuid"]:
            l = dict(l)
            l["parentUuid"] = boundary["uuid"]
        c_lines.append(l)
    write_fork(c_lines, proj_dir / f"{c_id}.jsonl", c_id)
    results.append(probe("C boundary-gates", c_id, expect_bluefin=False, expect_marigold=True))

    # D: chain-slice fork at the pre-boundary anchor, from C's file
    d_id = str(uuidlib.uuid4())
    fork_ids.append(d_id)
    write_fork(chain_slice(c_lines, anchor_uuid), proj_dir / f"{d_id}.jsonl", d_id)
    results.append(probe("D chain-slice pre-boundary", d_id, expect_bluefin=True, expect_marigold=False))

    print("\n=== VERDICTS ===")
    for _, v in results:
        print(" ", v)
    assert sha256(parent) == parent_hash, "PARENT TRANSCRIPT MUTATED — FAILED"
    print("parent transcript unchanged: OK")
    print(f"cleanup: rm -rf {proj_dir} {SCRATCH}")
    if not all(ok for ok, _ in results):
        raise SystemExit(1)
    print("SPIKE 2 PASSED")


if __name__ == "__main__":
    main()
