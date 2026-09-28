#!/usr/bin/env python3
"""Supervised test-backend probe for managed-browser delegation.

This is a protocol fixture, not an LLM or a successful native acceptance report.
The disposable Intendant test daemon must launch it as an external backend in a
private project containing .intendant-delegation-proof.json. Requests use ONLY the
session credentials injected into that child; no owner-token fallback is allowed.
It never launches a browser, reads a personal clipboard, or posts OS input.

--self-test runs hermetic parser/identity/redaction tests without any network/GUI.
The coordinator still has to check actual page effects and revoke/expiry behavior
independently. Protocol acknowledgements alone are not application effects.
"""
from __future__ import annotations

import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import stat
import sys
import time
import urllib.parse
import uuid

SCHEMA = "intendant-browser-delegation-fixture-v1"
TOOLS = frozenset({
    "whoami", "execute_browser_workspace_keyboard",
    "grant_browser_workspace_session", "revoke_browser_workspace_session",
    "list_assigned_browser_workspaces", "observe_browser_workspace",
})
PRIVATE_KEYS = frozenset({
    "profile_dir", "process_id", "debugging_port", "cdp_http_url", "cdp_ws_url",
    "macos_window_binding", "browser_executable", "launch_arguments",
    "access_token", "bearer_token", "mcp_token", "authorization",
})
MAX_REPLY = 12 * 1024 * 1024


def require(value, message):
    if not value:
        raise ValueError(message)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate JSON field")
        result[key] = value
    return result


def loads(text):
    return json.loads(text, object_pairs_hook=unique_object)


def bootstrap(env, manifest, now_ms):
    require(manifest.get("schema") == SCHEMA, "missing disposable proof manifest")
    expires = manifest.get("expires_unix_ms")
    require(type(expires) is int and now_ms < expires <= now_ms + 300_000,
            "proof manifest expired or exceeds five-minute bound")
    url = env.get("INTENDANT_MCP_URL", "")
    token = env.get("INTENDANT_MCP_BEARER_TOKEN", "")
    session = env.get("INTENDANT_SESSION_ID", "")
    require(isinstance(url, str) and len(url) <= 16_384, "invalid injected endpoint")
    require(isinstance(token, str) and 0 < len(token) <= 4096 and
            not any(c in token for c in "\r\n"), "missing session bearer")
    require(isinstance(session, str) and 0 < len(session) <= 1024,
            "missing injected session identity")
    parsed = urllib.parse.urlsplit(url)
    require(parsed.scheme == "http" and parsed.hostname in ("127.0.0.1", "localhost")
            and parsed.path == "/mcp" and not parsed.fragment
            and parsed.username is None and parsed.password is None,
            "probe accepts only the injected local test endpoint")
    # External agents receive localhost, native sessions receive 127.0.0.1.
    # Connection below ALWAYS uses the numeric address, never DNS or proxies.
    require(parsed.port is not None and 1 <= parsed.port <= 65535, "missing explicit port")
    require(manifest.get("origin") == f"http://127.0.0.1:{parsed.port}",
            "injected endpoint differs from disposable test daemon")
    pairs = urllib.parse.parse_qsl(parsed.query, keep_blank_values=True, strict_parsing=True)
    query = unique_object(pairs)
    identities = [query[k] for k in ("session_id", "session", "intendant_session") if k in query]
    require(len(identities) == 1 and identities[0] == session,
            "query identity differs from injected session")
    # Old ctl bootstrap URLs may carry the same bearer in mcp_token. Remove it
    # before making the request, require agreement, and never expose it in argv.
    if "mcp_token" in query:
        require(query.pop("mcp_token") == token, "conflicting injected credentials")
    query = {k: v for k, v in query.items() if k in {
        "session_id", "session", "intendant_session", "tool_profile", "managed_context"
    }}
    return parsed.port, "/mcp?" + urllib.parse.urlencode(query), token, session


