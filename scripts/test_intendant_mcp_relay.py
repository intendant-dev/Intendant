#!/usr/bin/env python3
"""Hermetic tests for scripts/intendant-mcp-relay.py."""

from __future__ import annotations

import concurrent.futures
import http.client
import importlib.util
import json
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory
from types import ModuleType


SCRIPT = Path(__file__).with_name("intendant-mcp-relay.py")


def load_relay() -> ModuleType:
    spec = importlib.util.spec_from_file_location("intendant_mcp_relay", SCRIPT)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load {SCRIPT}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


relay = load_relay()


class Metrics:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.active = 0
        self.max_active = 0

    def enter(self) -> None:
        with self.lock:
            self.active += 1
            self.max_active = max(self.max_active, self.active)

    def leave(self) -> None:
        with self.lock:
            self.active -= 1


def upstream_handler(
    expected_token: str,
    label: str,
    metrics: Metrics,
    delay_seconds: float = 0.0,
) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def do_POST(self) -> None:
            metrics.enter()
            try:
                if delay_seconds:
                    time.sleep(delay_seconds)
                length = int(self.headers.get("Content-Length", "0"))
                request_body = self.rfile.read(length)
                if self.headers.get("X-Intendant-Loopback-Token") != expected_token:
                    response_body = b"unauthorized"
                    self.send_response(401)
                elif self.headers.get("Authorization") is not None:
                    response_body = b"authorization leaked"
                    self.send_response(400)
                else:
                    response_body = label.encode() + b":" + request_body
                    self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(response_body)))
                self.end_headers()
                self.wfile.write(response_body)
            finally:
                metrics.leave()

    return Handler


def start_server(server: ThreadingHTTPServer) -> threading.Thread:
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return thread


class RelayTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = TemporaryDirectory()
        self.state_root = Path(self.temp.name)
        (self.state_root / "loopback-tokens").mkdir()
        self.servers: list[ThreadingHTTPServer] = []

    def tearDown(self) -> None:
        for server in reversed(self.servers):
            server.shutdown()
            server.server_close()
        self.temp.cleanup()

    def add_upstream(
        self,
        token: str,
        label: str,
        metrics: Metrics,
        delay_seconds: float = 0.0,
    ) -> ThreadingHTTPServer:
        server = ThreadingHTTPServer(
            ("127.0.0.1", 0),
            upstream_handler(token, label, metrics, delay_seconds),
        )
        self.servers.append(server)
        start_server(server)
        return server

    def point_descriptor(self, server: ThreadingHTTPServer, token: str, pid: int) -> None:
        port = int(server.server_address[1])
        (self.state_root / "loopback-tokens" / f"{port}.token").write_text(
            token + "\n",
            encoding="utf-8",
        )
        descriptor = {"port": port, "pid": pid, "wrote_at_ms": pid * 10}
        (self.state_root / "cli-path.meta.json").write_text(
            json.dumps(descriptor),
            encoding="utf-8",
        )

    def add_relay(self) -> relay.RelayHTTPServer:
        config = relay.RelayConfig(state_root=self.state_root, timeout_seconds=3.0)
        server = relay.RelayHTTPServer(("127.0.0.1", 0), config)
        self.servers.append(server)
        start_server(server)
        return server

    @staticmethod
    def post(server: ThreadingHTTPServer, body: bytes) -> tuple[int, bytes]:
        connection = http.client.HTTPConnection(
            "127.0.0.1",
            int(server.server_address[1]),
            timeout=5,
        )
        try:
            connection.request(
                "POST",
                "/mcp?tool_profile=facade",
                body=body,
                headers={
                    "Content-Type": "application/json",
                    "Authorization": "Bearer must-not-reach-intendant",
                },
            )
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    def test_follows_descriptor_handover(self) -> None:
        first = self.add_upstream("first-private-token", "first", Metrics())
        second = self.add_upstream("second-private-token", "second", Metrics())
        relay_server = self.add_relay()

        self.point_descriptor(first, "first-private-token", 101)
        self.assertEqual(self.post(relay_server, b"one"), (200, b"first:one"))

        self.point_descriptor(second, "second-private-token", 202)
        self.assertEqual(self.post(relay_server, b"two"), (200, b"second:two"))

    def test_handles_parallel_requests_concurrently(self) -> None:
        metrics = Metrics()
        upstream = self.add_upstream(
            "concurrent-private-token",
            "parallel",
            metrics,
            delay_seconds=0.08,
        )
        self.point_descriptor(upstream, "concurrent-private-token", 303)
        relay_server = self.add_relay()

        bodies = [f"request-{index}".encode() for index in range(12)]
        with concurrent.futures.ThreadPoolExecutor(max_workers=len(bodies)) as pool:
            results = list(pool.map(lambda body: self.post(relay_server, body), bodies))

        self.assertTrue(all(status == 200 for status, _ in results))
        self.assertEqual(
            {body for _, body in results},
            {b"parallel:" + body for body in bodies},
        )
        self.assertGreater(metrics.max_active, 1)

    def test_returns_502_without_exposing_failure_details(self) -> None:
        relay_server = self.add_relay()
        status, body = self.post(relay_server, b"unavailable")
        self.assertEqual(status, 502)
        self.assertEqual(body, b"relay unavailable\n")


if __name__ == "__main__":
    unittest.main()
