#!/usr/bin/env python3
"""Behavioral grader for polyglot-pipeline.

Generates inputs at check time from a random seed, runs the agent's tools, and
compares their output to the independent oracle in oracle.py. Emits the suite
JSON contract on stdout. Never inspects file/string presence; only behavior.

Usage: grade.py <scratch_workdir> [--seed N]   (scratch is graded in place)
"""
import argparse
import csv
import glob
import io
import json
import os
import random
import string
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import oracle  # noqa: E402

# The canonical Makefile is the task's own skeleton/Makefile (single source of
# truth); the grader pins it over the agent's copy before the integration run
# so a tampered Makefile can't fake the pipeline wiring.
TASK_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CANONICAL_MAKEFILE = os.path.join(TASK_ROOT, "skeleton", "Makefile")
PYTHON = sys.executable
RUN_TIMEOUT = 30
MAKE_TIMEOUT = 240


# ----------------------------------------------------------- random builders
def rand_id(rng):
    n = rng.randint(1, 6)
    return "".join(rng.choice(string.ascii_lowercase + string.digits) for _ in range(n))


def rand_name(rng):
    pool = ["Lee, Ann", "Bo", "", "  ", "O'Hara", "Zed Zee", "mary jane", "X, Y, Z"]
    return rng.choice(pool)


def rand_valid_email(rng):
    user = "".join(rng.choice(string.ascii_letters) for _ in range(rng.randint(1, 6)))
    dom = "".join(rng.choice(string.ascii_letters) for _ in range(rng.randint(1, 5)))
    tld = rng.choice(["com", "io", "net", "co.uk"])
    s = "%s@%s.%s" % (user, dom, tld)
    # randomly upper-case some letters; oracle lowercases.
    return "".join(c.upper() if rng.random() < 0.4 else c for c in s)


def rand_invalid_email(rng):
    return rng.choice(["plainstring", "a@@b.com", "@nodomain.com", "nolocal@",
                       "two@at@sign.com", "spaces in@x.com".replace(" ", "")])


def rand_valid_amount(rng):
    whole = rng.randint(0, 9999999)
    s = str(whole)
    if rng.random() < 0.5:
        dec = rng.randint(0, 99)
        s = "%s.%02d" % (whole, dec) if rng.random() < 0.5 else "%s.%d" % (whole, rng.randint(0, 9))
    # optional thousands separators
    if rng.random() < 0.4 and "." in s:
        intpart, frac = s.split(".")
        s = _group_commas(intpart) + "." + frac
    elif rng.random() < 0.4:
        s = _group_commas(s)
    if rng.random() < 0.3:
        s = "$" + s
    if rng.random() < 0.3:
        s = "-" + s
    return s


def _group_commas(digits):
    if len(digits) <= 3:
        return digits
    out = []
    while len(digits) > 3:
        out.insert(0, digits[-3:])
        digits = digits[:-3]
    out.insert(0, digits)
    return ",".join(out)


def rand_invalid_amount(rng):
    return rng.choice(["12.345", "1.2.3", "$", "", "abc", ".5", "--5",
                       "1,2,3.456", "12.", "$-12", "1 000", "0x10"])


def rand_valid_date(rng):
    y = rng.randint(2000, 2030)
    m = rng.randint(1, 12)
    d = rng.randint(1, 28)
    if rng.random() < 0.5:
        return "%04d-%02d-%02d" % (y, m, d)
    return "%02d/%02d/%04d" % (m, d, y)


def rand_invalid_date(rng):
    return rng.choice(["2025-13-01", "2025-02-30", "2025/01/01", "1/9/2025",
                       "2025-1-9", "01-09-2025", "2025.01.01", "Jan 5 2025",
                       "20250101", "2025-00-10", ""])


def rand_tags_field(rng):
    pool = ["red", "blue", "green", "vip", "eu", "us", "x", "y", "m", "n"]
    k = rng.randint(0, 5)
    chosen = [rng.choice(pool) for _ in range(k)]
    # inject dups, empties, whitespace
    if chosen and rng.random() < 0.5:
        chosen.append(chosen[0])
    if rng.random() < 0.3:
        chosen.append("")
    rng.shuffle(chosen)
    return ";".join(" %s " % c if rng.random() < 0.3 else c for c in chosen)