def parse_probe(message):
    require(isinstance(message, dict) and set(message) == {"probe"}, "expected explicit probe request")
    probe = message["probe"]
    require(isinstance(probe, dict) and set(probe) == {"id", "tool", "arguments"},
            "invalid probe fields")
    require(str(uuid.UUID(probe["id"])) == probe["id"], "probe ID must be a canonical UUID")
    require(probe["tool"] in TOOLS, "tool is outside the delegation probe allowlist")
    require(isinstance(probe["arguments"], dict), "arguments must be an object")
    # Actor identity never rides in arguments even in this test backend.
    require(not ({"actor", "principal_id", "owner_surface", "cdp_ws_url"} & set(probe["arguments"])),
            "probe cannot supply identity or endpoint authority")
    return probe


def reject_private(value, token):
    if isinstance(value, dict):
        for key, item in value.items():
            require(key.lower() not in PRIVATE_KEYS, "private metadata exposed to delegated session")
            reject_private(item, token)
    elif isinstance(value, list):
        for item in value:
            reject_private(item, token)
    elif isinstance(value, str):
        require(token not in value, "credential reflected by server")
        require("/devtools/" not in value and "macos_window:" not in value,
                "private endpoint or binding exposed to delegated session")


def summarize(envelope, token):
    require(isinstance(envelope, dict), "invalid MCP envelope")
    reject_private(envelope.get("error"), token)
    if "error" in envelope:
        return {"protocol_error": envelope["error"], "effects_verified": False}
    result = envelope.get("result")
    require(isinstance(result, dict) and isinstance(result.get("content"), list),
            "missing MCP content")
    require(len(result["content"]) <= 16, "MCP content budget exceeded")
    texts, images = [], []
    for content in result["content"]:
        require(isinstance(content, dict), "invalid MCP content entry")
        if content.get("type") == "text":
            text = content.get("text")
            require(isinstance(text, str) and len(text.encode()) <= 131_072, "text response budget")
            try:
                data = loads(text)
            except json.JSONDecodeError:
                data = text
            reject_private(data, token)
            texts.append(data)
        elif content.get("type") == "image":
            require(content.get("mimeType") == "image/png", "unexpected image type")
            pixels = base64.b64decode(content["data"], validate=True)
            require(24 <= len(pixels) <= MAX_REPLY and pixels[:8] == b"\x89PNG\r\n\x1a\n",
                    "invalid image receipt")
            images.append({"sha256": hashlib.sha256(pixels).hexdigest(),
                           "width": int.from_bytes(pixels[16:20], "big"),
                           "height": int.from_bytes(pixels[20:24], "big")})
        else:
            raise ValueError("unexpected MCP content type")
    return {"tool_error": result.get("isError", False), "texts": texts, "images": images,
            "effects_verified": False}


class Client:
    def __init__(self, env, manifest, now_ms):
        self.port, self.path, self.token, self.session = bootstrap(env, manifest, now_ms)
        self.ids = set()

    def call(self, probe):
        require(probe["id"] not in self.ids, "probe request ID already used; never replay")
        require(len(self.ids) < 64, "probe command budget exhausted")
        self.ids.add(probe["id"])
        body = json.dumps({"jsonrpc": "2.0", "id": probe["id"], "method": "tools/call",
                           "params": {"name": probe["tool"], "arguments": probe["arguments"]}})
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=40)
        try:
            conn.request("POST", self.path, body, {
                "Authorization": "Bearer " + self.token,
                "Content-Type": "application/json", "Accept": "application/json",
            })
            response = conn.getresponse()
            raw = response.read(MAX_REPLY + 1)
            require(len(raw) <= MAX_REPLY, "response budget exceeded")
            require(response.status == 200, "MCP HTTP refusal or transport failure")
            envelope = loads(raw)
            require(envelope.get("id") == probe["id"], "MCP reply identity mismatch")
            return summarize(envelope, self.token)
        finally:
            conn.close()


def user_text(frame):
    require(isinstance(frame, dict) and frame.get("type") == "user", "expected user protocol frame")
    content = frame.get("message", {}).get("content")
    if isinstance(content, str):
        return content
    require(isinstance(content, list), "missing user content")
    texts = [item.get("text") for item in content if item.get("type") == "text"]
    require(len(texts) == 1 and isinstance(texts[0], str), "probe expects one text request")
    return texts[0]


