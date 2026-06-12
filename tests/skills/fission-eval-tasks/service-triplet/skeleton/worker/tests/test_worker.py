#!/usr/bin/env python3
"""Starter test for worker compute (see worker/SPEC.md)."""
import json
import subprocess
import sys
from pathlib import Path

WORKER = Path(__file__).resolve().parents[1] / "worker.py"


def compute(op, input_value):
    p = subprocess.run([sys.executable, str(WORKER), "compute", op, json.dumps(input_value)],
                       capture_output=True, text=True, timeout=30)
    assert p.returncode == 0, "exit %s: %s" % (p.returncode, p.stderr.strip())
    return json.loads(p.stdout)


CASES = [
    ("sum", [1, 2, 3], {"status": "done", "result": 6}),
    ("max", [4, 9, 2], {"status": "done", "result": 9}),
    ("sort_desc", [3, 1, 2], {"status": "done", "result": [3, 2, 1]}),
    ("reverse", "abc", {"status": "done", "result": "cba"}),
    ("wordcount", "  a  b c ", {"status": "done", "result": 3}),
    ("uppercase", "aBc", {"status": "done", "result": "ABC"}),
]


def main():
    for op, inp, want in CASES:
        got = compute(op, inp)
        assert got == want, "compute(%r, %r) = %r, want %r" % (op, inp, got, want)
    # error paths: status must be "error"; result is not checked.
    for op, inp in [("max", []), ("sum", "nope"), ("reverse", 5), ("frobnicate", 1)]:
        got = compute(op, inp)
        assert got.get("status") == "error", "compute(%r, %r) = %r, want status error" % (op, inp, got)
    print("worker starter test: OK")


if __name__ == "__main__":
    main()
