#!/usr/bin/env python3
"""Starter test for normalizer/normalize.py (see normalizer/SPEC.md)."""
import json
import subprocess
import sys
import tempfile
from pathlib import Path

NORMALIZE = Path(__file__).resolve().parents[1] / "normalize.py"

# Header order is shuffled on purpose: columns map by name, not position.
INPUT_CSV = """\
date,id,tags,amount,name,email
2025-01-15 ,a1,"red; blue;red","$1,234.50","Lee, Ann",ANN@Example.com
01/09/2025, b2,,-12,Bo,
,,,,,
2025-13-01,c3,x,5,Cy,cy@ex.com
2025-02-10,,y,5,NoId,n@ex.com
2025-03-05,d4,z,1.2.3,Dee,dee@ex.com
2025-04-01,e5,solo,0.75,Eve,not-an-email
2025-05-02,f6,m;m; n,33,Fay, fay@EX.com
"""

EXPECTED = [
    {"id": "a1", "name": "Lee, Ann", "email": "ann@example.com",
     "amount": 1234.5, "date": "2025-01-15", "tags": ["blue", "red"]},
    {"id": "b2", "name": "Bo", "email": None,
     "amount": -12, "date": "2025-01-09", "tags": []},
    {"id": "f6", "name": "Fay", "email": "fay@ex.com",
     "amount": 33, "date": "2025-05-02", "tags": ["m", "n"]},
]


def main():
    with tempfile.TemporaryDirectory() as td:
        src = Path(td) / "in.csv"
        dst = Path(td) / "out.jsonl"
        src.write_text(INPUT_CSV, encoding="utf-8")
        proc = subprocess.run([sys.executable, str(NORMALIZE), str(src), str(dst)],
                              capture_output=True, text=True, timeout=60)
        assert proc.returncode == 0, f"exit {proc.returncode}: {proc.stderr.strip()}"
        lines = [ln for ln in dst.read_text(encoding="utf-8").splitlines() if ln.strip()]
        got = [json.loads(ln) for ln in lines]
        assert got == EXPECTED, (
            "output mismatch\n--- got ---\n%s\n--- want ---\n%s"
            % (json.dumps(got, indent=1), json.dumps(EXPECTED, indent=1)))
    print("normalizer starter test: OK")


if __name__ == "__main__":
    main()
