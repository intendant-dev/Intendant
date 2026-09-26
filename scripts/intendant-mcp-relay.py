#!/usr/bin/env python3
"""Stable loopback relay from tunnel-client to the active Intendant daemon.

Intendant may change its web-gateway port during a daemon handover. This relay
keeps a fixed loopback address for tunnel-client, resolves the active port for
every request, and injects the current per-boot loopback admission token without
printing or returning it.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import hmac
import re
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



# Terminal handles are routing identifiers, never credentials. They are also
# checked by the owning daemon, including its normal per-call IAM/scope gate.
TERMINAL_REFERENCE_PREFIX = "iterm1."
TERMINAL_TOOLS = {
    "terminal_open", "terminal_read", "terminal_write",
    "terminal_resize", "terminal_close",
}


class TerminalRouteError(RuntimeError):
    def __init__(self, code: str, request_id: object, delivery_unknown: bool = False):
        super().__init__(code)
        self.code = code
        self.request_id = request_id
        self.delivery_unknown = delivery_unknown


@dataclass(frozen=True)
class TerminalRoute:
    boot_id: str
    request_id: object


def terminal_route(body: bytes) -> TerminalRoute | None:
    """Inspect only a terminal tool's ID slot, never arbitrary input text."""
    try:
        request = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return None  # Legacy opaque requests retain their existing behavior.
    if not isinstance(request, dict) or request.get("method") != "tools/call":
        return None
    params = request.get("params")
    if not isinstance(params, dict):
        return None
    tool, args = params.get("name"), params.get("arguments")
    if not isinstance(tool, str) or not isinstance(args, dict):
        return None
    terminal_id = None
    if tool in TERMINAL_TOOLS:
        terminal_id = args.get("terminal_id")
    elif tool in {"inspect", "act", "authorize"}:
        argv = args.get("argv")
        if not isinstance(argv, list) or not all(isinstance(arg, str) for arg in argv):
            return None
        if len(argv) < 3 or argv[0] != "terminal" or "terminal_" + argv[1] not in TERMINAL_TOOLS:
            return None
        # The facade accepts flags before positional args. Skip only the
        # flags belonging to this operation, not a generic command grammar.
        valued = {"open": {"--cols", "--rows"}, "read": {"--cursor", "--max-bytes"}}
        booleans = {"open": {"--shared"}, "write": {"--no-enter"}}
        index = 2
        while index < len(argv):
            word = argv[index]
            if word == "--":
                index += 1
                break
            flag = word.split("=", 1)[0]
            if flag in valued.get(argv[1], set()):
                index += 1 if "=" in word else 2
            elif flag in booleans.get(argv[1], set()):
                index += 1
            elif word.startswith("--"):
                return None  # Let the daemon reject an unknown flag.
            else:
                break
        if index < len(argv):
            terminal_id = argv[index]
    if isinstance(terminal_id, str) and (tool == "terminal_open" or (tool in {"inspect", "act", "authorize"} and args.get("argv", [None, None])[1] == "open")):
        terminal_id = terminal_id.strip()  # Match the daemon's open normalization.
    if not isinstance(terminal_id, str) or not terminal_id.startswith(TERMINAL_REFERENCE_PREFIX):
        return None
    request_id = request.get("id")
    fields = terminal_id[len(TERMINAL_REFERENCE_PREFIX):].split(".")
    if (len(terminal_id) > 1607 or len(fields) != 3
            or re.fullmatch(r"(?:[a-z0-9]{26}|[a-z0-9]{32})", fields[0]) is None
            or re.fullmatch(r"[a-f0-9]{32}", fields[1]) is None
            or re.fullmatch(r"[A-Za-z0-9_-]+", fields[2]) is None):
        raise TerminalRouteError("terminal_invalid_handle", request_id)
    try:
        name = base64.b64decode(fields[2] + "=" * (-len(fields[2]) % 4), altchars=b"-_", validate=True)
        name.decode("utf-8")
        if not 1 <= len(name) <= 1024 or base64.urlsafe_b64encode(name).rstrip(b"=").decode() != fields[2]:
            raise ValueError("noncanonical terminal name")
    except (ValueError, UnicodeDecodeError, binascii.Error):
        raise TerminalRouteError("terminal_invalid_handle", request_id) from None
    return TerminalRoute(fields[0], request_id)


