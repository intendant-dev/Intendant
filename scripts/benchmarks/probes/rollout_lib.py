"""Shared helpers for reading Codex rollout JSONL files.

Used by inject_probes.py (live answer extraction) and grade_probes.py
(ground-truth extraction). Rollout format (codex-minimal-lineage and npm
codex 0.133.0 alike): one JSON object per line,
    {"timestamp": ..., "type": "session_meta"|"event_msg"|"response_item",
     "payload": {...}}
- event_msg payloads of interest: {"type": "task_complete",
  "last_agent_message": ...}
- response_item payloads of interest: {"type": "message", "role": ...,
  "content": [{"type": "input_text"|"output_text", "text": ...}]},
  {"type": "function_call", "name": ..., "arguments": ...},
  {"type": "function_call_output", "output": ...}
Fission branch rollouts (managed lane) carry a developer message containing
"<fission_charter>"; the parent rollout never does.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path

FISSION_CHARTER_MARK = "<fission_charter>"

# Intendant recovery/records MCP tools a managed session may legitimately use
# during a probe turn (counted, not penalized). Everything else is flagged.
RECOVERY_TOOLS = {
    "get_status",
    "list_rewind_anchors",
    "inspect_rewind_anchor",
    "rewind_context",
    "rewind_backout",
}


@dataclass
class RolloutLine:
    lineno: int  # 1-based
    type: str
    payload: dict


@dataclass
class TaskComplete:
    lineno: int
    turn_id: str | None
    last_agent_message: str | None


@dataclass
class ToolCall:
    lineno: int
    name: str
    arguments: str


def iter_rollout(path: Path):
    """Yield RolloutLine for each parseable line (tolerates partial last line)."""
    with path.open("r", errors="replace") as handle:
        for lineno, raw in enumerate(handle, start=1):
            raw = raw.strip()
            if not raw:
                continue
            try:
                obj = json.loads(raw)
            except ValueError:
                continue  # partial line mid-write
            if not isinstance(obj, dict):
                continue
            yield RolloutLine(
                lineno=lineno,
                type=str(obj.get("type", "")),
                payload=obj.get("payload") or {},
            )


def find_rollouts(codex_home: Path) -> list[Path]:
    """All rollout files under <codex_home>/sessions, newest mtime first."""
    sessions = codex_home / "sessions"
    if not sessions.is_dir():
        return []
    files = sorted(
        sessions.rglob("rollout-*.jsonl"),
        key=lambda p: p.stat().st_mtime,
        reverse=True,
    )
    return files


def is_branch_rollout(path: Path) -> bool:
    """True only when the charter is a *developer message* response_item.

    A parent rollout can quote the marker inside a tool output (observed
    live 2026-06-12: terminal scrollback echoed into a function_call_output)
    — a raw byte scan would misclassify the parent as a branch, which both
    hid completions from the lane poll and broke parent-rollout selection
    here. Parse the line before believing the marker.
    """
    try:
        with path.open("r", errors="replace") as handle:
            for raw in handle:
                if FISSION_CHARTER_MARK not in raw:
                    continue
                try:
                    obj = json.loads(raw)
                except ValueError:
                    continue
                if not isinstance(obj, dict):
                    continue
                payload = obj.get("payload") or {}
                if (
                    obj.get("type") == "response_item"
                    and payload.get("type") == "message"
                    and (payload.get("role") or "") == "developer"
                ):
                    return True
    except OSError:
        pass
    return False


def parent_rollouts(codex_home: Path) -> list[Path]:
    return [p for p in find_rollouts(codex_home) if not is_branch_rollout(p)]


def session_id_of(path: Path) -> str | None:
    """Thread id from the session_meta line (fallback: filename suffix)."""
    for line in iter_rollout(path):
        if line.type == "session_meta":
            sid = line.payload.get("id")
            if sid:
                return str(sid)
            break
    # rollout-<ts>-<uuid>.jsonl — uuid is the last 36 chars of the stem
    stem = path.stem
    return stem[-36:] if len(stem) >= 36 else None


def task_completes(path: Path, after_line: int = 0) -> list[TaskComplete]:
    out: list[TaskComplete] = []
    for line in iter_rollout(path):
        if line.lineno <= after_line or line.type != "event_msg":
            continue
        if line.payload.get("type") == "task_complete":
            out.append(
                TaskComplete(
                    lineno=line.lineno,
                    turn_id=line.payload.get("turn_id"),
                    last_agent_message=line.payload.get("last_agent_message"),
                )
            )
    return out


def line_count(path: Path) -> int:
    try:
        with path.open("rb") as handle:
            return sum(1 for _ in handle)
    except OSError:
        return 0


def tool_calls(path: Path, after_line: int = 0, before_line: int | None = None) -> list[ToolCall]:
    out: list[ToolCall] = []
    for line in iter_rollout(path):
        if line.lineno <= after_line:
            continue
        if before_line is not None and line.lineno >= before_line:
            break
        if line.type != "response_item":
            continue
        if line.payload.get("type") != "function_call":
            continue
        out.append(
            ToolCall(
                lineno=line.lineno,
                name=str(line.payload.get("name", "")),
                arguments=str(line.payload.get("arguments", "")),
            )
        )
    return out


@dataclass
class MessageItem:
    lineno: int
    role: str
    text: str


def messages(path: Path) -> list[MessageItem]:
    out: list[MessageItem] = []
    for line in iter_rollout(path):
        if line.type != "response_item" or line.payload.get("type") != "message":
            continue
        texts = [
            part.get("text", "")
            for part in line.payload.get("content", [])
            if isinstance(part, dict)
        ]
        out.append(
            MessageItem(
                lineno=line.lineno,
                role=str(line.payload.get("role", "")),
                text="\n".join(t for t in texts if t),
            )
        )
    return out


@dataclass
class ToolOutput:
    lineno: int
    output: str


def tool_outputs(path: Path) -> list[ToolOutput]:
    out: list[ToolOutput] = []
    for line in iter_rollout(path):
        if line.type != "response_item":
            continue
        if line.payload.get("type") != "function_call_output":
            continue
        raw = line.payload.get("output", "")
        if isinstance(raw, dict):
            raw = raw.get("content") or json.dumps(raw)
        out.append(ToolOutput(lineno=line.lineno, output=str(raw)))
    return out


@dataclass
class ProbeTurnStats:
    """Tool usage observed between probe injection and the answering event."""

    recovery_tool_calls: dict = field(default_factory=dict)
    other_tool_calls: dict = field(default_factory=dict)

    @property
    def tainted(self) -> bool:
        return bool(self.other_tool_calls)


def probe_turn_stats(path: Path, after_line: int, before_line: int) -> ProbeTurnStats:
    stats = ProbeTurnStats()
    for call in tool_calls(path, after_line=after_line, before_line=before_line):
        bucket = (
            stats.recovery_tool_calls
            if call.name in RECOVERY_TOOLS
            else stats.other_tool_calls
        )
        bucket[call.name] = bucket.get(call.name, 0) + 1
    return stats
