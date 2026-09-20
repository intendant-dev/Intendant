#!/usr/bin/env python3
"""Hermetic assessment checks; no GUI, native input or OS permissions."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec=importlib.util.spec_from_file_location('bound_pointer',Path(__file__).with_name('verify-macos-bound-pointer.py'))
h=importlib.util.module_from_spec(spec);spec.loader.exec_module(h)
class Tests(unittest.TestCase):
    def rig(self):
        p={'x':-700.,'y':270.,'local_x':100.,'local_y':270.,'client_x':100.,'client_y':183.}
        action={'status':'dispatched','posting_calls':2,'action_attempted':True,
                'effects_unconfirmed':True,'effect_verified':False,'focus_interference':False,
                'detail':None,'point':{'x':100.,'y':270.},'global':{'x':-700.,'y':270.}}
        n={'ok':True,'action':action}
        nonce='a'*32
        s={'nonce':nonce,'clicks':1,'overflow':False,'unrelated':0,'events':[
            {'type':kind,'trusted':True,'button':0,'buttons':buttons,'client_x':100.,'client_y':183.,'screen_x':-700.,'screen_y':270.}
            for kind,buttons in [('mousedown',1),('mouseup',0),('click',0)]]}
        return p,n,s,nonce
    def test_valid_dispatch_and_dom_remain_separate_evidence(self):
        p,n,s,k=self.rig();r=h.assess_bound(p,n,s,k)
        self.assertTrue(r['passed'] and r['effect_verified'])
        self.assertFalse(r['dom_tag_correlation'] or r['continuous_isolation_verified'])
        self.assertFalse(n['action']['effect_verified'])
    def test_no_effect_or_duplicate_event_cannot_pass(self):
        p,n,s,k=self.rig()
        for key,v in [('clicks',0),('clicks',2),('overflow',True),('unrelated',1),('events',[]),('nonce','b'*32)]:
            self.assertFalse(h.assess_bound(p,n,dict(s,**{key:v}),k)['passed'])
        s['events']+=s['events'];self.assertFalse(h.assess_bound(p,n,s,k)['passed'])
    def test_partial_unknown_native_result_cannot_be_promoted(self):
        p,n,s,k=self.rig()
        for key,v in [('posting_calls',1),('posting_calls',True),('effect_verified',True),('focus_interference',None),
                      ('status','partial'),('detail','expired'),('action_attempted',False)]:
            bad=copy.deepcopy(n);bad['action'][key]=v
            self.assertFalse(h.assess_bound(p,bad,s,k)['passed'])
        self.assertFalse(h.assess_bound(p,dict(n,ok=False),s,k)['passed'])
    def test_off_center_reflection_global_and_local_mismatches_refuse(self):
        p,n,s,k=self.rig()
        for a,b in [('point','y'),('global','x')]:
            bad=copy.deepcopy(n);bad['action'][a][b]+=8
            self.assertFalse(h.assess_bound(p,bad,s,k)['passed'])
        for key in ['client_x','client_y','screen_x','screen_y']:
            bad=copy.deepcopy(s);bad['events'][0][key]+=8
            self.assertFalse(h.assess_bound(p,n,bad,k)['passed'])
    def test_fixture_cannot_supply_tested_input(self):
        text=Path(h.__file__).read_text()
        self.assertNotIn('encode_plan(',text)
        self.assertNotIn('Input.dispatch',text)
        self.assertNotIn('--disposable-chromium-pointer',text)
        self.assertIn("call('click_macos_window'",text)
        self.assertIn("'--disposable-chromium'",text)
        self.assertIn("call('unbind_macos_window'",text)
if __name__=='__main__': unittest.main()
