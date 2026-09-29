#!/usr/bin/env python3
"""Package an exact, clean candidate in a separate app; never install over Intendant.

Build the named controller first with the repository's normal compile governor.
This tool does not launch anything, request privacy access, change keychains, or
use the installed app's identity. Ad-hoc signing is local-only: a rebuilt candidate
may need its own permission approval again. Do not replace an approved bundle.
"""
from __future__ import annotations
import argparse
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
from macos_candidate_manifest import SCHEMA, BUNDLE_ID, sha256, validate_manifest


def command(args, **kwargs):
    return subprocess.check_output([str(x) for x in args], text=True, **kwargs).strip()


def payload_sources(repo: Path) -> list[Path]:
    names = ["verify-macos-task-browser.py", "verify-macos-browser-delegation.py",
             "verify-macos-managed-keyboard.py", "verify-macos-monitor-http.py",
             "verify-macos-chromium-controls.py", "macos_candidate_manifest.py"]
    scripts = {repo / "scripts" / name for name in names}
    scripts.update((repo / "scripts").glob("macos_*.py"))
    scripts.discard(repo / "scripts/macos_candidate_runner.py")
    return sorted(scripts) + [repo / "tests/fixtures/macos-monitor/task-browser.html"]


def check_version(version: str, head: str) -> None:
    if "dirty" in version or not re.search(r"commit " + re.escape(head[:8]) + r"[, )]", version):
        raise ValueError("controller must be freshly rebuilt from this exact clean commit")


def package(repo: Path, binary: Path, browser: Path, output: Path) -> dict:
    if sys.platform != "darwin":
        raise ValueError("candidate packaging requires macOS")
    output = output.absolute()
    if output.name != "Intendant CU Candidate.app" or output.exists() or output.is_symlink():
        raise ValueError("choose a fresh Intendant CU Candidate.app path; existing apps are never overwritten")
    if command(["git", "status", "--porcelain", "--untracked-files=no"], cwd=repo):
        raise ValueError("commit candidate sources before packaging")
    head = command(["git", "rev-parse", "HEAD"], cwd=repo)
    tree = command(["git", "rev-parse", "HEAD^{tree}"], cwd=repo)
    binary = binary.resolve(strict=True)
    version = command([binary, "--version"])
    check_version(version, head)
    browser = browser.resolve(strict=True)
    browser_exe = browser / "Contents/MacOS/Google Chrome for Testing"
    if browser.name != "Google Chrome for Testing.app" or not browser_exe.is_file():
        raise ValueError("supply the existing managed Chrome for Testing bundle, not a personal browser")
    sources = payload_sources(repo)
    host = repo / "tests/fixtures/macos-monitor/candidate-app.m"
    runner = repo / "scripts/macos_candidate_runner.py"
    for source in [*sources, host, runner]:
        command(["git", "ls-files", "--error-unmatch", str(source.relative_to(repo))], cwd=repo)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.mkdir(mode=0o700)
    try:
        contents = output / "Contents"
        executables, resources, frameworks = (contents / "MacOS", contents / "Resources", contents / "Frameworks")
        for path in (executables, resources, frameworks):
            path.mkdir(parents=True, exist_ok=True)
        controller = executables / "intendant"
        shutil.copy2(binary, controller)
        for source in sources:
            target = resources / "snapshot" / source.relative_to(repo)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
        shutil.copyfile(runner, resources / "candidate_runner.py")
        command(["xcrun", "clang", "-fobjc-arc", "-O2", "-Wall", "-Wextra", "-Werror",
                 "-mmacosx-version-min=13.0", host, "-framework", "AppKit",
                 "-framework", "ApplicationServices", "-framework", "CoreGraphics",
                 "-framework", "Security", "-o", executables / "IntendantCUCandidate"])
        # Only the known libvpx dependency is relocated. Reject unexpected local
        # dependencies instead of quietly relying on another app's libraries.
        dependencies = command(["/usr/bin/otool", "-L", controller]).splitlines()[1:]
        for row in dependencies:
            dependency = row.strip().split(" (", 1)[0]
            if dependency.startswith(("/System/", "/usr/lib/", "@rpath/libswift")):
                continue
            lib = Path(dependency)
            if not re.fullmatch(r"libvpx\.\d+\.dylib", lib.name) or not lib.is_file():
                raise ValueError("unpackaged controller dependency: " + dependency)
            target = frameworks / lib.name
            shutil.copy2(lib, target)
            for child in command(["/usr/bin/otool", "-L", target]).splitlines()[2:]:
                if not child.strip().startswith(("/System/", "/usr/lib/")):
                    raise ValueError("unexpected indirect libvpx dependency")
            command(["/usr/bin/install_name_tool", "-id", "@rpath/" + lib.name, target])
            command(["/usr/bin/install_name_tool", "-change", dependency,
                     "@executable_path/../Frameworks/" + lib.name, controller])
            command(["/usr/bin/codesign", "--force", "--sign", "-", "--identifier", BUNDLE_ID + ".libvpx", target])
        command(["/usr/bin/codesign", "--force", "--sign", "-", "--identifier", BUNDLE_ID + ".controller", controller])
        info = {"CFBundleIdentifier": BUNDLE_ID, "CFBundleName": "Intendant CU Candidate",
                "CFBundleDisplayName": "Intendant CU Candidate", "CFBundleExecutable": "IntendantCUCandidate",
                "CFBundlePackageType": "APPL", "CFBundleShortVersionString": "1.0",
                "CFBundleVersion": "1", "NSPrincipalClass": "NSApplication",
                "LSMinimumSystemVersion": "13.0", "LSMultipleInstancesProhibited": True,
                "NSHighResolutionCapable": True}
        (contents / "Info.plist").write_bytes(plistlib.dumps(info))
        # The outer signature seals the launcher and this manifest; listing the
        # outer signed launcher inside its own manifest would be circular.
        files = {str(p.relative_to(contents)): sha256(p) for p in sorted(contents.rglob("*"))
                 if p.is_file() and p != executables / "IntendantCUCandidate"}
        manifest = {"schema": SCHEMA, "bundle_id": BUNDLE_ID, "source_head": head,
                    "source_tree": tree, "source_dirty": False, "controller_version": version,
                    "source_binary_sha256": sha256(binary), "browser_app": str(browser),
                    "browser_executable_sha256": sha256(browser_exe), "files": files,
                    "native_acceptance": "not_run", "signing": "ad_hoc_local_only"}
        manifest_path = resources / "candidate-manifest.json"
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
        command(["/usr/bin/codesign", "--force", "--sign", "-", "--identifier", BUNDLE_ID, output])
        command(["/usr/bin/codesign", "--verify", "--deep", "--strict", output])
        validate_manifest(manifest_path, controller)
        check_version(command([controller, "--version"]), head)
        if command(["git", "rev-parse", "HEAD"], cwd=repo) != head or command(
                ["git", "status", "--porcelain", "--untracked-files=no"], cwd=repo):
            raise ValueError("source changed during packaging")
        return {"app": str(output), "source_head": head, "bundle_id": BUNDLE_ID,
                "controller_sha256": files["MacOS/intendant"], "native_acceptance": "not_run",
                "permissions_requested": False, "installed_intendant_changed": False}
    except BaseException:
        shutil.rmtree(output)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--browser-app", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    result = package(Path(__file__).resolve().parent.parent, args.binary, args.browser_app, args.output)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
