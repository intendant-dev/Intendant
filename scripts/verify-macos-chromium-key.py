#!/usr/bin/env python3
"""Opt-in background ArrowRight acceptance; no daemon or CDP input injection.

Only an explicit Chrome for Testing bundle and new private profile are used.
A successful post is not a verified keyboard effect. Native dispatch, independent DOM
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
from macos_key_evidence import assess_browser as assess, integer


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


transport = load('chromium_transport', 'verify-macos-chromium-controls.py')
raw = load('raw_pointer_supervisor', 'verify-macos-raw-pointer.py')
bootstrap = load('macos_key_bootstrap', 'macos_key_bootstrap.py')


def require(value, detail):
    if not value:
        raise RuntimeError(str(detail))


def number(value):
    try:
        return type(value) in (int, float) and math.isfinite(value) and abs(value) <= 1000000
    except OverflowError:
        return False


def encode_plan(plan):
    b=plan['bounds']
    require(integer(plan.get('window_id'),1,2**32-1) and integer(plan.get('tag'),1,2**63-1), 'invalid key plan')
    require(all(number(b.get(k)) for k in ('X','Y','Width','Height')), 'invalid key bounds')
    require(200 <= b['Width'] <= 16384 and 200 <= b['Height'] <= 16384, 'invalid key dimensions')
    return b'k'+struct.pack('<II4dq',1,plan['window_id'],b['X'],b['Y'],b['Width'],b['Height'],plan['tag'])


def request_owned_browser_close(cdp, ownership_verified):
    """One orderly close over this fixture's PID-verified private connection."""
    result={'requested':False}
    if cdp is None or not ownership_verified:
        return result
    result['requested']=True
    try:
        cdp.call('Browser.close')
    except Exception as error:
        # Closing may drop the socket before a reply; native exit remains the proof.
        result['reply_error']=str(error)
    return result


def close_control_input(child):
    try:
        child.stdin.close()
    except (BrokenPipeError, OSError) as error:
        return str(error)
    return None


def reap_supervisor(child, browser_terminated=False, grace=2):
    """Never kill the only cleanup owner of a separately launched browser."""
    if child.stdin is not None and not child.stdin.closed:
        try:
            child.stdin.close()
        except (BrokenPipeError, OSError):
            pass
    if child.poll() is None:
        if not browser_terminated:
            # EOF requests cleanup; keep the exact native browser owner alive.
            return False
        child.terminate()
        try:
            child.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=grace)
    else:
        child.wait(timeout=grace)
    return True


def retain_final_evidence(report, final, supervised_pid, receiver=None):
    """Preserve late native results without turning them into effect proof."""
    require(isinstance(final,dict) and type(final.get('supervisor_pid')) is int
            and final['supervisor_pid']==supervised_pid, 'final owner mismatch')
    require(receiver is None or final.get('browser_pid')==receiver, 'final browser mismatch')
    if 'key_result' in final:
        report['final_native_dispatch']=final['key_result']
        if 'native_dispatch' not in report:
            report['native_dispatch']=final['key_result']
        elif report['native_dispatch'] != final['key_result']:
            report['passed']=False
            report['cleanup']['evidence_error']='native dispatch changed before shutdown'
    report['final_replays_refused']=final.get('key_replays_refused')
    if 'click_result' in final:
        report['final_native_click_dispatch']=final['click_result']
        if 'native_click_dispatch' not in report:
            report['native_click_dispatch']=final['click_result']
        elif report['native_click_dispatch'] != final['click_result']:
            report['passed']=False
            report['cleanup']['click_evidence_error']='native click dispatch changed before shutdown'
    if 'click_plan' in final:
        report['final_native_click_plan']=final['click_plan']
        if 'native_click_plan' not in report:
            report['native_click_plan']=final['click_plan']
        elif report['native_click_plan'] != final['click_plan']:
            report['passed']=False
            report['cleanup']['click_plan_error']='native click plan changed before shutdown'
    if 'click_receipt' in final:
        report['final_click_receipt']=final['click_receipt']
        if 'click_receipt' not in report:
            report['click_receipt']=final['click_receipt']
        elif report['click_receipt'] != final['click_receipt']:
            report['passed']=False
            report['cleanup']['receipt_evidence_error']='click acknowledgement changed before shutdown'
    if 'click_challenge' in final:
        report['final_click_challenge']=final['click_challenge']
    if 'click_replays_refused' in final:
        report['final_click_replays_refused']=final['click_replays_refused']


