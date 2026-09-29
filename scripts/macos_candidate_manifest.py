"""Integrity/provenance checks for a separately authorized CU test bundle.

No GUI calls or permission requests. Tests inject temporary bundles.
"""
from __future__ import annotations
import hashlib
import json
import re
from pathlib import Path, PurePosixPath

SCHEMA = "intendant-cu-candidate-v1"
BUNDLE_ID = "dev.intendant.cu-candidate"
CONTROLLER = "MacOS/intendant"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate candidate manifest field")
        result[key] = value
    return result


def validate_manifest(path: Path, binary: Path | None = None) -> dict:
    path = path.resolve(strict=True)
    if path.name != "candidate-manifest.json" or path.parent.name != "Resources":
        raise ValueError("expected the bundled candidate manifest")
    if path.stat().st_size > 131072:
        raise ValueError("candidate manifest exceeds budget")
    manifest = json.loads(path.read_text(), object_pairs_hook=unique_object)
    if manifest.get("schema") != SCHEMA or manifest.get("bundle_id") != BUNDLE_ID:
        raise ValueError("not a separately identified CU candidate")
    for field in ("source_head", "source_tree"):
        if not re.fullmatch(r"[0-9a-f]{40}", manifest.get(field, "")):
            raise ValueError("missing exact candidate source identity")
    if manifest.get("source_dirty") is not False:
        raise ValueError("candidate source must be clean and pinned")
    root = path.parent.parent
    if root.name != "Contents" or root.parent.suffix != ".app":
        raise ValueError("candidate manifest must belong to an app bundle")
    files = manifest.get("files")
    if not isinstance(files, dict) or not 2 <= len(files) <= 128 or CONTROLLER not in files:
        raise ValueError("missing candidate payload hashes")
    for name, expected in files.items():
        relative = PurePosixPath(name)
        if (relative.is_absolute() or ".." in relative.parts or
                relative.as_posix() != name or not relative.parts):
            raise ValueError("candidate payload path is not canonical and relative")
        if not isinstance(expected, str) or not re.fullmatch(r"[0-9a-f]{64}", expected):
            raise ValueError("invalid candidate payload hash")
        item = root
        for component in relative.parts:
            item = item / component
            if item.is_symlink():
                raise ValueError("candidate payload may not use symlinks")
        if not item.is_file() or sha256(item) != expected:
            raise ValueError("candidate payload changed: " + name)
    controller = root / CONTROLLER
    if binary is not None and binary.resolve(strict=True) != controller:
        raise ValueError("candidate binary does not belong to this manifest")
    return manifest


def source_metadata(binary: Path, repo: Path, manifest_path: Path | None = None) -> dict:
    if manifest_path is not None:
        value = validate_manifest(manifest_path, binary)
        return {"source_head": value["source_head"], "source_tree": value["source_tree"],
                "source_dirty": False, "candidate_bundle_id": value["bundle_id"],
                "candidate_payload_verified": True}
    import subprocess
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
    tree = subprocess.check_output(["git", "rev-parse", "HEAD^{tree}"], cwd=repo, text=True).strip()
    result = subprocess.run(["git", "diff", "--quiet", "HEAD"], cwd=repo, check=False)
    if result.returncode not in (0, 1):
        raise ValueError("cannot establish source-tree state")
    return {"source_head": head, "source_tree": tree, "source_dirty": result.returncode == 1,
            "candidate_payload_verified": False}
