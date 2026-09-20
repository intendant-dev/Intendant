#!/usr/bin/env python3
"""Hermetic scroll evidence/CLI checks: no native APIs, daemon, browser or input."""
import argparse
import contextlib
import copy
import importlib.util
import io
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

from macos_scroll_evidence import (assess_scroll, certain_refusal, parse_scroll_delta,
                                   valid_scroll_delta)


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


h = load('scroll_bound_harness', 'verify-macos-bound-pointer.py')
outer = load('scroll_monitor_harness', 'verify-macos-monitor-http.py')


class EvidenceTests(unittest.TestCase):
    def rig(self, delta=120):
        bounds = {'X': -800., 'Y': 50., 'Width': 720., 'Height': 530.}
        metrics = {'screen_x': -800., 'screen_y': 50., 'outer_width': 720., 'outer_height': 530.,
                   'inner_width': 720., 'inner_height': 443., 'scale': 1, 'scroll_x': 0, 'scroll_y': 0,
                   'canvas': {'x': 24., 'y': 88., 'width': 400., 'height': 200.}}
        # Exercise the real planner; no copied coordinate conversion in this suite.
        plan = h.plan_pointer(metrics, bounds, 17, 224, 155, 42)
        rectangle = dict(zip(('x', 'y', 'width', 'height'), (bounds[k] for k in ('X', 'Y', 'Width', 'Height'))))
        observation = {'ax': dict(rectangle), 'cg': dict(rectangle)}
        action = {'status': 'dispatched', 'posting_calls': 1, 'action_attempted': True,
                  'effects_unconfirmed': True, 'effect_verified': False, 'focus_interference': False,
                  'detail': None, 'delta_y': delta,
                  'point': {'x': plan['local_x'], 'y': plan['local_y']},
                  'global': {'x': plan['x'], 'y': plan['y']},
                  'before': copy.deepcopy(observation), 'after': copy.deepcopy(observation)}
        native = {'ok': True, 'action_attempted': True, 'effects_unconfirmed': True,
                  'focus_interference': False, 'action': action}
        nonce = 'a' * 32
        state = {'nonce': nonce, 'wheels': 1, 'overflow': False, 'unrelated': 0, 'ready': 'complete',
                 'scroll_top': 700 + delta, 'scroll_left': 0, 'metrics': metrics,
                 'events': [{'type': 'wheel', 'trusted': True, 'delta_x': 0, 'delta_y': delta, 'delta_mode': 0,
                             'alt_key': False, 'ctrl_key': False, 'meta_key': False, 'shift_key': False,
                             'client_x': plan['client_x'], 'client_y': plan['client_y'],
                             'screen_x': plan['x'], 'screen_y': plan['y']}]}
        return plan, native, state, nonce

    def test_both_signs_smallest_and_largest_with_independent_effect(self):
        for delta in (-600, -120, -1, 1, 120, 600):
            with self.subTest(delta=delta):
                rig = self.rig(delta)
                original = copy.deepcopy(rig)
                result = assess_scroll(*rig, delta)
                self.assertTrue(result['passed'] and result['effect_verified'])
                self.assertTrue(result['native_dispatch_reported'] and result['wheel_observed']
                                and result['scroll_offset_verified'])
                self.assertFalse(result['dom_tag_correlation'] or result['continuous_isolation_verified'])
                self.assertFalse(rig[1]['action']['effect_verified'])
                self.assertEqual(rig, original, 'assessment must not modify evidence')

    def test_invalid_requested_deltas_refuse_without_coercion_or_overflow(self):
        rig = self.rig()
        for delta in (0, 601, -601, True, False, 120., -1., float('nan'), float('inf'),
                      -float('inf'), None, '120', '-120', '', [], {}, 2**31, -2**31, 2**4096, -2**4096):
            with self.subTest(delta=delta):
                self.assertFalse(valid_scroll_delta(delta))
                self.assertFalse(assess_scroll(*rig, delta)['passed'])

    def test_native_delta_must_be_the_exact_requested_integer(self):
        for delta in (-120, 120):
            p, native, state, nonce = self.rig(delta)
            for wrong in (-delta, delta+1, 0, 601, -601, True, float(delta), str(delta), None, 2**4096):
                with self.subTest(delta=delta, wrong=wrong):
                    bad = copy.deepcopy(native)
                    bad['action']['delta_y'] = wrong
                    self.assertFalse(assess_scroll(p, bad, state, nonce, delta)['passed'])

    def test_posting_and_even_correct_wheel_without_actual_scroll_refuse(self):
        for delta in (-600, -1, 1, 600):
            p, native, state, nonce = self.rig(delta)
            for offset in (700, 700-delta, 700+delta-1, 700+delta+1, 700+delta+.5,
                           True, None, '700', float('nan'), float('inf'), 2**4096):
                with self.subTest(delta=delta, offset=offset):
                    result = assess_scroll(p, native, dict(state, scroll_top=offset), nonce, delta)
                    self.assertTrue(result['native_dispatch_reported'] and result['wheel_observed'])
                    self.assertFalse(result['passed'] or result['effect_verified'] or result['scroll_offset_verified'])
            for offset in (-1, 1, False, None, float('inf')):
                self.assertFalse(assess_scroll(p, native, dict(state, scroll_left=offset), nonce, delta)['passed'])
            # A reported post without DOM delivery/effect is only a reported post.
            absent = dict(state, wheels=0, events=[], scroll_top=700)
            result = assess_scroll(p, native, absent, nonce, delta)
            self.assertTrue(result['native_dispatch_reported'])
            self.assertFalse(result['passed'] or result['effect_verified'] or result['wheel_observed'])

    def test_initial_offset_cannot_be_rebased_after_input(self):
        for initial in (0, 699, 701, None, True, '700', float('nan'), 2**4096):
            self.assertFalse(assess_scroll(*self.rig(), 120, initial)['passed'])

    def test_wrong_nonce_malformed_state_and_duplicate_or_unrelated_events_refuse(self):
        p, native, state, nonce = self.rig()
        for key, value in (('nonce', 'b'*32), ('nonce', None), ('wheels', 0), ('wheels', 2),
                           ('wheels', True), ('wheels', 1.), ('overflow', True), ('overflow', 0),
                           ('unrelated', 1), ('unrelated', False), ('events', []), ('events', None),
                           ('events', [None]), ('events', state['events']*2), ('ready', 'loading')):
            with self.subTest(key=key, value=value):
                self.assertFalse(assess_scroll(p, native, dict(state, **{key: value}), nonce, 120)['passed'])
        for nonce in ('b'*32, 'a'*31, 'a'*33, 'A'*32, 'g'*32, '', None, True):
            self.assertFalse(assess_scroll(p, native, state, nonce, 120)['passed'])
        for key in ('nonce', 'wheels', 'overflow', 'unrelated', 'events', 'ready', 'scroll_top', 'scroll_left'):
            bad = copy.deepcopy(state); del bad[key]
            self.assertFalse(assess_scroll(p, native, bad, 'a'*32, 120)['passed'])
        for index in (0, 1, 2):
            for malformed in (None, [], True, 'unknown'):
                args = list(self.rig()); args[index] = malformed
                self.assertFalse(assess_scroll(*args, 120)['passed'])

    def test_exact_trusted_unmodified_pixel_wheel_required(self):
        p, native, state, nonce = self.rig()
        bad_fields = [('type', 'click'), ('trusted', False), ('trusted', 1),
                      ('delta_mode', 1), ('delta_mode', 2), ('delta_mode', False), ('delta_mode', 0.),
                      ('delta_x', 1), ('delta_x', .001), ('delta_x', False),
                      ('delta_y', -120), ('delta_y', 0), ('delta_y', 119.999),
                      ('delta_y', True), ('delta_y', '120'), ('delta_y', float('nan')),
                      ('delta_y', float('inf')), ('delta_y', 2**4096)]
        for key in ('alt_key', 'ctrl_key', 'meta_key', 'shift_key'):
            bad_fields.extend((key, value) for value in (True, 0, None, 'false'))
        for key, value in bad_fields:
            with self.subTest(key=key, value=value):
                bad = copy.deepcopy(state); bad['events'][0][key] = value
                self.assertFalse(assess_scroll(p, native, bad, nonce, 120)['passed'])
        for key in state['events'][0]:
            bad = copy.deepcopy(state); del bad['events'][0][key]
            self.assertFalse(assess_scroll(p, native, bad, nonce, 120)['passed'])

    def test_client_and_global_coordinates_keep_existing_one_point_tolerance(self):
        p, native, state, nonce = self.rig()
        for key in ('client_x', 'client_y', 'screen_x', 'screen_y'):
            for amount in (-1, 1):
                near = copy.deepcopy(state); near['events'][0][key] += amount
                self.assertTrue(assess_scroll(p, native, near, nonce, 120)['passed'])
            for value in (state['events'][0][key]+1.01, state['events'][0][key]-1.01,
                          None, True, '224', float('nan'), float('inf'), 2**4096):
                bad = copy.deepcopy(state); bad['events'][0][key] = value
                self.assertFalse(assess_scroll(p, native, bad, nonce, 120)['passed'])
        for field in ('point', 'global'):
            for key in ('x', 'y'):
                bad = copy.deepcopy(native); bad['action'][field][key] += .01
                self.assertFalse(assess_scroll(p, bad, state, nonce, 120)['passed'])

    def test_failed_partial_incomplete_or_uncertain_native_receipt_refuses_even_with_dom_effect(self):
        p, native, state, nonce = self.rig()
        for key, value in (('status', 'partial'), ('status', 'unknown'), ('posting_calls', 0),
                           ('posting_calls', 2), ('posting_calls', True), ('posting_calls', 1.),
                           ('effect_verified', True), ('effect_verified', 0), ('action_attempted', False),
                           ('effects_unconfirmed', False), ('focus_interference', True),
                           ('focus_interference', None), ('detail', ''), ('detail', 'native post failed'),
                           ('before', None), ('after', None)):
            with self.subTest(key=key, value=value):
                bad = copy.deepcopy(native); bad['action'][key] = value
                self.assertFalse(assess_scroll(p, bad, state, nonce, 120)['passed'])
        for key in native['action']:
            bad = copy.deepcopy(native); del bad['action'][key]
            self.assertFalse(assess_scroll(p, bad, state, nonce, 120)['passed'])
        for key, value in (('ok', False), ('ok', 1), ('error', 'receipt expired'), ('action', None), ('effect_verified', True),
                           ('action_attempted', None), ('effects_unconfirmed', False), ('focus_interference', None)):
            self.assertFalse(assess_scroll(p, dict(native, **{key: value}), state, nonce, 120)['passed'])
        for key in native:
            bad = copy.deepcopy(native); del bad[key]
            self.assertFalse(assess_scroll(p, bad, state, nonce, 120)['passed'])

    def test_native_geometry_must_remain_available_and_match_independent_window(self):
        p, native, state, nonce = self.rig()
        for when in ('before', 'after'):
            for source in ('ax', 'cg'):
                for key in ('x', 'y', 'width', 'height'):
                    for value in (native['action'][when][source][key]+2, None, True, float('nan'), 2**4096):
                        bad = copy.deepcopy(native); bad['action'][when][source][key] = value
                        self.assertFalse(assess_scroll(p, bad, state, nonce, 120)['passed'])
        bad = copy.deepcopy(native)
        for source in ('ax', 'cg'):
            bad['action']['before'][source]['x'] -= 1
            bad['action']['after'][source]['x'] += 1
        self.assertFalse(assess_scroll(p, bad, state, nonce, 120)['passed'])

    def test_negative_checks_require_certain_nonposting_refusal(self):
        refused = {'ok': False, 'action_attempted': False, 'error': 'consumed token'}
        self.assertTrue(certain_refusal(refused))
        self.assertTrue(certain_refusal({'ok': False, 'error': 'invalid delta'}, dispatch=False))
        self.assertFalse(certain_refusal({'ok': False, 'error': 'missing receipt'}))
        for key, value in (('ok', True), ('ok', 0), ('action_attempted', None), ('action_attempted', True),
                           ('action_attempted', 0), ('effects_unconfirmed', True), ('effects_unconfirmed', None),
                           ('action', {'posting_calls': 1, 'status': 'partial'})):
            bad = dict(refused, **{key: value})
            self.assertFalse(certain_refusal(bad))
            self.assertFalse(certain_refusal(bad, dispatch=False))


