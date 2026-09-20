#!/usr/bin/env python3
"""Pure routing evidence tests; no applications or input."""
import copy
import unittest
from macos_key_routing import assess_routing, MODES
from macos_key_evidence import assess_self


class Routing(unittest.TestCase):
    def setUp(self):
        self.plan = dict(pid=100,source_pid=100,window_id=22,tag=987,keycode=124)
        pair = [dict(self.plan,kind=k,flags=1<<23,repeat=False,expected_character=True)
                for k in ('key_down','key_up')]
        sample = dict(front_pid=200,pointer_x=2,pointer_y=3,clipboard_change_count=4)
        trace = dict(app_active=False,key_window_id=0,main_window_id=0,
                     event_is_own_window=True,responder_is_view=True)
        self.negative = dict(mode=MODES['application'],cooperative_forwarding=False,
            plan=self.plan,queue_receipts=pair,window_receipts=[],receipts=[],
            routing=[trace.copy(),trace.copy()],posted_events=2,effect_count=0,
            overflow=False,window_closed=True,responder_unchanged=True,
            observations_complete=True,target_ever_front=False,before=sample,after=sample,
            ok=False,error='receiver key delivery/effect not verified; queue receipt is insufficient')

    def positive(self, control=True):
        n=copy.deepcopy(self.negative)
        n.update(mode=MODES['window-control'] if control else MODES['application'],
                 cooperative_forwarding=control,ok=True,error='',effect_count=1,
                 window_receipts=n['queue_receipts'],receipts=n['queue_receipts'])
        return n

    def test_queue_only_pinpoints_application_boundary(self):
        for route in ('application','psn'):
            n=dict(self.negative,mode=MODES[route]);r=assess_routing(n,100,route)
            self.assertTrue(r['diagnostic_valid'])
            self.assertEqual(r['routing_stop'],'application_to_window')
            self.assertFalse(r['native_background_delivery_verified'])

    def test_cooperative_control_is_not_native_support(self):
        n=self.positive();r=assess_routing(n,100,'window-control')
        self.assertTrue(r['diagnostic_valid']);self.assertTrue(r['control_effect_verified'])
        self.assertFalse(r['native_background_delivery_verified']);self.assertFalse(r['production_dispatch_enabled'])
        self.assertFalse(assess_routing(n,100,'application')['diagnostic_valid'])
        n['mode']=MODES['application']
        self.assertFalse(assess_self(n,100)['passed'])
        self.assertFalse(assess_routing(n,100,'application')['diagnostic_valid'])

    def test_real_receipt_path_remains_distinct(self):
        r=assess_routing(self.positive(False),100,'application')
        self.assertFalse(r['native_background_delivery_verified'])
        self.assertFalse(r['diagnostic_valid'])
        self.assertTrue(r['unexpected_receiver_effect'])
        self.assertFalse(r['control_effect_verified']);self.assertFalse(r['cross_process_verified'])

    def test_missing_or_contradictory_traces_refuse(self):
        for key,value in [('app_active',True),('key_window_id',22),('main_window_id',True),
                          ('event_is_own_window',1),('responder_is_view',False)]:
            n=copy.deepcopy(self.negative);n['routing'][0][key]=value
            self.assertFalse(assess_routing(n,100,'application')['diagnostic_valid'],key)
        for traces in (None,[],[{}]):
            self.assertFalse(assess_routing(dict(self.negative,routing=traces),100,'application')['diagnostic_valid'])

    def test_partial_reordered_and_forged_receipts_refuse(self):
        for receipts in ([{}],self.negative['queue_receipts'][:1],self.negative['queue_receipts'][::-1]):
            n=dict(self.negative,queue_receipts=receipts)
            self.assertFalse(assess_routing(n,100,'application')['diagnostic_valid'])
        n=self.positive();n['receipts'][0]['source_pid']=101
        self.assertFalse(assess_routing(n,100,'window-control')['diagnostic_valid'])

    def test_failed_cleanup_and_native_errors_not_success(self):
        for key,value in [('window_closed',False),('effect_count',True),('ok',True),
                          ('error','second call failed'),('posted_events',1),('overflow',True)]:
            self.assertFalse(assess_routing(dict(self.negative,**{key:value}),100,'application')['diagnostic_valid'],key)

    def test_desktop_changes_unattributed_and_activation_refused(self):
        n=dict(self.negative,after=dict(self.negative['after'],pointer_x=20))
        r=assess_routing(n,100,'application');self.assertTrue(r['diagnostic_valid'])
        self.assertEqual(r['desktop_observation_assessment']['change_attribution'],'undetermined')
        self.assertFalse(r['desktop_observation_assessment']['continuous_isolation_verified'])
        self.assertFalse(assess_routing(dict(n,target_ever_front=True),100,'application')['diagnostic_valid'])

    def test_malformed_mode_and_identity_refuse(self):
        for obj in (None,[],{},dict(self.negative,cooperative_forwarding=0)):
            self.assertFalse(assess_routing(obj,100,'application')['diagnostic_valid'])
        for pid in (None,True,101):
            self.assertFalse(assess_routing(self.negative,pid,'application')['diagnostic_valid'])


if __name__ == '__main__':
    unittest.main()