def connect_to_terminal_owner(config: RelayConfig, route: TerminalRoute) -> tuple[http.client.HTTPConnection, int, str]:
    """Use this boot's token, never a new occupant's token on a reused port."""
    connection = None
    try:
        record = json.loads((config.state_root / "daemons" / f"{route.boot_id}.json").read_text(encoding="utf-8"))
        if not isinstance(record, dict) or record.get("boot_id") != route.boot_id or record.get("state") not in {"running", "draining"}:
            raise ValueError("owner absent")
        port, expected = record.get("port"), record.get("terminal_token_sha256")
        if isinstance(port, bool) or not isinstance(port, int) or not 1 <= port <= 65535:
            raise ValueError("owner port invalid")
        if not isinstance(expected, str) or re.fullmatch(r"[a-f0-9]{64}", expected) is None:
            raise ValueError("owner does not support terminal continuity")
        token = (config.token_dir / f"{port}.token").read_text(encoding="utf-8").strip()
        if not token or not hmac.compare_digest(hashlib.sha256(token.encode()).hexdigest(), expected):
            raise ValueError("owner token changed")
        connection = http.client.HTTPConnection(config.upstream_host, port, timeout=config.timeout_seconds)
        connection.connect()
        # The connected request retains this exact token. If the port is
        # reused after this point the new process rejects it; never refresh
        # the token or retry after a terminal mutation may have been sent.
        return connection, port, token
    except (OSError, ValueError, RuntimeError, http.client.HTTPException):
        if connection is not None:
            connection.close()
        raise TerminalRouteError("terminal_owner_unavailable", route.request_id) from None



MCP_SESSION_PREFIX = "imcps1."


def session_route(header: str | None, body: bytes) -> tuple[TerminalRoute | None, str | None]:
    """Keep negotiated Tasks session affinity without a volatile relay map."""
    if header is None or not header.startswith(MCP_SESSION_PREFIX):
        return None, header
    try:
        request = json.loads(body)
        request_id = request.get("id") if isinstance(request, dict) else None
    except (ValueError, UnicodeDecodeError):
        request_id = None
    fields = header[len(MCP_SESSION_PREFIX):].split(".")
    try:
        if len(fields) != 2 or re.fullmatch(r"(?:[a-z0-9]{26}|[a-z0-9]{32})", fields[0]) is None or len(fields[1]) > 1400:
            raise ValueError("invalid session wrapper")
        raw = base64.b64decode(fields[1] + "=" * (-len(fields[1]) % 4), altchars=b"-_", validate=True)
        if not raw or any(byte < 33 or byte > 126 for byte in raw):
            raise ValueError("invalid session header")
        if base64.urlsafe_b64encode(raw).rstrip(b"=").decode() != fields[1]:
            raise ValueError("noncanonical session header")
    except (ValueError, binascii.Error):
        raise TerminalRouteError("mcp_session_invalid", request_id) from None
    return TerminalRoute(fields[0], request_id), raw.decode("ascii")


def boot_for_target(config: RelayConfig, port: int, token: str) -> str | None:
    """Discover only a locally recorded owner pinned by its token fingerprint."""
    expected = hashlib.sha256(token.encode()).hexdigest()
    matches = []
    for path in (config.state_root / "daemons").glob("*.json"):
        if re.fullmatch(r"(?:[a-z0-9]{26}|[a-z0-9]{32})", path.stem) is None:
            continue
        try:
            with path.open("rb") as source:
                record = json.loads(source.read(65537))
            if (isinstance(record, dict) and record.get("port") == port
                    and record.get("boot_id") == path.stem
                    and record.get("state") in {"running", "draining"}
                    and record.get("terminal_token_sha256") == expected):
                matches.append(path.stem)
        except (OSError, ValueError):
            continue
    return matches[0] if len(matches) == 1 else None


