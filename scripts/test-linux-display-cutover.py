#!/usr/bin/env python3
"""Keyless Linux cutover gate for concurrent Intendant virtual displays."""

from __future__ import annotations

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import signal
import struct
import subprocess
import sys
import threading
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from typing import Any


class CutoverError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise CutoverError(message)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def pid_signature(pid: int) -> dict[str, Any]:
    proc = Path("/proc") / str(pid)
    text = (proc / "stat").read_text()
    close = text.rfind(")")
    require(close > 0, f"malformed /proc/{pid}/stat")
    fields = text[close + 2 :].split()
    require(len(fields) >= 20, f"short /proc/{pid}/stat")
    try:
        exe = os.readlink(proc / "exe")
    except OSError:
        exe = "unavailable"
    return {
        "pid": pid,
        "startTimeTicks": fields[19],
        "comm": (proc / "comm").read_text().strip(),
        "exe": exe,
    }


def signature_live(signature: dict[str, Any]) -> bool:
    try:
        return pid_signature(int(signature["pid"])) == signature
    except (OSError, ValueError, CutoverError):
        return False


def process_environment(pid: int) -> dict[str, str]:
    result: dict[str, str] = {}
    raw = (Path("/proc") / str(pid) / "environ").read_bytes()
    for item in raw.split(b"\0"):
        if item and b"=" in item:
            key, value = item.split(b"=", 1)
            result[key.decode()] = value.decode()
    return result


def runner_snapshot() -> list[dict[str, Any]]:
    result: list[dict[str, Any]] = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            comm = (entry / "comm").read_text().strip()
            if comm in {"Runner.Listener", "Runner.Worker"}:
                result.append(pid_signature(int(entry.name)))
        except (OSError, CutoverError):
            pass
    return sorted(result, key=lambda item: (item["comm"], item["pid"]))


def wait_for(predicate, message: str, timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except Exception as error:
            last_error = error
        time.sleep(0.1)
    suffix = f"; last error: {last_error}" if last_error else ""
    raise CutoverError(message + suffix)


def parse_json_output(
    completed: subprocess.CompletedProcess[str],
    label: str,
    allow_failure: bool,
) -> Any:
    text = completed.stdout.strip()
    try:
        value = json.loads(text) if text else None
    except json.JSONDecodeError as error:
        raise CutoverError(
            f"{label} returned non-JSON output ({error}); "
            f"exit={completed.returncode}\nstdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
        ) from error
    if completed.returncode != 0 and not allow_failure:
        raise CutoverError(
            f"{label} failed with exit {completed.returncode}: "
            f"{value!r}\nstderr:\n{completed.stderr}"
        )
    return value


@dataclass
class Driver:
    binary: Path
    port: int
    home: Path

    def call(self, *args: str, label: str = "ctl", allow_failure: bool = False) -> Any:
        env = os.environ.copy()
        env["HOME"] = str(self.home)
        completed = subprocess.run(
            [
                str(self.binary),
                "ctl",
                "--port",
                str(self.port),
                "--json",
                *args,
            ],
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=90,
            check=False,
        )
        return parse_json_output(completed, label, allow_failure)


class ProofHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:  # noqa: N802
        route = self.path.split("?", 1)[0]
        pages = {
            "/alpha": ("LANE B ALPHA", "rgb(220,35,45)"),
            "/bravo": ("LANE B BRAVO", "rgb(30,65,220)"),
        }
        if route not in pages:
            self.send_error(404)
            return
        title, background = pages[route]
        body = f"""<!doctype html><meta charset="utf-8"><title>{title}</title>
<style>html,body{{margin:0;width:100%;height:100%;background:{background};color:white}}
div{{position:fixed;left:40px;top:90px;padding:25px;border:8px solid white;
font:800 52px sans-serif}}</style><div>{title}</div>""".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt: str, *args: object) -> None:
        print("proof-http: " + fmt % args)


def png_geometry(path: Path) -> tuple[int, int]:
    data = path.read_bytes()[:24]
    require(data[:8] == b"\x89PNG\r\n\x1a\n", f"{path} is not a PNG")
    require(data[12:16] == b"IHDR", f"{path} has no leading IHDR")
    return struct.unpack(">II", data[16:24])


def choose_foreign_display() -> int:
    for display_id in range(99, 200):
        if not Path(f"/tmp/.X{display_id}-lock").exists() and not Path(
            f"/tmp/.X11-unix/X{display_id}"
        ).exists():
            return display_id
    raise CutoverError("no free display in the managed range")


