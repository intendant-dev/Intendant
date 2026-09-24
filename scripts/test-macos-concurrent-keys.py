#!/usr/bin/env python3
"""Hermetic tests: no GUI, AX queries, event counters or native input."""
import copy
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
import macos_concurrent_keys as keys

WINDOW = {'x': -770, 'y': 30, 'width': 720, 'height': 530}
BOUNDS = {'x': -740, 'y': 180, 'width': 348, 'height': 27}
RECEIVER = {'bounds': BOUNDS, 'enabled': True, 'role': 'AXTextField'}
TOKEN = 'macos_key:' + 'a'*32

def native_witness(phase, sequence, activity=0, human=False):
    sample = {'status': 0, 'pid_status': 0, 'value_present': True, 'valid': True}
    result = {'sequence': sequence, 'phase': phase}
    if phase == 'before':
        result.update(human=sample, receiver=sample,
                      hid_activity={'source': 'hid_system', 'sampled': True})
    else:
        result.update(complete=True, human_before=sample, human_after=sample,
                      receiver_before=sample, receiver_after=sample,
                      human_changed=human, receiver_changed=False, foreground_changed=False,
                      target_foreground_observed=False, elapsed_us=1000,
                      continuous_isolation_verified=False,
                      hid_activity={'source': 'hid_system', 'counter_regression': False,
                                    'changed': bool(activity), 'attribution': 'not_authenticated',
                                    'deltas': {k: activity if k=='any_input' else 0 for k in keys.COUNTERS}})
    return json.loads(json.dumps(result))

def zero_refusal():
    return {'ok': False, 'action_attempted': False, 'effects_unconfirmed': False,
            'posting_calls': 0, 'effect_verified': False, 'focus_interference': None,
            'error': 'human global focused object changed during keyboard receiver observation'}

class Rig:
    def __init__(self, key='ArrowLeft'):
        self.key=key
        self.current={'active': 'first', 'events': [], 'overflow': False,
                      'count': 0, 'caret': 2, 'end': 2, 'value': 'fixture-a'}
        self.geometry={'active': 'first', 'click_overflow': False, 'click_events': [{}, {}, {}],
                       'rect': BOUNDS, 'metrics': WINDOW}
        self.calls=[]; self.markers=[]; self.report={}; self.checkpoints=[]
        self.prepare_error=False; self.post_error=None; self.bad_witness=False
        self.activity=0; self.human=False; self.prep_activity=0; self.bad_receiver=False
        self.preparation={'ok': True, 'action_attempted': False, 'prepared': {
            'key': key, 'token': TOKEN, 'expires_in_ms': 10000,
            'receiver': RECEIVER, 'window': {'ax': WINDOW, 'cg': WINDOW}}}

    def evaluate(self, expression):
        if expression == 'keyboardTargetFixtureState()': return copy.deepcopy(self.geometry)
        if expression in ('armArrowFixture()', 'arrowFixtureState()'): return copy.deepcopy(self.current)
        raise AssertionError(expression)

    def call(self, tool, **arguments):
        self.calls.append(tool)
        if tool == 'prepare_macos_window_'+self.key.lower():
            return {'ok': False, 'error': 'required AX attribute AXFrontmost unavailable'} if self.prepare_error else copy.deepcopy(self.preparation)
        assert tool == 'press_macos_window_'+self.key.lower()
        assert arguments['token'] == TOKEN
        if self.post_error == 'zero': return zero_refusal()
        self.current.update(caret=1 if self.key=='ArrowLeft' else 3,
                            end=1 if self.key=='ArrowLeft' else 3, count=1,
                            events=[{'kind': kind, 'target': 'first', 'key': self.key, 'code': self.key,
                                     'trusted': True, 'repeat': False, 'alt': False, 'control': False,
                                     'meta': False, 'shift': False} for kind in ('keydown','keyup')])
        if self.post_error == 'transport': raise RuntimeError('lost dispatch reply')
        native={'ok': True, 'action_attempted': True, 'effects_unconfirmed': True, 'focus_interference': False, 'action': {'key': self.key, 'status': 'dispatched', 'posting_calls': 2,
            'effect_verified': False, 'effects_unconfirmed': True, 'action_attempted': True,
            'focus_interference': False, 'receiver_unchanged': True, 'detail': None,
            'receiver': RECEIVER, 'after_receiver': RECEIVER,
            'before': {'ax': WINDOW, 'cg': WINDOW}, 'after': {'ax': WINDOW, 'cg': WINDOW}}}
        if self.post_error == 'partial': native['ok']=False; native['action']['status']='partial'
        if self.bad_receiver: native['action']['receiver']={}
        return native

    def witness(self, phase, sequence):
        self.markers.append((phase,sequence))
        if self.bad_witness and phase=='after' and sequence==2: raise RuntimeError('lost witness')
        return native_witness(phase,sequence,self.activity if sequence==2 else self.prep_activity,self.human)

    def verify_geometry(self, receiver, geometry, window):
        assert receiver['bounds']==BOUNDS and geometry==self.geometry and window==WINDOW

    def run(self):
        return keys.collect(self.call,self.evaluate,'binding',self.witness,self.verify_geometry,
                            WINDOW,self.report,lambda:self.checkpoints.append(copy.deepcopy(self.report)),
                            10,self.key,clock=lambda:1,pause=lambda _:None)

