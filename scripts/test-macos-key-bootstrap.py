#!/usr/bin/env python3
"""Hermetic click-prerequisite and routing-verifier regressions. No GUI or input."""
import copy
import importlib.util
import io
import json
from pathlib import Path
import struct
import sys
import tempfile
import unittest
from unittest.mock import patch
import macos_key_bootstrap as b

def load(name):
    spec=importlib.util.spec_from_file_location(name,Path(__file__).with_name(name+'.py'))
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);return module

class Bootstrap(unittest.TestCase):
    def setUp(self):
        self.metrics=dict(width=720,height=443,outer_width=720,outer_height=530,
            screen_x=-770,screen_y=30,scroll_x=0,scroll_y=0,scale=1,device_scale_factor=2)
        self.bounds=dict(X=-770,Y=30,Width=720,Height=530)
        self.rect=dict(x=24,y=83,width=400,height=200)
        self.plan=b.build_click_plan(self.rect,self.metrics,self.bounds,123,77)
        self.nonce='a'*32;self.key_tag=88;self.receiver=99;self.sender=100
        self.native=dict(dispatch_attempted=True,effect_verified=False,posted_events=2,
            source_pid=self.sender,target_pid=self.receiver,window_id=123,tag=77)
        event={k:self.plan[k] for k in ('client_x','client_y','screen_x','screen_y')}
        event.update(type='click',trusted=True,button=0,buttons=0,shift=False,ctrl=False,alt=False,meta=False)
        self.state=dict(nonce=self.nonce,active=True,click_overflow=False,click_unrelated=0,
            count=0,events=[],click_events=[event],metrics=self.metrics,receiver_rect=self.rect)
    def assess(self):
        return b.assess_click(self.plan,self.native,self.state,self.nonce,self.receiver,self.sender,self.key_tag)
    def receipt(self,challenge=123456):
        return b.encode_click_receipt(self.plan,self.native,self.state,self.nonce,
            self.receiver,self.sender,self.key_tag,challenge)
    def test_exact_off_center_plan_and_receipt(self):
        self.assertTrue(self.assess()['passed']);self.assertTrue(b.valid_click_plan(self.plan))
        encoded=self.receipt();self.assertEqual(len(encoded),137);self.assertEqual(encoded[:1],b'a')
        self.assertEqual(encoded[1:81],b.encode_click_plan(self.plan)[1:])
        tail=struct.unpack('<qQII4d',encoded[81:])
        self.assertEqual(tail[:4],(88,123456,1,1))
        self.assertEqual(tail[4:],(self.plan['client_x'],self.plan['client_y'],720,443))
        self.assertFalse(self.assess()['dom_tag_correlation'])
    def test_posting_without_dom_receipt_refuses(self):
        self.state['click_events']=[]
        with self.assertRaises(RuntimeError):self.receipt()
    def test_untrusted_or_modified_event_refuses(self):
        for key,value in [('trusted',False),('button',True),('buttons',1),('shift',True),('ctrl',None)]:
            with self.subTest(key=key):
                original=copy.deepcopy(self.state);self.state['click_events'][0][key]=value
                self.assertFalse(self.assess()['passed']);self.state=original
    def test_duplicate_or_partial_click_receipts_refuse(self):
        original=self.state['click_events']
        for events in ([],original*2,[None]):
            self.state['click_events']=events;self.assertFalse(self.assess()['passed'])
    def test_coordinate_reflection_and_shift_refuse(self):
        for key in ('client_x','client_y','screen_x','screen_y'):
            self.state['click_events'][0][key]+=1
            self.assertFalse(self.assess()['passed']);self.state['click_events'][0][key]-=1
    def test_changed_layout_refuses(self):
        for key in self.metrics:
            state=copy.deepcopy(self.state);state['metrics'][key]+=1
            self.assertFalse(b.assess_click(self.plan,self.native,state,self.nonce,99,100,88)['passed'])
    def test_changed_receiver_refuses(self):
        self.state['receiver_rect']=dict(self.rect,x=self.rect['x']+1)
        self.assertFalse(self.assess()['passed'])
    def test_wrong_native_identity_or_failure_refuses(self):
        for key,value in [('tag',88),('source_pid',101),('window_id',True),('posted_events',True),
                          ('target_pid',100),('effect_verified',True),('error','')]:
            original=dict(self.native);self.native[key]=value
            self.assertFalse(self.assess()['passed']);self.native=original
    def test_key_state_must_be_pristine(self):
        for key,value in [('count',1),('count',False),('active',False),('events',[{}]),
                          ('click_unrelated',1),('nonce','b'*32),('click_overflow',True)]:
            original=copy.deepcopy(self.state);self.state[key]=value
            self.assertFalse(self.assess()['passed']);self.state=original
    def test_challenge_types_and_bounds(self):
        for value in (0,-1,True,1.0,2**63,None):
            with self.subTest(value=value),self.assertRaises(RuntimeError):self.receipt(value)
    def test_plan_types_nonfinite_and_inconsistent_space(self):
        for key,value in [('window_id',True),('tag',0),('client_x',float('nan')),('local_y',0)]:
            plan=dict(self.plan,**{key:value});self.assertFalse(b.valid_click_plan(plan))
        with self.assertRaises(RuntimeError):
            b.build_click_plan(self.rect,dict(self.metrics,scale=2),self.bounds,123,77)
    def test_click_and_key_tags_must_differ(self):
        self.key_tag=77;self.assertFalse(self.assess()['passed'])
        self.assertEqual(b.freeze_click_tag(77,77),78)
        self.assertEqual(b.freeze_click_tag(b.MAX_TAG,b.MAX_TAG),1)

