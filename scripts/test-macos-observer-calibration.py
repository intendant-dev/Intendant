#!/usr/bin/env python3
"""Hermetic calibration tests. Synthetic JSON and owned child pipes only."""
import copy
import importlib.util
import json
from pathlib import Path
import sys
import unittest
import macos_observer_calibration as model

spec = importlib.util.spec_from_file_location('runner', Path(__file__).with_name('verify-macos-observer-calibration.py'))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)
ACCESS = dict(listen_event_access=True, post_event_access=True, accessibility_trusted=True,
              secure_event_input=False, console_user_matches=True)
ZERO = {k: 0 for k in model.ZERO_EFFECTS}
UTC = '2026-09-26T12:00:00Z'


def series():
    rows = [dict(kind='ready', schema=1, profile='input_observer_calibration', pid=42,
                 seconds=1, period_ms=100, utc=UTC, os='synthetic', access=copy.deepcopy(ACCESS), **ZERO),
            dict(kind='started', utc=UTC, started_uptime=100.0)]
    for i in range(11):
        rows.append(dict(kind='sample', sequence=i, started_uptime=100+i/10,
                         finished_uptime=100.001+i/10,
                         hid_system={k: 0 for k in model.COUNTERS},
                         combined_session={k: 0 for k in model.COUNTERS}, access=copy.deepcopy(ACCESS)))
    rows.append(dict(kind='finished', completed=True, samples=11, reason='finished',
                     utc=UTC, finished_uptime=101.01, **ZERO))
    return rows


def progress(rows, sources=('hid_system', 'combined_session')):
    for row in rows:
        if row['kind'] == 'sample':
            for source in sources:
                row[source]['key_down'] = 50+row['sequence']
                row[source]['key_up'] = 40+row['sequence']
    return rows


