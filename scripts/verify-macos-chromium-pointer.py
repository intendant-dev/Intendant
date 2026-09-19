#!/usr/bin/env python3
"""Opt-in native Chromium canvas acceptance; no daemon or CDP input injection.

Only an explicit Chrome for Testing bundle and new private profile are used.
A successful post is not a successful click. Native dispatch, independent DOM
observations, cleanup and sampled desktop changes are reported separately.
"""
import argparse
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import plistlib
import secrets
import shutil
import struct
import subprocess
import tempfile
import time
from macos_input_evidence import assess_desktop


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


transport = load('chromium_transport', 'verify-macos-chromium-controls.py')
raw = load('raw_pointer_supervisor', 'verify-macos-raw-pointer.py')


def require(value, detail):
    if not value:
        raise RuntimeError(str(detail))


def number(value):
    try:
        return type(value) in (int, float) and math.isfinite(value) and abs(value) <= 1000000
    except OverflowError:
        return False


def plan_pointer(metrics, bounds, window_id, cx, cy, tag):
    """All coordinates are logical points, not backing pixels. Never clamp."""
    keys = ('screen_x', 'screen_y', 'outer_width', 'outer_height', 'inner_width',
            'inner_height', 'scale', 'scroll_x', 'scroll_y')
    require(isinstance(metrics, dict) and all(number(metrics.get(k)) for k in keys), 'invalid metrics')
    require(isinstance(bounds, dict) and all(number(bounds.get(k)) for k in ('X','Y','Width','Height')), 'invalid bounds')
    require(type(window_id) is int and 0 < window_id < 2**32, 'invalid exact window')
    require(type(tag) is int and 0 < tag < 2**63, 'invalid correlation tag')
    require(number(cx) and number(cy), 'invalid point')
    require(metrics['scale'] == 1 and metrics['scroll_x'] == metrics['scroll_y'] == 0, 'zoom/scroll changed')
    require(all(abs(metrics[a]-bounds[b]) <= 1 for a,b in
                [('screen_x','X'),('screen_y','Y'),('outer_width','Width'),('outer_height','Height')]), 'browser/native bounds disagree')
    require(metrics['inner_width'] == metrics['outer_width'], 'unsupported horizontal browser border')
    top = metrics['outer_height'] - metrics['inner_height']
    require(0 <= top <= 200 and 200 <= bounds['Width'] <= 16384 and 200 <= bounds['Height'] <= 16384, 'invalid viewport')
    canvas = metrics.get('canvas', {})
    require(isinstance(canvas,dict) and all(number(canvas.get(k)) for k in ('x','y','width','height')), 'invalid canvas')
    require(canvas['width'] > 40 and canvas['height'] > 40 and
            canvas['x']+20 <= cx < canvas['x']+canvas['width']-20 and
            canvas['y']+20 <= cy < canvas['y']+canvas['height']-20 and
            0 <= cx < metrics['inner_width'] and 0 <= cy < metrics['inner_height'], 'point outside canvas/viewport')
    x, y = bounds['X']+cx, bounds['Y']+top+cy
    lx, ly = x-bounds['X'], y-bounds['Y']
    require(24 <= lx <= bounds['Width']-24 and 24 <= ly <= bounds['Height']-24, 'point outside native interior')
    return {'window_id':window_id,'tag':tag,'x':x,'y':y,'local_x':lx,'local_y':ly,
            'client_x':cx,'client_y':cy,'bounds':dict(bounds)}


def encode_plan(plan):
    b=plan['bounds']
    return b'p'+struct.pack('<II8dq',1,plan['window_id'],b['X'],b['Y'],b['Width'],b['Height'],
                            plan['x'],plan['y'],plan['local_x'],plan['local_y'],plan['tag'])


