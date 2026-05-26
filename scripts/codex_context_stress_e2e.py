#!/usr/bin/env python3
"""Long-context Codex/Intendant E2E stress benchmark.

This intentionally exercises the feature boundary that matters for the Codex
lineage fork: a long session that would normally compact, versus a managed
Codex session under Intendant that should disable hidden compaction, enter
rewind-only mode from backend-reported usage, block ordinary tools, and recover
through an explicit rewind.
"""

from __future__ import annotations

import argparse
import base64
import dataclasses
import hashlib
import json
import os
import random
import re
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_VANILLA_CODEX = Path("/opt/homebrew/bin/codex")
DEFAULT_PATCHED_CODEX = Path(
    "/Users/vm/projects/codex/.worktrees/minimal-lineage-upstream/codex-rs/target/debug/codex"
)
DEFAULT_INTENDANT = ROOT / "target/debug/intendant"


def now_ms() -> int:
    return int(time.time() * 1000)


def load_json_line(line: str) -> dict[str, Any] | None:
    try:
        value = json.loads(line)
    except json.JSONDecodeError:
        return None
    return value if isinstance(value, dict) else None


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def copy_codex_auth(dst_home: Path) -> None:
    src_home = Path.home() / ".codex"
    dst_home.mkdir(parents=True, exist_ok=True)
    auth = src_home / "auth.json"
    if not auth.exists():
        raise RuntimeError(f"missing Codex auth file: {auth}")
    shutil.copy2(auth, dst_home / "auth.json")
    models_cache = src_home / "models_cache.json"
    if models_cache.exists():
        shutil.copy2(models_cache, dst_home / "models_cache.json")


def write_codex_config(home: Path, *, model: str, reasoning_effort: str, context_window: int) -> None:
    write_text(
        home / "config.toml",
        "\n".join(
            [
                f'model = "{model}"',
                f'model_reasoning_effort = "{reasoning_effort}"',
                f"model_context_window = {context_window}",
                "",
            ]
        ),
    )


def make_workspace(root: Path, line_count: int) -> Path:
    workspace = root / "workspace"
    workspace.mkdir(parents=True, exist_ok=True)
    write_text(
        workspace / "emit_context.py",
        """#!/usr/bin/env python3
import sys

n = int(sys.argv[1]) if len(sys.argv) > 1 else 1000
for i in range(n):
    print(f"CTX_PROBE_LINE_{i:05d} alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu")
""",
    )
    os.chmod(workspace / "emit_context.py", 0o755)
    write_text(
        workspace / "README.md",
        f"# Context stress workspace\n\nemit_context.py emits {line_count} deterministic lines.\n",
    )
    return workspace


def run_capture(
    argv: list[str],
    *,
    env: dict[str, str],
    cwd: Path,
    timeout: int,
    raw_dir: Path,
    name: str,
) -> tuple[subprocess.CompletedProcess[str], float]:
    started = time.monotonic()
    proc = subprocess.run(
        argv,
        cwd=str(cwd),
        env=env,
        stdin=subprocess.DEVNULL,
        text=True,
        capture_output=True,
        timeout=timeout,
    )
    elapsed = time.monotonic() - started
    write_text(raw_dir / f"{name}.stdout", proc.stdout)
    write_text(raw_dir / f"{name}.stderr", proc.stderr)
    write_text(raw_dir / f"{name}.argv.json", json.dumps(argv, indent=2))
    return proc, elapsed


def parse_json_events(text: str) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    for line in text.splitlines():
        item = load_json_line(line)
        if item is not None:
            events.append(item)
    return events


def iter_values(value: Any) -> Any:
    yield value
    if isinstance(value, dict):
        for child in value.values():
            yield from iter_values(child)
    elif isinstance(value, list):
        for child in value:
            yield from iter_values(child)


def find_thread_id(events: list[dict[str, Any]]) -> str | None:
    for event in events:
        for value in iter_values(event):
            if isinstance(value, dict):
                for key in ("thread_id", "threadId", "id"):
                    found = value.get(key)
                    if isinstance(found, str) and found.startswith("019"):
                        return found
    return None


