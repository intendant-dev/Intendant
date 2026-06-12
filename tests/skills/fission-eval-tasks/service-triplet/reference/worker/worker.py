#!/usr/bin/env python3
"""Reference worker (agent-facing solution). See worker/SPEC.md.
Excluded from agent visibility by the SKILL runner."""
import argparse
import json
import sys
import time
import urllib.error
import urllib.request


def _is_number(x):
    return isinstance(x, (int, float)) and not isinstance(x, bool)


def _numbers(x):
    return isinstance(x, list) and all(_is_number(e) for e in x)


def compute(op, value):
    if op == "sum" and _numbers(value):
        return "done", sum(value)
    if op == "max" and _numbers(value) and value:
        return "done", max(value)
    if op == "sort_desc" and _numbers(value):
        return "done", sorted(value, reverse=True)
    if op == "reverse" and isinstance(value, str):
        return "done", value[::-1]
    if op == "wordcount" and isinstance(value, str):
        return "done", len(value.split())
    if op == "uppercase" and isinstance(value, str):
        return "done", value.upper()
    return "error", None


def _http(method, url, body=None, timeout=5):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read() or b"null")
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read() or b"null")
        except json.JSONDecodeError:
            return e.code, None
    except (urllib.error.URLError, OSError, json.JSONDecodeError):
        return None, None


def process_one(base):
    code, listing = _http("GET", base + "/jobs?status=queued")
    if code != 200 or not isinstance(listing, dict):
        return False
    for job in listing.get("jobs", []):
        jid = job.get("id")
        c, claimed = _http("POST", "%s/jobs/%s/claim" % (base, jid))
        if c != 200:
            continue  # someone else claimed it (409) or it vanished
        status, result = compute(claimed.get("op"), claimed.get("input"))
        _http("POST", "%s/jobs/%s/result" % (base, jid), {"status": status, "result": result})
        return True
    return False


def serve(base, once, poll):
    if once:
        process_one(base)  # at most one job, then exit
        return
    while True:
        if not process_one(base):
            time.sleep(poll)


def main(argv):
    if len(argv) >= 2 and argv[1] == "compute":
        op = argv[2] if len(argv) > 2 else ""
        value = json.loads(argv[3]) if len(argv) > 3 else None
        status, result = compute(op, value)
        print(json.dumps({"status": status, "result": result}))
        return 0
    if len(argv) >= 2 and argv[1] == "serve":
        ap = argparse.ArgumentParser()
        ap.add_argument("url")
        ap.add_argument("--once", action="store_true")
        ap.add_argument("--poll", type=float, default=0.2)
        args = ap.parse_args(argv[2:])
        serve(args.url.rstrip("/"), args.once, args.poll)
        return 0
    print("usage: worker.py compute OP INPUT_JSON | serve API_URL [--once] [--poll S]", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
