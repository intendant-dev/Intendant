#!/usr/bin/env python3
"""Reference normalizer (agent-facing solution). See normalizer/SPEC.md.

Excluded from agent visibility by the SKILL runner (lives under reference/,
which is never copied into the agent's workdir)."""
import csv
import datetime
import json
import re
import sys

COLUMNS = ("id", "name", "email", "amount", "date", "tags")
_AMOUNT = re.compile(r"^[0-9]+(\.[0-9]{1,2})?$")
_ISO = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")
_US = re.compile(r"^[0-9]{2}/[0-9]{2}/[0-9]{4}$")


def conv_amount(s):
    sign = 1
    if s[:1] == "-":
        sign, s = -1, s[1:]
    if s[:1] == "$":
        s = s[1:]
    s = s.replace(",", "")
    if not _AMOUNT.match(s):
        return None
    return sign * float(s)


def conv_date(s):
    if _ISO.match(s):
        fmt, parts = "%Y-%m-%d", (int(s[:4]), int(s[5:7]), int(s[8:10]))
    elif _US.match(s):
        parts = (int(s[6:10]), int(s[:2]), int(s[3:5]))
    else:
        return None
    try:
        return datetime.date(*parts).strftime("%Y-%m-%d")
    except ValueError:
        return None


def conv_email(s):
    if not s:
        return True, None
    s = s.lower()
    if s.count("@") != 1:
        return False, None
    local, _, domain = s.partition("@")
    if not local or not domain:
        return False, None
    return True, s


def conv_tags(s):
    seen = []
    for piece in s.split(";"):
        piece = piece.strip()
        if piece and piece not in seen:
            seen.append(piece)
    return sorted(seen)


def normalize(reader, sink):
    rows = iter(reader)
    try:
        header = [h.strip().lower() for h in next(rows)]
    except StopIteration:
        return
    pos = {c: header.index(c) for c in COLUMNS if c in header}

    def get(row, name):
        i = pos.get(name)
        return row[i] if (i is not None and i < len(row)) else ""

    for row in rows:
        fields = {c: get(row, c) for c in COLUMNS}
        if all(not v.strip() for v in fields.values()):
            continue
        fields = {c: v.strip() for c, v in fields.items()}
        if not fields["id"]:
            continue
        ok, email = conv_email(fields["email"])
        if not ok:
            continue
        amount = conv_amount(fields["amount"])
        if amount is None:
            continue
        date = conv_date(fields["date"])
        if date is None:
            continue
        rec = {
            "id": fields["id"],
            "name": fields["name"],
            "email": email,
            "amount": amount,
            "date": date,
            "tags": conv_tags(fields["tags"]),
        }
        sink.write(json.dumps(rec) + "\n")


def main(argv):
    if len(argv) != 3:
        print("usage: normalize.py INPUT.csv OUTPUT.jsonl", file=sys.stderr)
        return 2
    try:
        fin = open(argv[1], newline="", encoding="utf-8")
    except OSError as e:
        print("cannot read input: %s" % e, file=sys.stderr)
        return 1
    with fin, open(argv[2], "w", encoding="utf-8") as fout:
        normalize(csv.reader(fin), fout)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