def valid_cells(rng):
    return {
        "id": rand_id(rng),
        "name": rand_name(rng),
        "email": rand_valid_email(rng) if rng.random() < 0.7 else "",
        "amount": rand_valid_amount(rng),
        "date": rand_valid_date(rng),
        "tags": rand_tags_field(rng),
    }


def render_csv(rng, rows):
    """rows: list of cell-dicts. Header order is shuffled per scenario."""
    header = oracle.COLUMNS[:]
    rng.shuffle(header)
    buf = io.StringIO()
    w = csv.writer(buf)
    w.writerow(header)
    for r in rows:
        w.writerow([r.get(h, "") for h in header])
    return buf.getvalue()


# --------------------------------------------------------- scenario builders
def normalizer_scenarios(rng):
    """List of (label, csv_text). Oracle defines expected."""
    scn = []
    # single-row rule probes: replace one field on an otherwise-valid row.
    for _ in range(8):
        c = valid_cells(rng)
        c["amount"] = rand_valid_amount(rng) if rng.random() < 0.5 else rand_invalid_amount(rng)
        scn.append(("amount", render_csv(rng, [c])))
    for _ in range(7):
        c = valid_cells(rng)
        c["date"] = rand_valid_date(rng) if rng.random() < 0.5 else rand_invalid_date(rng)
        scn.append(("date", render_csv(rng, [c])))
    for _ in range(6):
        c = valid_cells(rng)
        c["email"] = rng.choice([rand_valid_email(rng), "", rand_invalid_email(rng)])
        scn.append(("email", render_csv(rng, [c])))
    for _ in range(4):
        c = valid_cells(rng)
        c["tags"] = rand_tags_field(rng)
        scn.append(("tags", render_csv(rng, [c])))
    # id reject + blank-row skip (multi-row, ordering preserved)
    for _ in range(3):
        good1 = valid_cells(rng)
        blankish = {k: "" for k in oracle.COLUMNS}
        noid = valid_cells(rng)
        noid["id"] = rng.choice(["", "   "])
        good2 = valid_cells(rng)
        scn.append(("blank_id", render_csv(rng, [good1, blankish, noid, good2])))
    # multi-row fuzz: mix of valid/invalid, tests order + combined behavior
    for _ in range(6):
        rows = []
        for _ in range(rng.randint(3, 7)):
            c = valid_cells(rng)
            roll = rng.random()
            if roll < 0.2:
                c["amount"] = rand_invalid_amount(rng)
            elif roll < 0.4:
                c["date"] = rand_invalid_date(rng)
            elif roll < 0.5:
                c["email"] = rand_invalid_email(rng)
            rows.append(c)
        scn.append(("fuzz", render_csv(rng, rows)))
    return scn


def make_record(rng, id_pool=None, date_pool=None):
    rid = rng.choice(id_pool) if id_pool else rand_id(rng)
    d = rng.choice(date_pool) if date_pool else rand_valid_date_iso(rng)
    tags = sorted(set(rng.choice(["a", "b", "c", "d", "e", "x", "y"])
                      for _ in range(rng.randint(0, 4))))
    return {
        "id": rid,
        "name": rand_name(rng),
        "email": rand_valid_email(rng).lower() if rng.random() < 0.6 else None,
        "amount": rand_amount_value(rng),
        "date": d,
        "tags": tags,
    }


def rand_valid_date_iso(rng):
    return "%04d-%02d-%02d" % (rng.randint(2018, 2030), rng.randint(1, 12), rng.randint(1, 28))


def rand_amount_value(rng):
    roll = rng.random()
    if roll < 0.3:
        return rng.randint(-50, 5000)
    if roll < 0.6:
        return round(rng.uniform(-100, 9000), 2)
    return round(rng.uniform(0, 1000), 1)