class LateReceipt(unittest.TestCase):
    def test_late_receipt_preserved_without_effect_claim(self):
        verifier=load('verify-macos-chromium-key')
        receipt={'accepted':True,'key_tag':88,'challenge':12,'dom_provenance_authenticated':False}
        report={'passed':False,'cleanup':{}}
        verifier.retain_final_evidence(report,{'supervisor_pid':100,'browser_pid':99,
            'click_receipt':receipt,'click_challenge':12},100,99)
        self.assertEqual(report['click_receipt'],receipt)
        self.assertEqual(report['final_click_challenge'],12)
        self.assertFalse(report['passed'])
    def test_contradictory_receipt_never_overwrites_original(self):
        verifier=load('verify-macos-chromium-key')
        report={'passed':True,'cleanup':{},'click_receipt':{'accepted':True}}
        verifier.retain_final_evidence(report,{'supervisor_pid':100,'browser_pid':99,
            'click_receipt':{'accepted':False}},100,99)
        self.assertFalse(report['passed']);self.assertEqual(report['click_receipt'],{'accepted':True})
        self.assertEqual(report['final_click_receipt'],{'accepted':False})

class VerifierExit(unittest.TestCase):
    def test_cooperative_completion_still_returns_failure(self):
        verifier=load('verify-macos-key-routing')
        assessment=dict(native_background_delivery_verified=False,control_effect_verified=True,
            diagnostic_valid=True,production_dispatch_enabled=False)
        with tempfile.TemporaryDirectory() as root:
            report=Path(root)/'evidence.json'
            argv=['verify','--fixture',sys.executable,'--report',str(report),
                  '--route','window-control','--allow-disposable-key-routing']
            with (patch.object(sys,'argv',argv),patch.object(verifier.platform,'system',return_value='Darwin'),
                 patch.object(verifier.runner,'run_probe',return_value=(0,{},'',123)),
                 patch.object(verifier,'assess_routing',return_value=assessment),patch('sys.stdout',new=io.StringIO())):
                self.assertEqual(verifier.main(),1)
            value=json.loads(report.read_text());self.assertFalse(value['passed'])
            self.assertTrue(value['control_diagnostic_completed'])

if __name__=='__main__':unittest.main()
