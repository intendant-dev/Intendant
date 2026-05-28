#!/usr/bin/env python3
"""Summarize Harbor / Terminal-Bench runs with Intendant context signals.

The top-level Harbor result gives aggregate pass/cost/token counts, but the
Codex lineage work needs per-task context-management diagnostics too. This
script reads one or more Harbor run directories and emits a compact table plus
optional JSON records with:

- Terminal-Bench reward and exception status
- per-task token/cost counters
- agent/verifier durations
- max Intendant context snapshot token counts
- compaction, rewind, and auth-error signals from task logs
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
from pathlib import Path
from typing import Any


AUTH_ERROR_TERMS = (
    "refresh_token_reused",
    "token_expired",
    "401 Unauthorized",
    "codex auth",
)
COMPACTION_TERMS = (
    "context_compacted",
    "contextCompaction",
    "context compaction",
    "auto-compaction",
    "auto_compaction",
)
REWIND_TERMS = (
    "rewind_context",
    "context_rewind",
    "conversation_rolled_back",
    "rolled_back",
)
BACKEND_ERROR_TERMS = (
    "responseStreamDisconnected",
    "stream disconnected",
    "codex backend error",
)


def load_json(path: Path) -> dict[str, Any] | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8", errors="replace"))
    except (OSError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def parse_time(value: str | None) -> dt.datetime | None:
    if not value:
        return None
    if value.endswith("Z"):
        value = value[:-1] + "+00:00"
    try:
        return dt.datetime.fromisoformat(value)
    except ValueError:
        return None


def seconds_between(start: str | None, finish: str | None) -> float | None:
    started = parse_time(start)
    finished = parse_time(finish)
    if started is None or finished is None:
        return None
    return max(0.0, (finished - started).total_seconds())


def iter_jsonl(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    try:
        with path.open("r", encoding="utf-8", errors="replace") as handle:
            for line in handle:
                try:
                    value = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(value, dict):
                    rows.append(value)
    except OSError:
        pass
    return rows


def count_terms(path: Path, terms: tuple[str, ...]) -> int:
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return 0
    return sum(text.count(term) for term in terms)


def context_metrics(trial_dir: Path) -> dict[str, Any]:
    session_logs = sorted(trial_dir.glob("agent/intendant/session.jsonl"))
    max_tokens = None
    max_items = None
    snapshot_count = 0
    compactions = 0
    rewinds = 0
    auth_errors = 0
    backend_errors = 0

    for path in session_logs:
        for row in iter_jsonl(path):
            row_text = json.dumps(row, separators=(",", ":"))
            if any(term in row_text for term in COMPACTION_TERMS):
                compactions += 1
            if any(term in row_text for term in REWIND_TERMS):
                rewinds += 1
            if any(term in row_text for term in AUTH_ERROR_TERMS):
                auth_errors += 1
            if any(term in row_text for term in BACKEND_ERROR_TERMS):
                backend_errors += 1
            if row.get("event") != "context_snapshot":
                continue
            data = row.get("data")
            if not isinstance(data, dict):
                data = row
            token_count = data.get("token_count")
            item_count = data.get("item_count")
            if isinstance(token_count, int):
                snapshot_count += 1
                max_tokens = token_count if max_tokens is None else max(max_tokens, token_count)
            if isinstance(item_count, int):
                max_items = item_count if max_items is None else max(max_items, item_count)

    for path in [trial_dir / "trial.log", trial_dir / "agent" / "codex.txt"]:
        auth_errors += count_terms(path, AUTH_ERROR_TERMS)
        compactions += count_terms(path, COMPACTION_TERMS)
        rewinds += count_terms(path, REWIND_TERMS)
        backend_errors += count_terms(path, BACKEND_ERROR_TERMS)

    return {
        "context_snapshot_count": snapshot_count,
        "max_context_tokens": max_tokens,
        "max_context_items": max_items,
        "compaction_signals": compactions,
        "rewind_signals": rewinds,
        "auth_error_signals": auth_errors,
        "backend_error_signals": backend_errors,
    }


def summarize_trial(trial_dir: Path) -> dict[str, Any] | None:
    result = load_json(trial_dir / "result.json")
    if result is None:
        return None
    agent_result = result.get("agent_result") or {}
    verifier_result = result.get("verifier_result") or {}
    rewards = verifier_result.get("rewards") if isinstance(verifier_result, dict) else None
    reward = rewards.get("reward") if isinstance(rewards, dict) else None
    agent_execution = result.get("agent_execution") or {}
    verifier = result.get("verifier") or {}
    exception_info = result.get("exception_info")

    summary = {
        "task_name": result.get("task_name"),
        "trial_name": result.get("trial_name", trial_dir.name),
        "reward": reward,
        "exception": exception_info,
        "n_input_tokens": agent_result.get("n_input_tokens"),
        "n_cache_tokens": agent_result.get("n_cache_tokens"),
        "n_output_tokens": agent_result.get("n_output_tokens"),
        "cost_usd": agent_result.get("cost_usd"),
        "agent_seconds": seconds_between(
            agent_execution.get("started_at"),
            agent_execution.get("finished_at"),
        ),
        "verifier_seconds": seconds_between(
            verifier.get("started_at"),
            verifier.get("finished_at"),
        ),
    }
    summary.update(context_metrics(trial_dir))
    return summary


def summarize_run(run_dir: Path) -> dict[str, Any]:
    top = load_json(run_dir / "result.json") or {}
    trials = [
        item
        for item in (summarize_trial(path) for path in sorted(run_dir.glob("*__*")))
        if item is not None
    ]
    rewards = [trial["reward"] for trial in trials if isinstance(trial.get("reward"), (int, float))]
    return {
        "run_dir": str(run_dir),
        "started_at": top.get("started_at"),
        "finished_at": top.get("finished_at"),
        "n_total_trials": top.get("n_total_trials"),
        "n_trials_with_result": len(trials),
        "mean_reward": (sum(rewards) / len(rewards)) if rewards else None,
        "aggregate": top.get("stats", {}),
        "trials": trials,
    }


def fmt_float(value: Any, digits: int = 2) -> str:
    if isinstance(value, (int, float)):
        return f"{value:.{digits}f}"
    return "-"


def print_table(runs: list[dict[str, Any]]) -> None:
    for run in runs:
        print(f"\n{run['run_dir']}")
        print(
            "task reward cost input cache output agent_s max_ctx comp rewind auth backend"
        )
        for trial in run["trials"]:
            print(
                " ".join(
                    [
                        str(trial.get("task_name")),
                        fmt_float(trial.get("reward"), 1),
                        fmt_float(trial.get("cost_usd"), 3),
                        str(trial.get("n_input_tokens") or "-"),
                        str(trial.get("n_cache_tokens") or "-"),
                        str(trial.get("n_output_tokens") or "-"),
                        fmt_float(trial.get("agent_seconds"), 0),
                        str(trial.get("max_context_tokens") or "-"),
                        str(trial.get("compaction_signals") or 0),
                        str(trial.get("rewind_signals") or 0),
                        str(trial.get("auth_error_signals") or 0),
                        str(trial.get("backend_error_signals") or 0),
                    ]
                )
            )
        print(
            "summary "
            f"trials={run['n_trials_with_result']} "
            f"mean_reward={fmt_float(run['mean_reward'], 3)}"
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("run_dir", nargs="+", type=Path)
    parser.add_argument("--json", action="store_true", help="Emit JSON instead of a table")
    args = parser.parse_args()

    runs = [summarize_run(path) for path in args.run_dir]
    if args.json:
        print(json.dumps(runs, indent=2, sort_keys=True))
    else:
        print_table(runs)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
