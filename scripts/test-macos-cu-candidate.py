#!/usr/bin/env python3
"""Hermetic packaging/provenance regressions. No GUI, privacy state, or native input."""
from __future__ import annotations
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from macos_candidate_manifest import BUNDLE_ID, SCHEMA, sha256, validate_manifest, source_metadata


def load_builder():
    spec = importlib.util.spec_from_file_location("candidate_builder", Path(__file__).with_name("build-macos-cu-candidate.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class CandidateTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.contents = Path(self.tmp.name) / "Intendant CU Candidate.app/Contents"
        (self.contents / "MacOS").mkdir(parents=True)
        (self.contents / "Resources/snapshot/scripts").mkdir(parents=True)
        self.binary = self.contents / "MacOS/intendant"
        self.binary.write_bytes(b"fixture controller, not executable")
        self.fixture = self.contents / "Resources/snapshot/scripts/fixture.py"
        self.fixture.write_text("# fixture\n")
        self.path = self.contents / "Resources/candidate-manifest.json"
        self.manifest = {"schema": SCHEMA, "bundle_id": BUNDLE_ID, "source_head": "a" * 40,
                         "source_tree": "b" * 40, "source_dirty": False,
                         "files": {str(p.relative_to(self.contents)): sha256(p) for p in (self.binary, self.fixture)}}
        self.save()

    def save(self):
        self.path.write_text(json.dumps(self.manifest))

    def test_pinned_payload_accepts_and_reports_exact_source_without_git(self):
        self.assertEqual(validate_manifest(self.path, self.binary)["source_head"], "a" * 40)
        metadata = source_metadata(self.binary, Path("/nonexistent/check-out"), self.path)
        self.assertTrue(metadata["candidate_payload_verified"])
        self.assertFalse(metadata["source_dirty"])

    def test_changed_controller_refuses(self):
        self.binary.write_bytes(b"replacement")
        with self.assertRaisesRegex(ValueError, "changed"):
            validate_manifest(self.path)

    def test_changed_fixture_refuses(self):
        self.fixture.write_text("# changed")
        with self.assertRaisesRegex(ValueError, "changed"):
            validate_manifest(self.path)

    def test_foreign_binary_is_not_relabelled_as_candidate(self):
        other = Path(self.tmp.name) / "other"
        other.write_bytes(self.binary.read_bytes())
        with self.assertRaisesRegex(ValueError, "belong"):
            validate_manifest(self.path, other)

    def test_installed_app_identity_cannot_be_used(self):
        self.manifest["bundle_id"] = "com.intendant.app"
        self.save()
        with self.assertRaisesRegex(ValueError, "separately"):
            validate_manifest(self.path)

    def test_dirty_source_is_never_presented_as_exact_commit(self):
        self.manifest["source_dirty"] = True
        self.save()
        with self.assertRaisesRegex(ValueError, "clean"):
            validate_manifest(self.path)

    def test_source_pins_are_not_branch_names(self):
        self.manifest["source_head"] = "main"
        self.save()
        with self.assertRaisesRegex(ValueError, "source identity"):
            validate_manifest(self.path)

    def test_path_traversal_and_absolute_paths_refuse(self):
        for name in ("../foreign", "/tmp/foreign", "MacOS//intendant"):
            with self.subTest(name=name):
                original = dict(self.manifest["files"])
                self.manifest["files"][name] = "a" * 64
                self.save()
                with self.assertRaises(ValueError):
                    validate_manifest(self.path)
                self.manifest["files"] = original

    def test_symlinked_payload_refuses_even_with_identical_hash(self):
        other = Path(self.tmp.name) / "other"
        other.write_bytes(self.fixture.read_bytes())
        self.fixture.unlink()
        self.fixture.symlink_to(other)
        with self.assertRaisesRegex(ValueError, "symlinks"):
            validate_manifest(self.path)

    def test_duplicate_manifest_keys_refuse(self):
        self.path.write_text('{"schema":"x", "schema":"y"}')
        with self.assertRaisesRegex(ValueError, "duplicate"):
            validate_manifest(self.path)

    def test_incomplete_manifest_refuses(self):
        del self.manifest["files"]["MacOS/intendant"]
        self.save()
        with self.assertRaisesRegex(ValueError, "hashes"):
            validate_manifest(self.path)

    def test_stale_or_dirty_binary_version_refuses(self):
        builder = load_builder()
        builder.check_version("intendant (commit aaaaaaaa, built fixture)", "a" * 40)
        for value in ("intendant (commit bbbbbbbb)", "intendant (commit aaaaaaaa-dirty)", "unknown"):
            with self.assertRaises(ValueError):
                builder.check_version(value, "a" * 40)

    def test_payload_list_includes_complete_fixture_and_manifest_support(self):
        root = Path(__file__).resolve().parent.parent
        files = load_builder().payload_sources(root)
        self.assertIn(root / "scripts/macos_candidate_manifest.py", files)
        self.assertIn(root / "tests/fixtures/macos-monitor/task-browser.html", files)
        self.assertNotIn(root / "scripts/macos_candidate_runner.py", files)
        self.assertTrue(all(path.is_file() for path in files))


if __name__ == "__main__":
    unittest.main(verbosity=2)
