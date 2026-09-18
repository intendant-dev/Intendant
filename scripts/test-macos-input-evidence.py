#!/usr/bin/env python3
"""Hermetic tests: no GUI, input posting, display capture or user state access."""
import importlib.util
import os
import stat
import tempfile
from pathlib import Path
import sys
import unittest

from macos_input_evidence import assess_desktop, assess_pointer_receipts

spec = importlib.util.spec_from_file_location('raw_probe', Path(__file__).with_name('verify-macos-raw-pointer.py'))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class Tests(unittest.TestCase):
    sample = {'front_pid': 11, 'pointer_x': 10.5, 'pointer_y': -3.0, 'clipboard_change_count': 7}
    plan = {'pid': 99, 'source_pid': 99, 'window_id': 123, 'tag': 777, 'x': -700.5, 'y': 90.25}

    def receipts(self):
        return [dict(self.plan, kind=kind) for kind in ('left_down', 'left_up')]

    def test_owner_activity_is_unattributed_not_ignored(self):
        after = dict(self.sample, front_pid=12, pointer_x=42, clipboard_change_count=8)
        result = assess_desktop(self.sample, after, 99, False)
        self.assertEqual(result['observation_status'], 'sampled_change')
        self.assertTrue(all(result['sampled_changes'].values()))
        self.assertEqual(result['change_attribution'], 'undetermined')
        self.assertFalse(result['target_foreground_observed'])
        self.assertFalse(result['continuous_isolation_verified'])
        self.assertEqual(result['action_focus_checks'], 'reported_separately')

    def test_stable_samples_do_not_prove_continuous_isolation(self):
        result = assess_desktop(self.sample, self.sample, 99, False)
        self.assertEqual(result['observation_status'], 'no_sampled_change')
        self.assertFalse(result['continuous_isolation_verified'])

    def test_missing_malformed_and_boolean_samples_are_not_stability(self):
        for value in (None, {}, [], {'front_pid': True}, dict(self.sample, pointer_x=float('nan')),
                      dict(self.sample, clipboard_change_count=-1)):
            result = assess_desktop(value, value, 99, None)
            self.assertEqual(result['observation_status'], 'unavailable')
            self.assertIn(None, result['sampled_changes'].values())
            self.assertIsNone(result['target_foreground_observed'])

    def test_target_activation_is_not_excused_by_owner_activity(self):
        for before, after, tracked in ((self.sample, self.sample, True),
                                        (dict(self.sample, front_pid=99), self.sample, False),
                                        (self.sample, dict(self.sample, front_pid=99), False)):
            self.assertTrue(assess_desktop(before, after, 99, tracked)['target_foreground_observed'])
        self.assertTrue(assess_desktop(None, None, 99, True)['target_foreground_observed'])
        self.assertIsNone(assess_desktop(None, None, 99, False)['target_foreground_observed'])
        self.assertIsNone(assess_desktop({'front_pid': True}, None, 1, None)['target_foreground_observed'])

    def test_only_tagged_exact_pair_and_effect_verify(self):
        result = assess_pointer_receipts(self.plan, self.receipts(), 1)
        self.assertEqual(result['delivery'], 'verified')
        self.assertTrue(result['effect_verified'])
        for count in (0, 2):
            self.assertFalse(assess_pointer_receipts(self.plan, self.receipts(), count)['effect_verified'])

    def test_human_events_do_not_supply_our_positive_evidence(self):
        unrelated = [dict(item, tag=0) for item in self.receipts()]
        result = assess_pointer_receipts(self.plan, unrelated, 1)
        self.assertEqual(result['delivery'], 'not_observed')
        self.assertFalse(result['effect_verified'])
        result = assess_pointer_receipts(self.plan, unrelated + self.receipts(), 1)
        self.assertTrue(result['effect_verified'])
        self.assertEqual(result['unrelated_receipts'], 2)

    def test_partial_duplicate_reordered_pairs_do_not_verify(self):
        pair = self.receipts()
        for receipts in (pair[:1], pair[1:], pair[::-1], pair + pair, pair + pair[:1]):
            result = assess_pointer_receipts(self.plan, receipts, 1)
            self.assertFalse(result['effect_verified'])
            self.assertEqual(result['delivery'], 'partial_or_duplicate')

    def test_wrong_identity_coordinate_kind_and_types_refuse(self):
        for key, value in (('pid', 100), ('source_pid', 100), ('window_id', 124),
                           ('x', -702), ('y', float('inf')), ('kind', 'move'), ('pid', True)):
            receipts = self.receipts()
            receipts[0][key] = value
            self.assertEqual(assess_pointer_receipts(self.plan, receipts, 1)['delivery'], 'invalid')
        for key, value in (('tag', 0), ('tag', 2**63), ('pid', 0), ('window_id', 2**32), ('x', True)):
            self.assertEqual(assess_pointer_receipts(dict(self.plan, **{key: value}), [], 0)['delivery'], 'invalid')
        for receipts in ([None], self.receipts()*33, None):
            self.assertEqual(assess_pointer_receipts(self.plan, receipts, 1)['delivery'], 'invalid')

    def test_construction_cannot_be_promoted_to_delivery(self):
        native = {'ok': True, 'mode': 'construction_only', 'posted_events': 0,
                  'application_created': False, 'delivery_verified': False,
                  'effect_verified': False, 'native_cases': 13}
        result = probe.assess_native(native, False)
        self.assertTrue(result['passed'])
        self.assertEqual(result['delivery'], 'not_exercised')
        self.assertFalse(result['production_dispatch_enabled'])
        self.assertFalse(probe.assess_native(native, True, 99)['passed'])
        self.assertFalse(probe.assess_native(dict(native, posted_events=1), False)['passed'])
        self.assertFalse(probe.assess_native(dict(native, application_created=0), False)['passed'])

    def test_native_success_alone_and_foreground_cannot_pass(self):
        native = {'ok': True, 'mode': 'self_process_click', 'posted_events': 2,
                  'plan': self.plan, 'receipts': self.receipts(), 'tagged_click_count': 1,
                  'window_closed': True, 'receipt_overflow': False, 'target_ever_front': False,
                  'observations_complete': True,
                  'before': self.sample, 'after': dict(self.sample, pointer_y=400)}
        self.assertTrue(probe.assess_native(native, True, 99)['passed'])
        for key, value in (('receipts', []), ('tagged_click_count', 0), ('window_closed', False),
                           ('target_ever_front', True), ('after', None), ('posted_events', 0), ('observations_complete', False)):
            self.assertFalse(probe.assess_native(dict(native, **{key: value}), True, 99)['passed'])
        self.assertFalse(probe.assess_native(native, True, 99)['cross_process_verified'])
        self.assertFalse(probe.assess_native(native, True, 100)['passed'])
        self.assertFalse(probe.assess_native(native, True)['passed'])

    def test_bounded_runner_preserves_negative_native_result(self):
        code, data, stderr, pid = probe.run_probe([sys.executable, '-c',
            'import sys; print(\'{"ok":false}\'); sys.stderr.write("fixture refusal"); sys.exit(1)'])
        self.assertEqual(code, 1)
        self.assertEqual(data, {'ok': False})
        self.assertEqual(stderr, 'fixture refusal')
        self.assertGreater(pid, 0)

    def test_runner_limits_output_and_lifetime(self):
        for code, timeout in [('print("x"*20000)', 5),
                              ('import sys; sys.stderr.write("x"*5000)', 5),
                              ('import time; time.sleep(20)', 0.01)]:
            with self.assertRaises(RuntimeError):
                probe.run_probe([sys.executable, '-c', code], timeout)

    def test_report_reserved_private_and_never_overwritten(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / 'proof.json'
            fd = probe.reserve_report(path)
            try:
                self.assertEqual(stat.S_IMODE(os.fstat(fd).st_mode), 0o600)
            finally:
                os.close(fd)
            path.write_text('original evidence')
            with self.assertRaises(FileExistsError):
                probe.reserve_report(path)
            self.assertEqual(path.read_text(), 'original evidence')
            alias = Path(root) / 'alias.json'
            alias.symlink_to(path)
            with self.assertRaises(FileExistsError):
                probe.reserve_report(alias)

    def test_native_json_rejects_duplicates_and_nonfinite_values(self):
        for data in ('{"ok":true,"ok":false}', '{"x":NaN}', '{"x":Infinity}'):
            with self.assertRaises(ValueError):
                probe.strict_json(data)
        self.assertEqual(probe.strict_json('{"ok":false}'), {'ok': False})

    def test_huge_numbers_and_bad_pid_remain_unknown_or_invalid(self):
        result = assess_desktop(dict(self.sample, pointer_x=10**500), self.sample, 99)
        self.assertIsNone(result['sampled_changes']['pointer'])
        result = assess_pointer_receipts(dict(self.plan, pid=2**40), self.receipts(), 1)
        self.assertEqual(result['delivery'], 'invalid')

    def test_native_probe_has_only_self_process_posting(self):
        source = Path(__file__).resolve().parent.parent / 'tests/fixtures/macos-monitor/raw-pointer.m'
        text = source.read_text()
        self.assertEqual(text.count('CGEventPostToPid(getpid(),'), 2)
        for name in ('CGEventPost(', 'CGWarpMouseCursorPosition(', 'CGEventTapCreate(',
                     'CGEventCreateKeyboardEvent(', 'activateWithOptions:', 'makeKeyAndOrderFront:'):
            self.assertNotIn(name, text)
        self.assertIn('argc == 2 && strcmp(argv[1],"--allow-disposable-process-click") == 0', text)


if __name__ == '__main__':
    unittest.main()
