#!/usr/bin/env python3
"""Stable loopback relay from tunnel-client to the active Intendant daemon.

Intendant may change its web-gateway port during a daemon handover. This relay
keeps a fixed loopback address for tunnel-client, resolves the active port for
every request, and injects the current per-boot loopback admission token without
printing or returning it.
"""

from __future__ import annotations

import argparse
import http.client
import ipaddress
import json
import os
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import cast
from urllib.parse import urlsplit


DEFAULT_LISTEN_HOST = "127.0.0.1"
DEFAULT_LISTEN_PORT = 18766
DEFAULT_UPSTREAM_HOST = "127.0.0.1"
DEFAULT_TIMEOUT_SECONDS = 300.0
DEFAULT_MAX_BODY_BYTES = 16 * 1024 * 1024

HOP_BY_HOP_HEADERS = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
}

# The relay is the local trust boundary. Never let a remote caller choose the
# credential Intendant sees, and do not forward ambient browser credentials.
PRIVATE_REQUEST_HEADERS = {
    "authorization",
    "cookie",
    "x-intendant-loopback-token",
}


@dataclass(frozen=True)
class RelayConfig:
    state_root: Path
    upstream_host: str = DEFAULT_UPSTREAM_HOST
    timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS
    max_body_bytes: int = DEFAULT_MAX_BODY_BYTES

    @property
    def active_daemon_meta_file(self) -> Path:
        return self.state_root / "cli-path.meta.json"

    @property
    def token_dir(self) -> Path:
        return self.state_root / "loopback-tokens"


def default_state_root() -> Path:
    configured = os.environ.get("INTENDANT_HOME")
    if configured:
        return Path(configured).expanduser()
    return Path.home() / ".intendant"


def active_descriptor(config: RelayConfig) -> tuple[int, tuple[object, ...]]:
    """Return the active gateway port and an identity that changes at handover."""
    raw = config.active_daemon_meta_file.read_text(encoding="utf-8")
    meta = json.loads(raw)
    if not isinstance(meta, dict):
        raise RuntimeError("invalid Intendant daemon descriptor")

    port = meta.get("port")
    if isinstance(port, bool) or not isinstance(port, int) or not 1 <= port <= 65535:
        raise RuntimeError("invalid Intendant daemon port")

    fingerprint = (port, meta.get("pid"), meta.get("wrote_at_ms"))
    return port, fingerprint


def active_target(config: RelayConfig) -> tuple[int, str, tuple[object, ...]]:
    port, fingerprint = active_descriptor(config)
    token = (config.token_dir / f"{port}.token").read_text(encoding="utf-8").strip()
    if not token:
        raise RuntimeError("empty Intendant loopback token")
    return port, token, fingerprint


def connect_to_active_daemon(
    config: RelayConfig,
) -> tuple[http.client.HTTPConnection, int, str]:
    """Connect before sending, retrying only across an observed handover."""
    attempted_fingerprint: tuple[object, ...] | None = None
    last_connect_error: OSError | http.client.HTTPException | None = None

    for _ in range(2):
        port, token, fingerprint = active_target(config)
        if fingerprint == attempted_fingerprint and last_connect_error is not None:
            raise last_connect_error

        upstream = http.client.HTTPConnection(
            config.upstream_host,
            port,
            timeout=config.timeout_seconds,
        )
        try:
            upstream.connect()
        except (OSError, http.client.HTTPException) as error:
            upstream.close()
            attempted_fingerprint = fingerprint
            last_connect_error = error
            continue

        _, current_fingerprint = active_descriptor(config)
        if current_fingerprint != fingerprint:
            upstream.close()
            attempted_fingerprint = fingerprint
            last_connect_error = None
            continue

        return upstream, port, token

    if last_connect_error is not None:
        raise last_connect_error
    raise RuntimeError("Intendant daemon changed during relay connection")


class RelayHTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True
    request_queue_size = 128

    def __init__(
        self,
        server_address: tuple[str, int],
        config: RelayConfig,
    ) -> None:
        self.relay_config = config
        super().__init__(server_address, RelayHandler)


class RelayHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, _format: str, *_args: object) -> None:
        # Deliberately silent: requests are authenticated locally by this relay.
        return

    @property
    def config(self) -> RelayConfig:
        return cast(RelayHTTPServer, self.server).relay_config

    def _request_body(self) -> bytes:
        if self.headers.get("Transfer-Encoding"):
            raise ValueError("transfer-encoded request bodies are not supported")

        raw_length = self.headers.get("Content-Length")
        if raw_length is None:
            return b""
        length = int(raw_length)
        if length < 0 or length > self.config.max_body_bytes:
            raise ValueError("invalid request body length")
        return self.rfile.read(length)

    def _send_error_without_details(self, status: int) -> None:
        body = b"relay unavailable\n"
        try:
            self.send_response(status)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True

    def _forward(self) -> None:
        try:
            if not ipaddress.ip_address(self.client_address[0]).is_loopback:
                self._send_error_without_details(403)
                return

            parsed = urlsplit(self.path)
            if parsed.path != "/mcp":
                self._send_error_without_details(404)
                return

            body = self._request_body()
            upstream, upstream_port, token = connect_to_active_daemon(self.config)
            try:
                headers: dict[str, str] = {}
                for name, value in self.headers.items():
                    lower_name = name.lower()
                    if lower_name in HOP_BY_HOP_HEADERS:
                        continue
                    if lower_name in PRIVATE_REQUEST_HEADERS:
                        continue
                    if lower_name in {"host", "content-length"}:
                        continue
                    headers[name] = value
                headers["Host"] = f"{self.config.upstream_host}:{upstream_port}"
                headers["Content-Length"] = str(len(body))
                headers["X-Intendant-Loopback-Token"] = token

                upstream.request(self.command, self.path, body=body, headers=headers)
                response = upstream.getresponse()
                is_event_stream = (
                    response.getheader("Content-Type", "")
                    .lower()
                    .startswith("text/event-stream")
                )

                self.send_response(response.status, response.reason)
                for name, value in response.getheaders():
                    lower_name = name.lower()
                    if lower_name in HOP_BY_HOP_HEADERS:
                        continue
                    if lower_name in {"content-length", "x-intendant-loopback-token"}:
                        continue
                    self.send_header(name, value)
                if is_event_stream:
                    self.send_header("Connection", "close")
                    self.end_headers()
                    if self.command != "HEAD":
                        while True:
                            chunk = response.read1(64 * 1024)
                            if not chunk:
                                break
                            self.wfile.write(chunk)
                            self.wfile.flush()
                else:
                    response_body = response.read()
                    self.send_header("Content-Length", str(len(response_body)))
                    self.send_header("Connection", "close")
                    self.end_headers()
                    if self.command != "HEAD":
                        self.wfile.write(response_body)
                self.close_connection = True
            finally:
                upstream.close()
        except (
            OSError,
            ValueError,
            RuntimeError,
            json.JSONDecodeError,
            http.client.HTTPException,
        ):
            self._send_error_without_details(502)

    do_GET = _forward
    do_POST = _forward
    do_DELETE = _forward
    do_HEAD = _forward


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--listen-host", default=DEFAULT_LISTEN_HOST)
    parser.add_argument("--listen-port", type=int, default=DEFAULT_LISTEN_PORT)
    parser.add_argument("--upstream-host", default=DEFAULT_UPSTREAM_HOST)
    parser.add_argument("--state-root", type=Path, default=default_state_root())
    parser.add_argument("--timeout-seconds", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if not ipaddress.ip_address(args.listen_host).is_loopback:
        raise SystemExit("--listen-host must be a loopback address")
    if not ipaddress.ip_address(args.upstream_host).is_loopback:
        raise SystemExit("--upstream-host must be a loopback address")
    if not 1 <= args.listen_port <= 65535:
        raise SystemExit("--listen-port must be between 1 and 65535")
    if args.timeout_seconds <= 0:
        raise SystemExit("--timeout-seconds must be positive")

    os.umask(0o077)
    config = RelayConfig(
        state_root=args.state_root,
        upstream_host=args.upstream_host,
        timeout_seconds=args.timeout_seconds,
    )
    server = RelayHTTPServer((args.listen_host, args.listen_port), config)
    try:
        server.serve_forever(poll_interval=0.25)
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
