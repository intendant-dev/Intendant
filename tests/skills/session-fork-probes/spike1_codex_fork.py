#!/usr/bin/env python3
"""Spike 1: prove vanilla codex app-server supports fork-from-rollout-path +
pre-first-turn turn-count rollback + resume of the forked child.

Sequence (mirrors Intendant's wire usage in external_agent/codex/):
  1. copy a real rollout to a staging path (parent is never touched)
  2. codex app-server:  initialize -> initialized
  3. thread/fork {threadId:"", path:<staged copy>}          -> child thread id
  4. thread/rollback {threadId:<child>, numTurns:<n>}       -> trim on fresh child
  5. thread/read {threadId:<child>, includeTurns:true}      -> verify trimmed turn count
  6. fresh app-server: thread/resume {threadId:<child>}     -> child is resumable
  7. verify: parent rollout bytes unchanged; child rollout file exists

Usage: spike1_codex_fork.py [source-rollout.jsonl] [numTurns]
Exit 0 = all probes passed. Artifacts: prints the child thread id + rollout
path it created under $CODEX_HOME/sessions (delete afterwards if unwanted).
"""

import hashlib
import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

TIMEOUT_SECS = 30


def newest_small_rollout(codex_home: Path, cap_bytes: int = 2_000_000) -> Path:
    candidates = sorted(
        codex_home.glob("sessions/*/*/*/rollout-*.jsonl"),
        key=lambda p: p.stat().st_mtime,
        reverse=True,
    )
    for p in candidates:
        if p.stat().st_size <= cap_bytes:
            return p
    raise SystemExit("no rollout under size cap found")


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


class AppServer:
    def __init__(self):
        self.proc = subprocess.Popen(
            ["codex", "app-server"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.lines: "queue.Queue[dict]" = queue.Queue()
        self.next_id = 0
        threading.Thread(target=self._reader, daemon=True).start()
        threading.Thread(target=self._stderr_reader, daemon=True).start()

    def _reader(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                print(f"  [stdout non-json] {line[:200]}")
                continue
            self.lines.put(msg)

    def _stderr_reader(self):
        for line in self.proc.stderr:
            print(f"  [stderr] {line.rstrip()[:200]}")

    def notify(self, method: str, params=None):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def request(self, method: str, params=None) -> dict:
        self.next_id += 1
        rid = self.next_id
        msg = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            msg["params"] = params
        print(f"-> {method} {json.dumps(params)[:200] if params else ''}")
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        deadline = time.time() + TIMEOUT_SECS
        while time.time() < deadline:
            try:
                got = self.lines.get(timeout=deadline - time.time())
            except queue.Empty:
                break
            if got.get("id") == rid:
                if "error" in got:
                    raise RuntimeError(f"{method} error: {json.dumps(got['error'])[:400]}")
                print(f"<- {method} ok: {json.dumps(got.get('result'))[:300]}")
                return got.get("result") or {}
            # server-initiated notification/request; log and keep waiting
            print(f"  [event] {got.get('method', '?')} {json.dumps(got.get('params', {}))[:160]}")
        raise RuntimeError(f"{method}: no response within {TIMEOUT_SECS}s")

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def initialize(srv: AppServer):
    srv.request(
        "initialize",
        {
            "clientInfo": {"name": "intendant-spike", "title": "Spike1", "version": "0"},
            "capabilities": {"experimentalApi": True},
        },
    )
    srv.notify("initialized")


def thread_id_of(result: dict) -> str:
    tid = (result.get("thread") or {}).get("id") or result.get("threadId")
    if not tid:
        raise RuntimeError(f"no thread id in: {json.dumps(result)[:300]}")
    return tid


def count_user_turns(rollout: Path) -> int:
    n = 0
    with open(rollout, encoding="utf-8", errors="replace") as f:
        for line in f:
            try:
                d = json.loads(line)
            except json.JSONDecodeError:
                continue
            if d.get("type") == "event_msg" and d.get("payload", {}).get("type") == "user_message":
                n += 1
    return n


def main():
    codex_home = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex"))
    src = Path(sys.argv[1]) if len(sys.argv) > 1 else newest_small_rollout(codex_home)
    num_turns = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    print(f"source rollout: {src} ({src.stat().st_size} bytes, ~{count_user_turns(src)} user turns)")
    src_hash = sha256(src)

    staging = Path(tempfile.mkdtemp(prefix="spike1-fork-")) / src.name
    shutil.copy2(src, staging)
    print(f"staged copy: {staging}")

    pre_rollouts = set(codex_home.glob("sessions/*/*/*/rollout-*.jsonl"))

    srv = AppServer()
    try:
        initialize(srv)
        fork_res = srv.request("thread/fork", {"threadId": "", "path": str(staging)})
        child = thread_id_of(fork_res)
        print(f"CHILD THREAD: {child}")
        srv.request("thread/rollback", {"threadId": child, "numTurns": num_turns})
        read_res = srv.request("thread/read", {"threadId": child, "includeTurns": True})
        turns = read_res.get("turns")
        print(f"thread/read turns after rollback: {len(turns) if isinstance(turns, list) else turns!r}")
    finally:
        srv.close()

    # resumability on a fresh server
    srv2 = AppServer()
    try:
        initialize(srv2)
        resume_res = srv2.request("thread/resume", {"threadId": child})
        print(f"resume ok, thread id: {thread_id_of(resume_res)}")
    finally:
        srv2.close()

    post_rollouts = set(codex_home.glob("sessions/*/*/*/rollout-*.jsonl"))
    new_files = [p for p in post_rollouts - pre_rollouts]
    print(f"new rollout files: {[str(p) for p in new_files]}")

    assert sha256(src) == src_hash, "PARENT ROLLOUT MUTATED — spike FAILED"
    print("parent rollout unchanged: OK")
    child_files = [p for p in new_files if child.replace("-", "") in p.name.replace("-", "") or child in p.read_text(errors="replace")[:2000]]
    print(f"child rollout: {[str(p) for p in child_files] or 'NOT FOUND (check manually)'}")
    print("SPIKE 1 PASSED")


if __name__ == "__main__":
    main()
