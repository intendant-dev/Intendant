#!/usr/bin/env python3
"""Starter test for cli/client.py (see cli/SPEC.md). Runs the CLI against a
tiny in-process stub API, so the CLI is tested independently of api/."""
import json
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

CLIENT = Path(__file__).resolve().parents[1] / "client.py"
JOBS = {}
_counter = [0]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        if self.path == "/jobs":
            n = int(self.headers.get("Content-Length", 0))
            body = json.loads(self.rfile.read(n) or b"{}")
            _counter[0] += 1
            jid = "job-%d" % _counter[0]
            job = {"id": jid, "op": body.get("op"), "input": body.get("input"),
                   "status": "queued", "result": None}
            JOBS[jid] = job
            self._send(201, job)
        else:
            self._send(404, {"error": "not found"})

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path.startswith("/jobs/"):
            jid = path[len("/jobs/"):]
            if jid in JOBS:
                self._send(200, JOBS[jid])
            else:
                self._send(404, {"error": "unknown"})
        else:
            self._send(404, {"error": "not found"})


def run_cli(*args):
    return subprocess.run([sys.executable, str(CLIENT), *args],
                          capture_output=True, text=True, timeout=30)


def main():
    srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    base = "http://127.0.0.1:%d" % srv.server_address[1]
    try:
        # submit -> prints the new id; the stub should now hold that job.
        p = run_cli("submit", base, "sum", "[1, 2, 3]")
        assert p.returncode == 0, "submit exit %s: %s" % (p.returncode, p.stderr.strip())
        jid = p.stdout.strip()
        assert jid in JOBS and JOBS[jid]["op"] == "sum" and JOBS[jid]["input"] == [1, 2, 3], \
            "submit did not create the job correctly: %r / %r" % (jid, JOBS.get(jid))

        # get -> prints the job JSON
        p = run_cli("get", base, jid)
        assert p.returncode == 0, "get exit %s: %s" % (p.returncode, p.stderr.strip())
        got = json.loads(p.stdout)
        assert got["id"] == jid and got["op"] == "sum", got

        # get unknown -> non-zero
        p = run_cli("get", base, "nope")
        assert p.returncode != 0, "get on unknown id should fail"

        # wait on an already-done job -> prints it, exit 0
        JOBS["seeded"] = {"id": "seeded", "op": "sum", "input": [1], "status": "done", "result": 1}
        p = run_cli("wait", base, "seeded", "--timeout", "5")
        assert p.returncode == 0, "wait exit %s: %s" % (p.returncode, p.stderr.strip())
        w = json.loads(p.stdout)
        assert w["status"] == "done" and w["result"] == 1, w
        print("cli starter test: OK")
    finally:
        srv.shutdown()


if __name__ == "__main__":
    main()
