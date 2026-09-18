#!/usr/bin/env python3
"""Hermetic default-off / developer-opt-in plugin packaging tests."""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "examples/chatgpt-plugin/configure_plugin.py"
SPEC = importlib.util.spec_from_file_location("configure_intendant_plugin", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
plugin = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(plugin)
APP_ID = "plugin_asdk_app_0123456789abcdef0123456789abcdef"


class PluginDogfoodTests(unittest.TestCase):
    def test_default_package_has_no_dogfood_payload_or_instruction(self) -> None:
        with TemporaryDirectory() as home:
            output = plugin.configure(APP_ID, Path(home) / "plugin")
            manifest = json.loads((output / ".codex-plugin/plugin.json").read_text())
            self.assertNotIn("skills", manifest)
            self.assertFalse((output / "skills").exists())
            self.assertEqual(json.loads((output / ".app.json").read_text()),
                             {"apps": {"intendant": {"id": APP_ID}}})

    def test_developer_package_copies_only_the_explicit_internal_skill(self) -> None:
        with TemporaryDirectory() as home:
            output = plugin.configure(APP_ID, Path(home) / "plugin", dev_dogfood=True)
            manifest = json.loads((output / ".codex-plugin/plugin.json").read_text())
            self.assertEqual(manifest["skills"], ["skills/skill-dogfood-feedback"])
            self.assertEqual(
                (output / "skills/skill-dogfood-feedback/SKILL.md").read_bytes(),
                (ROOT / "skills-internal/skill-dogfood-feedback/SKILL.md").read_bytes(),
            )
            self.assertFalse((plugin.TEMPLATE / "skills").exists())

    def test_cli_defaults_off_and_requires_the_flag(self) -> None:
        with TemporaryDirectory() as home:
            for dev in [False, True]:
                output = Path(home) / str(dev)
                argv = [sys.executable, str(SCRIPT), "--app-id", APP_ID, "--output", str(output)]
                if dev:
                    argv.append("--dev-dogfood")
                subprocess.run(argv, check=True, capture_output=True, text=True)
                manifest = json.loads((output / ".codex-plugin/plugin.json").read_text())
                self.assertEqual("skills" in manifest, dev)

    def test_missing_internal_skill_does_not_leave_partial_output(self) -> None:
        with TemporaryDirectory() as home:
            output = Path(home) / "plugin"
            with patch.object(plugin, "HERE", Path(home) / "examples/chatgpt-plugin"):
                with self.assertRaises(FileNotFoundError):
                    plugin.configure(APP_ID, output, dev_dogfood=True)
            self.assertFalse(output.exists())

    def test_existing_output_is_never_overwritten(self) -> None:
        with TemporaryDirectory() as home:
            output = Path(home) / "plugin"
            output.mkdir()
            marker = output / "user-owned"
            marker.write_text("keep")
            with self.assertRaises(FileExistsError):
                plugin.configure(APP_ID, output, dev_dogfood=True)
            self.assertEqual(marker.read_text(), "keep")

    def test_invalid_app_id_creates_nothing(self) -> None:
        with TemporaryDirectory() as home:
            output = Path(home) / "plugin"
            with self.assertRaises(ValueError):
                plugin.configure("not-an-app-id", output)
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
