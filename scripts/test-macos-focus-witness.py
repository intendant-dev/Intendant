#!/usr/bin/env python3
"""Hermetic evidence/flow tests; no native APIs, GUI or networking."""
import copy
import importlib.util
from pathlib import Path
import unittest
import macos_focus_witness as f

def valid_sample():
    return dict(status=0, pid_status=0, value_present=True, valid=True)

def end_receipt(seq=1, changed=False, human=False):
    return dict(sequence=seq, phase="after", complete=True,
                human_before=valid_sample(), human_after=valid_sample(),
                receiver_before=valid_sample(), receiver_after=valid_sample(),
                human_changed=human, receiver_changed=changed, foreground_changed=False,
                target_foreground_observed=False, elapsed_us=1000, continuous_isolation_verified=False)

class Rig:
    def __init__(self):
        self.active="first"; self.reads=0; self.ends=0; self.stops=0; self.checkpoints=0
        self.key_count=0; self.error=None; self.human=False; self.mutate=None
    def state(self):
        x={"first":10, "second":40, "protected":70}[self.active]
        return dict(active=self.active, key_event_count=self.key_count, key_overflow=False,
            rect=dict(x=x,y=10,width=20,height=20),
            metrics=dict(screen_x=0,screen_y=0,outer_width=100,outer_height=100,
                inner_width=100,inner_height=100,scale=1,zoom=1,scroll_x=0,scroll_y=0),
            click_events=[{}, {}, {}], click_overflow=False)
    def select(self, name): self.active=name; return self.state()
    def witness(self, phase, seq):
        if phase=="before":
            self.baseline=self.active
            return dict(sequence=seq,phase=phase,human=valid_sample(),receiver=valid_sample())
        self.ends+=1
        receipt=end_receipt(seq,self.active!=self.baseline,self.human)
        if self.human is None:
            receipt['human_after']=dict(status=-25204,pid_status=-25200,value_present=False,valid=False)
        if self.mutate: self.mutate(receipt)
        return receipt
    def read(self):
        self.reads+=1
        if self.error: raise RuntimeError(self.error)
        if self.active=="protected": return dict(ok=False,error="focused keyboard receiver is protected")
        return dict(ok=True,keyboard_target=dict(role="AXTextField",enabled=True,
                    keyboard_dispatch_supported=False,bounds=self.state()['rect']))
    def churn(self): return None
    def stop(self): self.stops+=1; return dict(changes=8,active=False)
    def geometry(self,target,state,window):
        if target['bounds']!=state['rect']: raise RuntimeError('geometry')
    def checkpoint(self): self.checkpoints+=1
    def collect(self, report):
        f.collect(self.read,self.select,self.state,self.witness,self.churn,self.stop,
                  self.geometry,{},report,self.checkpoint,100,clock=lambda:0)

