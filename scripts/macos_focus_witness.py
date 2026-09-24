"""Fixture-only focus correlation; not an input authorizer or isolation proof."""
import time
from collections import Counter
import macos_receiver_study as study

PLAN = (("stable", "first", ()), ("switch", "first", ("second",)),
        ("roundtrip", "second", ("first", "second")),
        ("churn", "first", ()), ("protected", "protected", ()),
        ("recovery", "first", ()))
require = study.require

def sample(value):
    require(isinstance(value, dict) and set(value) == {"status", "pid_status", "value_present", "valid"}, "native focus sample schema")
    require(all(type(value[k]) is int and -(2**31) <= value[k] < 2**31 for k in ("status", "pid_status")), "native status type")
    require(all(type(value[k]) is bool for k in ("valid", "value_present")), "native validity type")
    if value["valid"]:
        require(value["status"] == value["pid_status"] == 0 and value["value_present"], "contradictory native focus sample")

def validate(value, sequence, phase):
    require(isinstance(value, dict) and type(value.get("sequence")) is int
            and value["sequence"] == sequence and value.get("phase") == phase, "native witness sequence/phase")
    if phase == "before":
        require(set(value) == {"sequence", "phase", "human", "receiver"}, "native begin fields")
        sample(value["human"]); sample(value["receiver"]); return value
    require(set(value) == {"sequence", "phase", "complete", "human_before", "human_after",
            "receiver_before", "receiver_after", "human_changed", "receiver_changed",
            "foreground_changed", "target_foreground_observed", "elapsed_us",
            "continuous_isolation_verified"}, "native end fields")
    require(type(value["complete"]) is bool, "native completion type")
    require(type(value["elapsed_us"]) is int and 0 <= value["elapsed_us"] <= 30000000
            and value["complete"] is True, "native witness deadline")
    for who in ("human", "receiver"):
        a, b = value[who+"_before"], value[who+"_after"]; sample(a); sample(b)
        changed = value[who+"_changed"]
        require(type(changed) is bool if a["valid"] and b["valid"] else changed is None,
                "unknown native focus must not be unchanged")
    require(value["foreground_changed"] is None or type(value["foreground_changed"]) is bool, "foreground witness type")
    require(value["target_foreground_observed"] is False and value["continuous_isolation_verified"] is False,
            "foreground target or unsupported isolation claim")
    return value

def summarize(report):
    rows = report["samples"]
    transitions = [r for r in rows if r["case"] == "switch"]
    roundtrips = [r for r in rows if r["case"] == "roundtrip"]
    return {"planned": len(PLAN), "recorded": len(rows),
        "attempted_reads": sum(r.get("read_attempted") is True for r in rows),
        "categories": dict(Counter(r.get("category", "incomplete") for r in rows)),
        "native_transition_observed": bool(transitions) and all(r.get("native_after", {}).get("receiver_changed") is True for r in transitions),
        "roundtrip_native_endpoints_equal": bool(roundtrips) and all(r.get("controlled_transitions") == 2 and r.get("native_after", {}).get("receiver_changed") is False for r in roundtrips),
        "human_changed_intervals": sum(r.get("native_after", {}).get("human_changed") is True for r in rows),
        "human_unknown_intervals": sum("native_after" in r and r["native_after"].get("human_changed") is None for r in rows),
        "continuous_isolation_verified": False, "human_activity_attribution": "undetermined"}

def collect(read, select, state, witness, churn, stop_churn, validate_geometry,
            window, report, checkpoint, deadline, clock=time.monotonic):
    report.update(completed=False, measurement_valid=False, samples=[],
                  keyboard_input_requested=False, automatic_input_retry=False,
                  human_focus_mutated=False, plan=[x[0] for x in PLAN])
    checkpoint()
    try:
        for sequence, (case, initial, changes) in enumerate(PLAN, 1):
            row = {"case": case, "read_attempted": False, "controlled_transitions": 0}
            report["samples"].append(row); checkpoint()
            require(clock() < deadline, "focus study deadline")
            # Capture only disposable nonsecret field geometry for churn validation.
            candidates = [select(name) for name in ("first", "second")] if case == "churn" else []
            before = select(initial); study.validate_state(before, initial)
            row["native_before"] = validate(witness("before", sequence), sequence, "before")
            started_churn = False
            try:
                for name in changes:
                    observed = select(name); study.validate_state(observed, name)
                    row["controlled_transitions"] += 1
                expected = state(); study.validate_state(expected, expected["active"])
                if case == "churn":
                    started_churn = True; churn()
                require(clock() < deadline, "focus study deadline before read")
                row["read_attempted"] = True; checkpoint(); start = clock()
                try:
                    reply = read()
                finally:
                    row["client_elapsed_us"] = max(0, int((clock()-start)*1000000))
            finally:
                # Cleanup evidence cannot overwrite the original exception.
                if started_churn:
                    try: row["churn"] = stop_churn()
                    except Exception as error: row["churn_cleanup_error"] = str(error)[:512]
                try:
                    row["native_after"] = validate(witness("after",sequence), sequence, "after")
                except Exception as error:
                    row["native_finish_error"] = str(error)[:512]
                checkpoint()
            require("native_finish_error" not in row and "churn_cleanup_error" not in row, "focus witness cleanup failed")
            native = row["native_after"]
            require(native["human_before"] == row["native_before"]["human"] and
                    native["receiver_before"] == row["native_before"]["receiver"], "native baseline changed")
            after = state(); study.validate_state(after, after["active"])
            require(after["active"] in (("first", "second") if case == "churn" else (expected["active"],)), "unplanned receiver")
            require(all(before[k] == after[k] for k in ("metrics", "click_events", "click_overflow")), "unplanned fixture geometry or input")
            if case != "churn":
                require(all(expected[k] == after[k] for k in ("active", "rect", "metrics")), "fixture changed outside planned transition")
            if case == "churn":
                value = row["churn"]
                require(isinstance(value, dict) and set(value) == {"changes", "active"}
                        and type(value["changes"]) is int and 0 <= value["changes"] <= 8
                        and value["active"] is False, "invalid finite churn evidence")
                def geometry(target, ignored, bounds):
                    for candidate in candidates:
                        try: validate_geometry(target, candidate, bounds); return
                        except RuntimeError: pass
                    raise RuntimeError("churn reply does not match either known receiver")
            else: geometry = validate_geometry
            row.update(study.validate_reply(reply, after, window, geometry))
            row["reply_ok"] = reply["ok"]
            row["internal_snapshot_overlap_verified"] = False
            require(clock() < deadline, "focus study deadline after read"); checkpoint()
        report.update(completed=True, measurement_valid=True)
    except Exception as error:
        report["stop_reason"] = str(error)[:8192]; raise
    finally:
        report["summary"] = summarize(report); checkpoint()
