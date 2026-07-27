#!/usr/bin/env python3
"""Spike 3: pin the Claude Code IN-PLACE rewind surfaces the pencil's edit
ladder is built on (src/bin/caller/session_supervisor/claude_edit.rs).

Run on every CC version bump and whenever pencil edits misbehave. NOT CI:
real haiku calls via the installed CLI (subscription OAuth; pennies).

Findings this probe encodes (first proven on CC 2.1.220, 2026-07-27; the
handler is present in every installed version since 2.1.218):

  LANE A — `rewind_conversation` control subtype (the wire rung):
  - request: {"subtype":"rewind_conversation","target_message_uuid":U,
    "interrupt_if_running":B}; refusals arrive as SUCCESS-subtype
    responses with rewound:false + an `error` string ("stale target",
    "target not found", "turn running", "commands queued",
    "no preceding assistant").
  - only the CURRENT last real-human turn may be rewound per call →
    earlier edits walk back newest-first (N+1 calls).
  - success appends an {explicit:true,rewound:true} `last-prompt` pin
    (append-only; no rows deleted) and the next user message lands as a
    sibling branch at precedingAssistantUuid — ghost-free, same id.
  - `interrupt_if_running:true` aborts a mid-stream turn and rewinds it.
  - the walked-back state survives a clean exit + `--resume <same id>`.
  - first-message rewind rides the remote `tengu_rewind_first_message`
    gate: this probe only REPORTS which way the gate points today.
  - post-`result` the CLI stays non-idle briefly (worst observed 3.3 s):
    "turn running" refusals are retried at 250 ms, mirroring the daemon.

  LANE B — same-id transcript truncation (the surgery rung):
  - with NO process attached, truncating the session's own .jsonl at a
    user row and resuming the same id continues ghost-free from the cut;
    the resumed CLI parents the next user row onto the physical tail.
  - sid-keyed sidecars (subagents/) survive byte-identical.

The daemon's canary (`rewind_wire_subtype_canary` +
`claude_rewind_wire_capability`) scans the installed binary for the
subtype token; this probe is the LIVE half — it exercises the wire for
real and exits non-zero on any drift.

Cost: ~8 short haiku turns. Cleanup command printed at the end.
"""

import glob
import hashlib
import json
import os
import subprocess
import sys
import threading
import time

CLAUDE = os.path.expanduser("~/.local/bin/claude")
SCRATCH = f"/tmp/cc-rewind-spike-{int(time.time())}"
LOG_PATH = os.path.join(SCRATCH, "spike3.wire.jsonl")
DAEMON_FLAGS = [
    "-p",
    "--output-format", "stream-json",
    "--input-format", "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--permission-prompt-tool", "stdio",
]
NO_TOOLS = ["--disallowedTools",
            "Bash,Skill,Task,TodoWrite,Read,Write,Edit,MultiEdit,NotebookEdit,"
            "WebSearch,WebFetch,Glob,Grep,BashOutput,KillShell"]

checks = []


def check(name, ok, detail=""):
    checks.append((name, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}: {name}  {detail}")


class CcProc:
    """Minimal stream-json driver mirroring the daemon's spawn shape
    (src/bin/caller/external_agent/claude_code.rs `initialize`)."""

    def __init__(self, cwd, extra_flags=None, approve_tools=False):
        self.approve_tools = approve_tools
        args = [CLAUDE] + DAEMON_FLAGS + ["--permission-mode", "default", "--model", "haiku"]
        if extra_flags:
            args += extra_flags
        os.makedirs(cwd, exist_ok=True)
        self.log = open(LOG_PATH, "a")
        self.log.write(f"\n=== SPAWN {time.strftime('%H:%M:%S')} {' '.join(args)}\n")
        env = dict(os.environ)
        env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] = "1"  # keep ghost checks transcript-pure
        self.proc = subprocess.Popen(args, cwd=cwd, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                     text=True, bufsize=1, env=env)
        self.lines, self.lock, self.ctr, self.cursor = [], threading.Lock(), 0, 0
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            self.log.write(line + "\n"); self.log.flush()
            try:
                obj = json.loads(line)
            except Exception:
                continue
            with self.lock:
                self.lines.append(obj)
            if obj.get("type") == "control_request":
                req = obj.get("request", {})
                if req.get("subtype") == "can_use_tool":
                    behavior = ({"behavior": "allow", "updatedInput": req.get("input", {})}
                                if self.approve_tools else
                                {"behavior": "deny", "message": "tools disabled in this probe"})
                    self.send({"type": "control_response", "response": {
                        "subtype": "success", "request_id": obj.get("request_id"),
                        "response": behavior}})

    def send(self, obj):
        line = json.dumps(obj)
        self.log.write(">>> " + line + "\n"); self.log.flush()
        self.proc.stdin.write(line + "\n")
        self.proc.stdin.flush()

    def send_user(self, text):
        self.send({"type": "user", "message": {"role": "user",
                   "content": [{"type": "text", "text": text}]}})

    def send_control(self, kind, request):
        self.ctr += 1
        rid = f"spike3-{kind}-{self.ctr}"
        self.send({"type": "control_request", "request_id": rid, "request": request})
        return rid

    def wait(self, pred, timeout=180, desc="condition"):
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.lock:
                for i in range(self.cursor, len(self.lines)):
                    if pred(self.lines[i]):
                        self.cursor = i + 1
                        return self.lines[i]
                exited = self.proc.poll() is not None and self.cursor >= len(self.lines)
            if exited:
                raise RuntimeError(f"process exited waiting for {desc}")
            time.sleep(0.05)
        raise TimeoutError(f"timeout waiting for {desc}")

    def wait_result(self):
        return self.wait(lambda o: o.get("type") == "result", desc="result")

    def session_id(self):
        obj = self.wait(lambda o: o.get("type") == "system" and o.get("subtype") == "init",
                        60, "system:init")
        return obj.get("session_id")

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.proc.terminate()
            self.proc.wait(timeout=10)
        return self.proc.returncode


