#!/usr/bin/env python3
"""Opt-in owned-monitor HTTP pointer acceptance; no CDP/native-fixture input.

Only an explicit Chrome for Testing bundle and new private profile are used.
A successful post is not a verified click or scroll. Native dispatch, independent DOM
observations, cleanup and sampled desktop changes are reported separately.
Each scroll sign needs a separate invocation with a fresh fixture/profile.
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
from macos_scroll_evidence import (INITIAL_SCROLL_TOP, assess_scroll, certain_refusal,
                                   matches_window_observation, parse_scroll_delta)


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


def assess_bound(plan, native, state, nonce):
    """Independent DOM effect evidence; never invent native tag/PID receipts."""
    result = {'passed': False, 'effect_verified': False, 'dom_tag_correlation': False,
              'continuous_isolation_verified': False}
    if not all(isinstance(x, dict) for x in (plan, native, state)):
        return result
    action = native.get('action')
    if (native.get('ok') is not True or not isinstance(action, dict)
            or action.get('status') != 'dispatched' or action.get('effect_verified') is not False
            or action.get('action_attempted') is not True or action.get('effects_unconfirmed') is not True
            or action.get('focus_interference') is not False or action.get('detail') is not None
            or type(action.get('posting_calls')) is not int or action['posting_calls'] != 2):
        return result
    for field, keys in [('point', ('local_x', 'local_y')), ('global', ('x', 'y'))]:
        point = action.get(field)
        if (not isinstance(point, dict) or any(not number(point.get(a)) or not number(plan.get(b))
                or abs(point[a] - plan[b]) > .001 for a, b in zip(('x', 'y'), keys))):
            return result
    events = state.get('events')
    if (not isinstance(nonce, str) or len(nonce) != 32 or any(c not in '0123456789abcdef' for c in nonce)
            or state.get('nonce') != nonce or type(state.get('clicks')) is not int or state['clicks'] != 1
            or state.get('overflow') is not False or type(state.get('unrelated')) is not int
            or state['unrelated'] != 0 or not isinstance(events, list) or len(events) != 3):
        return result
    for event, kind, buttons in zip(events, ('mousedown', 'mouseup', 'click'), (1, 0, 0)):
        if (not isinstance(event, dict) or event.get('type') != kind or event.get('trusted') is not True
                or type(event.get('button')) is not int or event['button'] != 0
                or type(event.get('buttons')) is not int or event['buttons'] != buttons):
            return result
        for a, b in [('client_x', 'client_x'), ('client_y', 'client_y'), ('screen_x', 'x'), ('screen_y', 'y')]:
            if not number(event.get(a)) or not number(plan.get(b)) or abs(event[a] - plan[b]) > 1:
                return result
    result.update(passed=True, effect_verified=True)
    return result


def parse_args(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin', required=True, type=Path)
    parser.add_argument('--port', required=True, type=int)
    parser.add_argument('--monitor', required=True)
    parser.add_argument('--browser-app',required=True,type=Path)
    parser.add_argument('--supervisor',required=True,type=Path)
    parser.add_argument('--report',required=True,type=Path)
    parser.add_argument('--allow-disposable-chromium',action='store_true')
    parser.add_argument('--scroll-delta', type=parse_scroll_delta, help='Instead test one vertical wheel event; positive down, nonzero abs <= 600')
    args=parser.parse_args(argv)
    if platform.system()!='Darwin' or not args.allow_disposable_chromium:
        parser.error('requires macOS and explicit disposable Chromium opt-in')
    return args


def main():
    args=parse_args()
    scrolling=args.scroll_delta is not None
    prepare_tool='prepare_macos_window_scroll' if scrolling else 'prepare_macos_window_click'
    dispatch_tool='scroll_macos_window' if scrolling else 'click_macos_window'
    binary=args.bin.resolve(strict=True)
    require(1 <= args.port <= 65535 and args.monitor.startswith('macos_virtual:'), 'exact isolated daemon monitor/port required')
    require(not os.getenv('INTENDANT_MCP_URL'), 'refuse remote ambient daemon')
    bundle=args.browser_app.resolve(strict=True); supervisor=args.supervisor.resolve(strict=True)
    info=plistlib.loads((bundle/'Contents/Info.plist').read_bytes())
    require(info['CFBundleIdentifier']=='com.google.chrome.for.testing','only Chrome for Testing accepted')
    fd=raw.reserve_report(args.report)
    report={'passed':False,'uses_daemon_tool':True,'browser_version':info['CFBundleShortVersionString'],
            'checks':{},'cleanup':{},'on_virtual_monitor':True}
    if scrolling:
        report.update(scroll_delta_y=args.scroll_delta, negative_checks={}, preparations=[])
    root=None
    child=cdp=None; status_path=None; receiver=None; binding=None
    end=time.monotonic()+180
    def call(tool, **arguments):
        require(time.monotonic() < end, "HTTP fixture deadline; never retry uncertain input")
        data=transport.run_bounded([str(binary), "ctl", "--port", str(args.port), "--json",
            "tools", "call", tool, "--args", json.dumps(arguments)], min(end,time.monotonic()+25))
        return raw.strict_json(data)
    last_tick=-1; last_tick_time=time.monotonic()
    try:
        owned=call('list_macos_monitors').get('monitors',[])
        require(sum(m.get('display_target')==args.monitor for m in owned)==1,'exact monitor not owned by isolated daemon')
        owned_monitor=next(m for m in owned if m.get('display_target')==args.monitor)
        root=Path(tempfile.mkdtemp(prefix='intendant-chromium-pointer-'))
        status_path=root/'status.json'
        profile=root/'profile'; profile.mkdir(mode=0o700)
        page=(Path(__file__).resolve().parent.parent/('tests/fixtures/macos-monitor/browser-scroll.html' if scrolling else 'tests/fixtures/macos-monitor/browser-pointer.html')).as_uri()
        child=subprocess.Popen([str(supervisor),'--disposable-chromium',str(bundle),str(profile),str(status_path),page],
                               stdin=subprocess.PIPE,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        end=time.monotonic()+180
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
        report['before']=current['before']; report['receiver_pid']=receiver
        # The supervisor launches/observes Chromium; the daemon owns posting.
        report['supervisor_pid' if scrolling else 'sender_pid']=child.pid
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
        window=status()['windows'][0]
        attempts=[]
        candidate=None
        for _ in range(3):
            listed=call('list_macos_windows',pid=receiver);attempts.append(listed)
            candidates=[w for w in listed.get('candidates',[]) if w['identity']['window_id']==window['window_id']]
            if len(candidates)==1:
                candidate=candidates[0];break
            # Read-only startup readiness; no mutation retries.
            time.sleep(.05)
        report['window_listing_attempts']=attempts
        require(candidate is not None,'exact disposable browser AX window unavailable')
        if scrolling:
            identity=candidate['identity']
            require(type(identity.get('pid')) is int and identity['pid']==receiver
                    and type(identity.get('window_id')) is int and identity['window_id']==window['window_id']
                    and type(identity.get('start_seconds')) is int and identity['start_seconds']>0
                    and type(identity.get('start_micros')) is int and 0<=identity['start_micros']<1000000,
                    'candidate process/window identity mismatch')
        bound=call('bind_macos_window',display_target=args.monitor,**{k:candidate[k] for k in ('candidate','identity')})
        report['binding']=bound;require(bound.get('ok') is True,bound)
        binding=bound['bound_window']['binding']
        if scrolling:
            require(isinstance(binding,str) and binding.startswith('macos_window:')
                    and bound['bound_window'].get('display_target')==args.monitor
                    and bound['bound_window'].get('identity')==identity,'bound identity/monitor mismatch')
        rectangle={'x':30,'y':30,'width':720,'height':530}
        placed=call('place_macos_window',binding=binding,bounds=rectangle)
        report['placement']=placed;require(placed.get('ok') is True,placed)
        expected=placed['placement']['requested_global']
        def placed_window():
            rows=status().get('windows',[])
            return len(rows)==1 and rows[0]['window_id']==window['window_id'] and all(
                abs(rows[0]['bounds'][a]-expected[b])<=1 for a,b in [('X','x'),('Y','y'),('Width','width'),('Height','height')])
        wait(placed_window,5)
        window=status()['windows'][0];bounds=window['bounds']
        browser_bounds=cdp.call('Browser.getWindowForTarget',{'targetId':target})['bounds']
        require(all(abs(browser_bounds[a]-bounds[b])<=1 for a,b in
                    [('left','X'),('top','Y'),('width','Width'),('height','Height')]),'independent geometry mismatch')
        initial=evaluate('pointerFixtureState()'); report['initial']=initial
        require(initial['wheels' if scrolling else 'clicks']==0 and initial['events']==[] and initial['nonce'] is None,'fixture not pristine')
        if scrolling:
            require(type(initial['wheels']) is int and initial['scroll_top']==INITIAL_SCROLL_TOP
                    and number(initial['scroll_top']) and number(initial['scroll_left'])
                    and initial['scroll_left']==0 and type(initial.get('unrelated')) is int and initial['unrelated']==0
                    and initial.get('overflow') is False and initial.get('ready')=='complete','scroll fixture not pristine')
        r=initial['metrics']['canvas']; cx=round(r['x']+r['width']/2)+secrets.randbelow(31)-15
        cy=round(r['y']+r['height']/3)+secrets.randbelow(31)-15
        plan=plan_pointer(initial['metrics'],bounds,window['window_id'],cx,cy,secrets.randbelow(2**63-1)+1)
        report['plan']=plan; nonce=secrets.token_hex(16)
        point={'x':plan['local_x'],'y':plan['local_y']}

        def unchanged(before, label, evidence=None):
            # Observation only: allow delayed unexpected input to become visible.
            stop=min(end,time.monotonic()+.25)
            while True:
                require(status()['windows']==[window],label+': native window changed')
                after=evaluate('pointerFixtureState()')
                if evidence is not None: evidence['after']=after
                require(after==before,label+': semantic snapshot changed')
                if time.monotonic()>=stop: return after
                time.sleep(.05)

        def refused(label, tool, dispatch=True, **arguments):
            before=evaluate('pointerFixtureState()') if scrolling else None
            receipt=call(tool,**arguments)
            if scrolling:
                evidence={'tool':tool,'before':before,'receipt':receipt}
                report['negative_checks'][label]=evidence
                evidence['certain_refusal']=certain_refusal(receipt,dispatch=dispatch)
                unchanged(before,label,evidence)
                require(evidence['certain_refusal'],receipt)
            else:
                require(receipt.get('ok') is False and
                        (not dispatch or receipt.get('action_attempted') is False),receipt)
            return receipt

        baseline=initial
        if scrolling:
            # Arm once before negatives, so a wrongly accepted cross-kind click
            # cannot disappear just because the positive scroll has not begun.
            require(evaluate('armPointer('+json.dumps(nonce)+')') is True,'fixture arm failed')
            baseline=dict(initial,nonce=nonce)
            report['armed']=unchanged(baseline,'arming')

        def prepare(tool=prepare_tool):
            delta={'delta_y':args.scroll_delta} if tool=='prepare_macos_window_scroll' else {}
            prepared=call(tool,binding=binding,point=point,**delta)
            report['last_preparation']=prepared
            if scrolling: report['preparations'].append({'tool':tool,'receipt':prepared})
            require(prepared.get('ok') is True,prepared)
            require(prepared['prepared']['global']=={'x':plan['x'],'y':plan['y']},'prepared global coordinate mismatch')
            if scrolling:
                p=prepared['prepared']; token=p.get('token')
                require(prepared.get('error') is None and prepared.get('action') is None
                        and prepared.get('action_attempted') is False
                        and prepared.get('effects_unconfirmed',False) is False
                        and p.get('point')==point and matches_window_observation(p.get('window'),bounds)
                        and all(number(p[field].get(axis)) for field in ('point','global') for axis in ('x','y'))
                        and type(p.get('expires_in_ms')) is int and p['expires_in_ms']==10000
                        and isinstance(token,str) and token.startswith('macos_pointer:')
                        and len(token)==46 and all(c in '0123456789abcdef' for c in token[14:]),
                        'incomplete or mismatched preparation')
                if delta:
                    require(type(p.get('delta_y')) is int and p['delta_y']==args.scroll_delta,'prepared delta mismatch')
                else:
                    require('delta_y' not in p,'click preparation returned scroll fields')
            return prepared['prepared']['token']
        old=prepare();fresh=prepare()
        if scrolling: require(old!=fresh,'refresh reused a preparation token')
        refused('refresh_old_token',dispatch_tool,binding=binding,token=old)
        refused('refresh_consumed_fresh_token',dispatch_tool,binding=binding,token=fresh)
        require(evaluate('pointerFixtureState()')==baseline,'rejected preparation changed fixture')
        report['checks']['refresh_and_consumed_snapshot_refused']=True
        refused('out_of_bounds',prepare_tool,dispatch=False,binding=binding,point={'x':720,'y':10},
                **({'delta_y':args.scroll_delta} if scrolling else {}))
        report['checks']['out_of_bounds_refused']=True
        if scrolling:
            for bad in (0,601,-601):
                refused('delta_'+str(bad),prepare_tool,dispatch=False,binding=binding,point=point,delta_y=bad)
            token=prepare()
            refused('scroll_token_into_click','click_macos_window',binding=binding,token=token)
            refused('scroll_token_consumed',dispatch_tool,binding=binding,token=token)
            click_token=prepare('prepare_macos_window_click')
            refused('click_token_into_scroll',dispatch_tool,binding=binding,token=click_token)
            refused('click_token_consumed','click_macos_window',binding=binding,token=click_token)
            require(evaluate('pointerFixtureState()')==baseline,'cross-kind negative dispatched input')
            report['checks']['delta_bounds_and_cross_kind_refused']=True
            owned_now=call('list_macos_monitors').get('monitors',[])
            require([m for m in owned_now if m.get('display_target')==args.monitor]==[owned_monitor],
                    'owned monitor generation changed before dispatch')
        token=prepare()
        if not scrolling:
            require(evaluate('armPointer('+json.dumps(nonce)+')') is True,'fixture arm failed')
        else:
            require(evaluate('pointerFixtureState()')==baseline,'preparation changed scroll fixture')
        require(evaluate('pointerFixtureState().metrics')==initial['metrics'],'viewport changed before dispatch')
        require(status()['windows']==[window],'native window changed before dispatch')
        # The supervisor is launched in semantic-only mode and receives no input
        # commands. The tested input MUST go through ctl -> HTTP -> owned helper.
        native=(call('scroll_macos_window',binding=binding,token=token) if scrolling else
                call('click_macos_window',binding=binding,token=token))
        report['native_dispatch']=native
        stop=time.monotonic()+2
        observed=evaluate('pointerFixtureState()')
        # Observe the entire scroll window, including after the first correct
        # effect, so a delayed second wheel cannot masquerade as a single wheel.
        while (scrolling or len(observed['events'])<3) and time.monotonic()<stop:
            if scrolling: require(status()['windows']==[window],'native window changed during scroll observation')
            time.sleep(.05);observed=evaluate('pointerFixtureState()')
        report['dom_after']=observed
        assessment=(assess_scroll(plan,native,observed,nonce,args.scroll_delta,initial['scroll_top']) if scrolling else assess_bound(plan,native,observed,nonce));report['assessment']=assessment
        require(assessment['passed'],'HTTP dispatch/independent DOM evidence did not verify')
        require(observed['metrics']==initial['metrics'],'viewport changed during dispatch')
        require(status()['windows']==[window],'native window changed during dispatch')
        refused('replay',dispatch_tool,binding=binding,token=token)
        require(evaluate('pointerFixtureState()')==observed,'replay changed canvas evidence')
        report['checks']['replay_refused']=True
        token=prepare()
        placement_before=evaluate('pointerFixtureState()') if scrolling else None
        noop=call('place_macos_window',binding=binding,bounds=rectangle)
        require(noop.get('ok') is True and noop['placement']['writes_attempted']==0,noop)
        if scrolling:
            report['noop_placement']=noop
            evidence={'before':placement_before,'receipt':noop}
            report['negative_checks']['no_op_placement']=evidence
            require(type(noop['placement']['writes_attempted']) is int
                    and noop['placement'].get('status')=='verified'
                    and noop['placement'].get('focus_interference') is False
                    and 'detail' in noop['placement'] and noop['placement']['detail'] is None
                    and all(matches_window_observation(noop['placement'].get(field),bounds)
                            for field in ('before','after')),noop)
            unchanged(placement_before,'no-op placement',evidence)
        refused('placement_invalidated_token',dispatch_tool,binding=binding,token=token)
        report['checks']['placement_invalidated_preparation']=True
        token=prepare()
        unbind_before=evaluate('pointerFixtureState()') if scrolling else None
        unbound=call('unbind_macos_window',binding=binding)
        require(unbound.get('ok') is True,unbound)
        if scrolling:
            report['unbind']=unbound
            require(unbound.get('unbound') is True,unbound)
            evidence={'before':unbind_before,'receipt':unbound}
            report['negative_checks']['unbind']=evidence
            unchanged(unbind_before,'unbind',evidence)
        refused('unbound_token',dispatch_tool,binding=binding,token=token)
        binding=None
        report['checks']['unbind_refused']=True
        require(evaluate('pointerFixtureState()')==observed,'negative checks changed canvas')
        if scrolling:
            owned_after=call('list_macos_monitors').get('monitors',[])
            require([m for m in owned_after if m.get('display_target')==args.monitor]==[owned_monitor],
                    'owned monitor generation changed during scroll run')
            unchanged(observed,'final scroll evidence')
        report['passed']=True
    except Exception as error:
        report['error']=str(error)
    finally:
        end=time.monotonic()+30
        if binding:
            try:
                unbound=call('unbind_macos_window',binding=binding)
                report['cleanup']['unbind']=unbound
                require(unbound.get('ok') is True,unbound)
            except Exception as error:
                report['cleanup']['unbind_error']=str(error);report['passed']=False
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
                report['cleanup'].update(browser_terminated=True,supervisor_reaped=True,exit_code=child.returncode)
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