def extract_usage(events: list[dict[str, Any]]) -> list[dict[str, int]]:
    snapshots: list[dict[str, int]] = []
    for event in events:
        for value in iter_values(event):
            if not isinstance(value, dict):
                continue
            input_tokens = value.get("input_tokens")
            total_tokens = value.get("total_tokens")
            cached = value.get("cached_input_tokens", value.get("cached_tokens"))
            context_window = value.get("model_context_window", value.get("context_window"))
            if isinstance(input_tokens, int) or isinstance(total_tokens, int):
                snap: dict[str, int] = {}
                for src, dst in [
                    ("input_tokens", "input_tokens"),
                    ("total_tokens", "total_tokens"),
                    ("cached_input_tokens", "cached_input_tokens"),
                    ("cached_tokens", "cached_tokens"),
                    ("model_context_window", "model_context_window"),
                    ("context_window", "context_window"),
                    ("output_tokens", "output_tokens"),
                    ("reasoning_output_tokens", "reasoning_output_tokens"),
                ]:
                    item = value.get(src)
                    if isinstance(item, int):
                        snap[dst] = item
                if cached is not None and isinstance(cached, int):
                    snap.setdefault("cached_input_tokens", cached)
                if context_window is not None and isinstance(context_window, int):
                    snap.setdefault("model_context_window", context_window)
                snapshots.append(snap)
    return snapshots


def rollout_paths(codex_home: Path) -> list[Path]:
    sessions = codex_home / "sessions"
    if not sessions.exists():
        return []
    return sorted(sessions.glob("**/rollout-*.jsonl"), key=lambda p: p.stat().st_mtime)


def parse_rollout(path: Path) -> list[dict[str, Any]]:
    items: list[dict[str, Any]] = []
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            item = load_json_line(line)
            if item is not None:
                items.append(item)
    return items


def rollout_metrics(codex_home: Path) -> dict[str, Any]:
    compacted = 0
    context_compactions = 0
    gate_messages = 0
    calls: list[dict[str, str]] = []
    usage: list[dict[str, int]] = []
    pressure_reports: list[dict[str, Any]] = []
    paths = rollout_paths(codex_home)
    for path in paths:
        with path.open("r", encoding="utf-8", errors="replace") as handle:
            lines = handle.readlines()
        gate_messages += sum(
            "Only get_status, rewind_context, and rewind_backout" in line for line in lines
        )
        for line in lines:
            item = load_json_line(line)
            if item is None:
                continue
            payload = item.get("payload")
            if item.get("type") == "compacted":
                compacted += 1
            if isinstance(payload, dict):
                if payload.get("type") in ("context_compacted", "contextCompaction"):
                    context_compactions += 1
                if payload.get("type") == "function_call":
                    name = payload.get("name")
                    call_id = payload.get("call_id")
                    arguments = payload.get("arguments")
                    if isinstance(name, str) and isinstance(call_id, str):
                        calls.append(
                            {
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments if isinstance(arguments, str) else "",
                                "rollout": str(path),
                            }
                        )
                if payload.get("type") in ("token_count", "turn_completed"):
                    usage.extend(extract_usage([payload]))
                pressure = extract_get_status_pressure(payload)
                if pressure is not None:
                    pressure["rollout"] = str(path)
                    timestamp = item.get("timestamp")
                    if isinstance(timestamp, str):
                        pressure["timestamp"] = timestamp
                    pressure_reports.append(pressure)
            usage.extend(extract_usage([item]))
    return {
        "rollout_paths": [str(path) for path in paths],
        "compacted_events": compacted,
        "context_compaction_items": context_compactions,
        "gate_messages": gate_messages,
        "function_calls": calls,
        "usage": usage,
        "pressure_reports": pressure_reports,
    }