def parse_args(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--browser-app',required=True,type=Path)
    parser.add_argument('--supervisor',required=True,type=Path)
    parser.add_argument('--report',required=True,type=Path)
    parser.add_argument('--allow-disposable-chromium-key',action='store_true')
    parser.add_argument('--native-click-first',action='store_true')
    args=parser.parse_args(argv)
    if platform.system()!='Darwin' or not args.allow_disposable_chromium_key:
        parser.error('requires macOS and explicit disposable Chromium key opt-in')
    return args


def main():
    args=parse_args()
    bundle=args.browser_app.resolve(strict=True); supervisor=args.supervisor.resolve(strict=True)
    info=plistlib.loads((bundle/'Contents/Info.plist').read_bytes())
    require(info['CFBundleIdentifier']=='com.google.chrome.for.testing','only Chrome for Testing accepted')
    fd=raw.reserve_report(args.report)
    report={'passed':False,'production_dispatch_enabled':False,'browser_version':info['CFBundleShortVersionString'],
            'checks':{},'cleanup':{},'on_virtual_monitor':False, 'native_click_first':args.native_click_first}
    root=None
    child=cdp=None; status_path=None; receiver=None; cdp_owned=False
    last_tick=-1; last_tick_time=time.monotonic()
    try:
        root=Path(tempfile.mkdtemp(prefix='intendant-chromium-click-key-'))
        status_path=root/'status.json'
        profile=root/'profile'; profile.mkdir(mode=0o700)
        page=(Path(__file__).resolve().parent.parent/'tests/fixtures/macos-monitor/browser-key.html').as_uri()
        child=subprocess.Popen([str(supervisor),'--disposable-chromium-click-key' if args.native_click_first else '--disposable-chromium-key',str(bundle),str(profile),str(status_path),page],
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
        cdp_owned=True
        target=cdp.call('Target.createTarget',{'url':page,'newWindow':True,'background':True,'width':720,'height':530})['targetId']
        session=cdp.call('Target.attachToTarget',{'targetId':target,'flatten':True})['sessionId']
        def evaluate(expression):
            value=cdp.call('Runtime.evaluate',{'expression':expression,'returnByValue':True},session)
            require('exceptionDetails' not in value,'fixture evaluation failed')
            return value['result'].get('value')
        wait(lambda:evaluate("document.readyState==='complete' && typeof keyFixtureState==='function'"))
        wait(lambda:len(status().get('windows',[]))==1)
        window=status()['windows'][0]; bounds=window['bounds']
        browser_bounds=cdp.call('Browser.getWindowForTarget',{'targetId':target})['bounds']
        require(all(abs(browser_bounds[a]-bounds[b])<=1 for a,b in
                    [('left','X'),('top','Y'),('width','Width'),('height','Height')]),'independent geometry mismatch')
        initial=evaluate('keyFixtureState()'); report['initial']=initial
        require(initial['count']==0 and initial['events']==[] and initial['nonce'] is None
                and initial['click_events']==[] and initial['click_unrelated']==0
                and initial['click_overflow'] is False,'fixture not pristine')
        plan={'window_id':window['window_id'],'bounds':bounds,'tag':secrets.randbelow(2**63-1)+1}
        report['plan']=plan; nonce=secrets.token_hex(16)
        if args.native_click_first:
            click_tag=bootstrap.freeze_click_tag(plan['tag'],secrets.randbelow(2**63-1)+1)
            click_plan=bootstrap.build_click_plan(initial['receiver_rect'],initial['metrics'],bounds,window['window_id'],click_tag)
            report['click_plan']=click_plan
            report['native_click_plan']=bootstrap.native_click_evidence(click_plan)
        require(evaluate('armKey('+json.dumps(nonce)+')') is True,'fixture arm failed')
        require(evaluate('keyFixtureState().metrics')==initial['metrics'],'viewport changed before dispatch')
        require(evaluate('keyFixtureState().active') is True,'fixture receiver not focused')
        require(status()['windows']==[window],'native window changed before dispatch')
        if args.native_click_first:
            # The only click frame is frozen from independent current DOM/native geometry.
            child.stdin.write(bootstrap.encode_click_plan(click_plan)); child.stdin.flush()
            wait(lambda:'click_result' in status(),5)
            native_click=status()['click_result']; report['native_click_dispatch']=native_click
            require(status().get('click_plan')==report['native_click_plan'],'native click plan changed or mismatched')
            stop=time.monotonic()+2
            click_observed=evaluate('keyFixtureState()')
            while len(click_observed['click_events'])<1 and time.monotonic()<stop:
                time.sleep(.05); click_observed=evaluate('keyFixtureState()')
            report['dom_after_click']=click_observed
            click_assessment=bootstrap.assess_click(click_plan,native_click,click_observed,nonce,receiver,child.pid,plan['tag'])
            report['click_assessment']=click_assessment
            require(click_assessment['passed'],'native click/DOM receipt did not verify')
            require(status()['windows']==[window],'native window changed after click')
            require(click_observed['metrics']==initial['metrics'] and click_observed['active'] is True,
                    'browser layout or receiver changed after click')
            # A replay is a negative test, never a retry or fallback.
            child.stdin.write(bootstrap.encode_click_plan(click_plan)); child.stdin.flush()
            wait(lambda:status().get('click_replays_refused')==1,5)
            require(status()['click_result']==native_click and status().get('click_plan')==report['native_click_plan'],
                    'click replay replaced original native evidence')
            require(evaluate('keyFixtureState()')==click_observed,'click replay changed DOM evidence')
            report['checks']['click_replay_refused']=True
            challenge=status().get('click_challenge')
            acknowledgement=bootstrap.encode_click_receipt(click_plan,native_click,
                click_observed,nonce,receiver,child.pid,plan['tag'],challenge)
            child.stdin.write(acknowledgement); child.stdin.flush()
            wait(lambda:'click_receipt' in status(),5)
            report['click_receipt']=status()['click_receipt']
            require(report['click_receipt']=={'accepted':True,'key_tag':plan['tag'],
                'challenge':challenge,'dom_provenance_authenticated':False},'native click acknowledgement refused')

        require(status()['windows']==[window],'native window changed immediately before key')
        require(evaluate('keyFixtureState().receiver_rect')==initial['receiver_rect'],
                'receiver geometry changed immediately before key')
        child.stdin.write(encode_plan(plan)); child.stdin.flush()
        wait(lambda:'key_result' in status(),5)
        native=status()['key_result']; report['native_dispatch']=native
        stop=time.monotonic()+2
        observed=evaluate('keyFixtureState()')
        while len(observed['events'])<2 and time.monotonic()<stop:
            time.sleep(.05); observed=evaluate('keyFixtureState()')
        report['dom_after']=observed
        assessment=assess(plan,native,observed,nonce,receiver,child.pid); report['assessment']=assessment
        require(observed['metrics']==initial['metrics'],'viewport changed during dispatch')
        require(observed['receiver_rect']==initial['receiver_rect'],'receiver geometry changed during key')
        if args.native_click_first:
            require(observed['click_events']==click_observed['click_events'] and
                    observed['click_unrelated']==0 and observed['click_overflow'] is False,
                    'click evidence changed during key')
        require(status()['windows']==[window],'native window changed during dispatch')
        # Explicit replay negative: the supervisor must refuse a second frame, not post it.
        child.stdin.write(encode_plan(plan)); child.stdin.flush()
        wait(lambda:status().get('key_replays_refused')==1,5)
        require(status()['key_result']==native,'replay replaced original dispatch evidence')
        require(evaluate('keyFixtureState()')==observed,'replay changed keyboard evidence')
        report['checks']['replay_refused']=True
        report['checks']['key_replay_refused']=True
        require(assessment['passed'],'native dispatch/DOM keyboard effect did not verify')
        report['passed']=True
    except Exception as error:
        report['error']=str(error)
    finally:
        if cdp:
            report['cleanup']['browser_close_request']=request_owned_browser_close(cdp,cdp_owned)
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
                        close_error=close_control_input(child)
                        if close_error:
                            report['cleanup']['input_close_error']=close_error
                child.wait(timeout=20)
                require(status_path.exists() and status_path.stat().st_size<=16384,'final status unavailable')
                final=raw.strict_json(status_path.read_text())
                retain_final_evidence(report,final,child.pid,receiver)
                require(final.get('supervisor_pid')==child.pid and final.get('launch_finished') is True
                        and final.get('browser_pid',0)>0 and final.get('browser_terminated') is True,'browser cleanup unconfirmed')
                report['cleanup'].update(browser_terminated=True,supervisor_reaped=True,exit_code=child.returncode)
                if args.native_click_first:
                    require(final.get('click_result')==report.get('native_click_dispatch'),
                            'final click dispatch missing or contradictory')
                    require(final.get('click_plan')==report.get('native_click_plan'),
                            'final click plan missing or contradictory')
                    require(final.get('click_receipt')==report.get('click_receipt'),
                            'final click acknowledgement missing or contradictory')
                if report['checks'].get('click_replay_refused'):
                    require(final.get('click_replays_refused')==1,'final click replay evidence missing')
                if report['checks'].get('key_replay_refused'):
                    require(final.get('key_replays_refused')==1,'final key replay evidence missing')

                report['after']=final.get('observation')
                report['desktop_observation_assessment']=assess_desktop(report.get('before'),report['after'],
                                                                       final['browser_pid'],final.get('browser_ever_front'))
                require(final.get('browser_ever_front') is False,'browser observed foreground during run')
                require(receiver is None or final['browser_pid']==receiver,'final browser identity changed')
                require(all(v is not None for v in report['desktop_observation_assessment']['sampled_changes'].values()),'final desktop observation incomplete')
                require(child.returncode==0,'supervisor failed')
            except Exception as error:
                report['cleanup']['error']=str(error);report['passed']=False
            finally:
                # Reaping the supervisor is not proof that its browser exited.
                # Keep the profile unless native browser cleanup was observed.
                try:
                    if child.poll() is None:
                        report['passed']=False
                    reaped=reap_supervisor(child,report['cleanup'].get('browser_terminated') is True)
                    report['cleanup']['supervisor_reaped']=reaped
                    if not reaped:
                        report['cleanup']['native_cleanup_owner_retained']=True
                        report['cleanup']['supervisor_pid']=child.pid
                        report['passed']=False
                    report['cleanup']['exit_code']=child.returncode
                except Exception as error:
                    report['cleanup']['reap_error']=str(error)
                    report['passed']=False
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