def dedup_scenarios(rng):
    """List of (label, [list-of-record-lists-per-file]). Files passed in order."""
    scn = []
    dates = ["2024-12-31", "2025-01-01", "2025-02-01", "2025-02-01", "2025-06-15"]
    # plain sort, unique ids, single file
    for _ in range(3):
        recs = [make_record(rng) for _ in range(rng.randint(2, 6))]
        scn.append(("sort_unique", [recs]))
    # date-conflict across files (newest wins) + tag union
    for _ in range(5):
        pool = ["id%d" % i for i in range(rng.randint(2, 4))]
        f1 = [make_record(rng, pool, dates) for _ in range(rng.randint(2, 4))]
        f2 = [make_record(rng, pool, dates) for _ in range(rng.randint(2, 4))]
        scn.append(("conflict2", [f1, f2]))
    # explicit position tie-break: same id, same (newest) date, different files
    for _ in range(4):
        rid = rand_id(rng)
        nd = "2025-09-09"
        a = {"id": rid, "name": "early", "email": None, "amount": 1, "date": nd, "tags": ["a"]}
        b = {"id": rid, "name": "late", "email": "z@z.io", "amount": 2, "date": nd, "tags": ["b", "c"]}
        older = {"id": rid, "name": "old", "email": None, "amount": 9, "date": "2020-01-01", "tags": ["d"]}
        extra = [make_record(rng) for _ in range(rng.randint(0, 2))]
        scn.append(("tie_position", [[older, a] + extra, [b]]))  # b is latest position -> wins, tags union d,a,b,c
    # three-file multi-overlap
    for _ in range(4):
        pool = ["k%d" % i for i in range(rng.randint(2, 5))]
        files = [[make_record(rng, pool, dates) for _ in range(rng.randint(1, 4))]
                 for _ in range(3)]
        scn.append(("three", files))
    return scn


def report_scenarios(rng):
    scn = []
    scn.append(("empty", []))
    for _ in range(3):
        scn.append(("single", [make_record(rng)]))
    for _ in range(5):
        scn.append(("many", [make_record(rng) for _ in range(rng.randint(2, 9))]))
    # forced top_spenders ties (equal amounts -> id tie-break)
    for _ in range(3):
        amt = rng.choice([100, 250.5, 42])
        recs = [dict(make_record(rng), amount=amt) for _ in range(rng.randint(3, 6))]
        recs += [make_record(rng) for _ in range(rng.randint(0, 3))]
        scn.append(("ties", recs))
    # heavy tag overlap
    for _ in range(2):
        recs = []
        for _ in range(rng.randint(3, 7)):
            r = make_record(rng)
            r["tags"] = sorted(set(rng.choice(["p", "q", "r"]) for _ in range(rng.randint(1, 3))))
            recs.append(r)
        scn.append(("tags", recs))
    return scn


# --------------------------------------------------------------- run helpers
def parse_jsonl(text):
    return [json.loads(ln) for ln in text.splitlines() if ln.strip()]


def run_normalizer(workdir, csv_text):
    script = os.path.join(workdir, "normalizer", "normalize.py")
    with tempfile.TemporaryDirectory() as td:
        ip = os.path.join(td, "in.csv")
        op = os.path.join(td, "out.jsonl")
        with open(ip, "w", encoding="utf-8") as fh:
            fh.write(csv_text)
        try:
            p = subprocess.run([PYTHON, script, ip, op], cwd=workdir,
                               capture_output=True, text=True, timeout=RUN_TIMEOUT)
        except (subprocess.TimeoutExpired, OSError):
            return None
        if p.returncode != 0 or not os.path.exists(op):
            return None
        try:
            with open(op, encoding="utf-8") as fh:
                return parse_jsonl(fh.read())
        except (json.JSONDecodeError, OSError):
            return None


def dedup_bin(workdir):
    return os.path.join(workdir, "dedup", "target", "release", "dedup")


def run_dedup(workdir, files):
    """files: list of record-lists; written to temp files, passed in order."""
    binpath = dedup_bin(workdir)
    if not os.path.exists(binpath):
        return None
    with tempfile.TemporaryDirectory() as td:
        paths = []
        for i, recs in enumerate(files):
            p = os.path.join(td, "f%02d.jsonl" % i)
            with open(p, "w", encoding="utf-8") as fh:
                for r in recs:
                    fh.write(json.dumps(r) + "\n")
            paths.append(p)
        try:
            p = subprocess.run([binpath] + paths, capture_output=True, text=True,
                               timeout=RUN_TIMEOUT)
        except (subprocess.TimeoutExpired, OSError):
            return None
        if p.returncode != 0:
            return None
        try:
            return parse_jsonl(p.stdout)
        except json.JSONDecodeError:
            return None