class ArgumentTests(unittest.TestCase):
    base = ['--bin', 'unused-bin', '--fixture', 'unused-fixture', '--report', 'unused-report',
            '--allow-shared-session-monitor']
    chromium = ['--chromium-app', 'unused.app', '--chromium-supervisor', 'unused-supervisor']
    inner = ['--bin', 'unused-bin', '--port', '1234', '--monitor', 'macos_virtual:unused',
             '--browser-app', 'unused.app', '--supervisor', 'unused-supervisor', '--report', 'unused-report',
             '--allow-disposable-chromium']

    def test_strict_signed_cli_delta(self):
        for spelling, value in (('-600', -600), ('-1', -1), ('1', 1), ('+1', 1), ('+600', 600)):
            self.assertEqual(parse_scroll_delta(spelling), value)
        for spelling in ('0', '+0', '-0', '601', '-601', '1.0', '1e2', 'True', 'false', 'NaN', 'inf',
                         '', ' 1', '1 ', '1_0', '--1', '0x10', '9'*5000, None, True, 120., 120):
            with self.subTest(spelling=spelling), self.assertRaises(argparse.ArgumentTypeError):
                parse_scroll_delta(spelling)

    def test_both_cli_layers_preserve_each_sign_and_default_click_mode(self):
        with patch.object(h.platform, 'system', return_value='Darwin'):
            self.assertIsNone(h.parse_args(self.inner).scroll_delta)
            for delta in (-600, -1, 1, 600):
                inner = h.parse_args(self.inner+['--scroll-delta', str(delta)])
                wrapper = outer.parse_args(self.base+self.chromium+['--chromium-scroll-delta', str(delta)])
                self.assertEqual(inner.scroll_delta, delta)
                self.assertEqual(wrapper.chromium_scroll_delta, delta)
                self.assertFalse(wrapper.chromium_bound_pointer or wrapper.chromium_placement_only)
        click = outer.parse_args(self.base+self.chromium+['--chromium-bound-pointer'])
        self.assertTrue(click.chromium_bound_pointer)
        self.assertIsNone(click.chromium_scroll_delta)

    def test_wrapper_invalid_modes_refuse_before_inventory_temporary_files_or_daemon(self):
        scroll = ['--chromium-scroll-delta', '120']
        invalid = [self.base+scroll,
                   self.base+['--chromium-app', 'unused.app']+scroll,
                   self.base+['--chromium-supervisor', 'unused-supervisor']+scroll,
                   self.base+self.chromium+scroll+['--chromium-bound-pointer'],
                   self.base+self.chromium+scroll+['--chromium-placement-only'],
                   self.base[:-1]+self.chromium+scroll,
                   self.base+['--chromium-bound-pointer'],
                   self.base+self.chromium+['--chromium-bound-pointer', '--chromium-placement-only']]
        invalid.extend(self.base+self.chromium+['--chromium-scroll-delta='+value]
                       for value in ('0', '-0', '601', '-601', 'True', '1.0', '1e2', '9'*5000))
        for argv in invalid:
            with self.subTest(argv=argv), contextlib.redirect_stderr(io.StringIO()), \
                    patch.object(sys, 'argv', ['verify-macos-monitor-http.py']+argv), \
                    patch.object(outer, 'inventory', side_effect=AssertionError('native inventory reached')) as inventory, \
                    patch.object(outer.tempfile, 'TemporaryDirectory', side_effect=AssertionError('temporary files reached')) as temporary, \
                    patch.object(outer.subprocess, 'Popen', side_effect=AssertionError('daemon reached')) as spawn:
                with self.assertRaises(SystemExit) as error:
                    outer.main()
                self.assertEqual(error.exception.code, 2)
                inventory.assert_not_called(); temporary.assert_not_called(); spawn.assert_not_called()

    def test_inner_invalid_delta_or_missing_consent_refuses_before_paths_or_input(self):
        invalid = [self.inner[:-1]+['--scroll-delta', '120']]
        invalid.extend(self.inner+['--scroll-delta='+value] for value in ('0', '601', '-601', 'true', '1.0', '9'*5000))
        for argv in invalid:
            with contextlib.redirect_stderr(io.StringIO()), patch.object(h.platform, 'system', return_value='Darwin'), \
                    patch.object(sys, 'argv', ['verify-macos-bound-pointer.py']+argv), \
                    patch.object(h.Path, 'resolve', side_effect=AssertionError('fixture paths reached')) as resolve, \
                    patch.object(h.transport, 'run_bounded', side_effect=AssertionError('ctl reached')) as ctl, \
                    patch.object(h.subprocess, 'Popen', side_effect=AssertionError('browser reached')) as spawn:
                with self.assertRaises(SystemExit) as error:
                    h.main()
                self.assertEqual(error.exception.code, 2)
                resolve.assert_not_called(); ctl.assert_not_called(); spawn.assert_not_called()

    def test_scroll_fixture_configures_only_before_arm_and_never_supplies_input(self):
        page = (Path(__file__).resolve().parent.parent/'tests/fixtures/macos-monitor/browser-scroll.html').read_text()
        setup, armed = page.split('window.armPointer=', 1)
        self.assertIn('pane.scrollTop=700;', setup)
        self.assertIn('pane.scrollLeft=0;', setup)
        for source in (setup, armed):
            for forbidden in ('dispatchEvent(', 'preventDefault(', 'scrollTo(', 'scrollBy('):
                self.assertNotIn(forbidden, source)
        self.assertNotIn('pane.scrollTop=', armed)
        self.assertNotIn('pane.scrollLeft=', armed)
        self.assertIn('scroll_top:pane.scrollTop', armed)
        self.assertIn('scroll_left:pane.scrollLeft', armed)
        source = Path(h.__file__).read_text()
        for forbidden in ('Input.dispatch', 'encode_plan(', '--disposable-chromium-pointer'):
            self.assertNotIn(forbidden, source)


if __name__ == '__main__':
    unittest.main()