def wrapped_session_id(boot_id: str | None, raw: str) -> str:
    # Old servers without the pinned presence descriptor retain the legacy
    # header contract. They cannot acquire continuity retroactively.
    if boot_id is None:
        return raw
    return MCP_SESSION_PREFIX + boot_id + "." + base64.urlsafe_b64encode(raw.encode("ascii")).rstrip(b"=").decode()


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

    def _send_terminal_error(self, error: TerminalRouteError) -> None:
        detail = {
            "ok": False, "code": error.code, "retry_safe": False,
            "delivery": "unknown" if error.delivery_unknown else "not_sent",
            "error": "the original terminal could not be reached safely; no replacement was opened and no request was replayed",
            "hint": "preserve the handle and verify the prior command outcome before deliberately opening a replacement; use the protocol session that created this terminal, or a stateless connection",
        }
        payload = {"jsonrpc": "2.0", "id": error.request_id, "result": {
            "isError": True, "content": [{"type": "text", "text": json.dumps(detail)}],
        }}
        body = json.dumps(payload).encode()
        try:
            self.send_response(200 if error.request_id is not None else 202)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body) if error.request_id is not None else 0))
            self.send_header("Connection", "close")
            self.end_headers()
            if error.request_id is not None:
                self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True

    def _send_protocol_session_error(self, error: TerminalRouteError) -> None:
        # tasks/get, initialize and session DELETE are not tools/call: never
        # return a tool-result envelope where the protocol expects an error.
        # A proven missing session uses the normal HTTP 404 reinitialize path;
        # uncertain delivery remains 502 and must not invite mutation replay.
        status = 502 if error.delivery_unknown else (400 if error.code == "mcp_session_invalid" else 404)
        body = json.dumps({"jsonrpc": "2.0", "id": error.request_id, "error": {
            "code": -32001,
            "message": "MCP session owner unavailable" if error.code != "mcp_session_invalid" else "Invalid MCP session identifier",
            "data": {"code": error.code, "retry_safe": False,
                     "delivery": "unknown" if error.delivery_unknown else "not_sent"},
        }}).encode()
        try:
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True

    def _forward(self) -> None:
        protocol_only = False
        route = None
        sent = False
        response_started = False
        try:
            if not ipaddress.ip_address(self.client_address[0]).is_loopback:
                self._send_error_without_details(403)
                return

            parsed = urlsplit(self.path)
            if parsed.path != "/mcp":
                self._send_error_without_details(404)
                return

            body = self._request_body()
            route = terminal_route(body) if self.command == "POST" else None
            protocol_route, raw_session = session_route(self.headers.get("Mcp-Session-Id"), body)
            if route is not None and protocol_route is not None and route.boot_id != protocol_route.boot_id:
                raise TerminalRouteError("terminal_session_mismatch", route.request_id)
            protocol_only = route is None and protocol_route is not None
            route = route or protocol_route
            if route is not None:
                upstream, upstream_port, token = connect_to_terminal_owner(self.config, route)
                if self.headers.get("Mcp-Session-Id") and protocol_route is None:
                    # Protocol Tasks sessions are process-bound too. Never
                    # drop this header or send B's session authority to A.
                    try:
                        current_port, _ = active_descriptor(self.config)
                    except (OSError, ValueError, RuntimeError):
                        upstream.close()
                        raise TerminalRouteError("terminal_session_reinitialize", route.request_id) from None
                    if current_port != upstream_port:
                        upstream.close()
                        raise TerminalRouteError("terminal_session_reinitialize", route.request_id)
            else:
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
                    headers[name] = raw_session if lower_name == "mcp-session-id" and raw_session is not None else value
                headers["Host"] = f"{self.config.upstream_host}:{upstream_port}"
                headers["Content-Length"] = str(len(body))
                headers["X-Intendant-Loopback-Token"] = token

                sent = True
                upstream.request(self.command, self.path, body=body, headers=headers)
                response = upstream.getresponse()
                is_event_stream = (
                    response.getheader("Content-Type", "")
                    .lower()
                    .startswith("text/event-stream")
                )

                session_owner = None
                if response.getheader("Mcp-Session-Id") is not None:
                    session_owner = route.boot_id if route is not None else boot_for_target(self.config, upstream_port, token)
                response_started = True
                self.send_response(response.status, response.reason)
                for name, value in response.getheaders():
                    lower_name = name.lower()
                    if lower_name in HOP_BY_HOP_HEADERS:
                        continue
                    if lower_name in {"content-length", "x-intendant-loopback-token"}:
                        continue
                    self.send_header(name, wrapped_session_id(session_owner, value) if lower_name == "mcp-session-id" else value)
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
        except TerminalRouteError as error:
            if protocol_only or error.code == "mcp_session_invalid":
                self._send_protocol_session_error(error)
            else:
                self._send_terminal_error(error)
        except (
            OSError,
            ValueError,
            RuntimeError,
            json.JSONDecodeError,
            http.client.HTTPException,
        ):
            if response_started:
                # A partial response is an uncertain delivery, not a second
                # HTTP response and never a reason to replay a shell write.
                self.close_connection = True
            elif route is not None:
                error = TerminalRouteError("terminal_owner_unavailable", route.request_id, sent)
                if protocol_only:
                    self._send_protocol_session_error(error)
                else:
                    self._send_terminal_error(error)
            else:
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
