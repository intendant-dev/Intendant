#!/usr/bin/env python3
"""Materialize the Intendant plugin template for one registered MCP app."""

from __future__ import annotations

import argparse
import json
import re
import shutil
from pathlib import Path


APP_ID_PATTERN = re.compile(r"^plugin_asdk_app_[0-9a-f]{32}$")
HERE = Path(__file__).resolve().parent
TEMPLATE = HERE / "plugin-template"


def configure(app_id: str, output: Path, *, dev_dogfood: bool = False) -> Path:
    if not APP_ID_PATTERN.fullmatch(app_id):
        raise ValueError("app ID must match plugin_asdk_app_<32 lowercase hex characters>")
    if output.exists():
        raise FileExistsError(f"output already exists: {output}")

    # Developer teaching is never in the default template. Read before creating
    # output so an incomplete checkout cannot leave a seemingly valid package.
    dogfood_skill = None
    if dev_dogfood:
        dogfood_skill = (
            HERE.parent.parent / "skills-internal" / "skill-dogfood-feedback" / "SKILL.md"
        ).read_bytes()

    shutil.copytree(TEMPLATE, output)
    manifest_path = output / ".codex-plugin" / "plugin.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    plugin_name = manifest["name"]
    manifest["apps"] = "./.app.json"
    if dogfood_skill is not None:
        skill_dir = output / "skills" / "skill-dogfood-feedback"
        skill_dir.mkdir(parents=True)
        (skill_dir / "SKILL.md").write_bytes(dogfood_skill)
        manifest["skills"] = ["skills/skill-dogfood-feedback"]
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

    app_manifest = {"apps": {plugin_name: {"id": app_id}}}
    (output / ".app.json").write_text(
        json.dumps(app_manifest, indent=2) + "\n",
        encoding="utf-8",
    )
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app-id", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--dev-dogfood", action="store_true",
        help="developer opt-in: include the internal dogfood skill (daemon opt-in also required)",
    )
    args = parser.parse_args()

    try:
        output = configure(
            args.app_id, args.output.expanduser().resolve(), dev_dogfood=args.dev_dogfood
        )
    except (OSError, ValueError) as error:
        raise SystemExit(str(error)) from error
    print(f"Created plugin package at {output}")


if __name__ == "__main__":
    main()
