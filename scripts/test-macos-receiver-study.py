#!/usr/bin/env python3
"""Hermetic receiver-study tests: no native APIs, processes or real files."""
import copy
import importlib.util
from pathlib import Path
import unittest
import macos_receiver_study as study

class Rig:
    def __init__(self):
        self.active = 'first'
        self.reads = 0
        self.selections = []
        self.elapsed = 0.0
        self.mutate_reply = None
        self.mutate_after = None
        self.fail_read = False
        self.snapshots = []
        self.report = {}
        self.window = dict(x=0, y=0, width=500, height=400)

    def state(self):
        s = dict(active=self.active, rect=dict(x=10 if self.active == 'first' else 100, y=20, width=80, height=24),
                 metrics=dict(screen_x=0,screen_y=0,outer_width=500,outer_height=400,inner_width=500,inner_height=300,scale=1,zoom=1,scroll_x=0,scroll_y=0), click_events=[{'fixture_setup': True}]*3,
                 click_overflow=False, key_event_count=0, key_overflow=False)
        if self.mutate_after and self.reads:
            self.mutate_after(s)
        return s

    def select(self, name):
        self.active = name
        self.selections.append(name)
        return self.state()

    def read(self):
        self.reads += 1
        self.elapsed += .005
        if self.fail_read:
            raise OSError('transport failed')
        if self.active == 'protected':
            r = dict(ok=False, error='focused keyboard receiver is protected')
        else:
            r = dict(ok=True, keyboard_target=dict(role='AXTextField', bounds=self.state()['rect'],
                                                  enabled=True, keyboard_dispatch_supported=False))
        if self.mutate_reply:
            self.mutate_reply(r)
        return r

    def validate_geometry(self, target, state, window):
        study.require(target['bounds'] == state['rect'] and window == self.window, 'wrong geometry')

    def run(self, deadline=10):
        study.collect(self.read, self.select, self.state, lambda: {'synthetic_observation': self.reads},
                      self.validate_geometry, self.window, self.report,
                      lambda: self.snapshots.append(copy.deepcopy(self.report)), deadline, lambda: self.elapsed)
        return self.report

