#!/usr/bin/env python3
"""Hermetic browser-discovery coverage; no display or real browser is opened."""
import importlib.util
import os
from pathlib import Path
import sys
import tempfile
import unittest
spec = importlib.util.spec_from_file_location('external_proof_test', Path(__file__).with_name('test-external-cu-session.py'))
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

class BrowserDiscovery(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.binary = Path(self.temp.name) / 'browser'
        self.binary.write_text('#!/bin/sh\nexit 0\n')
        self.binary.chmod(0o700)
    def tearDown(self):
        self.temp.cleanup()
    def test_explicit_override(self):
        self.assertEqual(module.resolve_browser(self.binary, {}, lambda _: None), str(self.binary.resolve()))
    def test_environment_override(self):
        self.assertEqual(module.resolve_browser(None, {'INTENDANT_BROWSER_WORKSPACE_EXECUTABLE': str(self.binary)}, lambda _: None), str(self.binary.resolve()))
    def test_chrome_on_path_without_distro_path(self):
        calls = []
        def which(name):
            calls.append(name)
            return str(self.binary) if name == 'google-chrome' else None
        self.assertEqual(module.resolve_browser(None, {}, which), str(self.binary.resolve()))
        self.assertEqual(calls, ['google-chrome-stable', 'google-chrome'])
    def test_invalid_override_does_not_fallback(self):
        for path in ['relative', str(self.binary.parent), str(self.binary.parent / 'absent')]:
            with self.assertRaises(RuntimeError):
                module.resolve_browser(path, {}, lambda _: str(self.binary))
    def test_missing_or_non_executable_browser_refused(self):
        with self.assertRaises(RuntimeError):
            module.resolve_browser(None, {}, lambda _: None)
        self.binary.chmod(0o600)
        with self.assertRaises(RuntimeError):
            module.resolve_browser(self.binary, {}, lambda _: str(self.binary))

if __name__ == '__main__':
    unittest.main()