class Tests(unittest.TestCase):
    def test_zero_counts_are_not_user_inactivity(self):
        result = model.summarize(series())
        self.assertEqual(result['outcome'], 'no_keyboard_progress_observed')
        self.assertFalse(result['user_inactivity_established'])
        self.assertIsNone(result['user_typing_confirmed'])
        self.assertFalse(result['input_delivery_verified'])

    def test_both_tables_progress_without_authenticating_source(self):
        result = model.summarize(progress(series()))
        self.assertEqual(result['outcome'], 'keyboard_progress_both')
        self.assertEqual(result['sources']['hid_system']['deltas']['key_down'], 10)
        self.assertEqual(result['human_activity_attribution'], 'not_authenticated')

    def test_tables_disagree_without_silent_fallback(self):
        for source, suffix in [('hid_system', 'hid_only'), ('combined_session', 'session_only')]:
            result = model.summarize(progress(series(), (source,)))
            self.assertEqual(result['outcome'], 'keyboard_progress_'+suffix)

    def test_one_sided_key_counts_not_completed_pair_evidence(self):
        rows = series(); rows[-2]['hid_system']['key_down'] = 1
        result = model.summarize(rows)
        self.assertEqual(result['outcome'], 'no_keyboard_progress_observed')

    def test_mouse_progress_not_keyboard(self):
        rows = series(); rows[-2]['hid_system']['mouse_move'] = 100
        self.assertEqual(model.summarize(rows)['outcome'], 'no_keyboard_progress_observed')

    def test_mid_interval_regression_invalidates_delta(self):
        rows = progress(series()); rows[7]['hid_system']['key_up'] = 0
        result = model.summarize(rows)
        self.assertEqual(result['outcome'], 'counter_regression')
        self.assertIsNone(result['sources']['hid_system']['deltas'])

    def test_context_not_assumed_from_another_process(self):
        for key, value in [('listen_event_access', False), ('secure_event_input', True),
                           ('console_user_matches', False), ('secure_event_input', None),
                           ('console_user_matches', None)]:
            rows = progress(series()); rows[4]['access'][key] = value
            result = model.summarize(rows)
            self.assertFalse(result['access_context_consistent'])
            self.assertTrue(result['access_context_changed'])
            self.assertEqual(result['outcome'], 'keyboard_progress_both')

    def test_bad_counter_types_and_extra_fields(self):
        for bad in (True, -1, 2**32, 1.2, None):
            rows = series(); rows[2]['hid_system']['key_down'] = bad
            with self.assertRaises(ValueError): model.summarize(rows)
        rows = series(); rows[2]['key_name'] = 'must-not-collect'
        with self.assertRaises(ValueError): model.summarize(rows)

    def test_invalid_sequence_and_clock(self):
        for field, bad in [('sequence', 7), ('started_uptime', 0), ('finished_uptime', float('nan')),
                           ('sequence', True)]:
            rows = series(); rows[4][field] = bad
            with self.assertRaises(ValueError): model.summarize(rows)

    def test_false_complete_and_duplicate_records(self):
        for rows in (series()[:-3]+series()[-1:], series()[:4]+series()[1:]):
            with self.assertRaises(ValueError): model.summarize(rows)
        rows = series(); rows[-1]['samples'] = 10
        with self.assertRaises(ValueError): model.summarize(rows)

    def test_tight_loop_cannot_fake_duration(self):
        rows = series()
        for row in rows[2:-1]:
            row['started_uptime'] = 100.0; row['finished_uptime'] = 100.0
        with self.assertRaises(ValueError): model.summarize(rows)

    def test_no_start_and_partial_records_remain_incomplete(self):
        ready = series()[0]
        self.assertFalse(model.summarize([ready])['completed'])
        finish = series()[-1]; finish.update(completed=False, samples=0, reason='cancelled_before_start')
        self.assertEqual(model.summarize([ready, finish])['outcome'], 'incomplete')
        result = model.summarize(series()[:-3])
        self.assertFalse(result['completed'])

    def test_access_flags_reject_numbers_instead_of_json_booleans(self):
        for field in model.ACCESS:
            for bad in (0, 1, "true", [], {}):
                rows = series(); rows[0]['access'][field] = bad
                with self.assertRaises(ValueError): model.summarize(rows)

    def test_finish_reason_type_is_a_validated_error(self):
        for bad in ([], {}, 0, None):
            rows = series(); rows[-1]['reason'] = bad
            with self.assertRaises(ValueError): model.summarize(rows)

    def test_native_effect_or_claim_contradiction_refused(self):
        for key in model.ZERO_EFFECTS:
            rows = series(); rows[0][key] = 1
            with self.assertRaises(ValueError): model.summarize(rows)
        rows = series(); rows[-1]['reason'] = 'cancelled'
        with self.assertRaises(ValueError): model.summarize(rows)

    def test_verified_copy_survives_source_replacement_and_cleans_up(self):
        import hashlib, os, stat, subprocess, tempfile
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'source'
            image = b'#!/bin/sh\nprintf verified-original'
            source.write_bytes(image); source.chmod(0o700)
            digest = hashlib.sha256(image).hexdigest()
            with runner.verified_supervisor(source, digest) as executable:
                self.assertNotEqual(executable, source)
                self.assertEqual(stat.S_IMODE(executable.parent.stat().st_mode), 0o700)
                self.assertEqual(stat.S_IMODE(executable.stat().st_mode), 0o500)
                replacement = Path(directory) / 'replacement'
                replacement.write_bytes(b'#!/bin/sh\nprintf unverified-replacement')
                replacement.chmod(0o700); os.replace(replacement, source)
                self.assertEqual(executable.read_bytes(), image)
                if os.name == 'posix':
                    result = subprocess.run([str(executable)], check=True, capture_output=True, timeout=3)
                    self.assertEqual(result.stdout, b'verified-original')
            self.assertFalse(executable.exists())
            self.assertFalse(executable.parent.exists())
            self.assertIn(b'unverified-replacement', source.read_bytes())

    def test_verified_copy_survives_in_place_source_rebuild(self):
        import hashlib, tempfile
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'source'
            image = b'original bytes'; source.write_bytes(image); source.chmod(0o700)
            with runner.verified_supervisor(source, hashlib.sha256(image).hexdigest()) as executable:
                source.write_bytes(b'different rebuilt bytes')
                self.assertEqual(executable.read_bytes(), image)

    def test_verified_copy_is_removed_on_collection_exception(self):
        import hashlib, tempfile
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'source'
            image = b'original bytes'; source.write_bytes(image); source.chmod(0o700)
            with self.assertRaisesRegex(RuntimeError, 'collection failed'):
                with runner.verified_supervisor(source, hashlib.sha256(image).hexdigest()) as executable:
                    raise RuntimeError('collection failed')
            self.assertFalse(executable.parent.exists())
            self.assertTrue(source.exists())

    def test_bad_hash_and_non_executable_never_yield(self):
        import hashlib, tempfile
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'source'; source.write_bytes(b'bytes'); source.chmod(0o700)
            for digest in ('0' * 64, 'A' * 64, 'not-a-digest'):
                with self.assertRaises(ValueError):
                    with runner.verified_supervisor(source, digest):
                        self.fail('invalid provenance admitted')
            source.chmod(0o600)
            with self.assertRaisesRegex(ValueError, 'regular executable'):
                with runner.verified_supervisor(source, hashlib.sha256(b'bytes').hexdigest()):
                    self.fail('non-executable admitted')
            with self.assertRaisesRegex(ValueError, 'regular executable'):
                with runner.verified_supervisor(Path(directory), '0' * 64):
                    self.fail('directory admitted')

    def test_supervisor_size_is_bounded_before_copy(self):
        import hashlib, tempfile
        from unittest import mock
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'source'; source.write_bytes(b'ninebytes'); source.chmod(0o700)
            with mock.patch.object(runner, 'MAX_SUPERVISOR_BYTES', 8):
                with self.assertRaisesRegex(ValueError, 'size limit'):
                    with runner.verified_supervisor(source, hashlib.sha256(b'ninebytes').hexdigest()):
                        self.fail('oversized executable admitted')

    def test_main_passes_private_copy_and_records_execution_provenance(self):
        import contextlib, hashlib, io, tempfile
        from unittest import mock
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'source'; image = b'verified bytes'
            source.write_bytes(image); source.chmod(0o700)
            report = Path(directory) / 'report.json'; launched = []
            def collect(command, seconds, checkpoint):
                executable = Path(command[0]); launched.append(executable)
                self.assertNotEqual(executable, source)
                source.write_bytes(b'rebuilt')
                self.assertEqual(executable.read_bytes(), image)
                self.assertEqual(command[1:], ['--calibrate-input-observer', '1'])
                self.assertEqual(seconds, 1)
                value = {'measurement_valid': True, 'completed': True, 'observer_reaped': True}
                checkpoint(value)
                return value
            argv = ['verify', '--supervisor', str(source), '--sha256', hashlib.sha256(image).hexdigest(),
                    '--seconds', '1', '--report', str(report), '--allow-readonly-input-observation']
            with mock.patch.object(sys, 'argv', argv), mock.patch.object(sys, 'platform', 'darwin'), \
                 mock.patch.object(runner, 'collect', side_effect=collect), contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(runner.main(), 0)
            self.assertEqual(json.loads(report.read_text())['supervisor_execution'], 'private_verified_copy')
            self.assertEqual(json.loads(report.read_text())['supervisor_sha256'], hashlib.sha256(image).hexdigest())
            self.assertFalse(launched[0].exists())

    def test_existing_report_is_not_replaced_by_verified_copy_run(self):
        import hashlib, tempfile
        from unittest import mock
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'source'; source.write_bytes(b'verified'); source.chmod(0o700)
            report = Path(directory) / 'report.json'; report.write_text('previous evidence')
            argv = ['verify', '--supervisor', str(source), '--sha256', hashlib.sha256(b'verified').hexdigest(),
                    '--report', str(report), '--allow-readonly-input-observation']
            with mock.patch.object(sys, 'argv', argv), mock.patch.object(sys, 'platform', 'darwin'), \
                 mock.patch.object(runner, 'collect') as collect:
                with self.assertRaises(FileExistsError): runner.main()
                collect.assert_not_called()
            self.assertEqual(report.read_text(), 'previous evidence')

    def test_oversized_uptime_integers_are_normal_validation_errors(self):
        for index, field in ((1, 'started_uptime'), (2, 'started_uptime'),
                             (2, 'finished_uptime'), (-1, 'finished_uptime')):
            for value in (10**400, -(10**400)):
                rows = series(); rows[index][field] = value
                with self.assertRaises(ValueError): model.summarize(rows)
        self.assertFalse(model.finite(10**400))
        self.assertFalse(model.finite(-(10**400)))
        self.assertTrue(model.finite(123))

    def test_oversized_child_timestamp_preserves_report_and_reaps(self):
        ready = series()[0]
        started = series()[1]; started['started_uptime'] = 10**400
        script = ('import os,sys,json\nr='+repr(ready)+'\nr["pid"]=os.getpid()\n'
                  'print(json.dumps(r),flush=True)\nassert sys.stdin.read(1)=="s"\n'
                  'print(json.dumps('+repr(started)+'),flush=True)\n')
        checkpoints = []
        result = runner.collect([sys.executable, '-u', '-c', script], 1,
                                lambda r: checkpoints.append(copy.deepcopy(r)), timeout=3)
        self.assertFalse(result['measurement_valid'])
        self.assertFalse(result['completed'])
        self.assertTrue(result['observer_reaped'])
        self.assertEqual(result['error'], 'start schema')
        self.assertEqual(len(result['records']), 1)
        self.assertEqual(checkpoints[-1], result)

    def test_child_ready_handshake_and_completion(self):
        checkpoints = []
        script = ('import os,sys,json\nr='+repr(series())+'\nr[0]["pid"]=os.getpid()\n'
                  'print(json.dumps(r[0]),flush=True)\nassert sys.stdin.read(1)=="s"\n'
                  'for row in r[1:]: print(json.dumps(row),flush=True)\n')
        result = runner.collect([sys.executable, '-u', '-c', script], 1,
                                lambda r: checkpoints.append(copy.deepcopy(r)), timeout=3)
        self.assertTrue(result['measurement_valid'], result)
        self.assertTrue(result['observer_reaped'])
        self.assertTrue(any(len(r['records']) == 1 and not r['completed'] for r in checkpoints))

    def test_transport_eof_retains_partial_and_reaps(self):
        script = 'import os,json;r='+repr(series()[0])+';r["pid"]=os.getpid();print(json.dumps(r),flush=True)'
        result = runner.collect([sys.executable, '-u', '-c', script], 1, lambda _: None, timeout=2)
        self.assertFalse(result['measurement_valid'])
        self.assertTrue(result['observer_reaped'])
        self.assertEqual(len(result['records']), 1)

    def test_timeout_reaps_child_and_preserves_ready(self):
        script = 'import os,json,time;r='+repr(series()[0])+';r["pid"]=os.getpid();print(json.dumps(r),flush=True);time.sleep(10)'
        result = runner.collect([sys.executable, '-u', '-c', script], 1, lambda _: None, timeout=.2)
        self.assertFalse(result['measurement_valid'])
        self.assertTrue(result['observer_reaped'])
        self.assertIn('deadline', result['error'])

    def test_oversized_line_fails_bounded(self):
        result = runner.collect([sys.executable, '-u', '-c', 'print("x"*17000)'], 1, lambda _: None, timeout=2)
        self.assertFalse(result['measurement_valid'])
        self.assertTrue(result['observer_reaped'])
        self.assertEqual(result['records'], [])

    def test_identity_mismatch_prevents_start(self):
        script = 'import json;print(json.dumps('+repr(series()[0])+'),flush=True)'
        result = runner.collect([sys.executable, '-u', '-c', script], 1, lambda _: None, timeout=2)
        self.assertIn('identity', result['error'])
        self.assertEqual(result['records'], [])


if __name__ == '__main__':
    unittest.main(verbosity=2)