class Tests(unittest.TestCase):
    def test_fixed_plan_and_controls(self):
        rig=Rig();r={};rig.collect(r)
        self.assertEqual(rig.reads,6);self.assertEqual(rig.ends,6);self.assertEqual(rig.stops,1)
        self.assertTrue(r['completed']);self.assertTrue(r['measurement_valid'])
        self.assertTrue(r['summary']['native_transition_observed'])
        self.assertTrue(r['summary']['roundtrip_native_endpoints_equal'])
        self.assertEqual(r['summary']['categories'],{'accepted':5,'protected_receiver':1})
        self.assertFalse(r['keyboard_input_requested']);self.assertGreater(rig.checkpoints,12)
    def test_unknown_human_focus_is_not_stability(self):
        rig=Rig();rig.human=None;r={};rig.collect(r)
        self.assertEqual(r['summary']['human_unknown_intervals'],6)
        self.assertFalse(r['summary']['continuous_isolation_verified'])
    def test_observed_human_change_is_not_causation(self):
        rig=Rig();rig.human=True;r={};rig.collect(r)
        self.assertEqual(r['summary']['human_changed_intervals'],6)
        self.assertEqual(r['summary']['human_activity_attribution'],'undetermined')
    def test_transport_error_preserves_final_witness_and_stops(self):
        rig=Rig();rig.error='transport lost';r={}
        with self.assertRaisesRegex(RuntimeError,'transport lost'):rig.collect(r)
        self.assertEqual(rig.reads,1);self.assertEqual(rig.ends,1)
        self.assertIn('native_after',r['samples'][0]);self.assertFalse(r['completed'])
    def test_end_failure_does_not_mask_transport_error(self):
        rig=Rig();rig.error='primary';rig.mutate=lambda v:v.update(sequence=99);r={}
        with self.assertRaisesRegex(RuntimeError,'primary'):rig.collect(r)
        self.assertIn('native_finish_error',r['samples'][0])
    def test_native_begin_end_baseline_is_pinned(self):
        rig=Rig();rig.mutate=lambda v:v['human_before'].update(valid=False);r={}
        with self.assertRaises(RuntimeError):rig.collect(r)
        self.assertEqual(rig.reads,1);self.assertFalse(r['completed'])
    def test_unexpected_input_aborts_before_read(self):
        rig=Rig();rig.key_count=1;r={}
        with self.assertRaises(RuntimeError):rig.collect(r)
        self.assertEqual(rig.reads,0)
    def test_native_schema_rejects_lying_or_extra_evidence(self):
        mutations=[lambda v:v.update(sequence=True),lambda v:v.update(phase='before'),
          lambda v:v.update(complete=1),lambda v:v.update(elapsed_us=True),
          lambda v:v.update(elapsed_us=30000001),lambda v:v.update(complete=False),
          lambda v:v.update(target_foreground_observed=True),
          lambda v:v.update(continuous_isolation_verified=True),lambda v:v.update(label='private'),
          lambda v:v.update(human_changed=0),lambda v:v.update(foreground_changed=1),
          lambda v:v['human_before'].update(status=-25204),
          lambda v:v['receiver_before'].update(valid=False)]
        for mutation in mutations:
            v=end_receipt();mutation(v)
            with self.assertRaises(RuntimeError):f.validate(v,1,'after')
    def test_missing_read_is_null_not_false(self):
        v=end_receipt();v['human_after'].update(status=-25204,value_present=False,valid=False)
        with self.assertRaises(RuntimeError):f.validate(v,1,'after')
        v['human_changed']=None;self.assertIsNone(f.validate(v,1,'after')['human_changed'])
    def test_churn_is_stopped_if_start_reply_is_lost(self):
        rig=Rig();r={}
        def lost(): raise RuntimeError('churn response lost')
        rig.churn=lost
        with self.assertRaisesRegex(RuntimeError,'churn response lost'):rig.collect(r)
        self.assertEqual(rig.reads,3);self.assertEqual(rig.stops,1);self.assertEqual(rig.ends,4)
    def test_refusal_is_recorded_not_retried(self):
        rig=Rig();rig.read=lambda:dict(ok=False,error='AX unavailable');r={};rig.collect(r)
        self.assertEqual(r['summary']['attempted_reads'],6)
        self.assertEqual(r['summary']['categories'],{'other_refusal':6})
    def test_native_opt_in_requires_study(self):
        spec=importlib.util.spec_from_file_location('outer',Path(__file__).with_name('verify-macos-monitor-http.py'))
        outer=importlib.util.module_from_spec(spec);spec.loader.exec_module(outer)
        base=['--bin','b','--fixture','f','--report','r','--allow-shared-session-monitor']
        with self.assertRaises(SystemExit):outer.parse_args(base+['--chromium-native-focus'])
        accepted=base+['--chromium-app','app','--chromium-supervisor','s','--chromium-keyboard-target',
           '--chromium-keyboard-target-click-first','--chromium-receiver-study','--chromium-native-focus']
        self.assertTrue(outer.parse_args(accepted).chromium_native_focus)
        for extra in ['--chromium-arrowleft','--chromium-arrowright','--chromium-bound-pointer','--chromium-placement-only']:
            with self.assertRaises(SystemExit):outer.parse_args(accepted+[extra])

if __name__=='__main__':unittest.main()
