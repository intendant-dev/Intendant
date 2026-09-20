#!/usr/bin/env python3
"""Hermetic key-evidence tests: no GUI, user input, OS queries or event posting."""
import copy
import importlib.util
from pathlib import Path
import unittest
import subprocess
import sys
from macos_key_evidence import assess_browser, assess_self, key_receipts

spec=importlib.util.spec_from_file_location('key_probe',Path(__file__).with_name('verify-macos-chromium-key.py'))
probe=importlib.util.module_from_spec(spec); spec.loader.exec_module(probe)

class Evidence(unittest.TestCase):
    def setUp(self):
        self.plan={'pid':100,'source_pid':100,'window_id':33,'tag':999,'keycode':124}
        self.sample={'front_pid':200,'pointer_x':5,'pointer_y':6,'clipboard_change_count':7}
        self.receipts=[dict(self.plan,kind=k,flags=1<<23,repeat=False,expected_character=True)
                       for k in ('key_down','key_up')]
        self.native={'mode':'self_process_key','ok':True,'posted_events':2,'error':'',
            'plan':self.plan,'queue_receipts':self.receipts,'receipts':self.receipts,
            'effect_count':1,'overflow':False,'window_closed':True,'responder_unchanged':True,
            'observations_complete':True,'target_ever_front':False,'before':self.sample,'after':self.sample}
        self.nonce='a'*32
        self.browser={'dispatch_attempted':True,'posted_events':2,'effect_verified':False,
            'source_pid':99,'target_pid':100,'window_id':33,'tag':999,'keycode':124}
        self.dom={'nonce':self.nonce,'count':1,'active':True,'overflow':False,'unrelated':0,
            'events':[dict(type=k,key='ArrowRight',code='ArrowRight',trusted=True,repeat=False,
                shift=False,ctrl=False,alt=False,meta=False,target='receiver') for k in ('keydown','keyup')]}

    def test_exact_self_evidence(self):
        r=assess_self(self.native,100)
        self.assertTrue(r['passed']);self.assertTrue(r['effect_verified'])
        self.assertFalse(r['production_dispatch_enabled']);self.assertFalse(r['cross_process_verified'])

    def test_queue_only_is_not_receiver_delivery(self):
        n=dict(self.native,ok=False,receipts=[],effect_count=0)
        r=assess_self(n,100)
        self.assertTrue(r['queue_delivery_verified'])
        self.assertFalse(r['receiver_delivery_verified']);self.assertFalse(r['passed'])

    def test_posting_alone_and_missing_queue_do_not_pass(self):
        for k,v in [('receipts',[]),('queue_receipts',[]),('effect_count',0),('posted_events',0),
                    ('overflow',True),('observations_complete',False),('window_closed',False),
                    ('responder_unchanged',False),('error','posting exception'),('after',{})]:
            self.assertFalse(assess_self(dict(self.native,**{k:v}),100)['passed'],k)

    def test_live_owner_changes_are_not_attributed(self):
        n=dict(self.native,after=dict(self.sample,pointer_x=20,front_pid=300))
        r=assess_self(n,100);self.assertTrue(r['passed'])
        self.assertEqual(r['desktop_observation_assessment']['change_attribution'],'undetermined')
        self.assertFalse(r['desktop_observation_assessment']['continuous_isolation_verified'])
        self.assertFalse(assess_self(dict(n,target_ever_front=True),100)['passed'])

    def test_receipt_identity_and_flags_are_exact(self):
        for k,v in [('pid',101),('source_pid',99),('window_id',34),('tag',1000),('keycode',125),
                    ('flags',0),('flags',(1<<23)|(1<<20)),('repeat',True),('expected_character',False)]:
            r=copy.deepcopy(self.receipts);r[0][k]=v
            self.assertFalse(key_receipts(self.plan,r),k)
        self.assertFalse(assess_self(self.native,101)['passed'])

    def test_missing_duplicate_reordered_receipts(self):
        for r in [None,[],self.receipts[:1],self.receipts[::-1],self.receipts*2,[{}]]:
            self.assertFalse(key_receipts(self.plan,r))

    def test_malformed_types_and_boolean_integers(self):
        for k,v in [('pid',True),('tag',True),('window_id',2**32),('keycode',False)]:
            self.assertFalse(key_receipts(dict(self.plan,**{k:v}),self.receipts))
        for v in [None,[],{},'bad']:
            self.assertFalse(assess_self(v,100)['passed'])
            self.assertFalse(assess_browser(self.plan,self.browser,v,self.nonce,100,99)['passed'])

    def test_exact_browser_effect(self):
        r=assess_browser(self.plan,self.browser,self.dom,self.nonce,100,99)
        self.assertTrue(r['passed']);self.assertFalse(r['dom_tag_correlation'])
        self.assertFalse(r['continuous_isolation_verified'])

    def test_browser_delivery_without_effect_refuses(self):
        for k,v in [('count',0),('count',2),('count',True),('events',[]),('active',False),
                    ('nonce','b'*32),('overflow',True),('unrelated',1)]:
            self.assertFalse(assess_browser(self.plan,self.browser,dict(self.dom,**{k:v}),self.nonce,100,99)['passed'])

    def test_browser_modifiers_repeats_untrusted_and_wrong_target_refuse(self):
        for k,v in [('key','ArrowLeft'),('code','KeyA'),('trusted',False),('repeat',True),
                    ('shift',True),('ctrl',True),('alt',True),('meta',True),('target','other')]:
            d=copy.deepcopy(self.dom);d['events'][0][k]=v
            self.assertFalse(assess_browser(self.plan,self.browser,d,self.nonce,100,99)['passed'],k)
        d=dict(self.dom,events=self.dom['events'][::-1])
        self.assertFalse(assess_browser(self.plan,self.browser,d,self.nonce,100,99)['passed'])

    def test_browser_native_exception_or_wrong_identity_refuses(self):
        for k,v in [('error','second-call failure'),('posted_events',1),('effect_verified',True),
                    ('keycode',125),('source_pid',100),('target_pid',99),('tag',True),('window_id',34)]:
            self.assertFalse(assess_browser(self.plan,dict(self.browser,**{k:v}),self.dom,self.nonce,100,99)['passed'],k)

    def test_key_frame_is_bounded_and_has_no_caller_pid(self):
        p=dict(self.plan,bounds={'X':-800,'Y':0,'Width':720,'Height':530})
        frame=probe.encode_plan(p);self.assertEqual(frame[:1],b'k');self.assertEqual(len(frame),49)
        for k,v in [('X',float('nan')),('Width',100),('Height',17000)]:
            with self.assertRaises(RuntimeError): probe.encode_plan(dict(p,bounds=dict(p['bounds'],**{k:v})))

