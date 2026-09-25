#!/usr/bin/env python3
"""Hermetic terminal owner routing regression tests; no real daemon or tokens."""
from __future__ import annotations

import base64
import hashlib
import http.client
import importlib.util
import json
import socket
import sys
import tempfile
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("terminal_handover_relay", Path(__file__).with_name("intendant-mcp-relay.py"))
assert SPEC and SPEC.loader
relay = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = relay
SPEC.loader.exec_module(relay)


def handle(boot: str, name: str = "same-name", generation: str = "1" * 32) -> str:
    return "iterm1." + boot + "." + generation + "." + base64.urlsafe_b64encode(name.encode()).rstrip(b"=").decode()


def call(tool: str, args: dict) -> dict:
    return {"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": tool, "arguments": args}}


class TerminalHandoverTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="intendant-relay-test-")
        self.root = Path(self.temp.name)
        (self.root / "loopback-tokens").mkdir()
        (self.root / "daemons").mkdir()
        self.servers = []
        self.a = self.owner("a" * 26, "fixture-token-a")
        self.b = self.owner("b" * 26, "fixture-token-b")
        self.point(self.a)
        self.front = relay.RelayHTTPServer(("127.0.0.1", 0), relay.RelayConfig(self.root, timeout_seconds=3))
        self.start(self.front)

    def tearDown(self):
        for server, thread in reversed(self.servers):
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
        self.temp.cleanup()

    def start(self, server):
        thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True)
        thread.start()
        self.servers.append((server, thread))

    def owner(self, boot, token):
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                request = json.loads(body)
                server = self.server
                if self.headers.get("X-Intendant-Loopback-Token") != server.fixture_token:
                    self.send_response(403)
                    self.end_headers()
                    return
                server.accepted.append((request, dict(self.headers)))
                args = request.get("params", {}).get("arguments", {})
                if args.get("input") == "disconnect-after-send":
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.connection.close()
                    return
                if args.get("input") == "hold-in-flight":
                    server.entered.set()
                    if not server.release.wait(timeout=4):
                        raise RuntimeError("test release timed out")
                response = json.dumps({"owner": server.boot, "args": args}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                if request.get("method") == "initialize":
                    self.send_header("Mcp-Session-Id", "fixture-session")
                self.send_header("Content-Length", str(len(response)))
                self.end_headers()
                self.wfile.write(response)

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        server.boot, server.fixture_token, server.accepted = boot, token, []
        server.entered, server.release = threading.Event(), threading.Event()
        self.start(server)
        port = server.server_address[1]
        (self.root / "loopback-tokens" / f"{port}.token").write_text(token)
        self.record(server)
        return server

    def record(self, server, **overrides):
        record = {"boot_id": server.boot, "port": server.server_address[1], "pid": 123, "state": "running", "terminal_token_sha256": hashlib.sha256(server.fixture_token.encode()).hexdigest()}
        record.update(overrides)
        (self.root / "daemons" / f"{server.boot}.json").write_text(json.dumps(record))

    def point(self, server):
        (self.root / "cli-path.meta.json").write_text(json.dumps({"port": server.server_address[1], "pid": 123, "wrote_at_ms": server.server_address[1]}))

    def exchange(self, request, headers=None):
        client = http.client.HTTPConnection("127.0.0.1", self.front.server_address[1], timeout=5)
        try:
            client.request("POST", "/mcp", body=json.dumps(request).encode(), headers={"Content-Type": "application/json", **(headers or {})})
            response = client.getresponse()
            body = response.read()
            return response.status, dict(response.getheaders()), json.loads(body) if body else {}
        finally:
            client.close()

    def code(self, response):
        self.assertTrue(response[2]["result"]["isError"])
        return json.loads(response[2]["result"]["content"][0]["text"])

    def test_old_shell_calls_follow_owner_across_chained_updates(self):
        self.point(self.b)
        for operation in ["read", "write", "resize", "close", "open"]:
            response = self.exchange(call("terminal_" + operation, {"terminal_id": handle(self.a.boot)}))
            self.assertEqual(response[2]["owner"], self.a.boot)
        c = self.owner("c" * 26, "fixture-token-c")
        self.point(c)
        self.assertEqual(self.exchange(call("terminal_read", {"terminal_id": handle(self.a.boot)}))[2]["owner"], self.a.boot)
        self.assertEqual(self.exchange(call("terminal_read", {"terminal_id": handle(self.b.boot)}))[2]["owner"], self.b.boot)
        self.assertEqual(self.exchange(call("terminal_open", {"terminal_id": "new"}))[2]["owner"], c.boot)
        self.assertEqual(self.exchange(call("get_status", {}))[2]["owner"], c.boot)

    def test_facade_routing_is_only_from_the_terminal_id_slot(self):
        self.point(self.b)
        ref = handle(self.a.boot)
        for tool, argv in [
            ("inspect", ["terminal", "read", ref]),
            ("inspect", ["terminal", "read", "--cursor", "9", ref]),
            ("authorize", ["terminal", "write", ref, "echo hi"]),
            ("authorize", ["terminal", "open", "--cols=80", ref]),
            ("act", ["terminal", "resize", ref, "80", "24"]),
            ("act", ["terminal", "close", ref]),
        ]:
            with self.subTest(argv=argv):
                self.assertEqual(self.exchange(call(tool, {"argv": argv}))[2]["owner"], self.a.boot)
        for request in [call("notify", {"text": ref}), call("terminal_write", {"terminal_id": "new", "input": ref}), call("authorize", {"argv": ["terminal", "write", "new", ref]})]:
            self.assertEqual(self.exchange(request)[2]["owner"], self.b.boot)

    def test_missing_or_exited_owner_never_falls_back(self):
        self.point(self.b)
        self.record(self.a, state="exited")
        response = self.exchange(call("terminal_open", {"terminal_id": handle(self.a.boot)}))
        self.assertEqual(self.code(response)["code"], "terminal_owner_unavailable")
        self.assertEqual(self.code(response)["delivery"], "not_sent")
        self.assertEqual(self.b.accepted, [])

    def test_reused_port_with_new_token_is_refused_before_delivery(self):
        self.point(self.b)
        self.record(self.a, port=self.b.server_address[1])
        response = self.exchange(call("terminal_write", {"terminal_id": handle(self.a.boot), "input": "danger"}))
        self.assertEqual(self.code(response)["delivery"], "not_sent")
        self.assertEqual(self.b.accepted, [])

    def test_port_reuse_after_token_read_still_fails_call_time_auth(self):
        self.point(self.b)
        self.record(self.a, port=self.b.server_address[1])
        (self.root / "loopback-tokens" / f"{self.b.server_address[1]}.token").write_text(self.a.fixture_token)
        response = self.exchange(call("terminal_write", {"terminal_id": handle(self.a.boot), "input": "danger"}))
        self.assertEqual(response[0], 403)
        self.assertEqual(self.b.accepted, [])

    def test_uncertain_mutation_is_never_replayed(self):
        self.point(self.b)
        response = self.exchange(call("terminal_write", {"terminal_id": handle(self.a.boot), "input": "disconnect-after-send"}))
        self.assertEqual(self.code(response)["delivery"], "unknown")
        self.assertFalse(self.code(response)["retry_safe"])
        self.assertEqual(len(self.a.accepted), 1)
        self.assertEqual(self.b.accepted, [])

    def test_in_flight_write_stays_on_old_owner_while_new_work_uses_successor(self):
        with ThreadPoolExecutor(max_workers=2) as pool:
            pending = pool.submit(self.exchange, call("terminal_write", {"terminal_id": handle(self.a.boot), "input": "hold-in-flight"}))
            self.assertTrue(self.a.entered.wait(timeout=3))
            self.point(self.b)
            self.assertEqual(self.exchange(call("terminal_open", {}))[2]["owner"], self.b.boot)
            self.a.release.set()
            self.assertEqual(pending.result(timeout=5)[2]["owner"], self.a.boot)

    def test_malformed_and_path_traversal_handles_are_rejected(self):
        for ref in ["iterm1.", "iterm1...", "iterm1.../../secret", "iterm1." + "a" * 26 + "." + "f" * 32 + ".Zg=="]:
            with self.subTest(ref=ref):
                response = self.exchange(call("terminal_open", {"terminal_id": ref}))
                self.assertEqual(self.code(response)["code"], "terminal_invalid_handle")
        self.assertEqual(self.a.accepted, [])

    def test_negotiated_session_affinity_survives_handover_and_relay_recreation(self):
        init = self.exchange({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
        session = init[1]["Mcp-Session-Id"]
        self.assertTrue(session.startswith("imcps1." + self.a.boot))
        self.point(self.b)
        # A fresh relay object has no affinity table to restore.
        self.front = relay.RelayHTTPServer(("127.0.0.1", 0), relay.RelayConfig(self.root, timeout_seconds=3))
        self.start(self.front)
        response = self.exchange(call("terminal_read", {"terminal_id": handle(self.a.boot)}), {"Mcp-Session-Id": session})
        self.assertEqual(response[2]["owner"], self.a.boot)
        self.assertEqual(self.a.accepted[-1][1]["Mcp-Session-Id"], "fixture-session")
        mismatch = self.exchange(call("terminal_write", {"terminal_id": handle(self.b.boot), "input": "bad"}), {"Mcp-Session-Id": session})
        self.assertEqual(self.code(mismatch)["code"], "terminal_session_mismatch")
        self.assertEqual(self.b.accepted, [])

    def test_legacy_unpinned_protocol_session_is_not_silently_stripped(self):
        self.point(self.b)
        response = self.exchange(call("terminal_read", {"terminal_id": handle(self.a.boot)}), {"Mcp-Session-Id": "legacy-session"})
        self.assertEqual(self.code(response)["code"], "terminal_session_reinitialize")
        self.assertEqual(self.a.accepted, [])


if __name__ == "__main__":
    unittest.main()
