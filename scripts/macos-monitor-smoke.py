#!/usr/bin/env python3
"""Opt-in native lifecycle/capture smoke; never builds or contacts a daemon."""

import argparse
import os
from pathlib import Path
import platform
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path,
                        help="already-built controller in your isolated worktree")
    parser.add_argument("--accept-shared-session-hotplug", action="store_true",
                        help="allow test monitor hotplug/removal and read-only capture; windows may move")
    args = parser.parse_args()
    if not args.accept_shared_session_hotplug:
        parser.error("native smoke requires --accept-shared-session-hotplug")
    if platform.system() != "Darwin":
        parser.error("native smoke requires a logged-in macOS session")
    binary = args.binary.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("--binary must be an executable controller")
    print(f"macOS {platform.mac_ver()[0]} / {platform.machine()}; controller: {binary}", flush=True)
    print("Requires existing Screen Recording permission; no automatic prompt. "
          "Creates only test-owned monitors and temporary PNGs.", flush=True)
    # No timeout abandons native cleanup; the private entry point joins its
    # broker/helper before exiting, including when the smoke fails.
    result = subprocess.run([str(binary), "--private-macos-monitor-smoke-v1",
                             "--accept-shared-session-hotplug"], check=False)
    print(f"native smoke exit status: {result.returncode}", flush=True)
    return result.returncode if result.returncode >= 0 else 128 - result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
