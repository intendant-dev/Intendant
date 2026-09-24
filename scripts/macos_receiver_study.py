"""Fixed observation-only study of the production bound-receiver HTTP path.

Collection completion is not availability or keyboard-delivery evidence.
The caller owns disposable setup/cleanup. This module has no input API.
"""
from collections import Counter
import math
import re
import statistics
import time

PLAN = (("first", 4), ("second", 4), ("protected", 2), ("first", 2))
TIMING_KEYS = ("ax_copy_us", "ax_timeout_us", "budget_before_us", "budget_after_us")

def require(condition, message):
    if not condition:
        raise RuntimeError(message)

def validate_options(enabled, keyboard_target, click_first, arrow=False, placement=False, controls=False):
    if enabled and (not keyboard_target or not click_first or arrow or placement or controls):
        raise ValueError("receiver study requires keyboard-target and explicit click-first; excludes action/placement/controls profiles")

def classify_refusal(error):
    """Extract observed diagnostics, never infer cause or human activity."""
    require(isinstance(error, str) and 0 < len(error) <= 8192, "invalid refusal diagnostic")
    status = re.search(r"(kAXError\w+|unknown AXError) \((-?\d{1,11})\); value_present=(true|false)", error)
    native = None
    if status:
        native = {"name": status[1], "code": int(status[2]), "value_present": status[3] == "true"}
    if native:
        category = "ax_cannot_complete" if native["code"] == -25204 else "ax_other"
    elif "human global focused object changed" in error or "human focus changed" in error:
        category = "human_focus_changed"
    elif "application-local focused receiver changed" in error:
        category = "receiver_changed"
    elif "focused keyboard receiver is protected" in error:
        category = "protected_receiver"
    elif "application-local focused receiver is absent" in error:
        category = "receiver_absent"
    elif "budget" in error or "deadline" in error:
        category = "deadline"
    else:
        category = "other_refusal"
    timing = {}
    for key in TIMING_KEYS:
        match = re.search(r"\b" + key + r"=(\d{1,20})(?!\d)", error)
        if match:
            timing[key] = int(match[1])
    return {"category": category, "native_status": native,
            "native_timing": timing if len(timing) == len(TIMING_KEYS) else None}

def validate_reply(reply, state, window, validate_geometry):
    require(isinstance(reply, dict) and type(reply.get("ok")) is bool, "malformed inspection envelope")
    if reply["ok"]:
        require(set(reply) == {"ok", "keyboard_target"}, "unexpected successful inspection fields")
        target = reply["keyboard_target"]
        require(isinstance(target, dict) and set(target) == {
            "role", "bounds", "enabled", "keyboard_dispatch_supported"}, "unexpected receiver fields")
        require(target["role"] == "AXTextField" and target["enabled"] is True
                and target["keyboard_dispatch_supported"] is False, "receiver metadata or authority mismatch")
        bounds = target["bounds"]
        require(isinstance(bounds, dict) and set(bounds) == {"x", "y", "width", "height"}, "invalid receiver bounds")
        require(all(type(v) in (int, float) and math.isfinite(v) and abs(v) <= 1000000
                    for v in bounds.values()) and bounds["width"] > 0 and bounds["height"] > 0,
                "nonfinite or invalid receiver bounds")
        require(state["active"] != "protected", "protected receiver unexpectedly accepted")
        validate_geometry(target, state, window)
        return {"category": "accepted", "target": target}
    require(set(reply) == {"ok", "error"}, "unexpected refused inspection fields")
    error = reply["error"]
    classification = classify_refusal(error)
    require(not any(x in error for x in ("synthetic-secret", "fixture-a", "fixture-b")), "fixture content in diagnostic")
    return dict(classification, error=error)

def validate_state(state, receiver):
    require(isinstance(state, dict) and state.get("active") == receiver, "fixture receiver was not selected")
    require(type(state.get("key_event_count")) is int and state["key_event_count"] == 0
            and state.get("key_overflow") is False, "unexpected keyboard input during observation study")
    rect, metrics = state.get("rect"), state.get("metrics")
    require(isinstance(rect, dict) and all(k in rect for k in ("x","y","width","height"))
            and all(type(rect[k]) in (int,float) and math.isfinite(rect[k]) and abs(rect[k]) <= 1000000
                    for k in ("x","y","width","height"))
            and rect["width"] > 0 and rect["height"] > 0, "missing or invalid fixture rectangle")
    required = ("screen_x","screen_y","outer_width","outer_height","inner_width","inner_height","scale","zoom","scroll_x","scroll_y")
    require(isinstance(metrics,dict) and all(type(metrics.get(k)) in (int,float)
            and math.isfinite(metrics[k]) and abs(metrics[k]) <= 1000000 for k in required),
            "missing or invalid fixture metrics")
    require(state.get("click_overflow") is False and isinstance(state.get("click_events"), list) and len(state["click_events"]) == 3,
            "missing setup mouse evidence")


