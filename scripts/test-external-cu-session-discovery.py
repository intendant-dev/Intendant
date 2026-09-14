#!/usr/bin/env python3
"""Hermetic browser-discovery and runner-guard coverage; no display is opened."""
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

class RunnerGuard(unittest.TestCase):
    def setUp(self):
        self.listeners = [self.runner('Runner.Listener', 10),
                          self.runner('Runner.Listener', 20)]
        self.own_worker = self.runner('Runner.Worker', 11)
        self.sibling_worker = self.runner('Runner.Worker', 21)
        self.before = self.listeners + [self.own_worker, self.sibling_worker]
        self.ancestors = {1, 10, 11, 12}

    @staticmethod
    def runner(comm, pid, start='100'):
        return {'comm': comm, 'pid': pid, 'startTimeTicks': start,
                'exe': '/fixture/' + comm}

    def test_sibling_jobs_may_finish_start_or_reuse_a_pid(self):
        expected = module.protected_runner_snapshot(self.before, self.ancestors)
        for workers in [[], [self.runner('Runner.Worker', 22)],
                        [self.runner('Runner.Worker', 21, start='200')],
                        [self.sibling_worker, self.runner('Runner.Worker', 22)]]:
            with self.subTest(workers=workers):
                after = self.listeners + [self.own_worker] + workers
                self.assertEqual(module.protected_runner_snapshot(after, self.ancestors), expected)

    def test_listener_loss_addition_or_replacement_is_detected(self):
        expected = module.protected_runner_snapshot(self.before, self.ancestors)
        for listeners in [self.listeners[:1],
                          self.listeners + [self.runner('Runner.Listener', 30)],
                          [self.listeners[0], self.runner('Runner.Listener', 20, start='200')]]:
            with self.subTest(listeners=listeners):
                after = listeners + [self.own_worker, self.sibling_worker]
                self.assertNotEqual(module.protected_runner_snapshot(after, self.ancestors), expected)

    def test_own_worker_pid_reuse_is_detected(self):
        after = self.listeners + [self.runner('Runner.Worker', 11, start='200'), self.sibling_worker]
        self.assertNotEqual(module.protected_runner_snapshot(after, self.ancestors),
                            module.protected_runner_snapshot(self.before, self.ancestors))

    def test_missing_own_worker_fails_closed(self):
        for snapshot in [[], self.listeners, self.listeners + [self.sibling_worker]]:
            with self.subTest(snapshot=snapshot):
                with self.assertRaisesRegex(RuntimeError, 'owning CI worker missing'):
                    module.protected_runner_snapshot(snapshot, self.ancestors)

    def test_standalone_guard_retains_all_workers(self):
        before = module.protected_runner_snapshot(self.before, None)
        self.assertEqual(before, self.before)
        self.assertNotEqual(module.protected_runner_snapshot(self.listeners + [self.own_worker], None), before)
        self.assertNotEqual(module.protected_runner_snapshot(self.before + [self.runner('Runner.Worker', 22)], None), before)

    def test_ancestry_parses_parentheses_in_process_names(self):
        with tempfile.TemporaryDirectory() as root:
            proc = Path(root)
            for pid, parent, comm in [(12, 11, 'test ) process'), (11, 10, 'Runner.Worker'),
                                      (10, 1, 'Runner.Listener'), (1, 0, 'init')]:
                (proc / str(pid)).mkdir()
                (proc / str(pid) / 'stat').write_text(f'{pid} ({comm}) S {parent} 0 0\n')
            self.assertEqual(module.process_ancestors(12, proc), self.ancestors)

    def test_unreadable_or_invalid_ancestry_fails_closed(self):
        with tempfile.TemporaryDirectory() as root:
            proc = Path(root)
            with self.assertRaises(FileNotFoundError):
                module.process_ancestors(12, proc)
            (proc / '12').mkdir()
            stat = proc / '12' / 'stat'
            for contents in ['12 (cycle) S 12', 'malformed', '12 (short) S', '12 (invalid) S -1']:
                with self.subTest(contents=contents):
                    stat.write_text(contents)
                    with self.assertRaises(RuntimeError):
                        module.process_ancestors(12, proc)


if __name__ == '__main__':
    unittest.main()
