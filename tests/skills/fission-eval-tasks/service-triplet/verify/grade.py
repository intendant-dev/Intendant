#!/usr/bin/env python3
"""Behavioral grader for service-triplet.

Four independent batteries — api (driven over raw HTTP), worker (pure compute
vs oracle), cli (against a conforming reference server), and a live-trio
integration (agent api + agent worker + agent cli, random ports, generated
payloads) — scored against the independent oracle. Emits the suite JSON
contract on stdout. Behavior only; never inspects file/string presence.

Usage: grade.py <workdir> [--seed N]
"""
import argparse
import json
import os
import random
import socket
import string
import subprocess
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import oracle          # noqa: E402
from ref_api import RefApi  # noqa: E402

PYTHON = sys.executable
NUMERIC_OPS = ["sum", "max", "sort_desc"]
STRING_OPS = ["reverse", "wordcount", "uppercase"]


# ------------------------------------------------------------- http client
def http(method, url, body=None, timeout=5):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read()
            code = resp.status
    except urllib.error.HTTPError as e:
        raw, code = e.read(), e.code
    except (urllib.error.URLError, OSError, ConnectionError):
        return None, None
    try:
        return code, (json.loads(raw) if raw else None)
    except json.JSONDecodeError:
        return code, None


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_healthy(base, tries=60, delay=0.1):
    for _ in range(tries):
        code, body = http("GET", base + "/healthz")
        if code == 200 and isinstance(body, dict) and body.get("ok") is True:
            return True
        time.sleep(delay)
    return False