def summarize(report):
    rows = report["samples"]
    plain = [r for r in rows if r["receiver"] != "protected"]
    protected = [r for r in rows if r["receiver"] == "protected"]
    latencies = [r["client_elapsed_us"] for r in rows if "client_elapsed_us" in r]
    return {
        "planned_samples": sum(count for _, count in PLAN),
        "recorded_samples": len(rows),
        "attempted_reads": sum(r.get("read_attempted") is True for r in rows),
        "nonprotected_samples": len(plain),
        "accepted_nonprotected": sum(r.get("category") == "accepted" and r.get("fixture_unchanged") is True for r in plain),
        "protected_samples": len(protected),
        "protected_refusal_reasons": dict(Counter(r.get("category", "incomplete") for r in protected)),
        "refused_protected": sum(r.get("reply_ok") is False for r in protected),
        "categories": dict(Counter(r.get("category", "incomplete") for r in rows)),
        "all_nonprotected_reads_succeeded": report["completed"] and bool(plain)
            and all(r.get("category") == "accepted" for r in plain),
        "client_latency_us": {"min": min(latencies), "median": statistics.median(latencies),
                              "max": max(latencies)} if latencies else None,
        "client_latency_includes_transport_and_process_startup": True,
        "native_failure_timings_are_separate": True,
        "continuous_isolation_verified": False,
        "human_activity_attribution": "undetermined",
    }

def collect(read, select, state, observe, validate_geometry, window, report,
            checkpoint, deadline, clock=time.monotonic):
    """One fixed series. A refusal does not trigger a repeated action.

    Only a well-formed production refusal permits the next planned read.
    Transport loss, malformed/unsafe replies, changed fixture or lost observation
    abort collection. Checkpoints precede each read and follow every exit.
    """
    require(math.isfinite(deadline), "finite study deadline required")
    report.update(completed=False, measurement_valid=False, samples=[],
                  plan=[{"receiver": r, "count": n} for r, n in PLAN],
                  keyboard_input_requested=False, input_delivery_verified=False,
                  automatic_action_retry=False)
    checkpoint()
    try:
        for phase, (receiver, count) in enumerate(PLAN):
            for within_phase in range(count):
                row = {"index": len(report["samples"]), "phase": phase,
                       "receiver": receiver, "read_attempted": False}
                report["samples"].append(row)
                checkpoint()
                require(clock() < deadline, "study deadline before planned read")
                # Only phase transitions select another synthetic DOM field.
                before = select(receiver) if within_phase == 0 else state()
                validate_state(before, receiver)
                desktop_before = observe()
                require(clock() < deadline, "study deadline before inspection")
                row["read_attempted"] = True
                checkpoint()
                start = clock()
                try:
                    reply = read()
                finally:
                    row["client_elapsed_us"] = max(0, int((clock() - start) * 1000000))
                if isinstance(reply, dict) and type(reply.get("ok")) is bool:
                    row["reply_ok"] = reply["ok"]
                row.update(validate_reply(reply, before, window, validate_geometry))
                after = state()
                validate_state(after, receiver)
                require(all(before.get(k) == after.get(k) for k in
                            ("active", "rect", "metrics", "click_events", "click_overflow")),
                        "fixture changed during read; no attribution assumed")
                row["desktop"] = {"before": desktop_before, "after": observe(),
                                  "change_attribution": "undetermined", "continuous_isolation_verified": False}
                require(clock() < deadline, "study deadline after inspection")
                row["fixture_unchanged"] = True
                checkpoint()
        report["completed"] = True
        report["measurement_valid"] = True
    except Exception as error:
        report["stop_reason"] = str(error)[:8192]
        raise
    finally:
        report["summary"] = summarize(report)
        checkpoint()