def cdp_target_matches(workspace: dict[str, Any], url: str, title: str) -> bool:
    try:
        with urllib.request.urlopen(workspace["cdp_http_url"] + "/json/list", timeout=2) as response:
            targets = json.load(response)
    except Exception:
        return False
    return any(
        target.get("type") == "page"
        and target.get("id") == workspace["active_target_id"]
        and target.get("url") == url
        and title in str(target.get("title", ""))
        for target in targets
    )


def validate_display(value: Any, width: int, height: int) -> dict[str, Any]:
    require(isinstance(value, dict) and value.get("ok") is True, f"display create failed: {value}")
    display_id = value.get("display_id")
    require(isinstance(display_id, int) and 99 <= display_id <= 199, f"bad display id: {value}")
    require(value.get("display_target") == f"display_{display_id}", f"target mismatch: {value}")
    require(value.get("width") == width and value.get("height") == height, f"geometry mismatch: {value}")
    require(isinstance(value.get("request_id"), str) and value["request_id"], f"missing request id: {value}")
    require(
        isinstance(value.get("capture_generation"), str)
        and value["capture_generation"].startswith("vdcg-"),
        f"missing generation: {value}",
    )
    return value


def validate_workspace(value: Any, url: str, display: dict[str, Any]) -> dict[str, Any]:
    require(isinstance(value, dict) and value.get("status") == "ready", f"workspace not ready: {value}")
    require(value.get("provider") == "cdp", f"wrong provider: {value}")
    require(value.get("url") == url, f"workspace URL mismatch: {value}")
    require(value.get("display_target") == display["display_target"], f"workspace display mismatch: {value}")
    require(isinstance(value.get("process_id"), int), f"missing browser pid: {value}")
    require(isinstance(value.get("active_target_id"), str), f"missing active target: {value}")
    require(isinstance(value.get("cdp_http_url"), str), f"missing CDP URL: {value}")
    return value


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", dest="binary", required=True, type=Path)
    parser.add_argument("--port", required=True, type=int)
    parser.add_argument("--daemon-pid", required=True, type=int)
    parser.add_argument("--rig", required=True, type=Path)
    args = parser.parse_args()

    require(sys.platform.startswith("linux"), "cutover gate must run on Linux")
    for command in ("Xvfb", "xauth"):
        require(shutil.which(command) is not None, f"missing executable: {command}")
    require(args.binary.is_file() and os.access(args.binary, os.X_OK), f"bad binary: {args.binary}")

    artifact_dir = args.rig / "artifacts" / "cdn-linux-cutover"
    if artifact_dir.exists():
        shutil.rmtree(artifact_dir)
    artifact_dir.mkdir(mode=0o700, parents=True)
    driver = Driver(args.binary.resolve(), args.port, (args.rig / "home").resolve())

    receipt: dict[str, Any] = {
        "schemaVersion": 1,
        "passed": False,
        "startedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "binarySha256": sha256_file(args.binary),
        "harnessSha256": sha256_file(Path(__file__).resolve()),
    }
    displays: list[dict[str, Any]] = []
    browser_pids: list[dict[str, Any]] = []
    foreign: subprocess.Popen[bytes] | None = None
    sentinel: subprocess.Popen[bytes] | None = None
    server: http.server.ThreadingHTTPServer | None = None
    server_thread: threading.Thread | None = None

    def cleanup_display(display: dict[str, Any], note: str) -> None:
        if display.get("destroyed"):
            return
        try:
            result = driver.call(
                "display",
                "destroy",
                str(display["display_id"]),
                display["capture_generation"],
                "--note",
                note,
                label="cleanup display",
                allow_failure=True,
            )
            display["cleanupResult"] = result
            display["destroyed"] = isinstance(result, dict) and result.get("ok") is True
        except Exception as error:
            display["cleanupError"] = str(error)

    try:
        inode_use = int(
            subprocess.check_output(["df", "-Pi", "/tmp"], text=True)
            .splitlines()[-1]
            .split()[4]
            .rstrip("%")
        )
        require(inode_use < 90, f"/tmp inode use is {inode_use}%")
        receipt["tmpInodeUsePercent"] = inode_use

        daemon_before = pid_signature(args.daemon_pid)
        runners_before = runner_snapshot()
        receipt["daemon"] = daemon_before
        receipt["runnerSentinelsBefore"] = runners_before

        foreign_id = choose_foreign_display()
        foreign_log = (artifact_dir / "foreign-xvfb.log").open("wb")
        foreign = subprocess.Popen(
            [
                shutil.which("Xvfb") or "Xvfb",
                f":{foreign_id}",
                "-screen",
                "0",
                "640x480x24",
                "-nolisten",
                "tcp",
            ],
            stdin=subprocess.DEVNULL,
            stdout=foreign_log,
            stderr=subprocess.STDOUT,
        )
        foreign_signature = pid_signature(foreign.pid)
        wait_for(
            lambda: foreign is not None
            and foreign.poll() is None
            and Path(f"/tmp/.X11-unix/X{foreign_id}").exists(),
            f"foreign Xvfb :{foreign_id} did not become ready",
        )
        receipt["foreignXvfb"] = {"displayId": foreign_id, "process": foreign_signature}

        sentinel = subprocess.Popen(["sleep", "600"], stdin=subprocess.DEVNULL)
        sentinel_signature = pid_signature(sentinel.pid)
        receipt["syntheticSentinel"] = sentinel_signature

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), ProofHandler)
        server_thread = threading.Thread(target=server.serve_forever, daemon=True)
        server_thread.start()
        proof_port = int(server.server_address[1])
        nonce = hashlib.sha256(os.urandom(32)).hexdigest()[:16]
        alpha_url = f"http://127.0.0.1:{proof_port}/alpha?job={nonce}-alpha"
        bravo_url = f"http://127.0.0.1:{proof_port}/bravo?job={nonce}-bravo"
        receipt["proofUrls"] = [alpha_url, bravo_url]

        dimensions = [(800, 600), (1024, 720)]
        with ThreadPoolExecutor(max_workers=2) as pool:
            futures = [
                pool.submit(
                    driver.call,
                    "display",
                    "create",
                    "--width",
                    str(width),
                    "--height",
                    str(height),
                    label=f"create {width}x{height}",
                )
                for width, height in dimensions
            ]
            displays = [
                validate_display(future.result(), width, height)
                for future, (width, height) in zip(futures, dimensions, strict=True)
            ]

        first, second = displays
        require(first["display_id"] != second["display_id"], "concurrent creates reused a display")
        require(first["request_id"] != second["request_id"], "concurrent creates reused a request id")
        require(foreign_id not in {first["display_id"], second["display_id"]}, "allocator claimed foreign Xvfb")
        require(signature_live(foreign_signature), "foreign Xvfb died during allocation")

        wrong_generation = first["capture_generation"][:-1] + (
            "0" if first["capture_generation"][-1] != "0" else "1"
        )
        stale = driver.call(
            "display",
            "destroy",
            str(first["display_id"]),
            wrong_generation,
            "--note",
            "cutover-stale-generation",
            label="stale generation destroy",
            allow_failure=True,
        )
        require(isinstance(stale, dict) and stale.get("ok") is False, f"stale destroy did not fail: {stale}")
        require(Path(f"/tmp/.X11-unix/X{first['display_id']}").exists(), "stale destroy killed display")
        receipt["staleGenerationRefusal"] = stale

        profiles = artifact_dir / "profiles"
        profiles.mkdir(mode=0o700)
        workspace_inputs = [
            (alpha_url, "LANE B ALPHA", first, profiles / "alpha"),
            (bravo_url, "LANE B BRAVO", second, profiles / "bravo"),
        ]
        with ThreadPoolExecutor(max_workers=2) as pool:
            futures = [
                pool.submit(
                    driver.call,
                    "browser",
                    "create",
                    url,
                    "--label",
                    title,
                    "--provider",
                    "cdp",
                    "--session",
                    "attempt-" + title.lower().replace(" ", "-"),
                    "--display-target",
                    display["display_target"],
                    "--profile-dir",
                    str(profile),
                    label="create browser " + title,
                )
                for url, title, display, profile in workspace_inputs
            ]
            workspaces = [
                validate_workspace(future.result(), url, display)
                for future, (url, _, display, _) in zip(futures, workspace_inputs, strict=True)
            ]

        require(workspaces[0]["id"] != workspaces[1]["id"], "workspaces reused an id")
        require(workspaces[0]["process_id"] != workspaces[1]["process_id"], "workspaces reused a process")
        for workspace, (url, title, display, profile) in zip(
            workspaces, workspace_inputs, strict=True
        ):
            wait_for(
                lambda workspace=workspace, url=url, title=title: cdp_target_matches(
                    workspace, url, title
                ),
                f"CDP did not bind {workspace['id']} to {url}",
            )
            signature = pid_signature(int(workspace["process_id"]))
            browser_pids.append(signature)
            environment = process_environment(int(workspace["process_id"]))
            require(
                environment.get("DISPLAY") == f":{display['display_id']}",
                f"browser DISPLAY mismatch: {environment.get('DISPLAY')}",
            )
            xauthority = environment.get("XAUTHORITY")
            require(xauthority is not None, "browser has no XAUTHORITY")
            authority_path = Path(xauthority)
            require(authority_path.is_file(), f"missing Xauthority: {authority_path}")
            require(
                authority_path.stat().st_mode & 0o777 == 0o600,
                f"Xauthority mode is not 0600: {authority_path}",
            )
            require(Path(workspace["profile_dir"]) == profile, "workspace profile changed")
            workspace["process"] = signature
            workspace["xauthority"] = xauthority
        require(
            workspaces[0]["xauthority"] != workspaces[1]["xauthority"],
            "displays reused Xauthority",
        )

        screenshots: list[dict[str, Any]] = []
        for name, display, geometry in [
            ("alpha", first, (800, 600)),
            ("bravo", second, (1024, 720)),
        ]:
            output = artifact_dir / f"{name}.png"
            ctl_receipt = driver.call(
                "display",
                "screenshot",
                "--target",
                display["display_target"],
                "--output",
                str(output),
                label="screenshot " + name,
            )
            require(output.is_file() and output.stat().st_size > 0, f"missing {name} screenshot")
            require(png_geometry(output) == geometry, f"{name} screenshot geometry mismatch")
            screenshots.append(
                {
                    "name": name,
                    "displayId": display["display_id"],
                    "sha256": sha256_file(output),
                    "byteLength": output.stat().st_size,
                    "geometry": list(geometry),
                    "ctlReceipt": ctl_receipt,
                }
            )
        require(
            screenshots[0]["sha256"] != screenshots[1]["sha256"],
            "distinct pages produced identical screenshots",
        )
        receipt["screenshots"] = screenshots
        receipt["workspaces"] = workspaces
        receipt["displays"] = displays

        with ThreadPoolExecutor(max_workers=2) as pool:
            futures = [
                pool.submit(
                    driver.call,
                    "display",
                    "destroy",
                    str(display["display_id"]),
                    display["capture_generation"],
                    "--note",
                    "cutover-exact",
                    label="exact destroy",
                )
                for display in displays
            ]
            for display, future in zip(displays, futures, strict=True):
                result = future.result()
                require(isinstance(result, dict) and result.get("ok") is True, f"destroy failed: {result}")
                require(result.get("display_id") == display["display_id"], f"wrong destroy id: {result}")
                require(
                    result.get("capture_generation") == display["capture_generation"],
                    f"wrong destroy generation: {result}",
                )
                display["destroyResult"] = result
                display["destroyed"] = True

        for display in displays:
            wait_for(
                lambda display=display: not Path(
                    f"/tmp/.X11-unix/X{display['display_id']}"
                ).exists()
                and not Path(f"/tmp/.X{display['display_id']}-lock").exists(),
                f"display :{display['display_id']} survived teardown",
            )
        for signature in browser_pids:
            wait_for(
                lambda signature=signature: not signature_live(signature),
                f"browser survived teardown: {signature}",
            )

        require(signature_live(foreign_signature), "foreign Xvfb was killed")
        require(signature_live(sentinel_signature), "synthetic sentinel was disturbed")
        require(pid_signature(args.daemon_pid) == daemon_before, "daemon identity changed")
        runners_after = runner_snapshot()
        require(runners_after == runners_before, "CI runner identities changed")
        receipt["runnerSentinelsAfter"] = runners_after
        receipt["passed"] = True
        receipt["finishedAt"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        print("CDN Linux two-display cutover: PASS")
        return 0
    except Exception as error:
        receipt["error"] = f"{type(error).__name__}: {error}"
        receipt["finishedAt"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        print(f"CDN Linux two-display cutover: FAIL: {error}", file=sys.stderr)
        return 1
    finally:
        for index, display in enumerate(displays):
            cleanup_display(display, f"cutover-cleanup-{index}")
        if server is not None:
            server.shutdown()
            server.server_close()
        if server_thread is not None:
            server_thread.join(timeout=3)
        for process in (sentinel, foreign):
            if process is None or process.poll() is not None:
                continue
            process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        (artifact_dir / "cutover-receipt.json").write_text(
            json.dumps(receipt, indent=2, sort_keys=True) + "\n"
        )
        print(json.dumps(receipt, indent=2, sort_keys=True))


if __name__ == "__main__":
    raise SystemExit(main())
