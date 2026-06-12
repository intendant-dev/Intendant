#!/usr/bin/env bash
# Starter test for api/server.py (see api/SPEC.md). Starts the server on a free
# port, drives it over HTTP, checks the job lifecycle.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SERVER="$(dirname "$HERE")/server.py"

PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
python3 "$SERVER" --port "$PORT" --host 127.0.0.1 &
PID=$!
trap 'kill $PID 2>/dev/null' EXIT
BASE="http://127.0.0.1:$PORT"

python3 - "$BASE" <<'PY'
import json, sys, time, urllib.error, urllib.request

base = sys.argv[1]

def req(method, path, body=None, expect=None):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(base + path, data=data, method=method,
                               headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(r, timeout=5) as resp:
            code, payload = resp.status, resp.read()
    except urllib.error.HTTPError as e:
        code, payload = e.code, e.read()
    if expect is not None:
        assert code == expect, "%s %s -> %s (want %s): %s" % (method, path, code, expect, payload[:200])
    try:
        return code, json.loads(payload) if payload else None
    except json.JSONDecodeError:
        return code, None

# wait for startup
for _ in range(50):
    try:
        if req("GET", "/healthz")[0] == 200:
            break
    except Exception:
        pass
    time.sleep(0.1)
else:
    raise SystemExit("api never became healthy")

# create
_, job = req("POST", "/jobs", {"op": "sum", "input": [1, 2, 3]}, expect=201)
assert job["status"] == "queued" and job["result"] is None and job["op"] == "sum", job
jid = job["id"]
assert isinstance(jid, str) and jid, job

# get
_, got = req("GET", "/jobs/%s" % jid, expect=200)
assert got["input"] == [1, 2, 3], got

# claim once -> running; claim again -> 409
_, claimed = req("POST", "/jobs/%s/claim" % jid, expect=200)
assert claimed["status"] == "running", claimed
req("POST", "/jobs/%s/claim" % jid, expect=409)

# result
_, done = req("POST", "/jobs/%s/result" % jid, {"status": "done", "result": 6}, expect=200)
assert done["status"] == "done" and done["result"] == 6, done

# unknown id -> 404
req("GET", "/jobs/does-not-exist", expect=404)

# list filter
_, lst = req("GET", "/jobs?status=done", expect=200)
assert any(j["id"] == jid for j in lst["jobs"]), lst
print("api starter test: OK")
PY
