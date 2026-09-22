#!/usr/bin/env python3
import copy
import importlib.util
from pathlib import Path
import unittest
import macos_arrow_acceptance as a

class Evidence(unittest.TestCase):
    def fixture(self):
        native={'ok':True,'action':dict(key='ArrowRight',status='dispatched',posting_calls=2,effect_verified=False,
            effects_unconfirmed=True,action_attempted=True,focus_interference=False,receiver_unchanged=True,detail=None)}
        before=dict(active='first',events=[],caret=2,end=2,value='fixture-a')
        events=[dict(kind=k,target='first',key='ArrowRight',code='ArrowRight',trusted=True,repeat=False,alt=False,control=False,meta=False,shift=False) for k in ('keydown','keyup')]
        after=dict(active='first',events=events,caret=3,end=3,value='fixture-a',count=1,overflow=False)
        return native,before,after
    def test_exact_pair_and_caret_effect(self):
        n,b,e=self.fixture();r=a.verify_effect(n,b,e);self.assertTrue(r['effect_verified']);self.assertFalse(r['dom_tag_correlation'])
    def test_posting_is_not_delivery_or_effect(self):
        for mutate in [lambda e:e.update(events=[]),lambda e:e.update(caret=2,end=2),lambda e:e.update(count=0),lambda e:e.update(value='changed')]:
            n,b,e=self.fixture();mutate(e)
            with self.assertRaises(RuntimeError):a.verify_effect(n,b,e)
    def test_wrong_receiver_key_repeat_or_modifier_refuses(self):
        for k,v in [('target','second'),('key','ArrowLeft'),('code','KeyA'),('trusted',False),('repeat',True),('control',True),('shift',True)]:
            n,b,e=self.fixture();e['events'][0][k]=v
            with self.assertRaises(RuntimeError):a.verify_effect(n,b,e)
    def test_partial_or_uncertain_native_reply_refuses(self):
        for k,v in [('posting_calls',1),('posting_calls',True),('effect_verified',True),('receiver_unchanged',None),('focus_interference',None),('detail','native failure'),('status','partial')]:
            n,b,e=self.fixture();n['action'][k]=v
            with self.assertRaises(RuntimeError):a.verify_effect(n,b,e)
    def test_duplicate_reordered_boolean_or_overflow_receipt_refuses(self):
        for mutate in [lambda e:e['events'].reverse(),lambda e:e['events'].append(e['events'][0]),lambda e:e.update(count=True),lambda e:e.update(overflow=True)]:
            n,b,e=self.fixture();mutate(e)
            with self.assertRaises(RuntimeError):a.verify_effect(n,b,e)
    def test_outer_requires_separate_keyboard_and_click_opt_ins(self):
        spec=importlib.util.spec_from_file_location('outer',Path(__file__).with_name('verify-macos-monitor-http.py'));m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
        base=['--bin','controller','--fixture','pattern','--report','out','--allow-shared-session-monitor','--chromium-app','browser','--chromium-supervisor','supervisor']
        self.assertFalse(m.parse_args(base).chromium_arrowright)
        for flags in [[],['--chromium-keyboard-target']]:
            with self.assertRaises(SystemExit):m.parse_args(base+flags+['--chromium-arrowright'])
        self.assertTrue(m.parse_args(base+['--chromium-keyboard-target','--chromium-keyboard-target-click-first','--chromium-arrowright']).chromium_arrowright)

if __name__=='__main__':unittest.main()
