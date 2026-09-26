#!/usr/bin/env python3
"""Keyless two-daemon, real-PTY handover through the actual MCP relay (Unix).

Usage: python3 scripts/smoke_intendant_terminal_handover.py /path/to/intendant
Only temporary homes, test-owned processes, and test-owned tokens are used.
"""
from __future__ import annotations

import argparse
import http.client
import importlib.util
import json
import os
import re
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import threading
import time

SPEC = importlib.util.spec_from_file_location("smoke_terminal_relay", Path(__file__).with_name("intendant-mcp-relay.py"))
assert SPEC and SPEC.loader
relay = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = relay
SPEC.loader.exec_module(relay)


def until(probe, label, timeout=45):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = probe()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError("timed out: " + label)


def exchange(port, method, path, body=None, token=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
        if token:
            headers["X-Intendant-Loopback-Token"] = token
        connection.request(method, path, body=json.dumps(body).encode() if body is not None else None, headers=headers)
        response = connection.getresponse()
        raw = response.read()
        assert response.status == 200, f"unexpected HTTP status {response.status}"
        return json.loads(raw)
    finally:
        connection.close()


def run(binary):
    with tempfile.TemporaryDirectory(prefix="intendant-terminal-handover-") as tmp:
        root = Path(tmp)
        state, home, project = root / "state", root / "home", root / "project"
        for folder in [state, home, project, project / "child"]:
            folder.mkdir()
        (project / "intendant.toml").write_text("")
        script = root / "mock.json"
        script.write_text(json.dumps({"profiles": [{"steps": [{"content": "ok"}]}]}))
        # Construct a clean environment instead of inheriting provider keys.
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home),
               "INTENDANT_HOME": str(state), "PROVIDER": "mock", "SHELL": "/bin/bash",
               "INTENDANT_MOCK_SCRIPT": str(script), "TERM": "dumb", "LANG": "C.UTF-8",
               "TMPDIR": str(root), "INTENDANT_TESTING_NOTICE": "0"}
        daemons, logs = [], []
        front = None
        front_thread = None
        try:
            def start(label, takeover=False):
                output = (root / (label + ".log")).open("wb")
                logs.append(output)
                args = [str(binary), "--web", "0", "--bind", "127.0.0.1", "--no-tls"]
                if takeover:
                    args.append("--takeover")
                process = subprocess.Popen(args, cwd=project, env=env, stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT)
                daemons.append(process)
                def ready():
                    assert process.poll() is None, f"test daemon {label} exited during startup"
                    try:
                        descriptor = json.loads((state / "cli-path.meta.json").read_text())
                        if descriptor.get("pid") != process.pid:
                            return None
                        port = descriptor["port"]
                        token = (state / "loopback-tokens" / f"{port}.token").read_text().strip()
                        status = exchange(port, "GET", "/api/daemon/handover", token=token)
                        return process, port, token, status
                    except (OSError, ValueError, AssertionError, http.client.HTTPException):
                        return None
                return until(ready, label + " readiness")

            first, first_port, first_token, _ = start("predecessor")
            front = relay.RelayHTTPServer(("127.0.0.1", 0), relay.RelayConfig(state, timeout_seconds=10))
            front_thread = threading.Thread(target=front.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True)
            front_thread.start()
            request_id = 0
            def tool(name, args):
                nonlocal request_id
                request_id += 1
                response = exchange(front.server_address[1], "POST", "/mcp", {
                    "jsonrpc": "2.0", "id": request_id, "method": "tools/call",
                    "params": {"name": name, "arguments": args},
                })
                assert "result" in response, "MCP request failed: " + repr(response.get("error"))
                blocks = response["result"].get("content", [])
                return json.loads(next(block["text"] for block in blocks if block.get("type") == "text"))

            opened = tool("terminal_open", {"terminal_id": "same-name"})
            assert opened.get("ok") is True
            old_id = opened["terminal_id"]
            assert old_id.startswith("iterm1.")
            cursor = 0
            transcript = ""
            def wait_output(needle):
                def read():
                    nonlocal cursor, transcript
                    page = tool("terminal_read", {"terminal_id": old_id, "cursor": cursor, "max_bytes": 65536})
                    assert page.get("ok"), "terminal read failed: " + str(page.get("code"))
                    assert not page["gap"], "test output unexpectedly fell out of scrollback"
                    cursor = page["next_cursor"]
                    transcript += page["output"]
                    return needle in transcript
                until(read, "terminal output: " + needle)

            # Await a prompt before sending input; an initializing shell can
            # flush queued keystrokes. In the empty HOME bash's prompt ends in $/#.
            until(lambda: tool("terminal_read", {"terminal_id": old_id, "cursor": 0}).get("output"), "shell startup")
            command = "stty -echo; cd " + shlex.quote(str(project / "child")) + "; export HANDOVER_SENTINEL=preserved; printf 'READY_%s\\n' state; while [ ! -f ../release ]; do sleep 0.1; done; printf 'FINISHED_%s:%s:%s\\n' command \"$HANDOVER_SENTINEL\" \"$PWD\""
            assert tool("terminal_write", {"terminal_id": old_id, "input": command})["ok"]
            wait_output("READY_state")
            second, second_port, second_token, _ = start("successor", takeover=True)
            def draining():
                status = exchange(first_port, "GET", "/api/daemon/handover", token=first_token)
                return status.get("draining") and status.get("terminal_count") == 1
            until(draining, "predecessor terminal drain holdout")
            assert first.poll() is None, "update killed the active terminal owner"
            fresh = tool("terminal_open", {"terminal_id": "same-name"})
            assert fresh.get("ok") and fresh["created"]
            assert fresh["terminal_id"] != old_id
            attached = tool("terminal_open", {"terminal_id": old_id})
            assert attached.get("ok") and not attached["created"]
            assert attached["terminal_id"] == old_id
            (project / "release").write_text("finish the already-running command")
            wait_output("FINISHED_command:preserved:" + str(project / "child"))
            assert tool("terminal_write", {"terminal_id": old_id, "input": "printf 'AFTER_%s:%s:%s\\n' update \"$HANDOVER_SENTINEL\" \"$PWD\"; exit 7"})["ok"]
            wait_output("AFTER_update:preserved:" + str(project / "child"))
            ended = until(lambda: (p if not p["alive"] else None) if (p := tool("terminal_read", {"terminal_id": old_id, "cursor": cursor})).get("ok") else None, "shell exit")
            assert ended["exit_status"] == 7
            assert first.poll() is None, "exited terminal lost its retained output before explicit close"
            assert tool("terminal_close", {"terminal_id": old_id})["ok"]
            until(lambda: first.poll() is not None, "predecessor exit after explicit close")
            stale = tool("terminal_write", {"terminal_id": old_id, "input": "should-never-run"})
            assert not stale.get("ok") and stale.get("code") in {"terminal_lost", "terminal_owner_unavailable"}
            assert tool("terminal_read", {"terminal_id": fresh["terminal_id"]})["alive"]
            assert tool("terminal_close", {"terminal_id": fresh["terminal_id"]})["ok"]
            print("PASS: real foreground command, cwd, environment, cursor and exit status survived takeover; close released predecessor; stale handle did not reach same-named successor shell")
        except BaseException:
            # These are this rig's disposable daemons, not owner logs. Keep
            # failure evidence before TemporaryDirectory removes the rig,
            # redacting even its generated test admission tokens.
            for log in root.glob("*.log"):
                try:
                    tail = log.read_bytes()[-12000:].decode("utf-8", errors="replace")
                    tail = re.sub(r"[a-fA-F0-9]{64}", "[redacted-test-token]", tail)
                    print(f"--- {log.name} (test daemon) ---\n{tail}", file=sys.stderr)
                except OSError:
                    pass
            raise
        finally:
            if front is not None:
                front.shutdown()
                front.server_close()
            if front_thread is not None:
                front_thread.join(timeout=5)
            for process in reversed(daemons):
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
            for output in logs:
                output.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    run(parser.parse_args().binary.resolve(strict=True))