class Tests(unittest.TestCase):
    def test_both_directions_one_pair_and_separate_brackets(self):
        for key in ('ArrowLeft','ArrowRight'):
            rig=Rig(key);rig.run()
            self.assertEqual(len(rig.calls),2)
            self.assertEqual(rig.markers,[('before',1),('after',1),('before',2),('after',2)])
            self.assertTrue(rig.report['measurement_valid'])
            self.assertTrue(rig.report['summary']['effect_verified'])
            self.assertNotIn(TOKEN,json.dumps(rig.report))

    def test_preparation_refusal_is_not_key_failure_or_a_retry(self):
        rig=Rig();rig.prepare_error=True;rig.run()
        self.assertEqual(len(rig.calls),1)
        self.assertEqual(rig.report['outcome'],'preparation_refused')
        self.assertFalse(rig.report['summary']['dispatch_request_attempted'])
        self.assertIsNone(rig.report['summary']['hid_activity_during_dispatch_bracket'])

    def test_zero_effect_refusal_records_no_delivery(self):
        rig=Rig();rig.post_error='zero';rig.run()
        self.assertEqual(rig.report['outcome'],'dispatch_refused')
        self.assertFalse(rig.report['summary']['effect_verified'])
        self.assertEqual(rig.report['after'],rig.report['before'])

    def test_uncertain_posting_never_retries_even_with_observed_effect(self):
        rig=Rig();rig.post_error='partial'
        with self.assertRaises(RuntimeError): rig.run()
        self.assertEqual(len(rig.calls),2)
        self.assertEqual(rig.report['after']['count'],1)
        self.assertFalse(rig.report['completed'])
        self.assertFalse(rig.report['summary']['effect_verified'])

    def test_lost_dispatch_reply_preserves_end_witness_and_effect(self):
        rig=Rig();rig.post_error='transport'
        with self.assertRaisesRegex(RuntimeError,'lost dispatch reply'): rig.run()
        self.assertIn('native_after',rig.report['dispatch'])
        self.assertEqual(rig.report['after']['count'],1)
        self.assertFalse(rig.report['measurement_valid'])

    def test_failed_witness_keeps_dispatch_receipt_and_effect(self):
        rig=Rig();rig.bad_witness=True
        with self.assertRaises(RuntimeError): rig.run()
        self.assertIn('reply',rig.report['dispatch'])
        self.assertEqual(rig.report['after']['count'],1)
        self.assertEqual(len(rig.calls),2)

    def test_receipt_must_match_prepared_receiver(self):
        rig=Rig();rig.bad_receiver=True
        with self.assertRaises(RuntimeError): rig.run()
        self.assertFalse(rig.report['summary']['effect_verified'])

    def test_activity_is_context_not_authenticated_human_or_isolation(self):
        rig=Rig();rig.activity=3;rig.human=True;rig.run()
        result=rig.report['summary']
        self.assertTrue(result['effect_verified'])
        self.assertTrue(result['hid_activity_during_dispatch_bracket'])
        self.assertFalse(result['hid_keyboard_activity_during_dispatch_bracket'])
        self.assertTrue(result['human_focus_changed_during_dispatch_bracket'])
        self.assertFalse(result['internal_posting_overlap_verified'])
        self.assertFalse(result['continuous_isolation_verified'])

    def test_preparation_activity_does_not_become_dispatch_activity(self):
        rig=Rig();rig.prep_activity=5;rig.run()
        self.assertFalse(rig.report['summary']['hid_activity_during_dispatch_bracket'])

    def test_counter_regression_is_unknown(self):
        value=native_witness('after',1)
        value['hid_activity'].update(counter_regression=True,changed=None,
                                     deltas={k:None for k in keys.COUNTERS})
        keys.validate_witness(value,1,'after')
        value['hid_activity']['changed']=False
        with self.assertRaises(RuntimeError): keys.validate_witness(value,1,'after')

    def test_counter_types_and_range(self):
        for bad in (True,-1,2**32,1.5,None):
            value=native_witness('after',1);value['hid_activity']['deltas']['key_down']=bad
            with self.assertRaises(RuntimeError):keys.validate_witness(value,1,'after')

    def test_witness_phase_sequence_schema_and_contradictions(self):
        for field,bad in [('sequence',True),('phase','before'),('complete',False),
                          ('target_foreground_observed',True),('continuous_isolation_verified',True)]:
            value=native_witness('after',1);value[field]=bad
            with self.assertRaises(RuntimeError):keys.validate_witness(value,1,'after')
        value=native_witness('after',1);value['secret']='must not silently accept'
        with self.assertRaises(RuntimeError):keys.validate_witness(value,1,'after')

    def test_unknown_focus_is_not_unchanged(self):
        value=native_witness('after',1);value['human_before']['valid']=False;value['human_changed']=None
        keys.validate_witness(value,1,'after')
        value['human_changed']=False
        with self.assertRaises(RuntimeError):keys.validate_witness(value,1,'after')

    def test_wrong_key_and_token_never_post(self):
        for field,bad in [('key','ArrowRight'),('token','macos_key:bad'),('expires_in_ms',True)]:
            rig=Rig();rig.preparation['prepared'][field]=bad
            with self.assertRaises(RuntimeError):rig.run()
            self.assertEqual(len(rig.calls),1)

    def test_implicit_zero_effect_is_not_accepted(self):
        for field in ('posting_calls','effects_unconfirmed','action_attempted','error'):
            reply=zero_refusal();del reply[field];self.assertFalse(keys.definite_no_post(reply))
        for bad in (False,1):
            reply=zero_refusal();reply['posting_calls']=bad;self.assertFalse(keys.definite_no_post(reply))
        reply=zero_refusal();reply['action']={'posting_calls':2};self.assertFalse(keys.definite_no_post(reply))

    def test_expired_before_start_posts_nothing(self):
        rig=Rig()
        with self.assertRaises(RuntimeError):
            keys.collect(rig.call,rig.evaluate,'binding',rig.witness,rig.verify_geometry,
                WINDOW,rig.report,lambda:None,0,'ArrowLeft',clock=lambda:1)
        self.assertEqual(rig.calls,[])

    def test_checkpoints_precede_actual_calls(self):
        rig=Rig();rig.run()
        self.assertTrue(any(r.get('dispatch',{}).get('attempted') is True and
                            'reply' not in r['dispatch'] for r in rig.checkpoints))

    def test_options_exclude_unrequested_or_ambiguous_input(self):
        keys.validate_options(True,True,True,True,False)
        for flags in [(True,False,True,True,False),(True,True,False,True,False),
                      (True,True,True,False,False),(True,True,True,True,True),
                      (True,True,True,True,False,True)]:
            with self.assertRaises(ValueError):keys.validate_options(*flags)

    def test_outer_flags_and_exact_timeout_checkpoint_profile(self):
        spec=importlib.util.spec_from_file_location('outer',Path(__file__).with_name('verify-macos-monitor-http.py'))
        outer=importlib.util.module_from_spec(spec);spec.loader.exec_module(outer)
        common=['--bin','unused','--fixture','unused','--report','unused','--allow-shared-session-monitor',
                '--chromium-app','unused','--chromium-supervisor','unused',
                '--chromium-keyboard-target','--chromium-keyboard-target-click-first',
                '--chromium-arrowleft','--chromium-concurrent-key-study']
        self.assertTrue(outer.parse_args(common).chromium_concurrent_key_study)
        for flag in ('--chromium-receiver-study','--chromium-native-focus','--chromium-arrowright','--chromium-bound-pointer'):
            with self.assertRaises(SystemExit),contextlib.redirect_stderr(io.StringIO()):outer.parse_args(common+[flag])
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'inner.json'
            for profile in ('receiver_study','native_focus','concurrent_key'):
                path.write_text(json.dumps({'profile':profile,'partial':True}))
                report={}
                with self.assertRaisesRegex(RuntimeError,'original timeout'):
                    with outer.preserve_study_evidence(report,path,True,profile):raise RuntimeError('original timeout')
                self.assertTrue(report['chromium']['partial'])
            report={}
            with outer.preserve_study_evidence(report,path,True,'native_focus'):pass
            self.assertNotIn('chromium',report)

    def test_malformed_end_counter_preserves_original_finish_failure(self):
        rig=Rig();original=rig.witness
        def broken(phase,sequence):
            value=original(phase,sequence)
            if phase=='after' and sequence==2:del value['hid_activity']['deltas']['key_down']
            return value
        rig.witness=broken
        with self.assertRaisesRegex(RuntimeError,'native witness finish failed'):rig.run()
        self.assertEqual(rig.report['dispatch']['witness_finish_error'],'activity counters')
        self.assertFalse(rig.report['summary']['effect_verified'])
        self.assertEqual(rig.report['after']['count'],1)

    def test_unexpected_fixture_text_and_keys_are_not_saved(self):
        state={'value':'private typed value','events':[{'key':'private key','code':'private code'}]}
        wire=json.dumps(keys.public_fixture(state))
        self.assertNotIn('private',wire)
        self.assertIn('[changed]',wire)
        self.assertIn('[unexpected]',wire)

    def test_actual_preparation_schema_uses_same_strict_geometry_without_read_only_field(self):
        spec=importlib.util.spec_from_file_location('inner',Path(__file__).with_name('verify-macos-chromium-controls.py'))
        inner=importlib.util.module_from_spec(spec);spec.loader.exec_module(inner)
        fixture={'active':'first','rect':{'x':30,'y':40,'width':348,'height':27},
                 'metrics':{'screen_x':-770,'screen_y':30,'outer_width':720,'outer_height':530,
                            'inner_width':720,'inner_height':420,'scale':1,'zoom':1,'scroll_x':0,'scroll_y':0}}
        rig=Rig()
        self.assertEqual(keys.preparation_token(rig.preparation,'ArrowLeft',fixture,WINDOW,
                         inner.validate_prepared_keyboard_receiver),TOKEN)
        with self.assertRaises(RuntimeError):inner.validate_keyboard_receiver(RECEIVER,fixture,WINDOW)
        self.assertEqual(inner.validate_keyboard_receiver(dict(RECEIVER,keyboard_dispatch_supported=False),fixture,WINDOW),BOUNDS)
        with self.assertRaises(RuntimeError):inner.validate_prepared_keyboard_receiver(dict(RECEIVER,keyboard_dispatch_supported=False),fixture,WINDOW)
        wrong=copy.deepcopy(RECEIVER);wrong['bounds']['y']+=3
        with self.assertRaises(RuntimeError):inner.validate_prepared_keyboard_receiver(wrong,fixture,WINDOW)


if __name__=='__main__': unittest.main(verbosity=2)