def rewind(p, target_uuid, interrupt=False):
    """One rewind with the daemon's 250 ms 'turn running' retry."""
    for _ in range(60):
        req = {"subtype": "rewind_conversation", "target_message_uuid": target_uuid,
               "interrupt_if_running": interrupt}
        rid = p.send_control("rewind", req)
        resp = p.wait(lambda o: o.get("type") == "control_response"
                      and o.get("response", {}).get("request_id") == rid,
                      60, f"control_response {rid}")
        if resp.get("response", {}).get("subtype") == "error":
            return {"error": resp["response"].get("error"), "_subtype": "error"}
        inner = resp.get("response", {}).get("response", {}) or {}
        if inner.get("error") in ("turn running", "commands queued") and not interrupt:
            time.sleep(0.25)
            continue
        return inner
    return inner


def transcript_path(sid):
    hits = glob.glob(os.path.expanduser(f"~/.claude/projects/*/{sid}.jsonl"))
    assert len(hits) == 1, f"expected 1 transcript for {sid}, got {hits}"
    return hits[0]


def read_rows(path):
    rows = []
    for line in open(path):
        line = line.strip()
        if line:
            try:
                rows.append(json.loads(line))
            except Exception:
                rows.append({"_torn": True})
    return rows


def human_uuid_by_text(path, needle, timeout=15):
    deadline = time.time() + timeout
    while time.time() < deadline:
        for r in read_rows(path):
            if r.get("type") != "user" or r.get("isMeta") or r.get("isSidechain"):
                continue
            content = r.get("message", {}).get("content")
            text = content if isinstance(content, str) else "".join(
                b.get("text", "") for b in content or [] if isinstance(b, dict))
            if needle in text:
                return r["uuid"]
        time.sleep(0.25)
    return None