class Cleanup(unittest.TestCase):
    def test_timeout_reaps_only_the_owned_child(self):
        child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(20)'],stdin=subprocess.PIPE)
        try:
            probe.reap_supervisor(child,browser_terminated=True,grace=.1)
            self.assertIsNotNone(child.poll())
            self.assertTrue(child.stdin.closed)
        finally:
            if child.poll() is None:
                child.kill();child.wait(timeout=2)

    def test_already_exited_child_is_reaped(self):
        child=subprocess.Popen([sys.executable,'-c','pass'],stdin=subprocess.PIPE)
        child.wait(timeout=2)
        probe.reap_supervisor(child)
        self.assertEqual(child.returncode,0)
        self.assertTrue(child.stdin.closed)

class ShutdownEvidence(unittest.TestCase):
    def test_unconfirmed_browser_keeps_its_native_owner_alive(self):
        child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(20)'],stdin=subprocess.PIPE)
        try:
            self.assertFalse(probe.reap_supervisor(child))
            self.assertIsNone(child.poll())
            self.assertTrue(child.stdin.closed)
        finally:
            child.kill();child.wait(timeout=2)

    def test_late_exception_evidence_survives_browser_exit(self):
        result={'passed':False,'cleanup':{}}
        native={'posted_events':2,'error':'second call failed'}
        final={'supervisor_pid':10,'browser_pid':11,'browser_terminated':True,
               'key_result':native,'key_replays_refused':1}
        probe.retain_final_evidence(result,final,10,11)
        self.assertEqual(result['native_dispatch'],native)
        self.assertEqual(result['final_replays_refused'],1)
        self.assertFalse(result['passed'])
        with self.assertRaises(RuntimeError): probe.retain_final_evidence(result,final,12,11)
        result['passed']=True
        probe.retain_final_evidence(result,dict(final,key_result={'posted_events':0}),10,11)
        self.assertFalse(result['passed'])
        self.assertEqual(result['native_dispatch'],native)

if __name__=='__main__': unittest.main()
