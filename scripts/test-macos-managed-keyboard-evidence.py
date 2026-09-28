#!/usr/bin/env python3
"""Hermetic evidence tests. Never opens a browser or posts native input."""
import copy
import unittest
from macos_managed_keyboard_evidence import verify_transition, verify_witness


def state():
    return dict(first='seed', second='', editor='rich seed', active='first',
                activations=0, protectedEvents=0, overflow=False, events=[])


def event(kind, key=None, receiver='first'):
    return dict(type=kind, id=receiver, key=key, trusted=True, repeat=False)


class KeyboardEvidence(unittest.TestCase):
    def test_insert_text_requires_input_not_physical_keys(self):
        before=state(); after=state(); after['first']='typed'; after['events']=[event('input')]
        result=verify_transition(before,after,{'first':'typed','second':''},minimum_input_events=1)
        self.assertTrue(result['effect_verified'])
        self.assertEqual(result['page_key_pairs'],0)
        self.assertFalse(result['physical_keyboard_provenance_verified'])
        self.assertFalse(result['continuous_isolation_verified'])

    def test_ack_without_effect_fails(self):
        with self.assertRaisesRegex(ValueError,'effect mismatch'):
            verify_transition(state(),state(),{'first':'typed'},minimum_input_events=1)

    def test_changed_value_without_input_event_fails(self):
        after=state(); after['first']='typed'
        with self.assertRaisesRegex(ValueError,'missing page input'):
            verify_transition(state(),after,{'first':'typed'},minimum_input_events=1)

    def test_tab_moves_receiver_without_retargeting_request(self):
        after=state(); after['active']='second'
        after['events']=[event('keydown','Tab'),event('keyup','Tab','second')]
        self.assertTrue(verify_transition(state(),after,{'active':'second','first':'seed'},
                                         key='Tab',receiver='first')['effect_verified'])

    def test_wrong_or_partial_or_repeated_key_rejected(self):
        for events in ([event('keydown','Enter')],
                       [event('keydown','Escape'),event('keyup','Escape')],
                       [event('keydown','Enter'),event('keyup','Enter')]*2):
            after=state(); after['events']=events
            with self.subTest(events=events),self.assertRaises(ValueError):
                verify_transition(state(),after,{},key='Enter',receiver='first')

    def test_synthetic_event_or_protected_input_or_overflow_fails(self):
        for change in ('untrusted','protected','overflow','unknown'):
            after=state(); after['events']=[event('input')]
            if change=='untrusted': after['events'][0]['trusted']=False
            elif change=='protected': after['protectedEvents']=1
            elif change=='overflow': after['overflow']=True
            else: after.pop('protectedEvents')
            with self.subTest(change=change),self.assertRaises(ValueError):
                verify_transition(state(),after,{},minimum_input_events=1)

    def test_event_history_cannot_be_replaced(self):
        before=state(); before['events']=[event('input')]
        after=copy.deepcopy(before); after['events'][0]['id']='second'
        with self.assertRaisesRegex(ValueError,'earlier evidence'):
            verify_transition(before,after,{'first':'seed'})

    def test_wrong_receiver_rejected(self):
        after=state(); after['events']=[event('keydown','Enter','editor'),event('keyup','Enter','editor')]
        with self.assertRaisesRegex(ValueError,'wrong key-down receiver'):
            verify_transition(state(),after,{},key='Enter',receiver='first')


class WitnessEvidence(unittest.TestCase):
    def witness(self):
        return dict(input_posting_calls=0, application_created=False, samples=[
            dict(monotonic_us=t,front_pid=42,front_started_ms=1000,
                 clipboard_change_count=7,hid_down=i,hid_up=i)
            for i,t in enumerate((10,20,30,40))])

    def test_stable_samples_with_activity_are_not_continuous_isolation(self):
        result=verify_witness(self.witness(),99,15,35)
        self.assertTrue(result['sampled_focus_unchanged'])
        self.assertTrue(result['keyboard_activity_observed_in_action_span'])
        self.assertFalse(result['continuous_isolation_verified'])
        self.assertFalse(result['human_provenance_verified'])

    def test_idle_is_not_relabelled_human_activity(self):
        value=self.witness()
        for sample in value['samples']: sample.update(hid_down=0,hid_up=0)
        self.assertFalse(verify_witness(value,99,15,35)['keyboard_activity_observed_in_action_span'])

    def test_unknown_or_changed_focus_clipboard_and_counters_refuse(self):
        for key,new in [('front_pid',99),('front_pid',71),('front_started_ms',2000),
                        ('front_pid',None),('clipboard_change_count',8),
                        ('hid_down',None),('hid_up',-1),('monotonic_us',10)]:
            value=self.witness();value['samples'][2][key]=new
            with self.subTest(key=key,new=new),self.assertRaises(ValueError):
                verify_witness(value,99,15,35)

    def test_pre_or_post_action_activity_cannot_manufacture_overlap(self):
        value=self.witness()
        for i,sample in enumerate(value['samples']):
            sample.update(hid_down=(1 if i<3 else 10),hid_up=(1 if i<3 else 10))
        self.assertFalse(verify_witness(value,99,15,35)['keyboard_activity_observed_in_action_span'])
        with self.assertRaisesRegex(ValueError,'bracket'):
            verify_witness(value,99,5,45)


if __name__=='__main__': unittest.main()