def main():
    os.makedirs(SCRATCH, exist_ok=True)

    # 0. Binary fingerprint — the canary's live half.
    binary = os.path.realpath(CLAUDE)
    blob = open(binary, "rb").read()
    check("installed CC binary carries the rewind_conversation subtype",
          b"rewind_conversation" in blob, binary)

    # 1. Lane A wire contract on a fresh 2-turn session.
    p = CcProc(SCRATCH, extra_flags=NO_TOOLS)
    p.send_user("For this conversation only, note (no tools): my lucky number is 41. Reply exactly: OK")
    sid = p.session_id()
    p.wait_result()
    p.send_user("One more fact (no tools): my favorite animal is the axolotl. Reply exactly: OK")
    p.wait_result()
    tp = transcript_path(sid)
    u1 = human_uuid_by_text(tp, "lucky number")
    u2 = human_uuid_by_text(tp, "axolotl")

    inner = rewind(p, u1)
    check("earlier target refused (stale target)",
          inner.get("rewound") is False and "stale" in (inner.get("error") or ""), str(inner))
    inner = rewind(p, "00000000-dead-beef-0000-000000000000")
    check("garbage target refused (target not found)",
          inner.get("rewound") is False and "not found" in (inner.get("error") or ""), str(inner))
    inner = rewind(p, u2)
    check("last-human rewind succeeds", inner.get("rewound") is True, str(inner))
    preceding = inner.get("precedingAssistantUuid")

    p.send_user("New fact (no tools): my favorite animal is the quokka. One line: every animal mentioned so far, plus my lucky number.")
    r = p.wait_result()
    t = (r.get("result") or "").lower()
    check("edited turn ran in the SAME session id", r.get("session_id") == sid)
    check("ghost-free: quokka+41 present, pruned axolotl absent",
          "quokka" in t and "41" in t and "axolotl" not in t, t[:120])
    u2p = human_uuid_by_text(tp, "quokka")
    edited = next((row for row in read_rows(tp) if row.get("uuid") == u2p), {})
    check("edited row is a sibling at precedingAssistantUuid",
          edited.get("parentUuid") == preceding)

    # First-message gate: report, don't fail — it's remote config.
    inner = rewind(p, u2p)
    check("walk-back loop viable (second rewind succeeds)", inner.get("rewound") is True)
    inner = rewind(p, u1)
    gate_on = inner.get("rewound") is True
    print(f"INFO: tengu_rewind_first_message gate is {'ON' if gate_on else 'OFF'} "
          f"in this environment ({inner})")
    check("clean exit", p.close() == 0)

    # 2. Restart continuity of the walked-back state.
    p2 = CcProc(SCRATCH, extra_flags=NO_TOOLS + ["--resume", sid])
    p2.send_user("One line (no tools): my lucky number, and every animal mentioned in this conversation (NO-ANIMALS if none).")
    sid2 = p2.session_id()
    r = p2.wait_result()
    t = (r.get("result") or "").lower()
    check("resume keeps the same backend session id", sid2 == sid)
    if gate_on:
        check("restart honors the walked-back state (everything pruned)",
              "quokka" not in t and "axolotl" not in t, t[:120])
    else:
        check("restart honors the walked-back state (41 kept, animals pruned)",
              "41" in t and "quokka" not in t and "axolotl" not in t, t[:120])

    # 3. interrupt_if_running against a mid-stream turn.
    p2.send_user("Slowly count from 1 to 40, one number per line, no tools, then say DONE-COUNTING.")
    p2.wait(lambda o: o.get("type") == "stream_event", 60, "slow turn streaming")
    u_slow = human_uuid_by_text(tp, "Slowly count")
    inner = rewind(p2, u_slow, interrupt=True)
    check("interrupt_if_running rewinds a RUNNING turn", inner.get("rewound") is True, str(inner))
    check("resumed process clean exit", p2.close() == 0)

    # 4. Lane B: same-id truncation with NO process attached, then resume.
    #    Also plant a sidecar to pin byte-identical survival.
    sidecar_dir = os.path.join(os.path.dirname(tp), sid, "subagents")
    os.makedirs(sidecar_dir, exist_ok=True)
    sidecar = os.path.join(sidecar_dir, "agent-spike3.jsonl")
    open(sidecar, "w").write('{"type":"user","uuid":"sidecar-row"}\n')
    sidecar_hash = hashlib.sha256(open(sidecar, "rb").read()).hexdigest()

    rows_raw = open(tp).read().splitlines(keepends=False)
    cut_uuid = u_slow if gate_on is not None else None
    cut_index = next(i for i, line in enumerate(rows_raw) if cut_uuid and cut_uuid in line)
    open(tp, "w").write("\n".join(rows_raw[:cut_index]) + "\n")
    p3 = CcProc(SCRATCH, extra_flags=NO_TOOLS + ["--resume", sid])
    p3.send_user("One line (no tools): did I ever ask you to count numbers in this conversation? YES or NO.")
    sid3 = p3.session_id()
    r = p3.wait_result()
    t = (r.get("result") or "").lower()
    check("surgery resume keeps the same backend session id", sid3 == sid)
    check("surgery pruned the counting turn ghost-free", t.strip().startswith("no") or " no" in t,
          t[:120])
    check("surgery leaves the subagent sidecar byte-identical",
          hashlib.sha256(open(sidecar, "rb").read()).hexdigest() == sidecar_hash)
    check("surgery-resumed process clean exit", p3.close() == 0)

    fails = [name for name, ok in checks if not ok]
    print(f"\n=== {len(checks) - len(fails)}/{len(checks)} passed ===")
    print(f"cleanup: rm -rf {SCRATCH} "
          f"~/.claude/projects/{os.path.basename(os.path.dirname(tp))}")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