def extract_get_status_pressure(payload: dict[str, Any]) -> dict[str, Any] | None:
    if payload.get("type") != "mcp_tool_call_end":
        return None
    invocation = payload.get("invocation")
    if not isinstance(invocation, dict) or invocation.get("tool") != "get_status":
        return None
    result = payload.get("result")
    if not isinstance(result, dict):
        return None
    ok = result.get("Ok")
    if not isinstance(ok, dict):
        return None
    content = ok.get("content")
    if not isinstance(content, list):
        return None
    for item in content:
        if not isinstance(item, dict) or item.get("type") != "text":
            continue
        text = item.get("text")
        if not isinstance(text, str):
            continue
        status = load_json_line(text)
        if not isinstance(status, dict):
            continue
        pressure = status.get("context_pressure")
        if not isinstance(pressure, dict):
            continue
        task = status.get("task")
        return {
            "task": task if isinstance(task, str) else "",
            "status": pressure.get("status"),
            "rewind_only": pressure.get("rewind_only"),
            "used_tokens": pressure.get("used_tokens"),
            "context_window": pressure.get("context_window"),
            "remaining_tokens": pressure.get("remaining_tokens"),
            "remaining_percent": pressure.get("remaining_percent"),
        }
    return None


def find_call_id(metrics: dict[str, Any], *, name: str, argument_substring: str) -> str | None:
    for call in metrics.get("function_calls", []):
        if call.get("name") == name and argument_substring in call.get("arguments", ""):
            return call.get("call_id")
    return None


@dataclasses.dataclass
class WsEvent:
    data: dict[str, Any]
    at_ms: int


