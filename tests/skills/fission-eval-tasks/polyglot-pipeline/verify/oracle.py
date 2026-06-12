#!/usr/bin/env python3
"""Independent reference implementation of the polyglot-pipeline specs, used by
the grader as the oracle. This is deliberately a SECOND implementation (the
agent-facing reference/ solution is a third); when two independent
implementations agree on randomly generated inputs, both are almost certainly
correct. Nothing here ever runs the agent's code — it only defines truth.

Pure stdlib. See each component's SPEC.md for the contract these encode.
"""
import csv
import io
import re
from datetime import date as _date

AMOUNT_RE = re.compile(r"^[0-9]+(\.[0-9]{1,2})?$")
ISO_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")
US_RE = re.compile(r"^[0-9]{2}/[0-9]{2}/[0-9]{4}$")
COLUMNS = ["id", "name", "email", "amount", "date", "tags"]


# ---------------------------------------------------------------- normalizer
def parse_amount(raw):
    """Return a number, or None to reject. Mirrors SPEC step 6 exactly."""
    s = raw
    neg = False
    if s.startswith("-"):
        neg = True
        s = s[1:]
    if s.startswith("$"):
        s = s[1:]
    s = s.replace(",", "")
    if not AMOUNT_RE.match(s):
        return None
    val = float(s)
    if neg:
        val = -val
    # Collapse -0.0 to 0.0 and integral floats stay numerically equal to ints.
    return val + 0.0


def parse_date(raw):
    """Return 'YYYY-MM-DD' or None to reject. Mirrors SPEC step 7."""
    if ISO_RE.match(raw):
        y, m, d = int(raw[0:4]), int(raw[5:7]), int(raw[8:10])
    elif US_RE.match(raw):
        m, d, y = int(raw[0:2]), int(raw[3:5]), int(raw[6:10])
    else:
        return None
    try:
        return _date(y, m, d).isoformat()
    except ValueError:
        return None


def parse_email(raw):
    """Return (ok, value). value is None for empty (-> JSON null)."""
    if raw == "":
        return True, None
    low = raw.lower()
    at = low.find("@")
    if low.count("@") != 1 or at <= 0 or at >= len(low) - 1:
        return False, None
    return True, low


def parse_tags(raw):
    parts = [p.strip() for p in raw.split(";")]
    parts = [p for p in parts if p]
    return sorted(set(parts))


def normalize_rows(rows):
    """rows: list of header+data lists already parsed from CSV. Returns the
    list of accepted record dicts in input order."""
    if not rows:
        return []
    header = [h.strip().lower() for h in rows[0]]
    idx = {name: header.index(name) for name in COLUMNS if name in header}
    out = []
    for raw_row in rows[1:]:
        def cell(name):
            i = idx.get(name)
            if i is None or i >= len(raw_row):
                return ""
            return raw_row[i]

        cells = {name: cell(name) for name in COLUMNS}
        # 1. blank row (raw, pre-trim)
        if all(v.strip() == "" for v in cells.values()):
            continue
        # 2. trim
        cells = {k: v.strip() for k, v in cells.items()}
        # 3. id
        if cells["id"] == "":
            continue
        # 5. email
        ok, email = parse_email(cells["email"])
        if not ok:
            continue
        # 6. amount
        amount = parse_amount(cells["amount"])
        if amount is None:
            continue
        # 7. date
        d = parse_date(cells["date"])
        if d is None:
            continue
        out.append({
            "id": cells["id"],
            "name": cells["name"],
            "email": email,
            "amount": amount,
            "date": d,
            "tags": parse_tags(cells["tags"]),
        })
    return out


def normalize_csv_text(text):
    rows = list(csv.reader(io.StringIO(text)))
    return normalize_rows(rows)


# --------------------------------------------------------------------- dedup
def dedup_records(seq):
    """seq: list of record dicts in global order (files in arg order, lines in
    file order). Returns merged list sorted by id. Mirrors dedup/SPEC.md."""
    groups = {}  # id -> list of (position, record)
    for pos, rec in enumerate(seq):
        groups.setdefault(rec["id"], []).append((pos, rec))
    out = []
    for rid, members in groups.items():
        # winner: newest date (string compare), tie -> largest position.
        winner = max(members, key=lambda pr: (pr[1]["date"], pr[0]))[1]
        union = set()
        for _pos, rec in members:
            union.update(rec.get("tags", []))
        out.append({
            "id": winner["id"],
            "name": winner["name"],
            "email": winner["email"],
            "amount": winner["amount"],
            "date": winner["date"],
            "tags": sorted(union),
        })
    out.sort(key=lambda r: r["id"])
    return out


# -------------------------------------------------------------------- report
def report_records(records):
    """Mirrors report/SPEC.md."""
    count = len(records)
    total = round(sum(r["amount"] for r in records) + 0.0, 2)
    by_tag = {}
    for r in records:
        for t in set(r.get("tags", [])):
            by_tag[t] = by_tag.get(t, 0) + 1
    ranked = sorted(records, key=lambda r: (-r["amount"], r["id"]))
    top = [{"id": r["id"], "amount": r["amount"]} for r in ranked[:3]]
    return {
        "count": count,
        "total_amount": total,
        "by_tag": by_tag,
        "top_spenders": top,
    }


# --------------------------------------------------------- comparison helpers
def _num_close(a, b, tol=1e-6):
    return isinstance(a, (int, float)) and isinstance(b, (int, float)) \
        and not isinstance(a, bool) and not isinstance(b, bool) \
        and abs(a - b) <= tol


def json_equal(a, b, tol=1e-6):
    """Structural JSON equality with numeric tolerance (so 42 == 42.0 and
    float dust is ignored). Lists are order-sensitive (the specs fix order)."""
    if _num_close(a, b, tol):
        return True
    if isinstance(a, bool) or isinstance(b, bool):
        return a is b
    if isinstance(a, dict) and isinstance(b, dict):
        if set(a) != set(b):
            return False
        return all(json_equal(a[k], b[k], tol) for k in a)
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(json_equal(x, y, tol) for x, y in zip(a, b))
    return a == b


def report_equal(got, want):
    """report-specific compare: total_amount rounded to 2 dp, everything else
    via json_equal."""
    if not isinstance(got, dict) or set(got) != set(want):
        return False
    if got.get("count") != want.get("count"):
        return False
    try:
        if round(float(got["total_amount"]) + 0.0, 2) != round(float(want["total_amount"]) + 0.0, 2):
            return False
    except (TypeError, ValueError, KeyError):
        return False
    if not json_equal(got.get("by_tag"), want.get("by_tag")):
        return False
    if not json_equal(got.get("top_spenders"), want.get("top_spenders")):
        return False
    return True
