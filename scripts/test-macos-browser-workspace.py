#!/usr/bin/env python3
"""Hermetic readiness tests: no daemon, GUI, input or real home access."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("workspace_proof", Path(__file__).with_name("verify-macos-browser-workspace.py"))
proof = importlib.util.module_from_spec(spec)
spec.loader.exec_module(proof)

class Readiness(unittest.TestCase):
    def run_case(self, replies):
        calls, pauses, evidence = [], [], []
        remaining = iter(replies)
        def tool(name, args):
            calls.append((name, args))
            return {}, next(remaining)
        result = proof.read_ready_controls(tool, "exact-binding", evidence, pauses.append)
        self.assertTrue(all(name == "read_macos_window_elements" and args == {"binding":"exact-binding"} for name, args in calls))
        self.assertLessEqual(len(calls), 3)
        return result, evidence, pauses

    def test_success_is_not_repeated(self):
        ok = {"ok":True, "controls":[]}
        self.assertEqual(self.run_case([ok]), (ok, [ok], []))

    def test_transient_read_requires_a_fresh_success_and_keeps_failure(self):
        fail = {"ok":False, "error":"AX kAXErrorCannotComplete (-25204)"}
        ok = {"ok":True, "controls":[]}
        self.assertEqual(self.run_case([fail, ok]), (ok, [fail, ok], [.2]))

    def test_exhaustion_is_not_reported_as_success(self):
        fail = {"ok":False, "error":"AX kAXErrorCannotComplete (-25204)"}
        self.assertEqual(self.run_case([fail]*3), (fail, [fail]*3, [.2, .2]))

    def test_other_refusals_are_not_retried(self):
        for error in ["protected content", "stale window binding", "permission denied"]:
            fail = {"ok":False, "error":error}
            self.assertEqual(self.run_case([fail]), (fail, [fail], []))

if __name__ == "__main__":
    unittest.main()
