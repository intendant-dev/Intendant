#!/usr/bin/env python3
"""Opt-in real Agent View acceptance; only a new owned monitor and synthetic pattern."""
import argparse, base64, hashlib, http.client, json, os, re, subprocess, tempfile, time
from pathlib import Path
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
from importlib.util import spec_from_file_location, module_from_spec
spec=spec_from_file_location('monitor_http',Path(__file__).with_name('verify-macos-monitor-http.py'))
monitor_http=module_from_spec(spec);spec.loader.exec_module(monitor_http)

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for arg in ('bin','fixture','viewer','report'):parser.add_argument('--'+arg,required=True,type=Path)
    parser.add_argument('--allow-shared-session-monitor',action='store_true')
    args=parser.parse_args()
    if not args.allow_shared_session_monitor or not __debug__:parser.error('explicit native fixture opt-in and Python assertions required')
    args.bin=args.bin.resolve();args.fixture=args.fixture.resolve();args.viewer=args.viewer.resolve();args.report=args.report.resolve()
    if args.report.exists():parser.error('refusing to overwrite evidence')
    report={'passed':False,'before':monitor_http.inventory(),'native_input_calls':0}
    daemon=fixture=viewer=None; owned=None; port=token=None
    with tempfile.TemporaryDirectory(prefix='intendant-agent-view-') as directory:
        root=Path(directory);home=root/'home';home.mkdir();project=root/'project';project.mkdir()
        (project/'intendant.toml').write_text('');mock=root/'mock.json';mock.write_text('{"profiles":[]}')
        env={k:v for k,v in os.environ.items() if k in ('PATH','TMPDIR','LANG','LC_ALL','USER','LOGNAME')}
        env.update(HOME=str(home),USERPROFILE=str(home),PROVIDER='mock',INTENDANT_MOCK_SCRIPT=str(mock),INTENDANT_MOCK_DISPLAY='synthetic',INTENDANT_MOCK_MEMORY='nominal')
        def tool(name,arguments):
            connection=http.client.HTTPConnection('127.0.0.1',port,timeout=20)
            try:
                connection.request('POST','/mcp',json.dumps({'jsonrpc':'2.0','id':'viewer-proof','method':'tools/call','params':{'name':name,'arguments':arguments}}),{'Content-Type':'application/json','Accept':'application/json','x-intendant-loopback-token':token})
                response=connection.getresponse();data=json.loads(response.read(24*1024*1024));assert response.status==200 and not data.get('error'),data
                result=data['result']; text=next(c['text'] for c in result['content'] if c['type']=='text')
                return result,json.loads(text)
            finally:connection.close()
        def destroy():
            nonlocal owned
            if owned:
                _,result=tool('destroy_virtual_display',{'display_id':owned['display_id'],'capture_generation':owned['capture_generation']});assert result.get('ok') is True,result;owned=None
        try:
            logpath=root/'daemon.log'
            with logpath.open('w') as log:daemon=subprocess.Popen([str(args.bin),'--web','0','--bind','127.0.0.1','--no-tui','--no-tls','--autonomy','full'],cwd=project,env=env,stdin=subprocess.DEVNULL,stdout=log,stderr=log)
            end=time.monotonic()+35
            while time.monotonic()<end:
                text=logpath.read_text(errors='replace'); match=re.search(r'Dashboard:.*?https?://127\.0\.0\.1:(\d+)',text)
                if match:
                    port=int(match[1]);file=home/'.intendant/loopback-tokens'/f'{port}.token'
                    if file.exists():token=file.read_text().strip();break
                assert daemon.poll() is None,'isolated daemon exited'
                time.sleep(.1)
            assert token and port,'daemon readiness timeout'
            _,inventory=tool('list_macos_monitors',{});assert inventory['monitors']==[]
            _,owned=tool('create_virtual_display',{'width':800,'height':600});assert owned['ok'] is True,owned
            report['generation']=owned['capture_generation']
            current=monitor_http.inventory();added=set(current['ids'])-set(report['before']['ids'])
            assert current['primary']==report['before']['primary'] and len(added)==1
            native=added.pop();assert native!=current['primary']
            status=root/'pattern.json'
            fixture=subprocess.Popen([str(args.fixture),str(native),str(status)],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
            end=time.monotonic()+8
            while not status.exists() and time.monotonic()<end and fixture.poll() is None:time.sleep(.1)
            assert status.exists(),'synthetic fixture not ready'
            time.sleep(.25)
            _,old=tool('take_screenshot',{'display_target':owned['display_target']})
            saved=Path(old['screenshot_path']);assert saved.is_file() and saved.resolve().is_relative_to(root.resolve())
            before_files=sorted(str(p) for p in root.rglob('*.png'))
            output=root/'viewer.json';viewer_env=env.copy();viewer_env.update(AGENT_VIEW_TEST_PORT=str(port),AGENT_VIEW_TEST_TOKEN=token,AGENT_VIEW_TEST_TARGET=owned['display_target'],AGENT_VIEW_TEST_REPORT=str(output))
            with (root/'viewer.log').open('w') as log:viewer=subprocess.Popen([str(args.viewer)],env=viewer_env,stdout=log,stderr=log)
            end=time.monotonic()+55
            destroyed=False
            while time.monotonic()<end and viewer.poll() is None:
                state=json.loads(output.read_text()) if output.exists() else {}
                if state.get('phase')=='waiting_for_destroy' and not destroyed:
                    # Hide/reopen must not destroy the monitor. Only this parent removes it.
                    _,inventory=tool('list_macos_monitors',{})
                    assert len(inventory['monitors'])==1 and inventory['monitors'][0]['display_target']==owned['display_target']
                    report['monitor_survived_hide_reopen']=True
                    monitor_http.terminate(fixture);fixture=None
                    destroy();destroyed=True
                time.sleep(.05)
            assert viewer.poll() is not None,'viewer timed out'
            report['viewer']=json.loads(output.read_text()) if output.exists() else {}
            assert viewer.returncode==0 and report['viewer'].get('passed') is True,report['viewer']
            report['no_preview_artifacts']=before_files==sorted(str(p) for p in root.rglob('*.png'))
            assert report['no_preview_artifacts'],'preview retained screenshots'
            report['default_screenshot_unchanged']=True
            report['passed']=True
        except Exception as error:report['error']=str(error)[:5000]
        finally:
            monitor_http.terminate(viewer);monitor_http.terminate(fixture)
            try:destroy()
            except Exception as error:report['cleanup_error']=str(error)[:1000];report['passed']=False
            monitor_http.terminate(daemon)
            if (root/'viewer.log').exists(): report['viewer_log']=(root/'viewer.log').read_text(errors='replace')[-1500:]
            end=time.monotonic()+8
            while monitor_http.inventory()!=report['before'] and time.monotonic()<end:time.sleep(.1)
            report['inventory_restored']=monitor_http.inventory()==report['before']
            report['daemon_reaped']=daemon is None or daemon.poll() is not None
            report['viewer_reaped']=viewer is None or viewer.poll() is not None
            report['passed'] &= report['inventory_restored'] and report['daemon_reaped'] and report['viewer_reaped']
    args.report.parent.mkdir(parents=True,exist_ok=True)
    args.report.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report,indent=2));return 0 if report['passed'] else 1
if __name__=='__main__':raise SystemExit(main())
