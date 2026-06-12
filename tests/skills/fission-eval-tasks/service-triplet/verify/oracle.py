#!/usr/bin/env python3
"""Independent oracle for service-triplet op semantics + JSON comparison.

A second implementation of worker/SPEC.md (the agent-facing reference/ solution
is a third). Defines truth; never runs the agent's code. Pure stdlib.
"""


def _is_number(x):
    return isinstance(x, (int, float)) and not isinstance(x, bool)


def _num_list(x):
    return isinstance(x, list) and all(_is_number(e) for e in x)


def compute(op, value):
    """Return (status, result). Mirrors worker/SPEC.md exactly."""
    if op == "sum":
        if _num_list(value):
            return "done", sum(value)
        return "error", None
    if op == "max":
        if _num_list(value) and len(value) > 0:
            return "done", max(value)
        return "error", None
    if op == "sort_desc":
        if _num_list(value):
            return "done", sorted(value, reverse=True)
        return "error", None
    if op == "reverse":
        if isinstance(value, str):
            return "done", value[::-1]
        return "error", None
    if op == "wordcount":
        if isinstance(value, str):
            return "done", len(value.split())
        return "error", None
    if op == "uppercase":
        if isinstance(value, str):
            return "done", value.upper()
        return "error", None
    return "error", None


# --------------------------------------------------------- comparison helpers
def _num_close(a, b, tol=1e-9):
    return _is_number(a) and _is_number(b) and abs(a - b) <= tol + 1e-9 * max(abs(a), abs(b))


def json_equal(a, b, tol=1e-9):
    if _num_close(a, b, tol):
        return True
    if isinstance(a, bool) or isinstance(b, bool):
        return a is b
    if isinstance(a, dict) and isinstance(b, dict):
        return set(a) == set(b) and all(json_equal(a[k], b[k], tol) for k in a)
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(json_equal(x, y, tol) for x, y in zip(a, b))
    return a == b