class StudyTests(unittest.TestCase):
    def test_fixed_count_and_phase_order_no_actions(self):
        r = Rig(); report = r.run()
        self.assertEqual(r.reads, 12)
        self.assertEqual(r.selections, ['first', 'second', 'protected', 'first'])
        self.assertTrue(report['completed'])
        self.assertTrue(report['measurement_valid'])
        self.assertEqual(report['summary']['accepted_nonprotected'], 10)
        self.assertEqual(report['summary']['refused_protected'], 2)
        self.assertFalse(report['keyboard_input_requested'])
        self.assertFalse(report['input_delivery_verified'])
        self.assertFalse(report['summary']['continuous_isolation_verified'])

    def test_refusals_are_recorded_not_retried_until_success(self):
        r = Rig()
        r.mutate_reply = lambda v: (v.clear(), v.update(ok=False, error='human global focused object changed during keyboard receiver observation'))
        report = r.run()
        self.assertEqual(r.reads, 12)
        self.assertTrue(report['completed'])
        self.assertFalse(report['summary']['all_nonprotected_reads_succeeded'])
        self.assertEqual(report['summary']['accepted_nonprotected'], 0)
        self.assertEqual(report['summary']['categories'], {'human_focus_changed': 12})

    def test_transport_failure_preserves_attempt_and_aborts(self):
        r = Rig(); r.fail_read = True
        with self.assertRaisesRegex(OSError, 'transport'):
            r.run()
        self.assertEqual(r.reads, 1)
        self.assertFalse(r.report['completed'])
        self.assertTrue(r.report['samples'][0]['read_attempted'])
        self.assertIn('client_elapsed_us', r.report['samples'][0])
        self.assertEqual(r.snapshots[-1], r.report)

    def test_expired_budget_records_unattempted_slot(self):
        r = Rig()
        with self.assertRaisesRegex(RuntimeError, 'deadline'):
            r.run(deadline=0)
        self.assertEqual(r.reads, 0)
        self.assertEqual(r.report['summary']['attempted_reads'], 0)
        self.assertIsNone(r.report['summary']['client_latency_us'])

    def test_post_read_budget_expiry_not_complete(self):
        r = Rig()
        with self.assertRaisesRegex(RuntimeError, 'deadline'):
            r.run(deadline=.001)
        self.assertEqual(r.reads, 1)
        self.assertFalse(r.report['measurement_valid'])

    def test_malformed_boolean_and_extra_fields_abort(self):
        for update in [dict(ok=1), dict(token='forged'), dict(label='private')]:
            r = Rig(); r.mutate_reply = lambda v: v.update(update)
            with self.assertRaises(RuntimeError):
                r.run()
            self.assertEqual(r.reads, 1)
            self.assertFalse(r.report['measurement_valid'])

    def test_wrong_metadata_geometry_or_authority_aborts(self):
        for update in [dict(enabled=False), dict(keyboard_dispatch_supported=True), dict(role='AXButton'),
                       dict(bounds=dict(x=float('nan'),y=20,width=80,height=24))]:
            r = Rig(); r.mutate_reply = lambda v: v['keyboard_target'].update(update)
            with self.assertRaises(RuntimeError):
                r.run()
            self.assertEqual(r.reads, 1)

    def test_unexpected_keyboard_input_stops_before_next_read(self):
        r = Rig(); r.mutate_after = lambda s: s.update(key_event_count=1)
        with self.assertRaisesRegex(RuntimeError, 'keyboard input'):
            r.run()
        self.assertEqual(r.reads, 1)
        self.assertFalse(r.report['completed'])
        self.assertEqual(r.report['summary']['accepted_nonprotected'], 0)

    def test_protected_success_is_not_a_measurement_pass(self):
        r = Rig()
        def fabricate(reply):
            if r.active == 'protected':
                reply.clear()
                reply.update(ok=True, keyboard_target=dict(role='AXTextField', bounds=r.state()['rect'],
                             enabled=True, keyboard_dispatch_supported=False))
        r.mutate_reply = fabricate
        with self.assertRaisesRegex(RuntimeError, 'protected receiver unexpectedly accepted'):
            r.run()
        self.assertEqual(r.reads, 9)
        self.assertFalse(r.report['measurement_valid'])

    def test_no_content_in_error_report(self):
        r = Rig()
        r.mutate_reply = lambda v: (v.clear(), v.update(ok=False, error='synthetic-secret-exposed'))
        with self.assertRaisesRegex(RuntimeError, 'content'):
            r.run()
        self.assertNotIn('synthetic-secret-exposed', str(r.report))

    def test_observation_changes_do_not_claim_attribution(self):
        report = Rig().run()
        for row in report['samples']:
            self.assertNotEqual(row['desktop']['before'], row['desktop']['after'])
            self.assertEqual(row['desktop']['change_attribution'], 'undetermined')
            self.assertFalse(row['desktop']['continuous_isolation_verified'])

    def test_error_code_and_timing_not_inferred_from_context(self):
        error = ('AX AXContainsProtectedContent read failed; kAXErrorCannotComplete (-25204); value_present=false; '
                 'ax_copy_us=52031; ax_timeout_us=50000; budget_before_us=3000000; budget_after_us=2947969')
        result = study.classify_refusal(error)
        self.assertEqual(result['category'], 'ax_cannot_complete')
        self.assertEqual(result['native_status']['code'], -25204)
        self.assertEqual(result['native_timing']['ax_copy_us'], 52031)
        unknown = study.classify_refusal('AX AXSubrole read failed; metadata is not known')
        self.assertEqual(unknown['category'], 'other_refusal')
        self.assertIsNone(unknown['native_status'])
        self.assertIsNone(unknown['native_timing'])

    def test_presence_and_status_preserved_not_treated_as_recoverable(self):
        r = study.classify_refusal('kAXErrorCannotComplete (-25204); value_present=true')
        self.assertTrue(r['native_status']['value_present'])
        self.assertIsNone(r['native_timing'])
        r = study.classify_refusal('unknown AXError (-123); value_present=false')
        self.assertEqual(r['category'], 'ax_other')
        self.assertEqual(r['native_status']['code'], -123)

    def test_flags_require_optins_and_exclude_actions(self):
        spec = importlib.util.spec_from_file_location('outer_study', Path(__file__).with_name('verify-macos-monitor-http.py'))
        m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
        base = ['--bin','b','--fixture','p','--report','r','--allow-shared-session-monitor',
                '--chromium-app','browser','--chromium-supervisor','supervisor']
        valid = base + ['--chromium-keyboard-target','--chromium-keyboard-target-click-first','--chromium-receiver-study']
        self.assertFalse(m.parse_args(base).chromium_receiver_study)
        self.assertTrue(m.parse_args(valid).chromium_receiver_study)
        for args in [base + ['--chromium-receiver-study'],
                     valid + ['--chromium-arrowleft'],valid + ['--chromium-arrowright'],
                     valid + ['--controls-fixture','x'],valid + ['--chromium-bound-pointer']]:
            with self.assertRaises(SystemExit):
                m.parse_args(args)

    def test_outer_preserves_last_checkpoint_on_timeout(self):
        import tempfile
        import json
        spec = importlib.util.spec_from_file_location('outer_preserve', Path(__file__).with_name('verify-macos-monitor-http.py'))
        m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
        with tempfile.TemporaryDirectory() as d:
            path = Path(d)/'report.json'
            checkpoint = {'profile':'receiver_study','passed':False,'receiver_study':{'completed':False,'samples':[{'read_attempted':True}]}}
            path.write_text(json.dumps(checkpoint))
            report = {}
            with self.assertRaisesRegex(RuntimeError, 'original timeout'):
                with m.preserve_study_evidence(report,path,True):
                    raise RuntimeError('original timeout')
            self.assertEqual(report['chromium'], checkpoint)
            path.write_text('malformed')
            report = {}
            with self.assertRaisesRegex(RuntimeError, 'original timeout'):
                with m.preserve_study_evidence(report,path,True):
                    raise RuntimeError('original timeout')
            self.assertIn('study_evidence_error',report)
            self.assertNotIn('chromium',report)
            report = {}
            with m.preserve_study_evidence(report,path,False):
                pass
            self.assertEqual(report,{})

if __name__ == '__main__':
    unittest.main()