def assess(plan, native, state, nonce, receiver, sender):
    result={'passed':False,'effect_verified':False,'dom_tag_correlation':False,
            'continuous_isolation_verified':False}
    if not all(isinstance(x,dict) for x in (plan,native,state)):
        return result
    if any(type(pid) is not int or not 0 < pid < 2**31 for pid in (receiver,sender)) or receiver==sender:
        return result
    if not isinstance(nonce,str) or len(nonce)!=32 or any(c not in '0123456789abcdef' for c in nonce):
        return result
    if (native.get('dispatch_attempted') is not True or type(native.get('posted_events')) is not int
            or native['posted_events'] != 2 or native.get('error') or
            any(type(native.get(k)) is not int or native[k] != v for k,v in
                [('target_pid',receiver),('source_pid',sender),('window_id',plan.get('window_id')),('tag',plan.get('tag'))])):
        return result
    events=state.get('events')
    if (state.get('nonce') != nonce or type(state.get('clicks')) is not int or state['clicks'] != 1
            or state.get('overflow') is not False or type(state.get('unrelated')) is not int
            or state['unrelated'] != 0 or not isinstance(events,list) or len(events) != 3):
        return result
    for event, kind, buttons in zip(events,('mousedown','mouseup','click'),(1,0,0)):
        if (not isinstance(event,dict) or event.get('type') != kind or event.get('trusted') is not True
                or type(event.get('button')) is not int or event['button'] != 0
                or type(event.get('buttons')) is not int or event['buttons'] != buttons):
            return result
        for a,b in [('client_x','client_x'),('client_y','client_y'),('screen_x','x'),('screen_y','y')]:
            if not number(event.get(a)) or not number(plan.get(b)) or abs(event[a]-plan[b])>1:
                return result
    result.update(passed=True,effect_verified=True)
    return result


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--browser-app',required=True,type=Path)
    parser.add_argument('--supervisor',required=True,type=Path)
    parser.add_argument('--report',required=True,type=Path)
    parser.add_argument('--allow-disposable-chromium-click',action='store_true')
    args=parser.parse_args()
    if platform.system()!='Darwin' or not args.allow_disposable_chromium_click:
        parser.error('requires macOS and explicit disposable Chromium click opt-in')
    bundle=args.browser_app.resolve(strict=True); supervisor=args.supervisor.resolve(strict=True)
    info=plistlib.loads((bundle/'Contents/Info.plist').read_bytes())
    require(info['CFBundleIdentifier']=='com.google.chrome.for.testing','only Chrome for Testing accepted')
    fd=raw.reserve_report(args.report)
    report={'passed':False,'production_dispatch_enabled':False,'browser_version':info['CFBundleShortVersionString'],
            'checks':{},'cleanup':{},'on_virtual_monitor':False}
    root=None
    child=cdp=None; status_path=None; receiver=None
    last_tick=-1; last_tick_time=time.monotonic()
    try:
        root=Path(tempfile.mkdtemp(prefix='intendant-chromium-pointer-'))
        status_path=root/'status.json'
        profile=root/'profile'; profile.mkdir(mode=0o700)
        page=(Path(__file__).resolve().parent.parent/'tests/fixtures/macos-monitor/browser-pointer.html').as_uri()
        child=subprocess.Popen([str(supervisor),'--disposable-chromium-pointer',str(bundle),str(profile),str(status_path),page],
                               stdin=subprocess.PIPE,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        end=time.monotonic()+120
        def status():
            nonlocal last_tick, last_tick_time
            require(child.poll() is None and status_path.exists(),'supervisor unavailable')
            require(status_path.stat().st_size <= 16384,'supervisor output limit')
            value=raw.strict_json(status_path.read_text())
            require(value.get('supervisor_pid')==child.pid and value.get('browser_terminated') is False,'owner mismatch')
            require(value.get('browser_ever_front') is False,'browser observed foreground')
            require(isinstance(value.get('observation'),dict),'desktop observation unavailable')
            pid=value.get('browser_pid')
            require(type(pid) is int and 0 < pid < 2**31 and (receiver is None or receiver==pid),'browser identity changed')
            desktop=assess_desktop(value['observation'],value['observation'],pid,False)
            require(desktop['observation_status']=='no_sampled_change' and desktop['target_foreground_observed'] is False,'desktop observation incomplete')
            tick=value.get('tick')
            require(type(tick) is int and tick >= last_tick,'invalid or regressed supervisor tick')
            if tick != last_tick:
                last_tick=tick; last_tick_time=time.monotonic()
            require(time.monotonic()-last_tick_time < 2,'supervisor heartbeat stalled')
            return value
        def wait(predicate, seconds=10):
            stop=min(end,time.monotonic()+seconds)
            while time.monotonic()<stop:
                if predicate(): return
                time.sleep(.05)
            raise RuntimeError('fixture observation deadline; no input retry')
        active=profile/'DevToolsActivePort'
        wait(lambda:active.exists() and status_path.exists(),25)
        require(active.stat().st_size<=8192,'CDP address limit')
        port,path=active.read_text().splitlines()[:2]; cdp=transport.CDP(int(port),path)
        current=status(); receiver=current['browser_pid']
        report['before']=current['before']; report['receiver_pid']=receiver; report['sender_pid']=child.pid
        processes=cdp.call('SystemInfo.getProcessInfo')['processInfo']
        require(any(x['type']=='browser' and x['id']==receiver for x in processes),'CDP ownership mismatch')
        target=cdp.call('Target.createTarget',{'url':page,'newWindow':True,'background':True,'width':720,'height':530})['targetId']
        session=cdp.call('Target.attachToTarget',{'targetId':target,'flatten':True})['sessionId']
        def evaluate(expression):
            value=cdp.call('Runtime.evaluate',{'expression':expression,'returnByValue':True},session)
            require('exceptionDetails' not in value,'fixture evaluation failed')
            return value['result'].get('value')
        wait(lambda:evaluate("document.readyState==='complete' && typeof pointerFixtureState==='function'"))
        wait(lambda:len(status().get('windows',[]))==1)
        window=status()['windows'][0]; bounds=window['bounds']
        browser_bounds=cdp.call('Browser.getWindowForTarget',{'targetId':target})['bounds']
        require(all(abs(browser_bounds[a]-bounds[b])<=1 for a,b in
                    [('left','X'),('top','Y'),('width','Width'),('height','Height')]),'independent geometry mismatch')
        initial=evaluate('pointerFixtureState()'); report['initial']=initial
        require(initial['clicks']==0 and initial['events']==[] and initial['nonce'] is None,'fixture not pristine')
        r=initial['metrics']['canvas']; cx=round(r['x']+r['width']/2)+secrets.randbelow(31)-15
        cy=round(r['y']+r['height']/3)+secrets.randbelow(31)-15
        plan=plan_pointer(initial['metrics'],bounds,window['window_id'],cx,cy,secrets.randbelow(2**63-1)+1)
        report['plan']=plan; nonce=secrets.token_hex(16)
        require(evaluate('armPointer('+json.dumps(nonce)+')') is True,'fixture arm failed')
        require(evaluate('pointerFixtureState().metrics')==initial['metrics'],'viewport changed before dispatch')
        require(status()['windows']==[window],'native window changed before dispatch')
        child.stdin.write(encode_plan(plan)); child.stdin.flush()
        wait(lambda:'pointer_result' in status(),5)
        native=status()['pointer_result']; report['native_dispatch']=native
        stop=time.monotonic()+2
        observed=evaluate('pointerFixtureState()')
        while len(observed['events'])<3 and time.monotonic()<stop:
            time.sleep(.05); observed=evaluate('pointerFixtureState()')
        report['dom_after']=observed
        assessment=assess(plan,native,observed,nonce,receiver,child.pid); report['assessment']=assessment
        require(assessment['passed'],'native dispatch/DOM canvas evidence did not verify')
        require(observed['metrics']==initial['metrics'],'viewport changed during dispatch')
        require(status()['windows']==[window],'native window changed during dispatch')
        # Explicit replay negative: the supervisor must refuse a second frame, not post it.
        child.stdin.write(encode_plan(plan)); child.stdin.flush()
        wait(lambda:status().get('pointer_replays_refused')==1,5)
        require(status()['pointer_result']==native,'replay replaced original dispatch evidence')
        require(evaluate('pointerFixtureState()')==observed,'replay changed canvas evidence')
        report['checks']['replay_refused']=True
        report['passed']=True
    except Exception as error:
        report['error']=str(error)
    finally:
        if cdp:
            try: cdp.close()
            except OSError as error:
                report['cleanup']['cdp_error']=str(error); report['passed']=False
        if child:
            try:
                if child.poll() is None:
                    try:
                        child.stdin.write(b'q');child.stdin.flush()
                    except (BrokenPipeError,OSError):
                        pass
                    finally:
                        child.stdin.close()
                child.wait(timeout=20)
                require(status_path.exists() and status_path.stat().st_size<=16384,'final status unavailable')
                final=raw.strict_json(status_path.read_text())
                require(final.get('supervisor_pid')==child.pid and final.get('launch_finished') is True
                        and final.get('browser_pid',0)>0 and final.get('browser_terminated') is True,'browser cleanup unconfirmed')
                report['cleanup']={'browser_terminated':True,'supervisor_reaped':True,'exit_code':child.returncode}
                report['after']=final.get('observation')
                report['desktop_observation_assessment']=assess_desktop(report.get('before'),report['after'],
                                                                       final['browser_pid'],final.get('browser_ever_front'))
                require(final.get('browser_ever_front') is False,'browser observed foreground during run')
                require(receiver is None or final['browser_pid']==receiver,'final browser identity changed')
                require(all(v is not None for v in report['desktop_observation_assessment']['sampled_changes'].values()),'final desktop observation incomplete')
                require(child.returncode==0,'supervisor failed')
            except Exception as error:
                report['cleanup']['error']=str(error);report['passed']=False
        if root:
            if not child or report['cleanup'].get('browser_terminated'):
                try:
                    shutil.rmtree(root);report['cleanup']['profile_removed']=True
                except OSError as error:
                    report['cleanup']['profile_error']=str(error);report['passed']=False
            else:
                report['cleanup']['profile_retained']=str(root)
        with os.fdopen(fd,'w') as output:
            json.dump(report,output,indent=2,allow_nan=False);output.write('\n')
    print(json.dumps({'passed':report['passed'],'error':report.get('error'),'cleanup':report['cleanup']}))
    return 0 if report['passed'] else 1


if __name__=='__main__':
    raise SystemExit(main())
