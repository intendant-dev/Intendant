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