class WebSocketClient:
    def __init__(self, host: str, port: int, path: str = "/ws", timeout: float = 20.0) -> None:
        self.host = host
        self.port = port
        self.path = path
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.sock.settimeout(timeout)
        self._handshake()

    def close(self) -> None:
        try:
            self._send_frame(b"", opcode=8)
        except OSError:
            pass
        try:
            self.sock.close()
        except OSError:
            pass

    def _handshake(self) -> None:
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        request = (
            f"GET {self.path} HTTP/1.1\r\n"
            f"Host: {self.host}:{self.port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        ).encode("ascii")
        self.sock.sendall(request)
        response = b""
        while b"\r\n\r\n" not in response:
            chunk = self.sock.recv(4096)
            if not chunk:
                break
            response += chunk
        if b" 101 " not in response.split(b"\r\n", 1)[0]:
            raise RuntimeError(f"websocket handshake failed: {response[:200]!r}")
        accept = None
        for line in response.decode("iso-8859-1", errors="replace").split("\r\n"):
            if line.lower().startswith("sec-websocket-accept:"):
                accept = line.split(":", 1)[1].strip()
        expected = base64.b64encode(
            hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")).digest()
        ).decode("ascii")
        if accept != expected:
            raise RuntimeError("websocket accept header mismatch")

    def send_json(self, value: dict[str, Any]) -> None:
        self._send_frame(json.dumps(value, separators=(",", ":")).encode("utf-8"), opcode=1)

    def recv_json(self, timeout: float) -> dict[str, Any] | None:
        old = self.sock.gettimeout()
        self.sock.settimeout(timeout)
        try:
            text = self._recv_message()
        except socket.timeout:
            return None
        finally:
            self.sock.settimeout(old)
        if text is None:
            return None
        try:
            value = json.loads(text)
        except json.JSONDecodeError:
            return None
        return value if isinstance(value, dict) else None

    def _send_frame(self, payload: bytes, opcode: int) -> None:
        first = 0x80 | (opcode & 0x0F)
        mask_key = os.urandom(4)
        length = len(payload)
        if length < 126:
            header = struct.pack("!BB", first, 0x80 | length)
        elif length <= 0xFFFF:
            header = struct.pack("!BBH", first, 0x80 | 126, length)
        else:
            header = struct.pack("!BBQ", first, 0x80 | 127, length)
        masked = bytes(byte ^ mask_key[i % 4] for i, byte in enumerate(payload))
        self.sock.sendall(header + mask_key + masked)

    def _recv_exact(self, size: int) -> bytes:
        chunks = bytearray()
        while len(chunks) < size:
            chunk = self.sock.recv(size - len(chunks))
            if not chunk:
                raise RuntimeError("websocket closed")
            chunks.extend(chunk)
        return bytes(chunks)

    def _recv_frame(self) -> tuple[int, bytes, bool]:
        b1, b2 = self._recv_exact(2)
        fin = bool(b1 & 0x80)
        opcode = b1 & 0x0F
        masked = bool(b2 & 0x80)
        length = b2 & 0x7F
        if length == 126:
            length = struct.unpack("!H", self._recv_exact(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", self._recv_exact(8))[0]
        mask_key = self._recv_exact(4) if masked else b""
        payload = self._recv_exact(length) if length else b""
        if masked:
            payload = bytes(byte ^ mask_key[i % 4] for i, byte in enumerate(payload))
        return opcode, payload, fin

    def _recv_message(self) -> str | None:
        chunks: list[bytes] = []
        first_opcode: int | None = None
        while True:
            opcode, payload, fin = self._recv_frame()
            if opcode == 8:
                return None
            if opcode == 9:
                self._send_frame(payload, opcode=10)
                continue
            if opcode == 10:
                continue
            if opcode in (1, 2):
                first_opcode = opcode
                chunks = [payload]
            elif opcode == 0:
                chunks.append(payload)
            else:
                continue
            if fin:
                break
        data = b"".join(chunks)
        if first_opcode == 1:
            return data.decode("utf-8", errors="replace")
        return None


def wait_port(host: str, port: int, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=1):
                return
        except OSError:
            time.sleep(0.2)
    raise RuntimeError(f"timed out waiting for {host}:{port}")


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def collect_until_idle(
    ws: WebSocketClient,
    *,
    idle_seconds: float,
    max_seconds: float,
    auto_approve: bool = True,
) -> list[WsEvent]:
    events: list[WsEvent] = []
    deadline = time.monotonic() + max_seconds
    last_event = time.monotonic()
    while time.monotonic() < deadline:
        timeout = min(2.0, max(0.1, deadline - time.monotonic()))
        event = ws.recv_json(timeout=timeout)
        if event is None:
            if events and time.monotonic() - last_event >= idle_seconds:
                break
            continue
        last_event = time.monotonic()
        events.append(WsEvent(event, now_ms()))
        if auto_approve and event.get("event") == "approval_required" and isinstance(event.get("id"), int):
            ws.send_json({"action": "approve", "id": event["id"]})
    return events


def event_summary(events: list[WsEvent]) -> dict[str, Any]:
    counts: dict[str, int] = {}
    session_ids: set[str] = set()
    backend_ids: set[str] = set()
    gate_messages = 0
    for wrapper in events:
        event = wrapper.data
        event_name = str(event.get("event") or event.get("t") or "")
        if event_name:
            counts[event_name] = counts.get(event_name, 0) + 1
        sid = event.get("session_id")
        if isinstance(sid, str) and sid:
            session_ids.add(sid)
        if event_name == "session_identity":
            backend = event.get("backend_session_id")
            if isinstance(backend, str) and backend:
                backend_ids.add(backend)
        text = json.dumps(event, ensure_ascii=False)
        if "Only get_status, rewind_context, and rewind_backout" in text:
            gate_messages += 1
    usage = []
    for wrapper in events:
        if wrapper.data.get("event") == "usage_update":
            usage.append(wrapper.data)
    context_snapshots = [
        compact_context_snapshot(e.data)
        for e in events
        if e.data.get("event") == "context_snapshot"
    ]
    actions = [e.data for e in events if e.data.get("event") == "codex_thread_action_result"]
    return {
        "event_counts": counts,
        "session_ids": sorted(session_ids),
        "backend_session_ids": sorted(backend_ids),
        "gate_messages": gate_messages,
        "usage_updates": usage,
        "context_snapshots": context_snapshots[-10:],
        "codex_thread_actions": actions,
}


def compact_context_snapshot(event: dict[str, Any]) -> dict[str, Any]:
    data = event.get("data")
    if not isinstance(data, dict):
        data = {}
    return {
        "event": event.get("event"),
        "session_id": event.get("session_id"),
        "source": event.get("source") or data.get("source"),
        "label": event.get("label") or data.get("label"),
        "format": event.get("format") or data.get("format"),
        "item_count": event.get("item_count") or data.get("item_count"),
        "token_count": event.get("token_count") or data.get("token_count"),
        "context_window": event.get("context_window") or data.get("context_window"),
        "file": event.get("file"),
    }


def save_events(path: Path, events: list[WsEvent]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        for event in events:
            row = dict(event.data)
            row["_observed_at_ms"] = event.at_ms
            handle.write(json.dumps(row, ensure_ascii=False) + "\n")


def run_vanilla(args: argparse.Namespace, root: Path, workspace: Path) -> dict[str, Any]:
    raw_dir = root / "raw" / "vanilla"
    home = root / "codex_home_vanilla"
    copy_codex_auth(home)
    write_codex_config(
        home,
        model=args.model,
        reasoning_effort=args.reasoning_effort,
        context_window=args.context_window,
    )
    env = os.environ.copy()
    env["CODEX_HOME"] = str(home)
    last_marker = f"CTX_PROBE_LINE_{args.line_count - 1:05d}"
    common = [
        str(args.vanilla_codex_bin),
        "exec",
        "--json",
        "--sandbox",
        "read-only",
        "--skip-git-repo-check",
        "--cd",
        str(workspace),
        "-c",
        f"model_context_window={args.context_window}",
        "-c",
        f"model_auto_compact_token_limit={args.vanilla_compact_limit}",
        "-c",
        'model_auto_compact_token_limit_scope="body_after_prefix"',
        "-c",
        f'model_reasoning_effort="{args.reasoning_effort}"',
    ]
    prompt = (
        "Controlled long-context benchmark. Run exactly: "
        f"python3 emit_context.py {args.line_count}. "
        "When you call exec_command, set max_output_tokens to 100000 so the full deterministic output remains in context. "
        f"Then reply with VANILLA_INITIAL_OUTPUT_SEEN {last_marker}."
    )
    proc, elapsed = run_capture(common + [prompt], env=env, cwd=workspace, timeout=args.turn_timeout, raw_dir=raw_dir, name="turn_1")
    events = parse_json_events(proc.stdout)
    thread_id = find_thread_id(events)
    resumes: list[dict[str, Any]] = []
    if thread_id:
        for index in range(1, args.vanilla_resume_turns + 1):
            resume_argv = [
                str(args.vanilla_codex_bin),
                "exec",
                "resume",
                "--json",
                "--skip-git-repo-check",
                "-c",
                f"model_context_window={args.context_window}",
                "-c",
                f"model_auto_compact_token_limit={args.vanilla_compact_limit}",
                "-c",
                'model_auto_compact_token_limit_scope="body_after_prefix"',
                "-c",
                f'model_reasoning_effort="{args.reasoning_effort}"',
                thread_id,
                f"Do not run tools. Reply with exactly VANILLA_RESUME_{index}.",
            ]
            proc_i, elapsed_i = run_capture(
                resume_argv,
                env=env,
                cwd=workspace,
                timeout=args.turn_timeout,
                raw_dir=raw_dir,
                name=f"resume_{index}",
            )
            resumes.append(
                {
                    "index": index,
                    "returncode": proc_i.returncode,
                    "elapsed_seconds": elapsed_i,
                    "usage": extract_usage(parse_json_events(proc_i.stdout)),
                    "marker_seen": f"VANILLA_RESUME_{index}" in proc_i.stdout,
                }
            )
            if rollout_metrics(home)["compacted_events"] > 0:
                break
    metrics = rollout_metrics(home)
    return {
        "codex_home": str(home),
        "returncode": proc.returncode,
        "elapsed_seconds": elapsed,
        "thread_id": thread_id,
        "initial_usage": extract_usage(events),
        "initial_marker_seen": last_marker in proc.stdout,
        "resumes": resumes,
        "rollout": metrics,
        "raw_dir": str(raw_dir),
    }


def run_managed(args: argparse.Namespace, root: Path, workspace: Path) -> dict[str, Any]:
    raw_dir = root / "raw" / "managed"
    home = root / "codex_home_managed"
    copy_codex_auth(home)
    write_codex_config(
        home,
        model=args.model,
        reasoning_effort=args.reasoning_effort,
        context_window=args.context_window,
    )
    port = args.managed_port or free_port()
    if port == 8765:
        raise RuntimeError("refusing to use protected Intendant port 8765")
    log_dir = root / "intendant_logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["CODEX_HOME"] = str(home)
    env["PATH"] = f"{args.managed_codex_bin.parent}:{env.get('PATH', '')}"
    argv = [
        str(args.intendant_bin),
        "--web",
        str(port),
        "--no-tui",
        "--no-presence",
        "--agent",
        "codex",
        "--log-file",
        str(log_dir),
    ]
    stdout_path = raw_dir / "intendant.stdout"
    stderr_path = raw_dir / "intendant.stderr"
    raw_dir.mkdir(parents=True, exist_ok=True)
    stdout_handle = stdout_path.open("w", encoding="utf-8")
    stderr_handle = stderr_path.open("w", encoding="utf-8")
    proc = subprocess.Popen(
        argv,
        cwd=str(workspace),
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=stdout_handle,
        stderr=stderr_handle,
        text=True,
    )
    ws: WebSocketClient | None = None
    all_events: list[WsEvent] = []
    turn_summaries: list[dict[str, Any]] = []
    try:
        wait_port("127.0.0.1", port, timeout=30)
        ws = WebSocketClient("127.0.0.1", port)
        ws.send_json({"action": "set_external_agent", "agent": "codex"})
        ws.send_json({"action": "set_codex_command", "command": str(args.managed_codex_bin)})
        ws.send_json({"action": "set_codex_sandbox", "mode": "read-only"})
        ws.send_json({"action": "set_codex_approval_policy", "policy": "never"})
        ws.send_json({"action": "set_codex_model", "model": args.model})
        ws.send_json({"action": "set_codex_reasoning_effort", "effort": args.reasoning_effort})
        all_events.extend(collect_until_idle(ws, idle_seconds=1.5, max_seconds=5))

        last_marker = f"CTX_PROBE_LINE_{args.line_count - 1:05d}"
        turn_1_prompt = (
            "Controlled managed-Codex context benchmark. "
            f"Run exactly `python3 emit_context.py {args.line_count}` in this workspace. "
            "When you call exec_command, set max_output_tokens to 100000 so the full deterministic output remains in context. "
            f"Then reply with `MANAGED_INITIAL_OUTPUT_SEEN {last_marker}`. "
            "Do not modify files."
        )
        started = time.monotonic()
        ws.send_json({"action": "start_task", "task": turn_1_prompt, "direct": True})
        turn_1 = collect_until_idle(
            ws,
            idle_seconds=args.managed_idle_seconds,
            max_seconds=args.turn_timeout,
        )
        all_events.extend(turn_1)
        turn_summaries.append(
            {
                "name": "initial_long_output",
                "elapsed_seconds": time.monotonic() - started,
                "summary": event_summary(turn_1),
            }
        )

        managed_rollout_after_1 = rollout_metrics(home)
        emit_call_id = find_call_id(
            managed_rollout_after_1,
            name="exec_command",
            argument_substring=f"emit_context.py {args.line_count}",
        )
        backend_ids = event_summary(all_events)["backend_session_ids"]
        backend_session_id = backend_ids[-1] if backend_ids else None

        if backend_session_id:
            pressure_prompt = (
                "Context-pressure probe. Call Intendant MCP `get_status` exactly once. "
                "Do not run shell commands. Reply with `MANAGED_PRESSURE_PROBE_DONE` and the reported context_pressure status."
            )
            started = time.monotonic()
            ws.send_json(
                {
                    "action": "resume_session",
                    "source": "codex",
                    "session_id": backend_session_id,
                    "resume_id": backend_session_id,
                    "project_root": str(workspace),
                    "task": pressure_prompt,
                    "direct": True,
                }
            )
            turn_pressure = collect_until_idle(
                ws,
                idle_seconds=args.managed_idle_seconds,
                max_seconds=args.turn_timeout,
            )
            all_events.extend(turn_pressure)
            turn_summaries.append(
                {
                    "name": "pressure_probe",
                    "elapsed_seconds": time.monotonic() - started,
                    "summary": event_summary(turn_pressure),
                }
            )

        if backend_session_id:
            gate_prompt = (
                "Context-pressure gate test. Try to run exactly `pwd` as an ordinary shell command. "
                "Do not call get_status before trying the command. "
                "After that, reply with `MANAGED_GATE_TEST_DONE` and state whether the ordinary command was blocked."
            )
            started = time.monotonic()
            ws.send_json(
                {
                    "action": "resume_session",
                    "source": "codex",
                    "session_id": backend_session_id,
                    "resume_id": backend_session_id,
                    "project_root": str(workspace),
                    "task": gate_prompt,
                    "direct": True,
                }
            )
            turn_gate = collect_until_idle(
                ws,
                idle_seconds=args.managed_idle_seconds,
                max_seconds=args.turn_timeout,
            )
            all_events.extend(turn_gate)
            turn_summaries.append(
                {
                    "name": "rewind_only_gate",
                    "elapsed_seconds": time.monotonic() - started,
                    "summary": event_summary(turn_gate),
                }
            )

        if backend_session_id and emit_call_id:
            rewind_prompt = (
                "Now reduce context pressure using the Intendant MCP `rewind_context` tool. "
                f"Use anchor.item_id `{emit_call_id}` and anchor.position `before`. "
                "Use reason `prune deterministic long-output benchmark`. "
                "Use a primer that preserves: the command `python3 emit_context.py "
                f"{args.line_count}` ran, it emitted CTX_PROBE_LINE_00000 through {last_marker}, "
                "the workspace was not modified, the bulky tool call/output should be discarded, "
                "and the next step is to call get_status. "
                "After calling rewind_context, do not call any other tools."
            )
            started = time.monotonic()
            ws.send_json(
                {
                    "action": "resume_session",
                    "source": "codex",
                    "session_id": backend_session_id,
                    "resume_id": backend_session_id,
                    "project_root": str(workspace),
                    "task": rewind_prompt,
                    "direct": True,
                }
            )
            turn_rewind = collect_until_idle(
                ws,
                idle_seconds=args.managed_idle_seconds,
                max_seconds=args.turn_timeout,
            )
            all_events.extend(turn_rewind)
            turn_summaries.append(
                {
                    "name": "model_rewind_context",
                    "elapsed_seconds": time.monotonic() - started,
                    "summary": event_summary(turn_rewind),
                }
            )

        if backend_session_id:
            status_prompt = (
                "Post-rewind status check. Call Intendant MCP `get_status` exactly once. "
                "Do not run shell commands. Reply with `MANAGED_REWIND_STRESS_OK` and the reported context_pressure status."
            )
            started = time.monotonic()
            ws.send_json(
                {
                    "action": "resume_session",
                    "source": "codex",
                    "session_id": backend_session_id,
                    "resume_id": backend_session_id,
                    "project_root": str(workspace),
                    "task": status_prompt,
                    "direct": True,
                }
            )
            turn_status = collect_until_idle(
                ws,
                idle_seconds=args.managed_idle_seconds,
                max_seconds=args.turn_timeout,
            )
            all_events.extend(turn_status)
            turn_summaries.append(
                {
                    "name": "post_rewind_status",
                    "elapsed_seconds": time.monotonic() - started,
                    "summary": event_summary(turn_status),
                }
            )

        save_events(raw_dir / "websocket_events.jsonl", all_events)
        return {
            "port": port,
            "codex_home": str(home),
            "log_dir": str(log_dir),
            "backend_session_id": event_summary(all_events)["backend_session_ids"][-1:]
            or None,
            "emit_call_id": emit_call_id,
            "turns": turn_summaries,
            "events": event_summary(all_events),
            "rollout": rollout_metrics(home),
            "raw_dir": str(raw_dir),
        }
    finally:
        if ws is not None:
            ws.close()
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=10)
        stdout_handle.close()
        stderr_handle.close()


def summarize_pass_fail(summary: dict[str, Any]) -> dict[str, Any]:
    vanilla = summary.get("vanilla") or {}
    managed = summary.get("managed") or {}
    vanilla_compacted = (vanilla.get("rollout") or {}).get("compacted_events", 0) > 0
    managed_compacted = (managed.get("rollout") or {}).get("compacted_events", 0) > 0
    gate_hits = (
        ((managed.get("events") or {}).get("gate_messages") or 0) > 0
        or ((managed.get("rollout") or {}).get("gate_messages") or 0) > 0
    )
    emit_call_id = bool(managed.get("emit_call_id"))
    rewind_actions = [
        a
        for turn in managed.get("turns", [])
        for a in ((turn.get("summary") or {}).get("codex_thread_actions") or [])
        if a.get("action") == "rewind_context"
    ]
    rewind_success = any(a.get("success") for a in rewind_actions)
    pressure_reports = (managed.get("rollout") or {}).get("pressure_reports") or []
    post_rewind_pressure_ok = any(
        "Post-rewind status check" in str(report.get("task", ""))
        and report.get("status") == "ok"
        and report.get("rewind_only") is False
        for report in pressure_reports
    )
    return {
        "vanilla_compacted_under_pressure": vanilla_compacted,
        "managed_avoided_hidden_compaction": not managed_compacted,
        "managed_found_exact_long_output_anchor": emit_call_id,
        "managed_rewind_only_gate_observed": gate_hits,
        "managed_model_rewind_succeeded": rewind_success,
        "managed_post_rewind_pressure_ok": post_rewind_pressure_ok,
        "production_ready_gate": bool(
            vanilla_compacted
            and not managed_compacted
            and emit_call_id
            and gate_hits
            and rewind_success
            and post_rewind_pressure_ok
        ),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vanilla-codex-bin", type=Path, default=DEFAULT_VANILLA_CODEX)
    parser.add_argument("--managed-codex-bin", type=Path, default=DEFAULT_PATCHED_CODEX)
    parser.add_argument("--intendant-bin", type=Path, default=DEFAULT_INTENDANT)
    parser.add_argument("--output-root", type=Path, default=Path(tempfile.gettempdir()) / "intendant-codex-context-e2e")
    parser.add_argument("--model", default="gpt-5.5")
    parser.add_argument("--reasoning-effort", default="low")
    parser.add_argument("--context-window", type=int, default=30000)
    parser.add_argument("--vanilla-compact-limit", type=int, default=5000)
    parser.add_argument("--vanilla-resume-turns", type=int, default=2)
    parser.add_argument("--line-count", type=int, default=3000)
    parser.add_argument("--turn-timeout", type=int, default=900)
    parser.add_argument("--managed-idle-seconds", type=float, default=45.0)
    parser.add_argument("--managed-port", type=int, default=0)
    parser.add_argument("--skip-vanilla", action="store_true")
    parser.add_argument("--skip-managed", action="store_true")
    args = parser.parse_args()

    if not args.vanilla_codex_bin.exists():
        raise SystemExit(f"vanilla Codex binary not found: {args.vanilla_codex_bin}")
    if not args.skip_managed:
        if not args.managed_codex_bin.exists():
            raise SystemExit(f"patched Codex binary not found: {args.managed_codex_bin}")
        if not args.intendant_bin.exists():
            raise SystemExit(f"Intendant binary not found: {args.intendant_bin}")

    run_root = args.output_root / time.strftime("%Y%m%d-%H%M%S")
    run_root.mkdir(parents=True, exist_ok=True)
    workspace = make_workspace(run_root, args.line_count)
    summary: dict[str, Any] = {
        "run_root": str(run_root),
        "workspace": str(workspace),
        "parameters": {
            "model": args.model,
            "reasoning_effort": args.reasoning_effort,
            "context_window": args.context_window,
            "vanilla_compact_limit": args.vanilla_compact_limit,
            "line_count": args.line_count,
            "managed_idle_seconds": args.managed_idle_seconds,
            "vanilla_codex_bin": str(args.vanilla_codex_bin),
            "managed_codex_bin": str(args.managed_codex_bin),
            "intendant_bin": str(args.intendant_bin),
        },
    }
    if not args.skip_vanilla:
        print("running vanilla Codex pressure baseline...", flush=True)
        summary["vanilla"] = run_vanilla(args, run_root, workspace)
    if not args.skip_managed:
        print("running managed Intendant/Codex stress scenario...", flush=True)
        summary["managed"] = run_managed(args, run_root, workspace)
    summary["pass_fail"] = summarize_pass_fail(summary)
    write_text(run_root / "summary.json", json.dumps(summary, indent=2, ensure_ascii=False))
    print(json.dumps(summary["pass_fail"], indent=2), flush=True)
    print(f"summary: {run_root / 'summary.json'}", flush=True)
    return 0 if summary["pass_fail"].get("production_ready_gate") else 2


if __name__ == "__main__":
    raise SystemExit(main())
