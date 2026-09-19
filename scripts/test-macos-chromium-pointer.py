#!/usr/bin/env python3
"""Hermetic canvas-probe tests: no browser, native input or desktop access."""
import copy
import importlib.util
import struct
import unittest
from pathlib import Path
spec=importlib.util.spec_from_file_location('pointer_probe',Path(__file__).with_name('verify-macos-chromium-pointer.py'))
h=importlib.util.module_from_spec(spec);spec.loader.exec_module(h)

class Tests(unittest.TestCase):
    def rig(self):
        bounds={'X':-770,'Y':30,'Width':720,'Height':530}
        metrics={'screen_x':-770,'screen_y':30,'outer_width':720,'outer_height':530,
                 'inner_width':720,'inner_height':443,'scale':1,'scroll_x':0,'scroll_y':0,
                 'canvas':{'x':24,'y':84,'width':400,'height':200}}
        return bounds,metrics

    def proof(self):
        b,m=self.rig();p=h.plan_pointer(m,b,123,200,140,777)
        n={'dispatch_attempted':True,'posted_events':2,'target_pid':99,'source_pid':98,
           'window_id':123,'tag':777}
        events=[{'type':kind,'trusted':True,'button':0,'buttons':buttons,
                 'client_x':p['client_x'],'client_y':p['client_y'],'screen_x':p['x'],'screen_y':p['y']}
                for kind,buttons in [('mousedown',1),('mouseup',0),('click',0)]]
        s={'nonce':'a'*32,'clicks':1,'unrelated':0,'overflow':False,'events':events}
        return p,n,s

    def valid(self,p,n,s):
        return h.assess(p,n,s,'a'*32,99,98)['passed']

    def test_negative_screen_and_asymmetric_binary_layout(self):
        b,m=self.rig();p=h.plan_pointer(m,b,123,200,140,777)
        self.assertEqual(p['x'],-570);self.assertEqual(p['y'],257)
        self.assertEqual(p['local_y'],227)
        packet=h.encode_plan(p);self.assertEqual(len(packet),81)
        self.assertEqual(packet[0:1],b'p')
        self.assertEqual(struct.unpack('<II8dq',packet[1:]),(1,123,-770,30,720,530,-570,257,200,227,777))
        self.assertNotEqual(p['local_y'],b['Height']/2)

    def test_bad_geometry_zoom_and_nonfinite_refuse(self):
        for key,value in [('screen_x',22),('outer_height',531.1),('scale',2),('scroll_y',1),
                          ('inner_width',700),('inner_height',600),('outer_width',True),('scale',float('nan'))]:
            b,m=self.rig();m[key]=value
            with self.subTest(key=key),self.assertRaises(RuntimeError):h.plan_pointer(m,b,123,200,140,777)
        for x,y in [(0,0),(24,84),(500,140),(200,float('inf'))]:
            b,m=self.rig()
            with self.assertRaises(RuntimeError):h.plan_pointer(m,b,123,x,y,777)

    def test_identity_and_tag_refuse(self):
        b,m=self.rig()
        for window,tag in [(0,777),(True,777),(2**32,777),(123,0),(123,True),(123,2**63)]:
            with self.assertRaises(RuntimeError):h.plan_pointer(m,b,window,200,140,tag)

    def test_full_evidence_not_just_posting(self):
        p,n,s=self.proof();self.assertTrue(self.valid(p,n,s))
        result=h.assess(p,n,s,'a'*32,99,98)
        self.assertFalse(result['dom_tag_correlation']);self.assertFalse(result['continuous_isolation_verified'])
        for key,value in [('clicks',0),('clicks',2),('clicks',True),('nonce','b'*32),('overflow',True),('unrelated',1),('events',[])]:
            bad=copy.deepcopy(s);bad[key]=value;self.assertFalse(self.valid(p,n,bad),key)

    def test_wrong_source_or_native_result_refuses(self):
        p,n,s=self.proof()
        for key,value in [('dispatch_attempted',False),('posted_events',True),('posted_events',1),
                          ('source_pid',99),('target_pid',98),('window_id',124),('tag',778),('error','uncertain')]:
            bad=dict(n,**{key:value});self.assertFalse(self.valid(p,bad,s),key)

    def test_untrusted_reordered_duplicate_and_wrong_coordinates_refuse(self):
        p,n,s=self.proof()
        for field,value in [('trusted',False),('trusted',1),('button',1),('buttons',True),
                            ('client_y',148),('screen_y',999),('screen_x',float('nan'))]:
            bad=copy.deepcopy(s);bad['events'][0][field]=value;self.assertFalse(self.valid(p,n,bad),field)
        for events in [s['events'][::-1],s['events']*2,s['events'][:2],[None]*3]:
            self.assertFalse(self.valid(p,n,dict(s,events=events)))

    def test_inverted_origin_cannot_pass_an_off_center_click(self):
        p,n,s=self.proof();inverted=copy.deepcopy(s)
        wrong_y=p['bounds']['Y']+p['bounds']['Height']-p['local_y']
        for event in inverted['events']:
            event['screen_y']=wrong_y
            event['client_y']+=wrong_y-p['y']
        self.assertFalse(self.valid(p,n,inverted))

    def test_bad_supervised_identity_and_nonce_are_not_acceptance(self):
        p,n,s=self.proof()
        for receiver,sender in [(True,98),(99,True),(0,98),(99,99),(2**31,98)]:
            self.assertFalse(h.assess(p,n,s,'a'*32,receiver,sender)['passed'])
        for nonce in [None,False,'','a'*31,'g'*32]:
            self.assertFalse(h.assess(p,n,dict(s,nonce=nonce),nonce,99,98)['passed'])

    def test_malformed_canvas_is_refused_without_substitution(self):
        for value in [None,[],False,{'x':True,'y':84,'width':400,'height':200}]:
            b,m=self.rig();m['canvas']=value
            with self.assertRaises(RuntimeError):h.plan_pointer(m,b,123,200,140,777)

    def test_no_cdp_or_global_input_and_one_use_native_gate(self):
        root=Path(__file__).resolve().parent.parent
        native=(root/'tests/fixtures/macos-monitor/browser-pointer.h').read_text()
        owner=(root/'tests/fixtures/macos-monitor/browser.m').read_text()
        script=(root/'scripts/verify-macos-chromium-pointer.py').read_text()
        self.assertEqual(native.count('CGEventPostToPid(browser.processIdentifier,'),2)
        self.assertIn('if (pointerConsumed) pointerReplays++',owner)
        self.assertIn('pointerConsumed=YES',owner)
        self.assertIn('!browserEverFront',owner)
        for forbidden in ['CGEventPost(', 'CGWarpMouseCursorPosition(', 'CGEventTapCreate(', 'activateWithOptions:']:
            self.assertNotIn(forbidden,native)
        for forbidden in ['Input.dispatch','dispatchEvent(','Target.activateTarget','Browser.setWindowBounds']:
            self.assertNotIn(forbidden,script)

if __name__=='__main__':unittest.main()