def run_report(workdir, records):
    script = os.path.join(workdir, "report", "report.sh")
    with tempfile.TemporaryDirectory() as td:
        ip = os.path.join(td, "merged.jsonl")
        with open(ip, "w", encoding="utf-8") as fh:
            for r in records:
                fh.write(json.dumps(r) + "\n")
        try:
            p = subprocess.run(["bash", script, ip], cwd=workdir,
                               capture_output=True, text=True, timeout=RUN_TIMEOUT)
        except (subprocess.TimeoutExpired, OSError):
            return None
        if p.returncode != 0:
            return None
        try:
            return json.loads(p.stdout)
        except json.JSONDecodeError:
            return None


# ------------------------------------------------------------------- scoring
def score_normalizer(workdir, rng):
    scn = normalizer_scenarios(rng)
    passed = 0
    fails = []
    for label, csv_text in scn:
        want = oracle.normalize_csv_text(csv_text)
        got = run_normalizer(workdir, csv_text)
        if got is not None and oracle.json_equal(got, want):
            passed += 1
        elif len(fails) < 3:
            fails.append({"rule": label, "want": want, "got": got})
    return passed, len(scn), fails


def score_dedup(workdir, rng):
    scn = dedup_scenarios(rng)
    passed = 0
    fails = []
    have_bin = os.path.exists(dedup_bin(workdir))
    for label, files in scn:
        seq = [r for f in files for r in f]
        want = oracle.dedup_records(seq)
        got = run_dedup(workdir, files)
        if got is not None and oracle.json_equal(got, want):
            passed += 1
        elif len(fails) < 3:
            fails.append({"case": label, "want": want, "got": got})
    return passed, len(scn), fails, have_bin


def score_report(workdir, rng):
    scn = report_scenarios(rng)
    passed = 0
    fails = []
    for label, records in scn:
        want = oracle.report_records(records)
        got = run_report(workdir, records)
        if got is not None and oracle.report_equal(got, want):
            passed += 1
        elif len(fails) < 3:
            fails.append({"case": label, "want": want, "got": got})
    return passed, len(scn), fails


