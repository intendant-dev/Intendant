#!/usr/bin/env python3
"""A conforming reference API server (per api/SPEC.md), used as the fixed
backend for the CLI battery so the CLI is graded independently of the agent's
api/. Runs in-process on an ephemeral port. NOT shown to the agent.
"""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs


class RefApi:
    def __init__(self):
        self.jobs = {}
        self._n = 0
        self._lock = threading.Lock()
        self._srv = None
        self.base_url = None

    # direct (in-process) helpers the grader uses to seed/inspect state
    def seed(self, op, value, status="queued", result=None):
        with self._lock:
            self._n += 1
            jid = "ref-%d" % self._n
            self.jobs[jid] = {"id": jid, "op": op, "input": value,
                              "status": status, "result": result}
            return jid

    def start(self):
        ref = self

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

            def _read_json(self):
                n = int(self.headers.get("Content-Length", 0) or 0)
                raw = self.rfile.read(n) if n else b""
                return json.loads(raw) if raw else None

            def do_GET(self):
                parsed = urlparse(self.path)
                path = parsed.path
                if path == "/healthz":
                    return self._send(200, {"ok": True})
                if path == "/jobs":
                    q = parse_qs(parsed.query)
                    status = q.get("status", [None])[0]
                    with ref._lock:
                        jobs = [j for j in ref.jobs.values()
                                if status is None or j["status"] == status]
                    return self._send(200, {"jobs": jobs})
                if path.startswith("/jobs/"):
                    jid = path[len("/jobs/"):]
                    with ref._lock:
                        job = ref.jobs.get(jid)
                    return self._send(200, job) if job else self._send(404, {"error": "unknown"})
                return self._send(404, {"error": "not found"})

            def do_POST(self):
                parsed = urlparse(self.path)
                path = parsed.path
                if path == "/jobs":
                    try:
                        body = self._read_json()
                    except json.JSONDecodeError:
                        return self._send(400, {"error": "bad json"})
                    if not isinstance(body, dict) or not isinstance(body.get("op"), str) \
                            or "input" not in body:
                        return self._send(400, {"error": "bad body"})
                    with ref._lock:
                        ref._n += 1
                        jid = "ref-%d" % ref._n
                        job = {"id": jid, "op": body["op"], "input": body["input"],
                               "status": "queued", "result": None}
                        ref.jobs[jid] = job
                    return self._send(201, job)
                if path.endswith("/claim") and path.startswith("/jobs/"):
                    jid = path[len("/jobs/"):-len("/claim")]
                    with ref._lock:
                        job = ref.jobs.get(jid)
                        if job is None:
                            return self._send(404, {"error": "unknown"})
                        if job["status"] != "queued":
                            return self._send(409, {"error": "not queued"})
                        job["status"] = "running"
                        return self._send(200, job)
                if path.endswith("/result") and path.startswith("/jobs/"):
                    jid = path[len("/jobs/"):-len("/result")]
                    try:
                        body = self._read_json()
                    except json.JSONDecodeError:
                        return self._send(400, {"error": "bad json"})
                    if not isinstance(body, dict) or body.get("status") not in ("done", "error"):
                        return self._send(400, {"error": "bad body"})
                    with ref._lock:
                        job = ref.jobs.get(jid)
                        if job is None:
                            return self._send(404, {"error": "unknown"})
                        job["status"] = body["status"]
                        job["result"] = body.get("result")
                        return self._send(200, job)
                return self._send(404, {"error": "not found"})

        self._srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.base_url = "http://127.0.0.1:%d" % self._srv.server_address[1]
        threading.Thread(target=self._srv.serve_forever, daemon=True).start()
        return self.base_url

    def stop(self):
        if self._srv:
            self._srv.shutdown()