def emit(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)


def backend():
    manifest_path = Path.cwd() / ".intendant-delegation-proof.json"
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    with os.fdopen(os.open(manifest_path, flags), "r") as source:
        info = os.fstat(source.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_size <= 4096, "invalid proof manifest file")
        if os.name == "posix":
            require(info.st_uid == os.getuid() and not info.st_mode & 0o077,
                    "proof manifest must be private and owned")
        manifest = loads(source.read(4097))
    client = Client(os.environ, manifest, time.time_ns() // 1_000_000)
    backend_id = str(uuid.uuid4())
    emit({"type": "system", "subtype": "init", "session_id": backend_id,
          "model": "delegation-protocol-fixture", "tools": [], "permissionMode": "default",
          "cwd": str(Path.cwd())})
    for _ in range(64):
        import select
        remaining = (manifest["expires_unix_ms"] - time.time_ns() // 1_000_000) / 1000
        require(remaining > 0 and select.select([sys.stdin.buffer], [], [], min(remaining, 300))[0],
                "proof input interval expired")
        line = sys.stdin.buffer.readline(65_537)
        if not line:
            return
        require(len(line) <= 65_536, "backend protocol frame budget")
        probe = parse_probe(loads(user_text(loads(line))))
        # This receipt deliberately has no env URL, auth header or bearer hash.
        try:
            result = client.call(probe)
            result.update(probe_id=probe["id"], token_bound_session=client.session,
                          transport="injected_session_bearer", owner_fallback_used=False)
        except Exception:
            # Do not echo exception bodies: an endpoint/error could contain a token.
            result = {"probe_id": probe["id"], "probe_failed": True,
                      "effects_unconfirmed": True, "effects_verified": False}
        emit({"type": "result", "subtype": "success", "is_error": result.get("probe_failed", False),
              "result": json.dumps(result), "session_id": backend_id})
        if manifest.get("single_probe") is True:
            destination = Path.cwd() / ".intendant-delegation-result.json"
            fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w") as output:
                json.dump(result, output)
            return


def check_injected_session():
    """No-GUI smoke through an actual temporary daemon and external supervisor."""
    import argparse
    import re
    import secrets
    import shlex
    import shutil
    import socket
    import struct
    import subprocess
    import tempfile
    parser = argparse.ArgumentParser(description=check_injected_session.__doc__)
    parser.add_argument("--check-injected-session", action="store_true")
    parser.add_argument("--bin", required=True, type=Path)
    parser.add_argument("--report", required=True, type=Path)
    args = parser.parse_args()
    require(not args.report.exists(), "report must be fresh")
    binary = args.bin.resolve(strict=True)
    root = Path(tempfile.mkdtemp(prefix="intendant-session-identity-proof-"))
    os.chmod(root, 0o700)
    home, project = root / "home", root / "project"
    home.mkdir(); project.mkdir()
    (project / "intendant.toml").write_text("")
    mock = root / "mock.json"; mock.write_text('{"profiles":[]}')
    report = {"ok": False, "schema": "intendant-session-identity-proof-v1",
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "probe_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "native_input_calls": 0, "browser_created": False,
              "installed_daemon_changed": False, "delegation_verified": False}
    daemon = None
    connection = None
    try:
        env = {k: v for k, v in os.environ.items() if k in ("PATH", "TMPDIR", "LANG", "LC_ALL", "USER", "LOGNAME")}
        env.update(HOME=str(home), USERPROFILE=str(home), PROVIDER="mock",
                   INTENDANT_MOCK_SCRIPT=str(mock), INTENDANT_MOCK_DISPLAY="synthetic",
                   INTENDANT_MOCK_MEMORY="nominal")
        with (root / "daemon.log").open("wb") as log:
            daemon = subprocess.Popen([str(binary), "--web", "0", "--bind", "127.0.0.1",
                "--no-tui", "--no-tls", "--autonomy", "full"], cwd=project, env=env,
                stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        deadline = time.monotonic() + 35
        port = token = None
        while time.monotonic() < deadline:
            text = (root / "daemon.log").read_text(errors="replace")
            found = re.search(r"Dashboard:.*?https?://127\.0\.0\.1:(\d+)", text)
            if found:
                port = int(found[1])
                path = home / ".intendant/loopback-tokens" / f"{port}.token"
                if path.is_file():
                    token = path.read_text().strip(); break
            require(daemon.poll() is None, "temporary daemon stopped before readiness")
            time.sleep(.1)
        require(port and token, "temporary daemon did not become ready")
        launcher = project / "delegation-test-backend"
        launcher.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable) + " " +
                            shlex.quote(str(Path(__file__).resolve())) + ' "$@"\n')
        launcher.chmod(0o700)
        manifest = {"schema": SCHEMA, "origin": f"http://127.0.0.1:{port}",
                    "expires_unix_ms": time.time_ns() // 1_000_000 + 120_000, "single_probe": True}
        fd = os.open(project / ".intendant-delegation-proof.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as output: json.dump(manifest, output)
        # Owner token starts only this disposable fixture; the child uses only
        # the daemon's ordinary derived session token, never this owner's token.
        connection = socket.create_connection(("127.0.0.1", port), timeout=5)
        key = base64.b64encode(secrets.token_bytes(16)).decode()
        path = "/ws?" + urllib.parse.urlencode({"token": token})
        connection.sendall((f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n" +
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        buffer = b""
        while b"\r\n\r\n" not in buffer:
            require(len(buffer) <= 8192, "dashboard handshake budget")
            part = connection.recv(4096); require(part, "dashboard handshake closed"); buffer += part
        header = buffer.split(b"\r\n\r\n", 1)[0]
        require(header.split(b"\r\n", 1)[0].split()[1] == b"101", "dashboard handshake refused")
        expected = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        headers = dict((k.lower(), v.strip()) for k, v in
                       (line.decode().split(":", 1) for line in header.split(b"\r\n")[1:]))
        require(headers.get("sec-websocket-accept") == expected, "dashboard handshake mismatch")
        probe = {"id": str(uuid.uuid4()), "tool": "whoami", "arguments": {}}
        message = {"action": "create_session", "task": json.dumps({"probe": probe}),
                   "agent": "claude-code", "agent_command": str(launcher), "project_root": str(project)}
        payload = json.dumps(message).encode(); require(len(payload) <= 65535, "request budget")
        mask = secrets.token_bytes(4)
        length = bytes([0x80 | len(payload)]) if len(payload) < 126 else b"\xfe" + struct.pack("!H", len(payload))
        # One create only. No retry-spawn loop or fixture duplication.
        time.sleep(.3)
        connection.sendall(b"\x81" + length + mask + bytes(v ^ mask[i % 4] for i, v in enumerate(payload)))
        deadline = time.monotonic() + 45
        result_path = project / ".intendant-delegation-result.json"
        while not result_path.is_file() and time.monotonic() < deadline:
            require(daemon.poll() is None, "temporary daemon stopped during probe")
            time.sleep(.1)
        require(result_path.is_file(), "supervised probe did not return a receipt")
        value = loads(result_path.read_text())
        require(value.get("probe_id") == probe["id"] and value.get("probe_failed") is not True,
                "session probe failed")
        require(value.get("owner_fallback_used") is False, "unexpected owner fallback")
        texts = value.get("texts"); require(isinstance(texts, list) and len(texts) == 1, "missing whoami result")
        identity = texts[0]
        require(identity.get("supervised") is True and identity.get("actor_kind") == "agent_session",
                "server did not bind a supervised agent actor")
        require(identity.get("daemon_session_id") == value.get("token_bound_session"),
                "server identity differs from child's injected session")
        require(isinstance(identity.get("principal_id"), str), "missing server principal")
        report.update(ok=True, gate_resolved_actor=identity["actor_kind"],
                      principal_id=identity["principal_id"], daemon_session_id=identity["daemon_session_id"],
                      session_bearer_identity_verified=True, owner_fallback_used=False,
                      supervisor_launch_requests=1)
    except Exception as error:
        report["error_type"] = type(error).__name__
        report["error"] = str(error) if isinstance(error, ValueError) else "local protocol smoke failed"
    finally:
        if connection is not None: connection.close()
        if daemon is not None:
            if daemon.poll() is None:
                daemon.terminate()
                try: daemon.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    daemon.kill(); daemon.wait(timeout=5)
            report["temporary_daemon_reaped"] = daemon.poll() is not None
        if not report["ok"]:
            report["private_rig_retained_for_diagnosis"] = str(root)
        require(not args.report.exists(), "refusing to overwrite concurrent report")
        args.report.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(args.report, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as output: json.dump(report, output, indent=2)
        if report["ok"]: shutil.rmtree(root)
    print(json.dumps(report, indent=2))
    return 0 if report["ok"] and report.get("temporary_daemon_reaped") else 1


def self_test():
    import unittest
    class ProbeTests(unittest.TestCase):
        def env(self):
            return {"INTENDANT_MCP_URL": "http://127.0.0.1:12345/mcp?session_id=test-session",
                    "INTENDANT_MCP_BEARER_TOKEN": "test-secret-bearer", "INTENDANT_SESSION_ID": "test-session"}
        def manifest(self):
            return {"schema": SCHEMA, "origin": "http://127.0.0.1:12345", "expires_unix_ms": 5000}
        def test_session_bootstrap_has_no_owner_fallback(self):
            self.assertEqual(bootstrap(self.env(), self.manifest(), 1000)[:2],
                             (12345, "/mcp?session_id=test-session"))
            for key in self.env():
                env = self.env(); env.pop(key)
                with self.subTest(key=key), self.assertRaises(ValueError):
                    bootstrap(env, self.manifest(), 1000)
        def test_foreign_endpoint_or_session_refused(self):
            for url in ("https://127.0.0.1:12345/mcp?session_id=test-session",
                        "http://localhost.example:12345/mcp?session_id=test-session",
                        "http://127.0.0.1:8766/mcp?session_id=test-session",
                        "http://127.0.0.1:12345/mcp?session_id=other",
                        "http://127.0.0.1:12345/mcp?session_id=test-session&session_id=test-session",
                        "http://127.0.0.1:12345/mcp?session_id=test-session&session=test-session"):
                env = self.env(); env["INTENDANT_MCP_URL"] = url
                with self.subTest(url=url), self.assertRaises(ValueError):
                    bootstrap(env, self.manifest(), 1000)
        def test_external_localhost_alias_uses_numeric_connection(self):
            env = self.env(); env["INTENDANT_MCP_URL"] = env["INTENDANT_MCP_URL"].replace("127.0.0.1", "localhost")
            self.assertEqual(bootstrap(env, self.manifest(), 1000)[:2],
                             (12345, "/mcp?session_id=test-session"))
        def test_session_bearer_and_no_owner_header_on_real_loopback(self):
            from http.server import BaseHTTPRequestHandler, HTTPServer
            from threading import Thread
            observed = []
            class Handler(BaseHTTPRequestHandler):
                def do_POST(handler):
                    body = loads(handler.rfile.read(int(handler.headers["Content-Length"])))
                    observed.append((handler.path, dict(handler.headers), body))
                    response = json.dumps({"jsonrpc": "2.0", "id": body["id"],
                        "result": {"content": [{"type": "text", "text": '{"ok":true}'}]}}).encode()
                    handler.send_response(200)
                    handler.send_header("Content-Length", str(len(response)))
                    handler.end_headers(); handler.wfile.write(response)
                def log_message(self, *unused):
                    pass
            server = HTTPServer(("127.0.0.1", 0), Handler)
            server.timeout = 3
            worker = Thread(target=server.handle_request, daemon=True); worker.start()
            try:
                env = self.env(); env["INTENDANT_MCP_URL"] = f"http://localhost:{server.server_port}/mcp?session_id=test-session"
                manifest = self.manifest(); manifest["origin"] = f"http://127.0.0.1:{server.server_port}"
                client = Client(env, manifest, 1000)
                probe = {"id": str(uuid.uuid4()), "tool": "whoami", "arguments": {}}
                receipt = client.call(probe)
                self.assertFalse(receipt["effects_verified"])
                with self.assertRaisesRegex(ValueError, "never replay"): client.call(probe)
                worker.join(4); self.assertFalse(worker.is_alive())
                self.assertEqual(len(observed), 1)
                path, headers, body = observed[0]
                self.assertEqual(path, "/mcp?session_id=test-session")
                self.assertEqual(headers["Authorization"], "Bearer test-secret-bearer")
                self.assertFalse(any(k.lower() == "x-intendant-loopback-token" for k in headers))
                self.assertEqual(body["params"], {"name": "whoami", "arguments": {}})
            finally:
                server.server_close(); worker.join(4)
        def test_legacy_query_credential_removed(self):
            env = self.env(); env["INTENDANT_MCP_URL"] += "&mcp_token=test-secret-bearer"
            self.assertNotIn("test-secret", bootstrap(env, self.manifest(), 1000)[1])
            env["INTENDANT_MCP_URL"] += "-wrong"
            with self.assertRaises(ValueError): bootstrap(env, self.manifest(), 1000)
        def test_manifest_expiry_is_closed(self):
            for expiry in (True, 1000, 999, 301001, None):
                manifest = self.manifest(); manifest["expires_unix_ms"] = expiry
                with self.subTest(expiry=expiry), self.assertRaises(ValueError):
                    bootstrap(self.env(), manifest, 1000)
        def test_no_general_tools_or_forged_authority(self):
            for tool in ("execute_cu_actions", "terminal_write", "act", "take_screenshot"):
                with self.subTest(tool=tool), self.assertRaises(ValueError):
                    parse_probe({"probe": {"id": str(uuid.uuid4()), "tool": tool, "arguments": {}}})
            with self.assertRaises(ValueError):
                parse_probe({"probe": {"id": str(uuid.uuid4()), "tool": "whoami", "arguments": {"owner_surface": True}}})
        def test_duplicate_json_keys_fail(self):
            with self.assertRaises(ValueError): loads('{"probe":{},"probe":{}}')
        def test_no_private_metadata_or_credentials_in_results(self):
            for value in ({"cdp_ws_url": "x"}, {"nested": [{"profile_dir": "x"}]},
                          "macos_window:secret", "test-secret-bearer", "ws://x/devtools/page/y"):
                with self.subTest(value=value), self.assertRaises(ValueError):
                    reject_private(value, "test-secret-bearer")
        def test_acknowledgement_is_not_effect_verification(self):
            reply = {"result": {"content": [{"type": "text", "text": '{"ok":true}'}]}}
            self.assertFalse(summarize(reply, "test-secret-bearer")["effects_verified"])
        def test_id_reuse_fails_before_transport(self):
            client = Client(self.env(), self.manifest(), 1000)
            probe = {"id": str(uuid.uuid4()), "tool": "whoami", "arguments": {}}
            client.ids.add(probe["id"])
            with self.assertRaisesRegex(ValueError, "never replay"): client.call(probe)
        def test_protocol_text_frames(self):
            self.assertEqual(user_text({"type": "user", "message": {"content": "request"}}), "request")
            self.assertEqual(user_text({"type": "user", "message": {"content": [{"type": "text", "text": "request"}]}}), "request")
            with self.assertRaises(ValueError): user_text({"type": "assistant", "message": {"content": "request"}})
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(ProbeTests))
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    if "--check-injected-session" in sys.argv[1:]:
        raise SystemExit(check_injected_session())
    if "--self-test" in sys.argv[1:]:
        raise SystemExit(self_test())
    if "--help" in sys.argv[1:]:
        print(__doc__)
        raise SystemExit(0)
    try:
        backend()
    except Exception:
        print("delegation protocol fixture refused; no owner-token fallback or retry", file=sys.stderr)
        raise SystemExit(2)