def score_integration(workdir, rng):
    """Run the real `make pipeline` (with a grader-pinned Makefile) on freshly
    generated CSVs; score the three pipeline stages 1/3 each vs the oracle."""
    detail = {"stages": {}}
    # Pin the Makefile so a tampered one can't fake the wiring.
    if os.path.exists(CANONICAL_MAKEFILE):
        with open(CANONICAL_MAKEFILE) as fh:
            mk = fh.read()
        with open(os.path.join(workdir, "Makefile"), "w") as fh:
            fh.write(mk)
    raw = os.path.join(workdir, ".grade_raw")
    out = os.path.join(workdir, ".grade_out")
    for d in (raw, out):
        if os.path.isdir(d):
            import shutil
            shutil.rmtree(d)
    os.makedirs(raw, exist_ok=True)
    # Several CSVs with sort-stable names; oracle must mirror sorted glob order.
    names = ["east", "north", "south", "west"][:rng.randint(2, 4)]
    csv_by_name = {}
    for nm in names:
        rows = [valid_cells(rng) for _ in range(rng.randint(2, 6))]
        # sprinkle some rejects + a duplicate id across files for the dedupe path
        if rng.random() < 0.6:
            rows[0]["amount"] = rand_invalid_amount(rng)
        csv_by_name[nm] = render_csv(rng, rows)
        with open(os.path.join(raw, nm + ".csv"), "w", encoding="utf-8") as fh:
            fh.write(csv_by_name[nm])

    try:
        p = subprocess.run(["make", "pipeline", "RAW=" + raw, "OUT=" + out],
                           cwd=workdir, capture_output=True, text=True, timeout=MAKE_TIMEOUT)
        detail["make_rc"] = p.returncode
        if p.returncode != 0:
            detail["make_stderr"] = p.stderr[-600:]
    except (subprocess.TimeoutExpired, OSError) as e:
        detail["make_error"] = str(e)[:200]

    # Oracle end-to-end in sorted filename order.
    sorted_names = sorted(names)
    oracle_norm = {nm: oracle.normalize_csv_text(csv_by_name[nm]) for nm in sorted_names}
    seq = [r for nm in sorted_names for r in oracle_norm[nm]]
    oracle_merged = oracle.dedup_records(seq)
    oracle_report = oracle.report_records(oracle_merged)

    # Stage 1: normalized/*.jsonl per file (fraction of files matching).
    norm_ok = 0
    for nm in sorted_names:
        path = os.path.join(out, "normalized", nm + ".jsonl")
        got = None
        if os.path.exists(path):
            try:
                with open(path, encoding="utf-8") as fh:
                    got = parse_jsonl(fh.read())
            except (OSError, json.JSONDecodeError):
                got = None
        if got is not None and oracle.json_equal(got, oracle_norm[nm]):
            norm_ok += 1
    stage_norm = norm_ok / len(sorted_names)
    detail["stages"]["normalized"] = {"matched_files": norm_ok, "files": len(sorted_names)}

    # Stage 2: merged.jsonl.
    merged_got = None
    mpath = os.path.join(out, "merged.jsonl")
    if os.path.exists(mpath):
        try:
            with open(mpath, encoding="utf-8") as fh:
                merged_got = parse_jsonl(fh.read())
        except (OSError, json.JSONDecodeError):
            merged_got = None
    stage_merged = 1.0 if (merged_got is not None and oracle.json_equal(merged_got, oracle_merged)) else 0.0
    detail["stages"]["merged"] = bool(stage_merged)

    # Stage 3: report.json.
    report_got = None
    rpath = os.path.join(out, "report.json")
    if os.path.exists(rpath):
        try:
            with open(rpath, encoding="utf-8") as fh:
                report_got = json.load(fh)
        except (OSError, json.JSONDecodeError):
            report_got = None
    stage_report = 1.0 if (report_got is not None and oracle.report_equal(report_got, oracle_report)) else 0.0
    detail["stages"]["report"] = bool(stage_report)

    integration = round((stage_norm + stage_merged + stage_report) / 3.0, 4)
    # cleanup generated dirs
    import shutil
    for d in (raw, out):
        shutil.rmtree(d, ignore_errors=True)
    return integration, detail


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("workdir")
    ap.add_argument("--seed", type=int, default=None)
    args = ap.parse_args()
    seed = args.seed if args.seed is not None else random.randrange(1, 2**31)

    workdir = os.path.abspath(args.workdir)
    # Independent RNG streams per component so the seed→inputs map is stable
    # even if one battery's size changes.
    rng_n = random.Random(seed ^ 0x11111111)
    rng_d = random.Random(seed ^ 0x22222222)
    rng_r = random.Random(seed ^ 0x33333333)
    rng_i = random.Random(seed ^ 0x44444444)

    n_pass, n_tot, n_fail = score_normalizer(workdir, rng_n)
    d_pass, d_tot, d_fail, have_bin = score_dedup(workdir, rng_d)
    r_pass, r_tot, r_fail = score_report(workdir, rng_r)
    integration, idetail = score_integration(workdir, rng_i)

    comp = {
        "normalizer": round(n_pass / n_tot, 4),
        "dedup": round(d_pass / d_tot, 4),
        "report": round(r_pass / r_tot, 4),
    }
    total = round(sum(comp.values()) + integration, 4)
    result = {
        "task": "polyglot-pipeline",
        "seed": seed,
        "component_scores": comp,
        "integration": integration,
        "total": total,
        "max_total": 4.0,
        "details": {
            "normalizer": {"passed": n_pass, "total": n_tot, "fails": n_fail},
            "dedup": {"passed": d_pass, "total": d_tot, "built": have_bin, "fails": d_fail},
            "report": {"passed": r_pass, "total": r_tot, "fails": r_fail},
            "integration": idetail,
        },
    }
    print(json.dumps(result, indent=2, default=str))


if __name__ == "__main__":
    main()