def start_api(workdir, port):
    return subprocess.Popen(
        [PYTHON, os.path.join(workdir, "api", "server.py"), "--port", str(port),
         "--host", "127.0.0.1"],
        cwd=workdir, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def stop(proc):
    if proc and proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


# ------------------------------------------------------------- generators
def rand_numbers(rng, allow_empty=True):
    lo = 0 if allow_empty else 1
    n = rng.randint(lo, 6)
    out = []
    for _ in range(n):
        if rng.random() < 0.6:
            out.append(rng.randint(-50, 50))
        else:
            out.append(round(rng.uniform(-50, 50), 2))
    return out


def rand_string(rng):
    words = rng.randint(0, 5)
    toks = ["".join(rng.choice(string.ascii_letters) for _ in range(rng.randint(1, 6)))
            for _ in range(words)]
    sep = rng.choice([" ", "  ", " \t ", "   "])
    s = sep.join(toks)
    if rng.random() < 0.3:
        s = "  " + s + "  "
    return s


def gen_valid_job(rng):
    op = rng.choice(NUMERIC_OPS + STRING_OPS)
    if op in NUMERIC_OPS:
        value = rand_numbers(rng, allow_empty=(op != "max"))
    else:
        value = rand_string(rng)
    return op, value


def gen_invalid_job(rng):
    return rng.choice([
        ("sum", "not-a-list"),
        ("max", []),
        ("sort_desc", [1, "two", 3]),
        ("reverse", 5),
        ("wordcount", [1, 2]),
        ("uppercase", 99),
        ("frobnicate", 1),
        ("", "x"),
        ("sum", [1, True, 3]),  # booleans are not numbers
    ])


def gen_jobs(rng, n, invalid_ratio=0.3):
    jobs = []
    for _ in range(n):
        jobs.append(gen_invalid_job(rng) if rng.random() < invalid_ratio else gen_valid_job(rng))
    return jobs


# --------------------------------------------------------------- batteries
def battery_worker(workdir, rng):
    jobs = gen_jobs(rng, 22, invalid_ratio=0.35)
    worker = os.path.join(workdir, "worker", "worker.py")
    passed, fails = 0, []
    for op, value in jobs:
        try:
            p = subprocess.run([PYTHON, worker, "compute", op, json.dumps(value)],
                               cwd=workdir, capture_output=True, text=True, timeout=20)
            got = json.loads(p.stdout) if p.returncode == 0 else None
        except (subprocess.TimeoutExpired, OSError, json.JSONDecodeError):
            got = None
        want_status, want_result = oracle.compute(op, value)
        ok = False
        if isinstance(got, dict):
            if want_status == "error":
                ok = got.get("status") == "error"
            else:
                ok = got.get("status") == "done" and oracle.json_equal(got.get("result"), want_result)
        if ok:
            passed += 1
        elif len(fails) < 4:
            fails.append({"op": op, "input": value, "want": [want_status, want_result], "got": got})
    return passed, len(jobs), fails


def battery_api(workdir, rng):
    port = free_port()
    proc = start_api(workdir, port)
    base = "http://127.0.0.1:%d" % port
    checks = []

    def add(name, cond):
        checks.append((name, bool(cond)))

    try:
        healthy = wait_healthy(base)
        add("healthz", healthy)
        if not healthy:
            return 0, 1 + 14, [{"fatal": "api did not become healthy"}]

        # create + round-trip on generated jobs
        ok_create = True
        ids = []
        for op, value in [gen_valid_job(rng) for _ in range(4)]:
            code, job = http("POST", base + "/jobs", {"op": op, "input": value})
            good = (code == 201 and isinstance(job, dict) and job.get("status") == "queued"
                    and job.get("result") is None and job.get("op") == op
                    and oracle.json_equal(job.get("input"), value)
                    and isinstance(job.get("id"), str) and job.get("id"))
            ok_create = ok_create and good
            if good:
                ids.append((job["id"], op, value))
        add("create_queued", ok_create)

        ok_get = all(
            http("GET", "%s/jobs/%s" % (base, jid))[1] is not None
            and oracle.json_equal(http("GET", "%s/jobs/%s" % (base, jid))[1].get("input"), value)
            for jid, op, value in ids) and bool(ids)
        add("get_roundtrip", ok_get)

        add("post_missing_op", http("POST", base + "/jobs", {"input": [1]})[0] == 400)
        # raw non-JSON body -> 400
        add("post_bad_json", _post_raw(base + "/jobs", b"not json{") == 400)
        add("get_unknown_404", http("GET", base + "/jobs/nope-xyz")[0] == 404)

        # claim lifecycle on a fresh job
        code, job = http("POST", base + "/jobs", {"op": "sum", "input": [1, 2]})
        jid = job["id"] if isinstance(job, dict) else None
        c1, claimed = http("POST", "%s/jobs/%s/claim" % (base, jid))
        add("claim_running", c1 == 200 and isinstance(claimed, dict) and claimed.get("status") == "running")
        c2, _ = http("POST", "%s/jobs/%s/claim" % (base, jid))
        add("claim_conflict_409", c2 == 409)
        add("claim_unknown_404", http("POST", base + "/jobs/nope/claim")[0] == 404)

        # status filters reflect lifecycle
        _, q_run = http("GET", base + "/jobs?status=running")
        add("filter_running", isinstance(q_run, dict) and any(j.get("id") == jid for j in q_run.get("jobs", [])))
        _, q_queued = http("GET", base + "/jobs?status=queued")
        add("filter_queued_excludes_running",
            isinstance(q_queued, dict) and all(j.get("id") != jid for j in q_queued.get("jobs", [])))

        # result
        rc, done = http("POST", "%s/jobs/%s/result" % (base, jid), {"status": "done", "result": 3})
        add("result_done", rc == 200 and isinstance(done, dict) and done.get("status") == "done"
            and oracle.json_equal(done.get("result"), 3))
        add("result_unknown_404", http("POST", base + "/jobs/nope/result", {"status": "done", "result": 1})[0] == 404)
        add("result_bad_status_400", http("POST", "%s/jobs/%s/result" % (base, jid),
                                          {"status": "bogus", "result": 1})[0] == 400)

        # list all
        _, all_jobs = http("GET", base + "/jobs")
        add("list_all", isinstance(all_jobs, dict) and isinstance(all_jobs.get("jobs"), list)
            and len(all_jobs["jobs"]) >= 1)
    finally:
        stop(proc)

    passed = sum(1 for _n, c in checks if c)
    fails = [n for n, c in checks if not c]
    return passed, len(checks), [{"failed_checks": fails}] if fails else []


def _post_raw(url, raw):
    req = urllib.request.Request(url, data=raw, method="POST",
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            return resp.status
    except urllib.error.HTTPError as e:
        return e.code
    except (urllib.error.URLError, OSError):
        return None


def battery_cli(workdir, rng):
    ref = RefApi()
    base = ref.start()
    client = os.path.join(workdir, "cli", "client.py")
    checks = []

    def run(*args, timeout=20):
        try:
            return subprocess.run([PYTHON, client, *args], cwd=workdir,
                                  capture_output=True, text=True, timeout=timeout)
        except (subprocess.TimeoutExpired, OSError):
            return None

    def add(name, cond):
        checks.append((name, bool(cond)))

    try:
        # submit: several generated jobs; cli must POST and print the new id
        ok_submit = True
        for op, value in [gen_valid_job(rng) for _ in range(3)]:
            p = run("submit", base, op, json.dumps(value))
            if p is None or p.returncode != 0:
                ok_submit = False
                continue
            jid = p.stdout.strip().splitlines()[-1].strip() if p.stdout.strip() else ""
            job = ref.jobs.get(jid)
            if not (job and job["op"] == op and oracle.json_equal(job["input"], value)):
                ok_submit = False
        add("submit_creates_job", ok_submit)

        # get: seed a job, cli get prints it
        op, value = gen_valid_job(rng)
        jid = ref.seed(op, value)
        p = run("get", base, jid)
        ok_get = False
        if p is not None and p.returncode == 0:
            try:
                got = json.loads(p.stdout)
                ok_get = got.get("id") == jid and oracle.json_equal(got.get("input"), value)
            except json.JSONDecodeError:
                ok_get = False
        add("get_prints_job", ok_get)

        # get unknown -> non-zero exit. GATED on the positive get working, so a
        # do-nothing stub (which "fails" everything) gets no credit here.
        p = run("get", base, "nope-unknown")
        add("get_unknown_rejected", ok_get and p is not None and p.returncode != 0)

        # wait on a done job -> exit 0, prints it
        jid = ref.seed("sum", [1, 2], status="done", result=3)
        p = run("wait", base, jid, "--timeout", "5")
        ok_wait = False
        if p is not None and p.returncode == 0:
            try:
                w = json.loads(p.stdout)
                ok_wait = w.get("status") == "done" and oracle.json_equal(w.get("result"), 3)
            except json.JSONDecodeError:
                ok_wait = False
        add("wait_done_exit0", ok_wait)

        # wait on an error job -> non-zero exit. GATED on wait_done working.
        jid = ref.seed("max", [], status="error", result=None)
        p = run("wait", base, jid, "--timeout", "5")
        add("wait_error_rejected", ok_wait and p is not None and p.returncode != 0)
    finally:
        ref.stop()

    passed = sum(1 for _n, c in checks if c)
    fails = [n for n, c in checks if not c]
    return passed, len(checks), [{"failed_checks": fails}] if fails else []


def integration(workdir, rng):
    detail = {}
    port = free_port()
    api = start_api(workdir, port)
    base = "http://127.0.0.1:%d" % port
    client = os.path.join(workdir, "cli", "client.py")
    worker = None
    jobs = [gen_valid_job(rng) for _ in range(5)] + [gen_invalid_job(rng) for _ in range(2)]
    rng.shuffle(jobs)
    correct = 0
    try:
        if not wait_healthy(base):
            detail["error"] = "api not healthy"
            return 0.0, detail
        worker = subprocess.Popen(
            [PYTHON, os.path.join(workdir, "worker", "worker.py"), "serve", base],
            cwd=workdir, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

        # Submit every job FIRST (the worker processes them concurrently), then
        # wait on each under a single shared deadline so a dead/broken worker
        # bounds total integration time instead of paying a timeout per job.
        submitted = []  # (op, value, jid|None)
        for op, value in jobs:
            jid = None
            try:
                sp = subprocess.run([PYTHON, client, "submit", base, op, json.dumps(value)],
                                    cwd=workdir, capture_output=True, text=True, timeout=15)
                if sp.returncode == 0 and sp.stdout.strip():
                    jid = sp.stdout.strip().splitlines()[-1].strip()
            except (subprocess.TimeoutExpired, OSError):
                jid = None
            submitted.append((op, value, jid))

        results = []
        deadline = time.time() + 15.0  # shared budget for the whole wait phase
        for op, value, jid in submitted:
            want_status, want_result = oracle.compute(op, value)
            final = None
            if jid is not None:
                per = max(1.0, deadline - time.time())
                try:
                    wp = subprocess.run([PYTHON, client, "wait", base, jid,
                                         "--timeout", "%.1f" % per],
                                        cwd=workdir, capture_output=True, text=True,
                                        timeout=per + 10)
                    if wp.stdout.strip():
                        final = json.loads(wp.stdout)
                except (subprocess.TimeoutExpired, OSError, json.JSONDecodeError):
                    final = None
                if final is None:  # fall back to a direct GET if the cli printed nothing
                    _, final = http("GET", "%s/jobs/%s" % (base, jid))
            ok = isinstance(final, dict) and final.get("status") == want_status
            if ok and want_status == "done":
                ok = oracle.json_equal(final.get("result"), want_result)
            results.append(bool(ok))
        correct = sum(1 for r in results if r)
        detail["jobs"] = len(jobs)
        detail["correct"] = correct
    finally:
        stop(worker)
        stop(api)
    return round(correct / len(jobs), 4), detail


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("workdir")
    ap.add_argument("--seed", type=int, default=None)
    args = ap.parse_args()
    seed = args.seed if args.seed is not None else random.randrange(1, 2**31)
    workdir = os.path.abspath(args.workdir)

    rng_w = random.Random(seed ^ 0xA1A1A1)
    rng_a = random.Random(seed ^ 0xB2B2B2)
    rng_c = random.Random(seed ^ 0xC3C3C3)
    rng_i = random.Random(seed ^ 0xD4D4D4)

    w_pass, w_tot, w_fail = battery_worker(workdir, rng_w)
    a_pass, a_tot, a_fail = battery_api(workdir, rng_a)
    c_pass, c_tot, c_fail = battery_cli(workdir, rng_c)
    integ, idetail = integration(workdir, rng_i)

    comp = {
        "api": round(a_pass / a_tot, 4),
        "worker": round(w_pass / w_tot, 4),
        "cli": round(c_pass / c_tot, 4),
    }
    total = round(sum(comp.values()) + integ, 4)
    print(json.dumps({
        "task": "service-triplet",
        "seed": seed,
        "component_scores": comp,
        "integration": integ,
        "total": total,
        "max_total": 4.0,
        "details": {
            "api": {"passed": a_pass, "total": a_tot, "fails": a_fail},
            "worker": {"passed": w_pass, "total": w_tot, "fails": w_fail},
            "cli": {"passed": c_pass, "total": c_tot, "fails": c_fail},
            "integration": idetail,
        },
    }, indent=2, default=str))


if __name__ == "__main__":
    main()
