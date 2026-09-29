"""User-started runner embedded in the separately authorized candidate app.

Not an authorization workaround: the candidate app and its native controller must
pass the normal macOS checks. No test or permission prompt runs at import time.
"""
from __future__ import annotations
import argparse
import json
import os
from pathlib import Path
import runpy
import signal
import subprocess
import sys


def run(output: Path | None, validate_only: bool = False) -> int:
    resources = Path(__file__).resolve().parent
    scripts = resources / "snapshot/scripts"
    sys.path.insert(0, str(scripts))
    from macos_candidate_manifest import validate_manifest, sha256
    manifest_path = resources / "candidate-manifest.json"
    contents = resources.parent
    # Validate the bundle signature as well as the exact controller/fixture hashes.
    subprocess.run(["/usr/bin/codesign", "--verify", "--deep", "--strict",
                    str(contents.parent)], check=True, capture_output=True)
    manifest = validate_manifest(manifest_path, contents / "MacOS/intendant")
    if validate_only:
        # Import every fixture dependency without calling main, inventory,
        # permission checks, browser launch, or any native operation.
        harness = scripts / "verify-macos-task-browser.py"
        namespace = runpy.run_path(str(harness), run_name="candidate_import_check")
        for index, name in enumerate(("verify-macos-managed-keyboard.py", "verify-macos-monitor-http.py",
                                       "verify-macos-chromium-controls.py", "verify-macos-browser-delegation.py")):
            namespace["load"]("candidate_dependency_" + str(index), name)
        print(json.dumps({"payload_verified": True, "fixture_imports_verified": True,
                          "source_head": manifest["source_head"], "native_operations": 0}))
        return 0
    if output is None:
        raise ValueError("the Run button must supply its fresh report directory")
    allowed = Path.home() / "Library/Application Support/Intendant CU Candidate/runs"
    output = output.resolve(strict=True)
    if output.parent != allowed.resolve(strict=True) or output.is_symlink():
        raise ValueError("reports must belong to the candidate application's private runs directory")
    report = output / "result.json"
    if report.exists():
        raise ValueError("never overwrite or repeat a completed test")
    browser = Path(manifest["browser_app"]).resolve(strict=True)
    browser_exe = browser / "Contents/MacOS/Google Chrome for Testing"
    if browser.name != "Google Chrome for Testing.app" or sha256(browser_exe) != manifest["browser_executable_sha256"]:
        raise ValueError("the pinned managed test-browser executable changed")
    checked = subprocess.run([str(contents / "MacOS/IntendantCUCandidate"), "--permission-check"],
                             check=True, capture_output=True, text=True, timeout=10)
    permissions = json.loads(checked.stdout)
    if permissions != {"accessibility": True, "screen_recording": True}:
        raise ValueError("the candidate app's Accessibility and Screen Recording permissions are not both approved")
    args = [str(scripts / "verify-macos-task-browser.py"),
            "--bin", str(contents / "MacOS/intendant"), "--browser-app", str(browser),
            "--report", str(report), "--candidate-manifest", str(manifest_path),
            "--allow-shared-session-monitor"]
    # This process is created only by the visible Run button. Its environment
    # carries no owner tokens, provider credentials, or Python injection hooks.
    def interrupt_once(signum, _frame):
        signal.alarm(0)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        if signum == signal.SIGALRM:
            raise TimeoutError("candidate test exceeded its ten-minute execution budget")
        raise KeyboardInterrupt("candidate test stopped by operator")
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGALRM):
        signal.signal(sig, interrupt_once)
    signal.alarm(600)
    sys.argv = args
    try:
        runpy.run_path(args[0], run_name="__main__")
    finally:
        signal.alarm(0)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--validate-only", action="store_true", help="Check sealed payload and imports; no GUI, permission, or native operations")
    args = parser.parse_args()
    os.umask(0o077)
    sys.dont_write_bytecode = True
    try:
        return run(args.output_dir, args.validate_only)
    except SystemExit as error:
        return int(error.code or 0)
    except KeyboardInterrupt:
        print("Stopped by operator; consult the harness report for cleanup status.")
        return 130
    except Exception as error:
        # No native operations have been retried. Only fixture/run diagnostics.
        print(type(error).__name__ + ": " + str(error))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
